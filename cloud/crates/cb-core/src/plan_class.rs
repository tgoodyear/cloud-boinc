//! Plan classes: which hosts may run an app version, and what it uses there.
//!
//! A port of `sched/plan_class_spec.cpp` (`PLAN_CLASS_SPEC::parse`,
//! `PLAN_CLASS_SPEC::check`, `PLAN_CLASS_SPECS::parse_specs`, `check` and
//! `wu_is_infeasible`). The empty plan class is a sequential CPU app, as
//! `HOST_USAGE::sequential_app` in `sched/sched_types.h` describes it. The
//! plan classes hard-coded in `sched/sched_customize.cpp` are not ported: a
//! name that is not in the spec file is rejected.
//!
//! Where this differs from upstream:
//!
//! - The job gates (`min_wu_id`, `max_wu_id`, `min_batch`, `max_batch`) are
//!   split off into `admits_workunit`, so the host check needs no workunit.
//! - Upstream reads the host's venue from the database to pick a section of
//!   the project preferences. No venue is available here, so the settings
//!   outside any `<venue>` element apply.
//! - `capped_host_fpops` in `sched/sched_types.cpp` caps the host's
//!   benchmark with project-wide percentiles from shared memory. There are
//!   no such statistics here; the benchmark is used as reported.
//! - `config.max_ncpus` is not available to the check; its default applies.
//! - Upstream records GPU requirements (`gpu_requirements`) for messages to
//!   the volunteer. Nothing is recorded here.
//! - Patterns are compiled by the `regex` crate, not as POSIX extended
//!   regular expressions. The common syntax behaves the same.
//! - `host_summary_regex` is not in current upstream; it is matched against
//!   the stored host summary the way older upstream versions did.

use cb_proto::consts::{
    PROC_TYPE_AMD_GPU, PROC_TYPE_APPLE_GPU, PROC_TYPE_CPU, PROC_TYPE_INTEL_GPU,
    PROC_TYPE_NVIDIA_GPU,
};
use cb_proto::coproc::{Coproc, OpenclDeviceProp};
use cb_proto::xml::{extract_venue, line, XmlParser};
use cb_proto::SchedulerRequest;
use regex::{Regex, RegexBuilder};

use crate::config::MAX_NCPUS;
use crate::{CoreError, CoreResult};

const MEGA: f64 = 1_048_576.0;
const GIGA: f64 = 1024.0 * MEGA;

/// `MAX_CPUS` in `sched/sched_send.h`.
const MAX_CPUS: i32 = 4096;
/// `DEFAULT_RAM_SIZE` in `sched/sched_send.cpp`, for hosts that report none.
const DEFAULT_RAM_SIZE: f64 = 4.0 * GIGA;
/// `GPU_MAX_PEAK_FLOPS` in `lib/coproc.h`.
const GPU_MAX_PEAK_FLOPS: f64 = 1e15;
/// `GPU_DEFAULT_PEAK_FLOPS` in `lib/coproc.h`.
const GPU_DEFAULT_PEAK_FLOPS: f64 = 100e9;
/// `BOINC_WSL_DISTRO_NAME` in `lib/wslinfo.h`.
const BOINC_WSL_DISTRO_NAME: &str = "boinc-buda-runner";
/// Benchmark assumed when the host reports none (`sequential_app`).
const DEFAULT_FPOPS: f64 = 1e9;

/// Resource usage of an app version on a host (upstream `HOST_USAGE`).
#[derive(Debug, Clone, PartialEq)]
pub struct HostUsage {
    /// `PROC_TYPE_CPU`, `PROC_TYPE_NVIDIA_GPU`, ...
    pub proc_type: i32,
    /// GPU instances used; 0 for CPU apps.
    pub gpu_usage: f64,
    /// Bytes of GPU RAM used.
    pub gpu_ram: f64,
    pub avg_ncpus: f64,
    /// Bytes of RAM, when the plan class computes it; overrides the
    /// workunit's memory bound. 0 otherwise.
    pub mem_usage: f64,
    /// Best estimate of the job's speed on this host.
    pub projected_flops: f64,
    /// Stored as the result's FLOPS estimate and used for credit.
    pub peak_flops: f64,
    /// Arguments to add to the app version's command line.
    pub cmdline: String,
}

impl HostUsage {
    /// One CPU, running at the host's benchmark speed
    /// (`HOST_USAGE::sequential_app`).
    pub fn sequential(p_fpops: f64) -> Self {
        let flops = if p_fpops <= 0.0 {
            DEFAULT_FPOPS
        } else {
            p_fpops
        };
        HostUsage {
            proc_type: PROC_TYPE_CPU,
            gpu_usage: 0.0,
            gpu_ram: 0.0,
            avg_ncpus: 1.0,
            mem_usage: 0.0,
            projected_flops: flops,
            peak_flops: flops,
            cmdline: String::new(),
        }
    }

    pub fn uses_gpu(&self) -> bool {
        self.proc_type != PROC_TYPE_CPU
    }
}

/// What a plan class check may look at besides the request itself.
pub struct PlanHost<'a> {
    pub request: &'a SchedulerRequest,
    pub user_id: i64,
    pub host_id: i64,
    /// The user's project preferences XML (may be empty).
    pub project_prefs: &'a str,
    /// Stored host summary string used by host_summary_regex (may be empty).
    pub host_serialnum: &'a str,
    /// Uniform random number in [0,1) for infeasible_random.
    pub random: f64,
}

/// Why a host cannot use a plan class.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanReject {
    /// For the scheduler's log.
    pub reason: String,
    /// What upstream tells the volunteer (`add_no_work_message`), for the
    /// rejections where it says anything.
    pub user_message: Option<&'static str>,
}

fn reject<T>(reason: impl Into<String>) -> Result<T, PlanReject> {
    Err(PlanReject {
        reason: reason.into(),
        user_message: None,
    })
}

fn reject_msg<T>(message: &'static str) -> Result<T, PlanReject> {
    Err(PlanReject {
        reason: message.to_string(),
        user_message: Some(message),
    })
}

/// One `<plan_class>` of the spec file (upstream `PLAN_CLASS_SPEC`).
///
/// Numeric limits are off when 0, as upstream.
#[derive(Debug, Clone)]
pub struct PlanClassSpec {
    pub name: String,
    /// `nvidia`, `amd`, `ati`, `intel...`, `apple`, or a coprocessor type
    /// name as the client reports it.
    pub gpu_type: String,
    pub cuda: bool,
    pub cal: bool,
    pub opencl: bool,
    pub virtualbox: bool,
    pub wsl: bool,
    pub docker: bool,
    /// On Windows, the oldest usable version of the BOINC WSL distro.
    pub min_boinc_wsl_distro_version: i32,
    pub is64bit: bool,
    /// Required CPU features, as written in the spec.
    pub cpu_features: Vec<String>,
    pub min_ncpus: f64,
    pub max_threads: i32,
    /// Bytes.
    pub mem_usage_base: f64,
    /// Bytes.
    pub mem_usage_per_cpu: f64,
    pub nthreads_cmdline: bool,
    pub projected_flops_scale: f64,
    pub os_regex: Option<Regex>,
    pub cpu_vendor_regex: Option<Regex>,
    pub cpu_model_regex: Option<Regex>,
    pub host_summary_regex: Option<Regex>,
    pub min_os_version: f64,
    pub max_os_version: f64,
    pub min_android_version: i32,
    pub max_android_version: i32,
    /// `100 * major + minor`; with `wsl`, applies to the WSL distro too.
    pub min_libc_version: i32,
    pub project_prefs_tag: String,
    pub project_prefs_regex: Option<Regex>,
    pub project_prefs_default_true: bool,
    /// CPU usage override: for apps that are not compute intensive, or for
    /// GPU apps.
    pub avg_ncpus: f64,
    pub min_core_client_version: i32,
    pub max_core_client_version: i32,
    pub user_id: i64,
    pub infeasible_random: f64,
    pub min_wu_id: i64,
    pub max_wu_id: i64,
    pub min_batch: i64,
    pub max_batch: i64,

    // GPU apps
    pub cpu_frac: f64,
    pub min_gpu_ram_mb: f64,
    pub gpu_ram_used_mb: f64,
    pub gpu_peak_flops_scale: f64,
    pub ngpus: f64,
    pub min_driver_version: i32,
    pub max_driver_version: i32,
    /// Project preferences tag holding the user's GPU utilization factor.
    pub gpu_utilization_tag: String,
    pub min_gpu_peak_flops: f64,
    pub max_gpu_peak_flops: f64,
    pub gpu_type_regex: Option<Regex>,

    // AMD/ATI apps
    pub need_ati_libs: bool,
    pub need_amd_libs: bool,
    pub min_amd_simd_width: i32,
    pub min_cal_target: i32,
    pub max_cal_target: i32,
    /// Restrict to GPUs without OpenCL.
    pub without_opencl: bool,

    // NVIDIA apps
    pub min_nvidia_compcap: i32,
    pub max_nvidia_compcap: i32,
    pub min_cuda_version: i32,
    pub max_cuda_version: i32,

    // OpenCL apps
    pub min_opencl_version: i32,
    pub max_opencl_version: i32,
    pub min_opencl_driver_revision: i32,
    pub max_opencl_driver_revision: i32,
    pub double_precision_fp: bool,

    // Apple GPU
    pub min_metal_support: i32,

    // VirtualBox apps
    pub min_vbox_version: i32,
    pub max_vbox_version: i32,
    pub exclude_vbox_version: Vec<i32>,
    pub vm_accel_required: bool,
}

impl Default for PlanClassSpec {
    /// The defaults of the upstream constructor.
    fn default() -> Self {
        PlanClassSpec {
            name: String::new(),
            gpu_type: String::new(),
            cuda: false,
            cal: false,
            opencl: false,
            virtualbox: false,
            wsl: false,
            docker: false,
            min_boinc_wsl_distro_version: 0,
            is64bit: false,
            cpu_features: Vec::new(),
            min_ncpus: 0.0,
            max_threads: 1,
            mem_usage_base: 0.0,
            mem_usage_per_cpu: 0.0,
            nthreads_cmdline: false,
            projected_flops_scale: 1.0,
            os_regex: None,
            cpu_vendor_regex: None,
            cpu_model_regex: None,
            host_summary_regex: None,
            min_os_version: 0.0,
            max_os_version: 0.0,
            min_android_version: 0,
            max_android_version: 0,
            min_libc_version: 0,
            project_prefs_tag: String::new(),
            project_prefs_regex: None,
            project_prefs_default_true: false,
            avg_ncpus: 0.0,
            min_core_client_version: 0,
            max_core_client_version: 0,
            user_id: 0,
            infeasible_random: 0.0,
            min_wu_id: 0,
            max_wu_id: 0,
            min_batch: 0,
            max_batch: 0,
            cpu_frac: 0.1,
            min_gpu_ram_mb: 0.0,
            gpu_ram_used_mb: 0.0,
            gpu_peak_flops_scale: 1.0,
            ngpus: 1.0,
            min_driver_version: 0,
            max_driver_version: 0,
            gpu_utilization_tag: String::new(),
            min_gpu_peak_flops: 0.0,
            max_gpu_peak_flops: 0.0,
            gpu_type_regex: None,
            need_ati_libs: false,
            need_amd_libs: false,
            min_amd_simd_width: 0,
            min_cal_target: 0,
            max_cal_target: 0,
            without_opencl: false,
            min_nvidia_compcap: 0,
            max_nvidia_compcap: 0,
            min_cuda_version: 0,
            max_cuda_version: 0,
            min_opencl_version: 0,
            max_opencl_version: 0,
            min_opencl_driver_revision: 0,
            max_opencl_driver_revision: 0,
            double_precision_fp: false,
            min_metal_support: 0,
            min_vbox_version: 0,
            max_vbox_version: 0,
            exclude_vbox_version: Vec::new(),
            vm_accel_required: false,
        }
    }
}

/// The upstream idiom `if (xp.parse_x("name", dst)) continue;`.
macro_rules! take {
    ($xp:ident . $method:ident ( $name:literal ) => $dst:expr) => {
        if let Some(v) = $xp.$method($name) {
            $dst = v;
            continue;
        }
    };
}

/// Up to `want` integers separated by single dots, read the way
/// `sscanf(s, "%d.%d.%d")` reads them. The caller checks how many it got.
fn scan_dotted(s: &str, want: usize) -> Vec<i64> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while out.len() < want {
        if !out.is_empty() {
            if b.get(i) != Some(&b'.') {
                break;
            }
            i += 1;
        }
        while b.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        let negative = match b.get(i) {
            Some(b'-') => {
                i += 1;
                true
            }
            Some(b'+') => {
                i += 1;
                false
            }
            _ => false,
        };
        let start = i;
        let mut v: i64 = 0;
        while let Some(c) = b.get(i).filter(|c| c.is_ascii_digit()) {
            // Clamped so that the version arithmetic of the callers cannot overflow.
            v = (v * 10 + i64::from(c - b'0')).min(i64::from(i32::MAX));
            i += 1;
        }
        if i == start {
            break;
        }
        out.push(if negative { -v } else { v });
    }
    out
}

/// A number for the OS version of Darwin, Windows, Android and Linux hosts,
/// so that ranges can be given; 0 when it cannot be worked out
/// (`os_version_num` in `sched/plan_class_spec.cpp`).
fn os_version_num(os_name: &str, os_version: &str) -> f64 {
    let three = |s: &str| match scan_dotted(s, 3)[..] {
        [a, b, c] => 10000.0 * a as f64 + 100.0 * b as f64 + c as f64,
        _ => 0.0,
    };
    if os_name.contains("Darwin") {
        three(os_version)
    } else if os_name.contains("Windows") {
        // Example: "Enterprise Server Edition, Service Pack 1, (06.01.7601.00)"
        let Some(at) = os_version.rfind('(') else {
            return 0.0;
        };
        match scan_dotted(&os_version[at + 1..], 4)[..] {
            [a, b, c, d] => {
                100_000_000.0 * a as f64 + 1_000_000.0 * b as f64 + 100.0 * c as f64 + d as f64
            }
            _ => 0.0,
        }
    } else if os_name.contains("Android") {
        three(os_version)
    } else if os_name.contains("Linux") {
        // The kernel version is either the whole string or inside brackets:
        // "Ubuntu 18.04.1 LTS [4.15.0-36-generic|libc 2.27 (...)]"
        match os_version.find('[') {
            Some(at) => three(&os_version[at + 1..]),
            None => three(os_version),
        }
    } else {
        0.0
    }
}

/// 227 for an OS version with `[...|libc 2.27 ...]`, else 0 (`libc_version`
/// in `sched/plan_class_spec.cpp`).
fn libc_version(os_version: &str) -> i64 {
    const MARK: &str = "|libc ";
    let Some(at) = os_version.find(MARK) else {
        return 0;
    };
    major_minor(&os_version[at + MARK.len()..])
}

/// `100 * major + minor` of a `major.minor` version, else 0
/// (`WSL_DISTRO::libc_version_int`).
fn major_minor(s: &str) -> i64 {
    match scan_dotted(s, 2)[..] {
        [major, minor] => major * 100 + minor,
        _ => 0,
    }
}

/// Version from `(Android 4.3.1)`, `(Android 4.3)` or `(Android 4)`, else 0
/// (`android_version_num`).
fn android_version_num(os_version: &str) -> i64 {
    const MARK: &str = "(Android ";
    let Some(at) = os_version.find(MARK) else {
        return 0;
    };
    match scan_dotted(&os_version[at + MARK.len()..], 3)[..] {
        [major, minor, release] => major * 10000 + minor * 100 + release,
        [major, minor] => major * 10000 + minor * 100,
        [major] => major * 10000,
        _ => 0,
    }
}

/// The OpenCL runtime revision times 100, from the last parenthesis of the
/// device version string; only AMD drivers put one there
/// (`OPENCL_DEVICE_PROP::get_opencl_driver_revision`).
fn opencl_driver_revision(device_version: &str) -> i64 {
    let rev = match device_version.rfind('(') {
        // Upstream does not look at a parenthesis that starts the string.
        Some(at) if at > 0 => leading_float(&device_version[at + 1..]),
        _ => 0.0,
    };
    (rev * 100.0 + 0.5).floor() as i64
}

/// The decimal number a string starts with, 0 if none.
fn leading_float(s: &str) -> f64 {
    let s = s.trim_start();
    let b = s.as_bytes();
    let mut i = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let mut seen_dot = false;
    while let Some(&c) = b.get(i) {
        if c == b'.' && !seen_dot {
            seen_dot = true;
        } else if !c.is_ascii_digit() {
            break;
        }
        i += 1;
    }
    s[..i].parse().unwrap_or(0.0)
}

/// `HOST::is_windows` in `db/boinc_db_types.h`.
fn is_windows(os_name: &str) -> bool {
    os_name.to_ascii_lowercase().contains("windows")
}

/// The number of CPUs the host lets BOINC use (`effective_ncpus`, computed in
/// `WORK_REQ::get_job_limits` in `sched/sched_send.cpp`). `prefs` is the
/// project preferences text that applies to the host.
fn effective_ncpus(req: &SchedulerRequest, prefs: &str) -> i32 {
    let mut n = req.host_info.p_ncpus;
    let pct = req.working_global_prefs.max_ncpus_pct;
    if pct != 0.0 && pct < 100.0 {
        n = (f64::from(n) * pct / 100.0) as i32;
    }
    n = n.clamp(1, MAX_NCPUS).min(MAX_CPUS);
    // PROJECT_PREFS::parse in sched/sched_types.cpp
    if let Some(max_cpus) = line::parse_int(prefs, "<max_cpus>") {
        if max_cpus != 0 && n > max_cpus {
            n = max_cpus;
        }
    }
    n
}

/// Bytes of RAM the host lets BOINC use (`get_mem_sizes` in
/// `sched/sched_send.cpp`).
fn usable_ram(req: &SchedulerRequest) -> f64 {
    let mut ram = req.host_info.m_nbytes;
    if ram <= 0.0 {
        ram = DEFAULT_RAM_SIZE;
    }
    let busy = req.working_global_prefs.ram_max_used_busy_frac;
    let idle = req.working_global_prefs.ram_max_used_idle_frac;
    if busy > 0.0 && idle > 0.0 {
        ram *= busy.max(idle).min(1.0);
    }
    ram
}

/// The host's CPU benchmark. Upstream `capped_host_fpops` also bounds it by
/// project-wide percentiles, which are not kept here; a missing benchmark
/// gets the same default as a sequential app.
fn host_fpops(req: &SchedulerRequest) -> f64 {
    let x = req.host_info.p_fpops;
    if x <= 0.0 {
        DEFAULT_FPOPS
    } else {
        x
    }
}

/// A computation alternates between two devices of speeds `s1` and `s2`,
/// with the fraction `frac` of the work done on the first. Returns the
/// overall speed and the utilization of the first device (`coproc_perf` in
/// `sched/sched_customize.h`).
fn coproc_perf(s1: f64, s2: f64, frac: f64) -> (f64, f64) {
    let y = frac * s2 + (1.0 - frac) * s1;
    (s1 * s2 / y, frac * s2 / y)
}

/// Peak FLOPS of a GPU, with the default in place of a value that cannot be
/// right (`COPROC::bad_gpu_peak_flops`).
fn gpu_peak_flops(cp: &Coproc) -> f64 {
    if cp.peak_flops <= 0.0 || cp.peak_flops > GPU_MAX_PEAK_FLOPS {
        GPU_DEFAULT_PEAK_FLOPS
    } else {
        cp.peak_flops
    }
}

/// Whether the CPU has hardware virtualization and it is enabled.
fn has_vm_acceleration(req: &SchedulerRequest) -> bool {
    let features = &req.host_info.p_features;
    (features.contains("vmx") || features.contains("svm"))
        && !req.host_info.p_vm_extensions_disabled
}

/// The GPU a plan class selected, with what the vendor-specific checks found.
struct SelectedGpu<'a> {
    coproc: &'a Coproc,
    ram: f64,
    /// 0 when unknown.
    driver_version: i64,
}

impl PlanClassSpec {
    /// Parse one class; the `<plan_class>` tag has been read
    /// (`PLAN_CLASS_SPEC::parse`). Tags that are not known are ignored.
    fn parse(xp: &mut XmlParser<'_>) -> CoreResult<PlanClassSpec> {
        let mut pc = PlanClassSpec::default();
        // Patterns are compiled at the end of the class, so that an error
        // can name the class whatever the order of the tags.
        let mut os_regex = None;
        let mut cpu_vendor_regex = None;
        let mut cpu_model_regex = None;
        let mut host_summary_regex = None;
        let mut project_prefs_regex = None;
        let mut gpu_type_regex = None;
        while xp.get_tag() {
            if xp.match_tag("/plan_class") {
                let compile = |tag: &str, pattern: Option<String>| -> CoreResult<Option<Regex>> {
                    let Some(pattern) = pattern else {
                        return Ok(None);
                    };
                    // POSIX regexec without REG_NEWLINE lets "." match a newline.
                    RegexBuilder::new(&pattern)
                        .dot_matches_new_line(true)
                        .build()
                        .map(Some)
                        .map_err(|e| {
                            CoreError::Xml(format!(
                                "plan class '{}': bad <{tag}> '{pattern}': {e}",
                                pc.name
                            ))
                        })
                };
                pc.os_regex = compile("os_regex", os_regex)?;
                pc.cpu_vendor_regex = compile("cpu_vendor_regex", cpu_vendor_regex)?;
                pc.cpu_model_regex = compile("cpu_model_regex", cpu_model_regex)?;
                pc.host_summary_regex = compile("host_summary_regex", host_summary_regex)?;
                pc.project_prefs_regex = compile("project_prefs_regex", project_prefs_regex)?;
                pc.gpu_type_regex = compile("gpu_type_regex", gpu_type_regex)?;
                return Ok(pc);
            }
            take!(xp.parse_str("name") => pc.name);
            take!(xp.parse_int("min_core_client_version") => pc.min_core_client_version);
            take!(xp.parse_int("max_core_client_version") => pc.max_core_client_version);
            take!(xp.parse_str("gpu_type") => pc.gpu_type);
            take!(xp.parse_bool("cuda") => pc.cuda);
            take!(xp.parse_bool("cal") => pc.cal);
            take!(xp.parse_bool("opencl") => pc.opencl);
            take!(xp.parse_bool("virtualbox") => pc.virtualbox);
            take!(xp.parse_bool("wsl") => pc.wsl);
            take!(xp.parse_bool("docker") => pc.docker);
            take!(xp.parse_int("min_boinc_wsl_distro_version") => pc.min_boinc_wsl_distro_version);
            take!(xp.parse_bool("is64bit") => pc.is64bit);
            if let Some(feature) = xp.parse_str("cpu_feature") {
                pc.cpu_features.push(feature);
                continue;
            }
            take!(xp.parse_double("min_ncpus") => pc.min_ncpus);
            take!(xp.parse_int("max_threads") => pc.max_threads);
            if let Some(mb) = xp.parse_double("mem_usage_base_mb") {
                pc.mem_usage_base = mb * MEGA;
                continue;
            }
            if let Some(mb) = xp.parse_double("mem_usage_per_cpu_mb") {
                pc.mem_usage_per_cpu = mb * MEGA;
                continue;
            }
            take!(xp.parse_bool("nthreads_cmdline") => pc.nthreads_cmdline);
            take!(xp.parse_double("projected_flops_scale") => pc.projected_flops_scale);
            if let Some(s) = xp.parse_str("os_regex") {
                os_regex = Some(s);
                continue;
            }
            if let Some(s) = xp.parse_str("cpu_vendor_regex") {
                cpu_vendor_regex = Some(s);
                continue;
            }
            if let Some(s) = xp.parse_str("cpu_model_regex") {
                cpu_model_regex = Some(s);
                continue;
            }
            if let Some(s) = xp.parse_str("host_summary_regex") {
                host_summary_regex = Some(s);
                continue;
            }
            take!(xp.parse_long("user_id") => pc.user_id);
            take!(xp.parse_double("infeasible_random") => pc.infeasible_random);
            take!(xp.parse_double("min_os_version") => pc.min_os_version);
            take!(xp.parse_double("max_os_version") => pc.max_os_version);
            take!(xp.parse_int("min_android_version") => pc.min_android_version);
            take!(xp.parse_int("max_android_version") => pc.max_android_version);
            take!(xp.parse_int("min_libc_version") => pc.min_libc_version);
            take!(xp.parse_str("project_prefs_tag") => pc.project_prefs_tag);
            if let Some(s) = xp.parse_str("project_prefs_regex") {
                project_prefs_regex = Some(s);
                continue;
            }
            take!(xp.parse_bool("project_prefs_default_true") => pc.project_prefs_default_true);
            take!(xp.parse_double("avg_ncpus") => pc.avg_ncpus);

            // GPU info
            take!(xp.parse_double("cpu_frac") => pc.cpu_frac);
            take!(xp.parse_double("min_gpu_ram_mb") => pc.min_gpu_ram_mb);
            take!(xp.parse_double("gpu_ram_used_mb") => pc.gpu_ram_used_mb);
            take!(xp.parse_double("gpu_peak_flops_scale") => pc.gpu_peak_flops_scale);
            take!(xp.parse_double("ngpus") => pc.ngpus);
            take!(xp.parse_double("min_gpu_peak_flops") => pc.min_gpu_peak_flops);
            take!(xp.parse_double("max_gpu_peak_flops") => pc.max_gpu_peak_flops);
            take!(xp.parse_int("min_driver_version") => pc.min_driver_version);
            take!(xp.parse_int("max_driver_version") => pc.max_driver_version);
            take!(xp.parse_str("gpu_utilization_tag") => pc.gpu_utilization_tag);

            take!(xp.parse_bool("need_ati_libs") => pc.need_ati_libs);
            take!(xp.parse_bool("need_amd_libs") => pc.need_amd_libs);
            take!(xp.parse_int("min_amd_simd_width") => pc.min_amd_simd_width);
            take!(xp.parse_bool("without_opencl") => pc.without_opencl);
            take!(xp.parse_int("min_cal_target") => pc.min_cal_target);
            take!(xp.parse_int("max_cal_target") => pc.max_cal_target);

            take!(xp.parse_int("min_nvidia_compcap") => pc.min_nvidia_compcap);
            take!(xp.parse_int("max_nvidia_compcap") => pc.max_nvidia_compcap);

            take!(xp.parse_int("min_cuda_version") => pc.min_cuda_version);
            take!(xp.parse_int("max_cuda_version") => pc.max_cuda_version);

            take!(xp.parse_int("min_opencl_version") => pc.min_opencl_version);
            take!(xp.parse_int("max_opencl_version") => pc.max_opencl_version);

            take!(xp.parse_int("min_opencl_driver_revision") => pc.min_opencl_driver_revision);
            take!(xp.parse_int("max_opencl_driver_revision") => pc.max_opencl_driver_revision);
            take!(xp.parse_bool("double_precision_fp") => pc.double_precision_fp);

            take!(xp.parse_int("min_metal_support") => pc.min_metal_support);
            if let Some(s) = xp.parse_str("gpu_type_regex") {
                gpu_type_regex = Some(s);
                continue;
            }

            // VirtualBox
            take!(xp.parse_int("min_vbox_version") => pc.min_vbox_version);
            take!(xp.parse_int("max_vbox_version") => pc.max_vbox_version);
            if let Some(v) = xp.parse_int("exclude_vbox_version") {
                pc.exclude_vbox_version.push(v);
                continue;
            }
            take!(xp.parse_bool("vm_accel_required") => pc.vm_accel_required);

            take!(xp.parse_long("min_wu_id") => pc.min_wu_id);
            take!(xp.parse_long("max_wu_id") => pc.max_wu_id);
            take!(xp.parse_long("min_batch") => pc.min_batch);
            take!(xp.parse_long("max_batch") => pc.max_batch);
        }
        Err(CoreError::Xml(format!(
            "plan class '{}': <plan_class> is not closed",
            pc.name
        )))
    }

    /// Host-dependent checks only. None when the host cannot use the class.
    pub fn check(&self, host: &PlanHost<'_>) -> Option<HostUsage> {
        self.evaluate(host).ok()
    }

    /// The job-dependent gates (min_wu_id, max_wu_id, min_batch, max_batch)
    /// (`wu_is_infeasible_for_plan_class`, negated).
    pub fn admits_workunit(&self, workunit_id: i64, batch: i64) -> bool {
        !(self.min_wu_id != 0 && workunit_id < self.min_wu_id
            || self.max_wu_id != 0 && workunit_id > self.max_wu_id
            || self.min_batch != 0 && batch < self.min_batch
            || self.max_batch != 0 && batch > self.max_batch)
    }

    /// [`check`](Self::check), with the reason when the host cannot use the
    /// class (`PLAN_CLASS_SPEC::check`).
    pub fn evaluate(&self, host: &PlanHost<'_>) -> Result<HostUsage, PlanReject> {
        let req = host.request;

        if self.infeasible_random != 0.0 && host.random < self.infeasible_random {
            return reject("infeasible_random");
        }
        if self.user_id != 0 && host.user_id != self.user_id {
            return reject(format!(
                "not specified user ID ({} {})",
                self.user_id, host.user_id
            ));
        }

        // No venue is known here; see the module comment.
        let prefs = extract_venue(host.project_prefs, "");
        let ncpus = effective_ncpus(req, &prefs);

        self.check_cpu_and_os(host, ncpus)?;
        self.check_client_version(req)?;
        let can_use_multicore = self.check_virtualbox(req)?;
        self.check_wsl(req)?;
        self.check_docker(req)?;
        self.check_project_prefs(&prefs)?;

        let mut gpu = self.select_gpu(req)?;

        if self.opencl {
            if let Some(g) = gpu.as_mut() {
                if !g.coproc.have_opencl {
                    return reject("GPU doesn't support OpenCL");
                }
                self.opencl_check(&g.coproc.opencl_prop)?;
                g.ram = g.coproc.opencl_prop.global_mem_size as f64;
            } else {
                // OpenCL CPU app version. The host may have several OpenCL
                // CPU libraries; any one that passes will do.
                let found = req
                    .host_info
                    .opencl_cpu_prop
                    .iter()
                    .any(|p| self.opencl_check(&p.opencl_prop).is_ok());
                if !found {
                    return reject("CPU doesn't support OpenCL");
                }
            }
        }

        match gpu {
            Some(g) => self.gpu_usage(req, &prefs, &g),
            None => self.cpu_usage(req, ncpus, can_use_multicore),
        }
    }

    fn check_cpu_and_os(&self, host: &PlanHost<'_>, ncpus: i32) -> Result<(), PlanReject> {
        let hi = &host.request.host_info;

        // Older clients report CPU features in p_model, within square
        // brackets. Features are looked up surrounded by spaces, so that
        // "sse" does not match "sse2".
        if !self.cpu_features.is_empty() {
            let mut have = format!(" {} ", hi.p_features);
            if let Some(at) = hi.p_model.rfind('[') {
                let inside = &hi.p_model[at + 1..];
                let inside = inside.split(']').next().unwrap_or(inside);
                have.push_str(&format!(" {inside} "));
            }
            let have = have.to_ascii_lowercase();
            for feature in &self.cpu_features {
                if !have.contains(&format!(" {feature} ")) {
                    return reject(format!(
                        "CPU lacks feature '{feature}' (got '{}')",
                        hi.p_features
                    ));
                }
            }
        }

        if self.min_ncpus != 0.0 && f64::from(ncpus) < self.min_ncpus {
            return reject(format!("not enough CPUs: {ncpus} < {}", self.min_ncpus));
        }

        if let Some(re) = &self.os_regex {
            if !re.is_match(&hi.os_version) {
                return reject(format!(
                    "OS version '{}' didn't match regexp",
                    hi.os_version
                ));
            }
        }
        if self.min_os_version != 0.0 || self.max_os_version != 0.0 {
            let v = os_version_num(&hi.os_name, &hi.os_version);
            if v == 0.0 {
                return reject(format!(
                    "can't determine numerical OS version '{}'",
                    hi.os_version
                ));
            }
            if self.min_os_version != 0.0 && v < self.min_os_version {
                return reject(format!(
                    "OS version '{}' too low ({v} / {})",
                    hi.os_version, self.min_os_version
                ));
            }
            if self.max_os_version != 0.0 && v > self.max_os_version {
                return reject(format!(
                    "OS version '{}' too high ({v} / {})",
                    hi.os_version, self.max_os_version
                ));
            }
        }
        if self.min_android_version != 0 || self.max_android_version != 0 {
            if !hi.os_name.eq_ignore_ascii_case("android") {
                return reject("not an Android host");
            }
            let v = android_version_num(&hi.os_version);
            let (min, max) = (
                i64::from(self.min_android_version),
                i64::from(self.max_android_version),
            );
            if v == 0 && min > 0 {
                return reject(format!(
                    "can't determine numerical Android version '{}'",
                    hi.os_version
                ));
            }
            if min != 0 && v < min {
                return reject(format!(
                    "Android version '{}' too low ({v} / {min})",
                    hi.os_version
                ));
            }
            if max != 0 && v > max {
                return reject(format!(
                    "Android version '{}' too high ({v} / {max})",
                    hi.os_version
                ));
            }
        }

        if self.min_libc_version != 0 {
            let v = libc_version(&hi.os_version);
            if v < i64::from(self.min_libc_version) {
                return reject(format!(
                    "libc version too low ({v} < {})",
                    self.min_libc_version
                ));
            }
        }

        if let Some(re) = &self.cpu_vendor_regex {
            if !re.is_match(&hi.p_vendor) {
                return reject(format!("CPU vendor '{}' didn't match regexp", hi.p_vendor));
            }
        }
        if let Some(re) = &self.cpu_model_regex {
            if !re.is_match(&hi.p_model) {
                return reject(format!("CPU model '{}' didn't match regexp", hi.p_model));
            }
        }
        if let Some(re) = &self.host_summary_regex {
            if !re.is_match(host.host_serialnum) {
                return reject(format!(
                    "host summary '{}' didn't match regexp",
                    host.host_serialnum
                ));
            }
        }
        Ok(())
    }

    fn check_client_version(&self, req: &SchedulerRequest) -> Result<(), PlanReject> {
        let v = req.core_client_version;
        if self.min_core_client_version != 0 && v < self.min_core_client_version {
            return Err(PlanReject {
                reason: format!(
                    "need newer BOINC client: {v} < {}",
                    self.min_core_client_version
                ),
                user_message: Some("A newer BOINC may be required for some tasks."),
            });
        }
        if self.max_core_client_version != 0 && v > self.max_core_client_version {
            return reject(format!(
                "need older BOINC client: {v} > {}",
                self.max_core_client_version
            ));
        }
        Ok(())
    }

    /// Returns whether the app may use several cores.
    fn check_virtualbox(&self, req: &SchedulerRequest) -> Result<bool, PlanReject> {
        if !self.virtualbox {
            return Ok(true);
        }
        let hi = &req.host_info;
        if req.core_client_major_version < 7 {
            return reject_msg("BOINC client 7.0+ required for Virtualbox jobs");
        }
        if hi.virtualbox_version.is_empty() {
            return reject_msg("VirtualBox is not installed");
        }
        if hi.virtualbox_version.contains("unusable") {
            return reject_msg("VirtualBox is not usable");
        }

        let [major, minor, release] = scan_dotted(&hi.virtualbox_version, 3)[..] else {
            return reject("can't parse vbox version");
        };
        let v = major * 10000 + minor * 100 + release;
        if self.min_vbox_version != 0 && v < i64::from(self.min_vbox_version) {
            return reject(format!(
                "vbox version too low: {v} < {}",
                self.min_vbox_version
            ));
        }
        if self.max_vbox_version != 0 && v > i64::from(self.max_vbox_version) {
            return reject(format!(
                "vbox version too high: {v} > {}",
                self.max_vbox_version
            ));
        }
        if self.exclude_vbox_version.iter().any(|&x| i64::from(x) == v) {
            return reject(format!("vbox version {v} excluded"));
        }

        let accelerated = has_vm_acceleration(req);
        if self.vm_accel_required && !accelerated {
            return reject("missing VM HW acceleration");
        }
        // Without VM acceleration a VM gets one core.
        let can_use_multicore = !(self.max_threads > 1 && !accelerated);

        // Only for the host's primary platform: a Win64 host can't run a
        // 32-bit VM app, it would look in the 32-bit half of the registry.
        // is_64b_platform in sched/sched_types.h
        if req.platform_name.contains("64") != self.is64bit {
            return reject("VM app is not for the host's primary platform");
        }
        Ok(can_use_multicore)
    }

    fn check_wsl(&self, req: &SchedulerRequest) -> Result<(), PlanReject> {
        if !self.wsl {
            return Ok(());
        }
        if req.dont_use_wsl {
            return reject_msg("Client config does not allow using WSL");
        }
        let distros = &req.host_info.wsl_distros;
        if distros.is_empty() {
            return reject_msg("No WSL distros found");
        }
        let min_libc = i64::from(self.min_libc_version);
        let found = distros
            .iter()
            .any(|d| !d.disallowed && (min_libc == 0 || major_minor(&d.libc_version) >= min_libc));
        if !found {
            return reject_msg("No usable WSL distros found");
        }
        Ok(())
    }

    /// Docker apps need Docker to be allowed and present. On Windows it is
    /// used through WSL, so WSL must be allowed and a usable distro must
    /// have Docker.
    fn check_docker(&self, req: &SchedulerRequest) -> Result<(), PlanReject> {
        if !self.docker {
            return Ok(());
        }
        let hi = &req.host_info;
        if req.dont_use_docker {
            return reject_msg("Client config does not allow using Docker");
        }
        if hi.os_name.contains("Windows") {
            if req.dont_use_wsl {
                return reject_msg("Client config does not allow using WSL");
            }
            if hi.use_wslc {
                return Ok(());
            }
            if self.min_boinc_wsl_distro_version != 0 {
                // WSL_DISTROS::boinc_distro_version
                let v = hi
                    .wsl_distros
                    .iter()
                    .find(|d| d.distro_name == BOINC_WSL_DISTRO_NAME)
                    .map_or(0, |d| d.boinc_buda_runner_version);
                if v < self.min_boinc_wsl_distro_version {
                    return reject_msg("Current BOINC WSL distro needed");
                }
            } else if !hi
                .wsl_distros
                .iter()
                .any(|d| !d.disallowed && !d.docker_version.is_empty())
            {
                return reject_msg("No usable WSL distros found");
            }
        } else {
            if hi.docker_version.is_empty() {
                return reject_msg("Docker not present");
            }
            if hi.os_name.contains("Darwin") && req.core_client_version < 80206 {
                return reject_msg("Docker jobs need 8.2.6+ client");
            }
        }
        Ok(())
    }

    /// A project-specific preference decides whether the class is used.
    fn check_project_prefs(&self, prefs: &str) -> Result<(), PlanReject> {
        let Some(re) = &self.project_prefs_regex else {
            return Ok(());
        };
        if self.project_prefs_tag.is_empty() {
            return Ok(());
        }
        let tag = format!("<{}>", self.project_prefs_tag);
        let allowed = match line::parse_str(prefs, &tag) {
            Some(value) => re.is_match(&value),
            None => self.project_prefs_default_true,
        };
        if !allowed {
            return reject(format!(
                "project prefs setting '{}' prevents using plan class",
                self.project_prefs_tag
            ));
        }
        Ok(())
    }

    /// On Windows a Docker app reaches the GPU through WSL: some distro
    /// must support it (`PLAN_CLASS_SPEC::check_wsl_gpu`).
    fn check_wsl_gpu(&self, req: &SchedulerRequest, gpu_name: &str) -> Result<(), PlanReject> {
        if !(self.docker && is_windows(&req.host_info.os_name)) {
            return Ok(());
        }
        let found = req
            .host_info
            .wsl_distros
            .iter()
            .flat_map(|d| d.wsl_gpus.iter())
            .any(|g| {
                g.name == gpu_name && (self.cuda && g.has_cuda || self.opencl && g.has_opencl)
            });
        if !found {
            return reject(format!("{gpu_name} GPU not supported in WSL"));
        }
        Ok(())
    }

    /// Find the GPU the class asks for and apply the vendor-specific
    /// requirements. `Ok(None)` for a CPU plan class.
    fn select_gpu<'a>(
        &self,
        req: &'a SchedulerRequest,
    ) -> Result<Option<SelectedGpu<'a>>, PlanReject> {
        let gpu_type = self.gpu_type.as_str();
        let coprocs = &req.coprocs;

        if gpu_type == "amd" || gpu_type == "ati" {
            let Some(cp) = coprocs.ati.as_ref().filter(|c| c.base.count != 0) else {
                return reject("No AMD GPUs found");
            };
            if self.need_ati_libs {
                if !cp.atirt_detected {
                    return reject("ATI libraries not found");
                }
            } else if self.need_amd_libs && !cp.amdrt_detected {
                return reject("AMD libraries not found");
            }
            if self.without_opencl && cp.base.have_opencl {
                return reject("OpenCL detected. Plan restricted to CAL only GPUs");
            }
            let simd_width = cp.base.opencl_prop.amd_simd_width;
            if self.min_amd_simd_width != 0 && simd_width < self.min_amd_simd_width {
                return reject(format!(
                    "AMD SIMD width less than minimum ({simd_width} < {})",
                    self.min_amd_simd_width
                ));
            }
            if self.min_cal_target != 0 && cp.target < self.min_cal_target {
                return reject(format!(
                    "CAL target less than minimum ({} < {})",
                    cp.target, self.min_cal_target
                ));
            }
            if self.max_cal_target != 0 && cp.target > self.max_cal_target {
                return reject(format!(
                    "CAL target greater than maximum ({} > {})",
                    cp.target, self.max_cal_target
                ));
            }
            // Without CAL the driver version is unknown and not checked.
            let mut driver_version = 0;
            if cp.have_cal {
                let [major, minor, release] = scan_dotted(&cp.version, 3)[..] else {
                    return reject(format!(
                        "driver version '{}' couldn't be parsed",
                        cp.version
                    ));
                };
                // ati_version_int in lib/coproc.h
                driver_version = major * 10_000_000 + minor * 10_000 + release;
            }
            self.check_wsl_gpu(req, "amd")?;
            return Ok(Some(SelectedGpu {
                coproc: &cp.base,
                ram: cp.base.opencl_prop.global_mem_size as f64,
                driver_version,
            }));
        }

        if gpu_type == "nvidia" {
            let Some(cp) = coprocs.nvidia.as_ref().filter(|c| c.base.count != 0) else {
                return reject("No NVIDIA GPUs found");
            };
            let compcap = cp.major * 100 + cp.minor;
            if self.min_nvidia_compcap != 0 && self.min_nvidia_compcap > compcap {
                return reject(format!(
                    "NVIDIA compute capability required min: {}, supplied: {compcap}",
                    self.min_nvidia_compcap
                ));
            }
            if self.max_nvidia_compcap != 0 && self.max_nvidia_compcap < compcap {
                return reject(format!(
                    "NVIDIA compute capability required max: {}, supplied: {compcap}",
                    self.max_nvidia_compcap
                ));
            }
            if self.cuda {
                if self.min_cuda_version != 0 && self.min_cuda_version > cp.cuda_version {
                    return reject(format!(
                        "CUDA version required min: {}, supplied: {}",
                        self.min_cuda_version, cp.cuda_version
                    ));
                }
                if self.max_cuda_version != 0 && self.max_cuda_version < cp.cuda_version {
                    return reject(format!(
                        "CUDA version required max: {}, supplied: {}",
                        self.max_cuda_version, cp.cuda_version
                    ));
                }
            }
            self.check_wsl_gpu(req, "nvidia")?;
            return Ok(Some(SelectedGpu {
                coproc: &cp.base,
                ram: cp.total_global_mem,
                driver_version: i64::from(cp.display_driver_version),
            }));
        }

        if gpu_type.starts_with("intel") {
            let Some(cp) = coprocs.intel_gpu.as_ref().filter(|c| c.base.count != 0) else {
                return reject("No Intel GPUs found");
            };
            self.check_wsl_gpu(req, "intel")?;
            // The amount of RAM is known for OpenCL apps only.
            return Ok(Some(SelectedGpu {
                coproc: &cp.base,
                ram: 0.0,
                driver_version: 0,
            }));
        }

        if gpu_type == "apple" {
            let Some(cp) = coprocs.apple_gpu.as_ref().filter(|c| c.base.count != 0) else {
                return reject("No Apple GPUs found");
            };
            if self.min_metal_support != 0 && cp.metal_support < self.min_metal_support {
                return reject(format!(
                    "Metal support less than minimum ({} < {})",
                    cp.metal_support, self.min_metal_support
                ));
            }
            return Ok(Some(SelectedGpu {
                coproc: &cp.base,
                ram: cp.base.available_ram,
                driver_version: 0,
            }));
        }

        // Any other type: a coprocessor found by its type name, with
        // nothing known about it beyond the common fields.
        let other = if let Some(re) = &self.gpu_type_regex {
            let found = coprocs
                .all()
                .into_iter()
                .find(|c| re.is_match(&c.coproc_type));
            if found.is_none() {
                return reject("No gpu_type match found");
            }
            found
        } else if !gpu_type.is_empty() {
            let found = coprocs.lookup_type(gpu_type);
            if found.is_none() {
                return reject(format!("No {gpu_type} found"));
            }
            found
        } else {
            None
        };
        let Some(cp) = other else {
            return Ok(None);
        };
        self.check_wsl_gpu(req, &cp.coproc_type)?;
        Ok(Some(SelectedGpu {
            coproc: cp,
            ram: 0.0,
            driver_version: 0,
        }))
    }

    /// `PLAN_CLASS_SPEC::opencl_check`. A device that reports no OpenCL
    /// version passes the version and revision limits.
    fn opencl_check(&self, prop: &OpenclDeviceProp) -> Result<(), PlanReject> {
        let version = prop.opencl_device_version_int;
        if version != 0 {
            if self.min_opencl_version != 0 && self.min_opencl_version > version {
                return reject(format!(
                    "OpenCL device version required min: {}, supplied: {version}",
                    self.min_opencl_version
                ));
            }
            if self.max_opencl_version != 0 && self.max_opencl_version < version {
                return reject(format!(
                    "OpenCL device version required max: {}, supplied: {version}",
                    self.max_opencl_version
                ));
            }
            let revision = opencl_driver_revision(&prop.opencl_device_version);
            let (min, max) = (
                i64::from(self.min_opencl_driver_revision),
                i64::from(self.max_opencl_driver_revision),
            );
            if min != 0 && min > revision {
                return reject(format!(
                    "OpenCL driver revision required min: {min}, supplied: {revision}"
                ));
            }
            if max != 0 && max < revision {
                return reject(format!(
                    "OpenCL driver revision required max: {max}, supplied: {revision}"
                ));
            }
        }
        if self.double_precision_fp && prop.double_fp_config == 0 {
            return reject("device (or driver) doesn't support double precision fp math");
        }
        Ok(())
    }

    /// Usage of a GPU app, after the requirements common to all GPU types.
    fn gpu_usage(
        &self,
        req: &SchedulerRequest,
        prefs: &str,
        gpu: &SelectedGpu<'_>,
    ) -> Result<HostUsage, PlanReject> {
        let fpops = host_fpops(req);
        let mut hu = HostUsage::sequential(req.host_info.p_fpops);

        if self.min_gpu_ram_mb != 0.0 && self.min_gpu_ram_mb * MEGA > gpu.ram {
            return reject(format!(
                "GPU RAM required min: {}, supplied: {}",
                self.min_gpu_ram_mb * MEGA,
                gpu.ram
            ));
        }

        // An unknown driver version (0) passes.
        let (min, max) = (
            i64::from(self.min_driver_version),
            i64::from(self.max_driver_version),
        );
        if min != 0 && gpu.driver_version != 0 && min > gpu.driver_version {
            return reject(format!(
                "driver version required min: {min}, supplied: {}",
                gpu.driver_version
            ));
        }
        if max != 0 && gpu.driver_version != 0 && max < gpu.driver_version {
            return reject(format!(
                "driver version required max: {max}, supplied: {}",
                gpu.driver_version
            ));
        }

        let ram_used_mb = if self.gpu_ram_used_mb != 0.0 {
            self.gpu_ram_used_mb
        } else {
            self.min_gpu_ram_mb
        };
        hu.gpu_ram = ram_used_mb * MEGA;

        // The user's own GPU utilization factor, from the project preferences.
        let mut gpu_utilization = 1.0;
        if !self.gpu_utilization_tag.is_empty() {
            let tag = format!("<{}>", self.gpu_utilization_tag);
            if let Some(v) = line::parse_double(prefs, &tag) {
                if v != 0.0 {
                    gpu_utilization = v;
                }
            }
        }

        // A negative ngpus fills the device memory with tasks: the usage is
        // the fraction of the video RAM one task takes.
        let gpu_usage = if self.ngpus < 0.0 {
            (gpu.ram / hu.gpu_ram).floor() * hu.gpu_ram / gpu.ram
        } else if self.ngpus > 0.0 {
            self.ngpus * gpu_utilization
        } else {
            gpu_utilization
        };

        // Upstream has a branch for a GPU of unknown speed, which cannot be
        // taken: bad_gpu_peak_flops has replaced a zero by the default.
        let peak = gpu_peak_flops(gpu.coproc);
        if self.min_gpu_peak_flops != 0.0 && peak < self.min_gpu_peak_flops {
            return reject("GPU peak FLOPS below minimum");
        }
        if self.max_gpu_peak_flops != 0.0 && peak > self.max_gpu_peak_flops {
            return reject("GPU peak FLOPS above maximum");
        }
        let gpu_flops = self.gpu_peak_flops_scale * gpu_usage * peak;
        let (speed, cpu_utilization) = coproc_perf(fpops, gpu_flops, self.cpu_frac);
        hu.projected_flops = speed * self.projected_flops_scale;
        hu.avg_ncpus = if self.avg_ncpus != 0.0 {
            self.avg_ncpus
        } else {
            cpu_utilization
        };
        hu.peak_flops = gpu_flops + hu.avg_ncpus * fpops;

        // As upstream, the type is taken from the spelling of gpu_type. A
        // type that is none of these leaves a CPU app with the speed
        // estimates above; "apple" is among them, only "apple_gpu" counts.
        let gpu_type = self.gpu_type.as_str();
        let proc_type = if gpu_type == "amd" || gpu_type == "ati" {
            PROC_TYPE_AMD_GPU
        } else if gpu_type == "nvidia" {
            PROC_TYPE_NVIDIA_GPU
        } else if gpu_type.starts_with("intel") {
            PROC_TYPE_INTEL_GPU
        } else if gpu_type.starts_with("apple_gpu") {
            PROC_TYPE_APPLE_GPU
        } else {
            PROC_TYPE_CPU
        };
        if proc_type != PROC_TYPE_CPU {
            hu.proc_type = proc_type;
            hu.gpu_usage = gpu_usage;
        }
        Ok(hu)
    }

    /// Usage of a CPU app.
    fn cpu_usage(
        &self,
        req: &SchedulerRequest,
        ncpus: i32,
        can_use_multicore: bool,
    ) -> Result<HostUsage, PlanReject> {
        let fpops = host_fpops(req);
        let mut hu = HostUsage::sequential(req.host_info.p_fpops);

        if self.avg_ncpus != 0.0 {
            hu.avg_ncpus = self.avg_ncpus;
        } else if can_use_multicore {
            hu.avg_ncpus = f64::from(self.max_threads.min(ncpus));

            // With a per-CPU memory usage, use no more CPUs than fit in RAM.
            if self.mem_usage_per_cpu != 0.0 {
                let min_ncpus = if self.min_ncpus != 0.0 {
                    self.min_ncpus
                } else {
                    1.0
                };
                let ram = usable_ram(req);
                let mem_usage_seq = self.mem_usage_base + min_ncpus * self.mem_usage_per_cpu;
                if mem_usage_seq > ram {
                    return reject(format!(
                        "insufficient multicore RAM; {ram} < {mem_usage_seq}"
                    ));
                }
                let n = f64::from(((ram - self.mem_usage_base) / self.mem_usage_per_cpu) as i32);
                if n < hu.avg_ncpus {
                    hu.avg_ncpus = n;
                }
                // Overrides the workunit's memory bound.
                hu.mem_usage = self.mem_usage_base + hu.avg_ncpus * self.mem_usage_per_cpu;
                hu.cmdline
                    .push_str(&format!(" --memory_size_mb {:.0}", hu.mem_usage / MEGA));
            }
        } else {
            hu.avg_ncpus = 1.0;
        }
        if self.nthreads_cmdline {
            hu.cmdline
                .push_str(&format!(" --nthreads {}", hu.avg_ncpus as i32));
        }

        hu.peak_flops = fpops * hu.avg_ncpus;
        hu.projected_flops = fpops * hu.avg_ncpus * self.projected_flops_scale;
        Ok(hu)
    }
}

/// The plan classes of a project (upstream `PLAN_CLASS_SPECS`).
#[derive(Debug, Clone, Default)]
pub struct PlanClassSpecs {
    pub classes: Vec<PlanClassSpec>,
}

impl PlanClassSpecs {
    /// Parse the `plan_class_spec.xml` document
    /// (`PLAN_CLASS_SPECS::parse_specs`). An empty string gives no classes.
    pub fn parse(xml: &str) -> CoreResult<Self> {
        let mut specs = PlanClassSpecs::default();
        if xml.trim().is_empty() {
            return Ok(specs);
        }
        let mut xp = XmlParser::new(xml);
        if !xp.parse_start("plan_classes") {
            return Err(CoreError::Xml(
                "plan classes: no <plan_classes> element".into(),
            ));
        }
        while xp.get_tag() {
            if !xp.is_tag() {
                continue;
            }
            if xp.match_tag("/plan_classes") {
                return Ok(specs);
            }
            if xp.match_tag("plan_class") {
                specs.classes.push(PlanClassSpec::parse(&mut xp)?);
            }
        }
        Err(CoreError::Xml(
            "plan classes: <plan_classes> is not closed".into(),
        ))
    }

    /// The first class with this name.
    pub fn find(&self, name: &str) -> Option<&PlanClassSpec> {
        self.classes.iter().find(|c| c.name == name)
    }

    /// "" → Some(HostUsage::sequential); unknown name → None; else spec.check.
    pub fn check(&self, name: &str, host: &PlanHost<'_>) -> Option<HostUsage> {
        self.evaluate(name, host).ok()
    }

    /// [`check`](Self::check), with the reason when the host cannot use the
    /// class (`PLAN_CLASS_SPECS::check`).
    pub fn evaluate(&self, name: &str, host: &PlanHost<'_>) -> Result<HostUsage, PlanReject> {
        if name.is_empty() {
            return Ok(HostUsage::sequential(host.request.host_info.p_fpops));
        }
        match self.find(name) {
            Some(spec) => spec.evaluate(host),
            None => reject(format!("Unknown plan class: {name}")),
        }
    }

    /// Whether the plan class may run this job; true for "" and for names
    /// that are not known (`PLAN_CLASS_SPECS::wu_is_infeasible`, negated).
    pub fn admits_workunit(&self, name: &str, workunit_id: i64, batch: i64) -> bool {
        self.find(name)
            .is_none_or(|spec| spec.admits_workunit(workunit_id, batch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = r#"<?xml version="1.0" ?>
<plan_classes>
    <!-- CPU -->
    <plan_class>
        <name>                  mt          </name>
        <min_ncpus>             2           </min_ncpus>
        <max_threads>           8           </max_threads>
        <projected_flops_scale> .99         </projected_flops_scale>
        <nthreads_cmdline>      1           </nthreads_cmdline>
    </plan_class>
    <plan_class>
        <name>mt_mem</name>
        <max_threads>8</max_threads>
        <mem_usage_base_mb>1024</mem_usage_base_mb>
        <mem_usage_per_cpu_mb>8192</mem_usage_per_cpu_mb>
        <nthreads_cmdline/>
    </plan_class>
    <plan_class>
        <name>avx_debian</name>
        <cpu_feature>avx</cpu_feature>
        <cpu_feature>sse2</cpu_feature>
        <os_regex>Debian.*\[6\.</os_regex>
        <cpu_vendor_regex>^AuthenticAMD$</cpu_vendor_regex>
        <min_libc_version>236</min_libc_version>
        <min_os_version>60100</min_os_version>
    </plan_class>
    <plan_class>
        <name>windows_only</name>
        <os_regex>^Professional</os_regex>
    </plan_class>
    <plan_class>
        <name>new_client</name>
        <min_core_client_version>80300</min_core_client_version>
    </plan_class>
    <plan_class>
        <name>old_client</name>
        <max_core_client_version>70000</max_core_client_version>
    </plan_class>
    <plan_class>
        <name>this_client</name>
        <min_core_client_version>80200</min_core_client_version>
        <max_core_client_version>80299</max_core_client_version>
    </plan_class>
    <plan_class>
        <name>gated</name>
        <min_wu_id>100</min_wu_id>
        <max_wu_id>200</max_wu_id>
        <min_batch>5</min_batch>
        <max_batch>6</max_batch>
    </plan_class>
    <plan_class>
        <name>cuda</name>
        <gpu_type>nvidia</gpu_type>
        <cuda/>
        <min_nvidia_compcap>100</min_nvidia_compcap>
        <min_cuda_version>3000</min_cuda_version>
        <min_driver_version>17700</min_driver_version>
        <min_gpu_ram_mb>254</min_gpu_ram_mb>
        <gpu_ram_used_mb>254</gpu_ram_used_mb>
        <gpu_peak_flops_scale>.20</gpu_peak_flops_scale>
        <cpu_frac>.01</cpu_frac>
    </plan_class>
    <plan_class>
        <name>cuda_big</name>
        <gpu_type>nvidia</gpu_type>
        <cuda/>
        <min_gpu_ram_mb>16000</min_gpu_ram_mb>
    </plan_class>
    <plan_class>
        <name>cuda_new_driver</name>
        <gpu_type>nvidia</gpu_type>
        <cuda/>
        <min_driver_version>60000</min_driver_version>
    </plan_class>
    <plan_class>
        <name>cuda_old_driver</name>
        <gpu_type>nvidia</gpu_type>
        <cuda/>
        <max_driver_version>50000</max_driver_version>
    </plan_class>
    <plan_class>
        <name>opencl_nvidia</name>
        <gpu_type>nvidia</gpu_type>
        <opencl/>
        <min_opencl_version>102</min_opencl_version>
        <min_gpu_ram_mb>4096</min_gpu_ram_mb>
        <gpu_ram_used_mb>2048</gpu_ram_used_mb>
        <ngpus>-1</ngpus>
        <avg_ncpus>1</avg_ncpus>
        <gpu_utilization_tag>gpu_share</gpu_utilization_tag>
    </plan_class>
    <plan_class>
        <name>opencl_future</name>
        <gpu_type>nvidia</gpu_type>
        <opencl/>
        <min_opencl_version>400</min_opencl_version>
    </plan_class>
    <plan_class>
        <name>half_gpu</name>
        <gpu_type>nvidia</gpu_type>
        <ngpus>.5</ngpus>
        <gpu_utilization_tag>gpu_share</gpu_utilization_tag>
    </plan_class>
    <plan_class>
        <name>opencl_cpu</name>
        <opencl/>
        <min_opencl_version>200</min_opencl_version>
    </plan_class>
    <plan_class>
        <name>amd</name>
        <gpu_type>amd</gpu_type>
    </plan_class>
    <plan_class>
        <name>docker</name>
        <docker/>
    </plan_class>
    <plan_class>
        <name>docker_cuda</name>
        <docker/>
        <gpu_type>nvidia</gpu_type>
        <cuda/>
    </plan_class>
    <plan_class>
        <name>docker_distro</name>
        <docker/>
        <min_boinc_wsl_distro_version>5</min_boinc_wsl_distro_version>
    </plan_class>
    <plan_class>
        <name>wsl</name>
        <wsl/>
    </plan_class>
    <plan_class>
        <name>vbox64_mt</name>
        <virtualbox/>
        <is64bit/>
        <max_threads>4</max_threads>
        <min_vbox_version>60000</min_vbox_version>
        <exclude_vbox_version>70019</exclude_vbox_version>
    </plan_class>
    <plan_class>
        <name>vbox32</name>
        <virtualbox/>
    </plan_class>
    <plan_class>
        <name>vbox_excluded</name>
        <virtualbox/>
        <is64bit/>
        <exclude_vbox_version>70020</exclude_vbox_version>
    </plan_class>
    <plan_class>
        <name>opt_in</name>
        <project_prefs_tag>use_beta</project_prefs_tag>
        <project_prefs_regex>^(1|yes)$</project_prefs_regex>
    </plan_class>
    <plan_class>
        <name>opt_out</name>
        <project_prefs_tag>use_beta</project_prefs_tag>
        <project_prefs_regex>^(1|yes)$</project_prefs_regex>
        <project_prefs_default_true/>
    </plan_class>
    <plan_class>
        <name>one_user</name>
        <user_id>7</user_id>
    </plan_class>
    <plan_class>
        <name>sometimes</name>
        <infeasible_random>.25</infeasible_random>
    </plan_class>
    <plan_class>
        <name>summary</name>
        <host_summary_regex>vbox\|7\.</host_summary_regex>
    </plan_class>
</plan_classes>
"#;

    const NVIDIA: &str = r#"<coproc_cuda>
   <count>1</count>
   <name>NVIDIA GeForce RTX 2060 SUPER</name>
   <have_cuda>1</have_cuda>
   <have_opencl>1</have_opencl>
   <peak_flops>4728000000000.000000</peak_flops>
   <cudaVersion>12020</cudaVersion>
   <drvVersion>53529</drvVersion>
   <totalGlobalMem>8589934592.000000</totalGlobalMem>
   <major>7</major>
   <minor>5</minor>
   <coproc_opencl>
      <name>NVIDIA GeForce RTX 2060 SUPER</name>
      <vendor>NVIDIA Corporation</vendor>
      <global_mem_size>8589934592</global_mem_size>
      <opencl_device_version>OpenCL 3.0 CUDA</opencl_device_version>
   </coproc_opencl>
</coproc_cuda>"#;

    /// A 16-core Linux host that lets BOINC use 75% of its CPUs and 90% of
    /// its 64 GiB of RAM, with Podman and VirtualBox.
    fn linux_request(coprocs: &str) -> SchedulerRequest {
        let xml = format!(
            r#"<scheduler_request>
    <hostid>4217</hostid>
    <core_client_major_version>8</core_client_major_version>
    <core_client_minor_version>2</core_client_minor_version>
    <core_client_release>4</core_client_release>
    <platform_name>x86_64-pc-linux-gnu</platform_name>
<working_global_preferences>
<global_preferences>
   <max_ncpus_pct>75.000000</max_ncpus_pct>
   <ram_max_used_busy_pct>50.000000</ram_max_used_busy_pct>
   <ram_max_used_idle_pct>90.000000</ram_max_used_idle_pct>
</global_preferences>
</working_global_preferences>
<host_info>
    <p_ncpus>16</p_ncpus>
    <p_vendor>AuthenticAMD</p_vendor>
    <p_model>AMD Ryzen 7 5800X 8-Core Processor [Family 25 Model 33 Stepping 2]</p_model>
    <p_features>fpu vme sse sse2 ht syscall nx avx avx2</p_features>
    <p_fpops>5000000000.000000</p_fpops>
    <m_nbytes>68719476736.000000</m_nbytes>
    <os_name>Linux Debian</os_name>
    <os_version>Debian GNU/Linux 12 (bookworm) [6.1.0-25-amd64|libc 2.36]</os_version>
    <docker_version>4.3.1</docker_version>
    <docker_type>2</docker_type>
    <virtualbox_version>7.0.20</virtualbox_version>
    <opencl_cpu_prop>
        <platform_vendor>Portable Computing Language</platform_vendor>
        <opencl_cpu_info>
            <name>cpu-haswell</name>
            <global_mem_size>65221427200</global_mem_size>
            <opencl_device_version>OpenCL 3.0 PoCL HSTR</opencl_device_version>
        </opencl_cpu_info>
    </opencl_cpu_prop>
</host_info>
    <coprocs>
{coprocs}
    </coprocs>
</scheduler_request>
"#
        );
        cb_proto::sched_request::parse(&xml).unwrap()
    }

    /// A Windows host whose BOINC WSL distro has Docker. `flags` goes at the
    /// top level of the request.
    fn windows_request(flags: &str, coprocs: &str) -> SchedulerRequest {
        let xml = format!(
            r#"<scheduler_request>
    <core_client_major_version>8</core_client_major_version>
    <core_client_minor_version>2</core_client_minor_version>
    <core_client_release>5</core_client_release>
    {flags}
    <platform_name>windows_x86_64</platform_name>
<host_info>
    <p_ncpus>8</p_ncpus>
    <p_vendor>GenuineIntel</p_vendor>
    <p_model>Intel(R) Core(TM) i7-9700 CPU @ 3.00GHz [Family 6 Model 158 Stepping 13]</p_model>
    <p_features>fpu vme de pse tsc msr</p_features>
    <p_fpops>4000000000.000000</p_fpops>
    <m_nbytes>34359738368.000000</m_nbytes>
    <os_name>Microsoft Windows 11</os_name>
    <os_version>Professional x64 Edition, (10.00.26100.00)</os_version>
    <wsl>
        <distro>
            <distro_name>boinc-buda-runner</distro_name>
            <wsl_version>2</wsl_version>
            <distro_version>4</distro_version>
            <libc_version>1.2.5</libc_version>
            <docker_version>5.3.2</docker_version>
            <docker_type>2</docker_type>
            <wsl_gpu>
                <name>nvidia</name>
                <has_cuda/>
            </wsl_gpu>
        </distro>
        <distro>
            <distro_name>Ubuntu-24.04</distro_name>
            <wsl_version>2</wsl_version>
            <disallowed/>
            <libc_version>2.39</libc_version>
        </distro>
    </wsl>
</host_info>
    <coprocs>
{coprocs}
    </coprocs>
</scheduler_request>
"#
        );
        cb_proto::sched_request::parse(&xml).unwrap()
    }

    fn plan_host(request: &SchedulerRequest) -> PlanHost<'_> {
        PlanHost {
            request,
            user_id: 7,
            host_id: 4217,
            project_prefs: "",
            host_serialnum: "",
            random: 0.5,
        }
    }

    fn specs() -> PlanClassSpecs {
        PlanClassSpecs::parse(SPEC).unwrap()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-9 * b.abs()
    }

    #[test]
    fn parses_a_spec_file() {
        let s = specs();
        assert_eq!(s.classes.len(), 29);
        let mt = s.find("mt").unwrap();
        assert_eq!(mt.min_ncpus, 2.0);
        assert_eq!(mt.max_threads, 8);
        assert_eq!(mt.projected_flops_scale, 0.99);
        assert!(mt.nthreads_cmdline);
        assert_eq!(mt.cpu_frac, 0.1);
        assert_eq!(mt.ngpus, 1.0);

        let mem = s.find("mt_mem").unwrap();
        assert_eq!(mem.mem_usage_base, 1024.0 * MEGA);
        assert_eq!(mem.mem_usage_per_cpu, 8192.0 * MEGA);

        let cuda = s.find("cuda").unwrap();
        assert_eq!(cuda.gpu_type, "nvidia");
        assert!(cuda.cuda && !cuda.opencl);
        assert_eq!(cuda.min_driver_version, 17700);
        assert_eq!(cuda.min_gpu_ram_mb, 254.0);
        assert_eq!(cuda.gpu_peak_flops_scale, 0.2);

        let avx = s.find("avx_debian").unwrap();
        assert_eq!(
            avx.cpu_features,
            vec!["avx".to_string(), "sse2".to_string()]
        );
        assert!(avx.os_regex.is_some() && avx.cpu_vendor_regex.is_some());

        let vbox = s.find("vbox64_mt").unwrap();
        assert!(vbox.virtualbox && vbox.is64bit);
        assert_eq!(vbox.exclude_vbox_version, vec![70019]);

        assert!(s.find("docker").unwrap().docker);
        assert_eq!(s.find("gated").unwrap().max_batch, 6);
        assert!(s.find("nope").is_none());
    }

    #[test]
    fn empty_and_broken_documents() {
        assert!(PlanClassSpecs::parse("").unwrap().classes.is_empty());
        assert!(PlanClassSpecs::parse(" \n").unwrap().classes.is_empty());
        let none = PlanClassSpecs::parse("<plan_classes>\n</plan_classes>").unwrap();
        assert!(none.classes.is_empty());
        assert!(PlanClassSpecs::parse("<config></config>").is_err());
        assert!(PlanClassSpecs::parse("<plan_classes><plan_class><name>a</name>").is_err());
        let unclosed = "<plan_classes><plan_class><name>a</name></plan_class>";
        assert!(PlanClassSpecs::parse(unclosed).is_err());
    }

    #[test]
    fn bad_regex_names_class_and_tag() {
        let err = PlanClassSpecs::parse(
            "<plan_classes><plan_class><os_regex>(</os_regex><name>broken</name></plan_class></plan_classes>",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("'broken'"), "{err}");
        assert!(err.contains("<os_regex>"), "{err}");
    }

    #[test]
    fn empty_class_is_a_sequential_app() {
        let req = linux_request("");
        let hu = specs().check("", &plan_host(&req)).unwrap();
        assert_eq!(hu, HostUsage::sequential(5e9));
        assert_eq!(hu.proc_type, PROC_TYPE_CPU);
        assert_eq!(hu.avg_ncpus, 1.0);
        assert_eq!(hu.projected_flops, 5e9);
        assert_eq!(hu.peak_flops, 5e9);
        assert!(!hu.uses_gpu());
        // A host without a benchmark is assumed to do 1 GFLOPS.
        assert_eq!(HostUsage::sequential(0.0).projected_flops, 1e9);
        // No spec file at all still serves plain CPU apps.
        let none = PlanClassSpecs::default();
        assert!(none.check("", &plan_host(&req)).is_some());
    }

    #[test]
    fn unknown_class_is_rejected() {
        let req = linux_request("");
        let s = specs();
        assert!(s.check("cuda55", &plan_host(&req)).is_none());
        // The legacy hard-coded names are not known either.
        assert!(PlanClassSpecs::default()
            .check("mt", &plan_host(&req))
            .is_none());
        let err = s.evaluate("cuda55", &plan_host(&req)).unwrap_err();
        assert!(err.reason.contains("cuda55"));
    }

    #[test]
    fn multithread_cpu_class() {
        let req = linux_request("");
        let s = specs();
        // 75% of 16 CPUs is 12; the class uses at most 8.
        let hu = s.check("mt", &plan_host(&req)).unwrap();
        assert_eq!(hu.proc_type, PROC_TYPE_CPU);
        assert_eq!(hu.avg_ncpus, 8.0);
        assert_eq!(hu.peak_flops, 5e9 * 8.0);
        assert_eq!(hu.projected_flops, 5e9 * 8.0 * 0.99);
        assert_eq!(hu.cmdline, " --nthreads 8");
        assert_eq!(hu.mem_usage, 0.0);

        // The user's max_cpus project preference lowers the count; a
        // setting inside a venue does not apply.
        let mut host = plan_host(&req);
        host.project_prefs = "<project_preferences>\n<max_cpus>4</max_cpus>\n\
            <venue name=\"work\">\n<max_cpus>1</max_cpus>\n</venue>\n</project_preferences>";
        let hu = s.check("mt", &host).unwrap();
        assert_eq!(hu.avg_ncpus, 4.0);
        assert_eq!(hu.cmdline, " --nthreads 4");

        host.project_prefs = "<max_cpus>1</max_cpus>";
        let err = s.evaluate("mt", &host).unwrap_err();
        assert!(err.reason.contains("not enough CPUs"), "{}", err.reason);
    }

    #[test]
    fn memory_limits_the_thread_count() {
        let req = linux_request("");
        // 90% of 64 GiB is 57.6 GiB: room for 1 GiB plus 7 times 8 GiB.
        let hu = specs().check("mt_mem", &plan_host(&req)).unwrap();
        assert_eq!(hu.avg_ncpus, 7.0);
        assert_eq!(hu.mem_usage, 57.0 * GIGA);
        assert_eq!(hu.cmdline, " --memory_size_mb 58368 --nthreads 7");
        assert_eq!(hu.peak_flops, 5e9 * 7.0);

        let small = PlanClassSpecs::parse(
            "<plan_classes><plan_class><name>huge</name><max_threads>2</max_threads>\
             <mem_usage_per_cpu_mb>100000</mem_usage_per_cpu_mb></plan_class></plan_classes>",
        )
        .unwrap();
        assert!(small.check("huge", &plan_host(&req)).is_none());
    }

    #[test]
    fn cpu_and_os_requirements() {
        let linux = linux_request("");
        let windows = windows_request("", "");
        let s = specs();
        assert!(s.check("avx_debian", &plan_host(&linux)).is_some());
        // No avx, not Debian, not AMD.
        let err = s.evaluate("avx_debian", &plan_host(&windows)).unwrap_err();
        assert!(
            err.reason.contains("CPU lacks feature 'avx'"),
            "{}",
            err.reason
        );

        assert!(s.check("windows_only", &plan_host(&windows)).is_some());
        let err = s.evaluate("windows_only", &plan_host(&linux)).unwrap_err();
        assert!(err.reason.contains("didn't match"), "{}", err.reason);
    }

    #[test]
    fn os_version_numbers() {
        assert_eq!(os_version_num("Darwin", "23.5.0"), 230500.0);
        assert_eq!(
            os_version_num(
                "Microsoft Windows 11",
                "Professional x64 Edition, (10.00.26100.00)"
            ),
            1_002_610_000.0
        );
        assert_eq!(os_version_num("Android", "3.0.31-g6fb96c9"), 30031.0);
        assert_eq!(
            os_version_num(
                "Linux Ubuntu",
                "Ubuntu 18.04.1 LTS [4.15.0-36-generic|libc 2.27]"
            ),
            41500.0
        );
        assert_eq!(os_version_num("Linux", "4.9.0-8-amd64"), 40900.0);
        assert_eq!(os_version_num("Linux", "Manjaro Linux"), 0.0);
        assert_eq!(os_version_num("FreeBSD", "14.0.1"), 0.0);
        assert_eq!(libc_version("x [6.1.0|libc 2.36 (Debian GLIBC)]"), 236);
        assert_eq!(libc_version("6.1.0"), 0);
        assert_eq!(android_version_num("x (Android 4.3.1)"), 40301);
        assert_eq!(android_version_num("x (Android 4.3)"), 40300);
        assert_eq!(android_version_num("x (Android 14)"), 140000);
        assert_eq!(android_version_num("x"), 0);
        assert_eq!(
            opencl_driver_revision("OpenCL 1.2 AMD-APP (1800.11)"),
            180011
        );
        assert_eq!(opencl_driver_revision("OpenCL 3.0 CUDA"), 0);
    }

    #[test]
    fn client_version_bounds() {
        let req = linux_request("");
        let s = specs();
        let host = plan_host(&req);
        assert_eq!(req.core_client_version, 80204);
        assert!(s.check("this_client", &host).is_some());
        let err = s.evaluate("new_client", &host).unwrap_err();
        assert_eq!(
            err.user_message,
            Some("A newer BOINC may be required for some tasks.")
        );
        let err = s.evaluate("old_client", &host).unwrap_err();
        assert_eq!(err.user_message, None);
    }

    #[test]
    fn workunit_and_batch_gates() {
        let s = specs();
        let gated = s.find("gated").unwrap();
        assert!(gated.admits_workunit(100, 5));
        assert!(gated.admits_workunit(200, 6));
        assert!(!gated.admits_workunit(99, 5));
        assert!(!gated.admits_workunit(201, 5));
        assert!(!gated.admits_workunit(150, 4));
        assert!(!gated.admits_workunit(150, 7));
        assert!(s.admits_workunit("gated", 150, 5));
        assert!(!s.admits_workunit("gated", 150, 7));
        // Classes without gates, the empty class and unknown names admit anything.
        assert!(s.admits_workunit("mt", 1, 0));
        assert!(s.admits_workunit("", 1, 0));
        assert!(s.admits_workunit("nope", 1, 0));
        // The gates are not part of the host check.
        let req = linux_request("");
        assert!(s.check("gated", &plan_host(&req)).is_some());
    }

    #[test]
    fn nvidia_cuda_class() {
        let req = linux_request(NVIDIA);
        let s = specs();
        let host = plan_host(&req);
        let hu = s.check("cuda", &host).unwrap();
        assert_eq!(hu.proc_type, PROC_TYPE_NVIDIA_GPU);
        assert!(hu.uses_gpu());
        assert_eq!(hu.gpu_usage, 1.0);
        assert_eq!(hu.gpu_ram, 254.0 * MEGA);

        // coproc_perf: the GPU at 20% of its peak, 1% of the work on the CPU.
        let (cpu, gpu) = (5e9, 0.2 * 4.728e12);
        let y = 0.01 * gpu + 0.99 * cpu;
        assert!(close(hu.projected_flops, cpu * gpu / y));
        assert!(close(hu.avg_ncpus, 0.01 * gpu / y));
        assert!(hu.avg_ncpus > 0.0 && hu.avg_ncpus < 1.0);
        assert!(close(hu.peak_flops, gpu + hu.avg_ncpus * cpu));
        assert_eq!(hu.cmdline, "");

        // 8 GiB of video RAM is not 16000 MB.
        let err = s.evaluate("cuda_big", &host).unwrap_err();
        assert!(err.reason.contains("GPU RAM"), "{}", err.reason);
        // Driver 535.29 is older than 600.00 and newer than 500.00.
        let err = s.evaluate("cuda_new_driver", &host).unwrap_err();
        assert!(err.reason.contains("driver version required min"));
        let err = s.evaluate("cuda_old_driver", &host).unwrap_err();
        assert!(err.reason.contains("driver version required max"));

        // No such GPU.
        let err = s.evaluate("amd", &host).unwrap_err();
        assert_eq!(err.reason, "No AMD GPUs found");
        let cpu_only = linux_request("");
        assert!(s.check("cuda", &plan_host(&cpu_only)).is_none());
    }

    #[test]
    fn nvidia_opencl_class() {
        let req = linux_request(NVIDIA);
        let s = specs();
        let host = plan_host(&req);
        // ngpus < 0: four 2 GiB tasks fill the 8 GiB card.
        let hu = s.check("opencl_nvidia", &host).unwrap();
        assert_eq!(hu.proc_type, PROC_TYPE_NVIDIA_GPU);
        assert_eq!(hu.gpu_ram, 2048.0 * MEGA);
        assert_eq!(hu.gpu_usage, 1.0);
        assert_eq!(hu.avg_ncpus, 1.0);
        assert!(close(hu.peak_flops, 4.728e12 + 5e9));

        let err = s.evaluate("opencl_future", &host).unwrap_err();
        assert!(err
            .reason
            .contains("OpenCL device version required min: 400, supplied: 300"));

        // A GPU that does not report OpenCL.
        let no_opencl = NVIDIA.replace("<have_opencl>1</have_opencl>", "");
        let req = linux_request(&no_opencl);
        let err = s.evaluate("opencl_nvidia", &plan_host(&req)).unwrap_err();
        assert_eq!(err.reason, "GPU doesn't support OpenCL");
    }

    #[test]
    fn gpu_utilization_from_project_prefs() {
        let req = linux_request(NVIDIA);
        let s = specs();
        let mut host = plan_host(&req);
        assert_eq!(s.check("half_gpu", &host).unwrap().gpu_usage, 0.5);
        host.project_prefs = "<project_specific>\n<gpu_share>0.5</gpu_share>\n</project_specific>";
        let hu = s.check("half_gpu", &host).unwrap();
        assert_eq!(hu.gpu_usage, 0.25);
        assert_eq!(hu.gpu_ram, 0.0);
    }

    #[test]
    fn opencl_cpu_class() {
        let s = specs();
        let linux = linux_request("");
        let hu = s.check("opencl_cpu", &plan_host(&linux)).unwrap();
        assert_eq!(hu.proc_type, PROC_TYPE_CPU);
        assert_eq!(hu.avg_ncpus, 1.0);
        // No OpenCL CPU library on the Windows host.
        let windows = windows_request("", "");
        let err = s.evaluate("opencl_cpu", &plan_host(&windows)).unwrap_err();
        assert_eq!(err.reason, "CPU doesn't support OpenCL");
    }

    #[test]
    fn docker_class() {
        let s = specs();
        let linux = linux_request("");
        let hu = s.check("docker", &plan_host(&linux)).unwrap();
        assert_eq!(hu, HostUsage::sequential(5e9));

        // On Windows, Docker runs in a WSL distro.
        let windows = windows_request("", NVIDIA);
        assert!(s.check("docker", &plan_host(&windows)).is_some());
        // The distro reports CUDA support for the GPU.
        let hu = s.check("docker_cuda", &plan_host(&windows)).unwrap();
        assert_eq!(hu.proc_type, PROC_TYPE_NVIDIA_GPU);
        // The BOINC distro is version 4.
        let err = s
            .evaluate("docker_distro", &plan_host(&windows))
            .unwrap_err();
        assert_eq!(err.user_message, Some("Current BOINC WSL distro needed"));

        let no_docker = windows_request("<dont_use_docker/>", "");
        let err = s.evaluate("docker", &plan_host(&no_docker)).unwrap_err();
        assert_eq!(
            err.user_message,
            Some("Client config does not allow using Docker")
        );
        let no_wsl = windows_request("<dont_use_wsl/>", "");
        let err = s.evaluate("docker", &plan_host(&no_wsl)).unwrap_err();
        assert_eq!(
            err.user_message,
            Some("Client config does not allow using WSL")
        );
    }

    #[test]
    fn wsl_class() {
        let s = specs();
        let windows = windows_request("", "");
        assert!(s.check("wsl", &plan_host(&windows)).is_some());
        let no_wsl = windows_request("<dont_use_wsl/>", "");
        let err = s.evaluate("wsl", &plan_host(&no_wsl)).unwrap_err();
        assert_eq!(
            err.user_message,
            Some("Client config does not allow using WSL")
        );
        let linux = linux_request("");
        let err = s.evaluate("wsl", &plan_host(&linux)).unwrap_err();
        assert_eq!(err.user_message, Some("No WSL distros found"));
    }

    #[test]
    fn virtualbox_class() {
        let s = specs();
        let linux = linux_request("");
        let host = plan_host(&linux);
        // The host reports neither vmx nor svm, so the VM gets one core.
        let hu = s.check("vbox64_mt", &host).unwrap();
        assert_eq!(hu.avg_ncpus, 1.0);
        // A 64-bit host does not get the 32-bit VM app.
        assert!(s.check("vbox32", &host).is_none());
        let err = s.evaluate("vbox_excluded", &host).unwrap_err();
        assert_eq!(err.reason, "vbox version 70020 excluded");

        let windows = windows_request("", "");
        let err = s.evaluate("vbox64_mt", &plan_host(&windows)).unwrap_err();
        assert_eq!(err.user_message, Some("VirtualBox is not installed"));
    }

    #[test]
    fn project_preference_switch() {
        let s = specs();
        let req = linux_request("");
        let mut host = plan_host(&req);
        // Tag missing: the default decides.
        assert!(s.check("opt_in", &host).is_none());
        assert!(s.check("opt_out", &host).is_some());
        host.project_prefs = "<project_specific><use_beta> yes </use_beta></project_specific>";
        assert!(s.check("opt_in", &host).is_some());
        assert!(s.check("opt_out", &host).is_some());
        host.project_prefs = "<project_specific><use_beta>0</use_beta></project_specific>";
        assert!(s.check("opt_in", &host).is_none());
        assert!(s.check("opt_out", &host).is_none());
    }

    #[test]
    fn user_random_and_summary_gates() {
        let s = specs();
        let req = linux_request("");
        let mut host = plan_host(&req);
        assert!(s.check("one_user", &host).is_some());
        host.user_id = 8;
        assert!(s.check("one_user", &host).is_none());

        assert!(s.check("sometimes", &host).is_some());
        host.random = 0.1;
        assert!(s.check("sometimes", &host).is_none());

        assert!(s.check("summary", &host).is_none());
        host.host_serialnum = "[BOINC|8.2.4][vbox|7.0.20]";
        assert!(s.check("summary", &host).is_some());
    }
}
