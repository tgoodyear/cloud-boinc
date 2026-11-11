//! The `<scheduler_request>` document a client posts to the scheduler.
//!
//! [`parse`] follows `SCHEDULER_REQUEST::parse` in `sched/sched_types.cpp`
//! and the parsers it calls (`HOST::parse`, `SCHED_DB_RESULT::parse_from_client`,
//! `CLIENT_APP_VERSION::parse`, `COPROCS::parse` and so on). Field names are
//! upstream's. The request is written by `CLIENT_STATE::make_scheduler_request`
//! in `client/cs_scheduler.cpp`; the test fixtures follow that function.
//!
//! Things a caller should know:
//!
//! - Unknown elements are skipped. Inside `<host_info>`, `<result>` and the
//!   other nested elements upstream does not skip unknown children, it just
//!   reads on; a child of an unknown element that has the name of a known
//!   field is therefore taken for that field. This parser does the same.
//! - Nothing is truncated or clamped. Upstream limits `stderr_out`,
//!   `xml_doc_out`, names and the number of results
//!   (`max_results_accepted`), and clamps the preferences `mod_time` to the
//!   current time; a caller that needs those limits applies them.
//! - `report_deadline` of an [`IpResult`] is the absolute time the client
//!   sent. Upstream subtracts the current time while parsing.

use crate::consts::PROC_TYPE_CPU;
use crate::coproc::{coproc_type_name_to_num, Coprocs, OpenclDeviceProp};
use crate::xml::{extract_venue, line, strip_whitespace, XmlParser};
use crate::ProtoError;

/// Largest number of `<opencl_cpu_prop>` elements kept
/// (`MAX_OPENCL_CPU_PLATFORMS` in `lib/opencl_boinc.h`).
pub const MAX_OPENCL_CPU_PLATFORMS: usize = 4;

/// An entry of `<app_versions>` (`CLIENT_APP_VERSION`): an app version the
/// client has. With the anonymous platform these are the versions the
/// scheduler may send work for.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientAppVersion {
    pub app_name: String,
    pub platform: String,
    pub plan_class: String,
    pub version_num: i32,
    /// CPUs used; 1 unless the client sent a positive value.
    pub avg_ncpus: f64,
    /// Upstream `host_usage.projected_flops`, from `<flops>`; 0 if absent.
    pub flops: f64,
    /// `PROC_TYPE_*` of the coprocessor used, `PROC_TYPE_CPU` if none.
    pub proc_type: i32,
    /// Coprocessor instances used.
    pub gpu_usage: f64,
    /// False when the element was not closed. The entry is kept anyway:
    /// [`OtherResult::app_version`] is an index into this list.
    pub parsed: bool,
}

impl Default for ClientAppVersion {
    fn default() -> Self {
        ClientAppVersion {
            app_name: String::new(),
            platform: String::new(),
            plan_class: String::new(),
            version_num: 0,
            avg_ncpus: 1.0,
            flops: 0.0,
            proc_type: PROC_TYPE_CPU,
            gpu_usage: 0.0,
            parsed: false,
        }
    }
}

impl ClientAppVersion {
    fn parse(xp: &mut XmlParser<'_>) -> Self {
        let mut v = ClientAppVersion::default();
        while xp.get_tag() {
            if xp.match_tag("/app_version") {
                v.parsed = true;
                return v;
            }
            take!(xp.parse_str("app_name") => v.app_name);
            take!(xp.parse_str("platform") => v.platform);
            take!(xp.parse_str("plan_class") => v.plan_class);
            take!(xp.parse_int("version_num") => v.version_num);
            if let Some(x) = xp.parse_double("avg_ncpus") {
                if x > 0.0 {
                    v.avg_ncpus = x;
                }
                continue;
            }
            if let Some(x) = xp.parse_double("flops") {
                if x > 0.0 {
                    v.flops = x;
                }
                continue;
            }
            if xp.match_tag("coproc") {
                // COPROC_REQ::parse
                let mut name = String::new();
                let mut count = 0.0;
                while xp.get_tag() {
                    if xp.match_tag("/coproc") {
                        if let Some(rt) = coproc_type_name_to_num(&name) {
                            v.proc_type = rt;
                            v.gpu_usage = count;
                        }
                        break;
                    }
                    take!(xp.parse_str("type") => name);
                    take!(xp.parse_double("count") => count);
                }
                continue;
            }
        }
        v
    }
}

/// OpenCL description of a CPU (`OPENCL_CPU_PROP`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OpenclCpuProp {
    pub platform_vendor: String,
    pub opencl_prop: OpenclDeviceProp,
}

impl OpenclCpuProp {
    fn parse(xp: &mut XmlParser<'_>) -> Option<Self> {
        let mut p = OpenclCpuProp::default();
        while xp.get_tag() {
            if xp.match_tag("/opencl_cpu_prop") {
                return (!p.platform_vendor.is_empty()).then_some(p);
            }
            take!(xp.parse_str("platform_vendor") => p.platform_vendor);
            if xp.match_tag("opencl_cpu_info") {
                p.opencl_prop = OpenclDeviceProp::parse(xp, "/opencl_cpu_info")?;
                continue;
            }
        }
        None
    }
}

/// A GPU visible inside a WSL distro (`WSL_GPU`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WslGpu {
    pub name: String,
    pub has_cuda: bool,
    pub has_opencl: bool,
}

/// A WSL distro on a Windows host (`WSL_DISTRO` in `lib/wslinfo.cpp`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WslDistro {
    pub distro_name: String,
    /// From `<distro_version>`: version of the BOINC BUDA runner distro,
    /// 0 for any other distro.
    pub boinc_buda_runner_version: i32,
    pub os_name: String,
    pub os_version: String,
    pub libc_version: String,
    pub is_default: bool,
    pub disallowed: bool,
    /// 1 or 2.
    pub wsl_version: i32,
    pub docker_version: String,
    /// `DOCKER_TYPE_*`.
    pub docker_type: i32,
    pub docker_compose_version: String,
    pub docker_compose_type: i32,
    pub wsl_gpus: Vec<WslGpu>,
}

impl WslDistro {
    /// Reads to `</distro>`; a distro that is not closed is kept with what
    /// was read, as upstream keeps it.
    fn parse(xp: &mut XmlParser<'_>) -> Self {
        let mut d = WslDistro::default();
        while xp.get_tag() {
            if xp.match_tag("/distro") {
                break;
            }
            take!(xp.parse_str("distro_name") => d.distro_name);
            take!(xp.parse_int("distro_version") => d.boinc_buda_runner_version);
            take!(xp.parse_str("os_name") => d.os_name);
            take!(xp.parse_str("os_version") => d.os_version);
            take!(xp.parse_str("libc_version") => d.libc_version);
            take!(xp.parse_bool("is_default") => d.is_default);
            take!(xp.parse_bool("disallowed") => d.disallowed);
            take!(xp.parse_int("wsl_version") => d.wsl_version);
            take!(xp.parse_str("docker_version") => d.docker_version);
            take!(xp.parse_int("docker_type") => d.docker_type);
            take!(xp.parse_str("docker_compose_version") => d.docker_compose_version);
            take!(xp.parse_int("docker_compose_type") => d.docker_compose_type);
            if xp.match_tag("wsl_gpu") {
                let mut g = WslGpu::default();
                let mut keep = true;
                while xp.get_tag() {
                    if xp.match_tag("/wsl_gpu") {
                        keep = !g.name.is_empty();
                        break;
                    }
                    take!(xp.parse_str("name") => g.name);
                    take!(xp.parse_bool("has_cuda") => g.has_cuda);
                    take!(xp.parse_bool("has_opencl") => g.has_opencl);
                }
                if keep {
                    d.wsl_gpus.push(g);
                }
            }
        }
        d
    }
}

/// The `<host_info>` element (`HOST::parse`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HostInfo {
    /// Seconds east of UTC.
    pub timezone: i32,
    pub domain_name: String,
    /// From `<ip_addr>`: the address the host reports for itself.
    pub last_ip_addr: String,
    pub host_cpid: String,
    /// 1 when `<host_info>` is present without `<p_ncpus>`.
    pub p_ncpus: i32,
    pub p_vendor: String,
    pub p_model: String,
    pub p_features: String,
    /// Whetstone result, floating point operations per second.
    pub p_fpops: f64,
    /// Dhrystone result, integer operations per second.
    pub p_iops: f64,
    pub p_membw: f64,
    pub os_name: String,
    pub os_version: String,
    pub product_name: String,
    pub m_nbytes: f64,
    pub m_cache: f64,
    pub m_swap: f64,
    pub d_total: f64,
    pub d_free: f64,
    /// Only very old clients send this here; see [`NetStats`].
    pub n_bwup: f64,
    /// Only very old clients send this here; see [`NetStats`].
    pub n_bwdown: f64,
    pub docker_version: String,
    /// `DOCKER_TYPE_*`.
    pub docker_type: i32,
    pub docker_compose_version: String,
    pub docker_compose_type: i32,
    pub virtualbox_version: String,
    pub p_vm_extensions_disabled: bool,
    pub opencl_cpu_prop: Vec<OpenclCpuProp>,
    /// Windows only: version of WSL itself, e.g. `2.4.13.0`.
    pub wsl_version: String,
    pub use_wslc: bool,
    pub wsl_distros: Vec<WslDistro>,
}

impl HostInfo {
    fn parse(&mut self, xp: &mut XmlParser<'_>) {
        self.p_ncpus = 1;
        while xp.get_tag() {
            if xp.match_tag("/host_info") {
                return;
            }
            take!(xp.parse_int("timezone") => self.timezone);
            take!(xp.parse_str("domain_name") => self.domain_name);
            take!(xp.parse_str("ip_addr") => self.last_ip_addr);
            take!(xp.parse_str("host_cpid") => self.host_cpid);
            take!(xp.parse_int("p_ncpus") => self.p_ncpus);
            take!(xp.parse_str("p_vendor") => self.p_vendor);
            take!(xp.parse_str("p_model") => self.p_model);
            take!(xp.parse_double("p_fpops") => self.p_fpops);
            take!(xp.parse_double("p_iops") => self.p_iops);
            take!(xp.parse_double("p_membw") => self.p_membw);
            take!(xp.parse_str("os_name") => self.os_name);
            take!(xp.parse_str("os_version") => self.os_version);
            take!(xp.parse_str("product_name") => self.product_name);
            take!(xp.parse_double("m_nbytes") => self.m_nbytes);
            take!(xp.parse_double("m_cache") => self.m_cache);
            take!(xp.parse_double("m_swap") => self.m_swap);
            take!(xp.parse_double("d_total") => self.d_total);
            take!(xp.parse_double("d_free") => self.d_free);
            take!(xp.parse_double("n_bwup") => self.n_bwup);
            take!(xp.parse_double("n_bwdown") => self.n_bwdown);
            take!(xp.parse_str("p_features") => self.p_features);
            take!(xp.parse_str("docker_version") => self.docker_version);
            take!(xp.parse_int("docker_type") => self.docker_type);
            take!(xp.parse_str("docker_compose_version") => self.docker_compose_version);
            take!(xp.parse_int("docker_compose_type") => self.docker_compose_type);
            take!(xp.parse_str("virtualbox_version") => self.virtualbox_version);
            take!(xp.parse_bool("p_vm_extensions_disabled") => self.p_vm_extensions_disabled);
            if xp.match_tag("opencl_cpu_prop") {
                if let Some(p) = OpenclCpuProp::parse(xp) {
                    if self.opencl_cpu_prop.len() < MAX_OPENCL_CPU_PLATFORMS {
                        self.opencl_cpu_prop.push(p);
                    }
                }
                continue;
            }
            take!(xp.parse_str("wsl_version") => self.wsl_version);
            take!(xp.parse_bool("use_wslc") => self.use_wslc);
            if xp.match_tag("wsl") {
                // WSL_DISTROS::parse
                self.wsl_distros.clear();
                while xp.get_tag() {
                    if xp.match_tag("/wsl") {
                        break;
                    }
                    if xp.match_tag("distro") {
                        self.wsl_distros.push(WslDistro::parse(xp));
                    }
                }
                continue;
            }
        }
    }

    /// Whether the host can run Docker jobs natively: a Docker or Podman
    /// version was reported outside WSL.
    pub fn has_docker(&self) -> bool {
        !self.docker_version.is_empty()
    }
}

/// The `<time_stats>` element (`HOST::parse_time_stats`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TimeStats {
    pub on_frac: f64,
    pub connected_frac: f64,
    pub active_frac: f64,
    pub gpu_active_frac: f64,
    pub cpu_and_network_available_frac: f64,
    pub client_start_time: f64,
    pub previous_uptime: f64,
}

/// The `<net_stats>` element (`HOST::parse_net_stats`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NetStats {
    /// From `<bwup>`: upload rate in bytes per second.
    pub n_bwup: f64,
    /// From `<bwdown>`: download rate in bytes per second.
    pub n_bwdown: f64,
}

/// The `<disk_usage>` element (`HOST::parse_disk_usage`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DiskUsage {
    pub d_boinc_used_total: f64,
    pub d_boinc_used_project: f64,
    /// From `<d_project_share>`: the project's share of the disk space
    /// BOINC may use.
    pub d_boinc_max: f64,
}

/// An output file reported with a result, read from
/// [`ReportedResult::xml_doc_out`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OutputFileInfo {
    pub name: String,
    pub nbytes: f64,
    pub max_nbytes: f64,
    pub md5_cksum: String,
}

/// A completed result the client reports
/// (`SCHED_DB_RESULT::parse_from_client`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReportedResult {
    pub name: String,
    /// From `<state>`: the client's `RESULT_*` state
    /// ([`crate::consts::RESULT_FILES_UPLOADED`] and so on).
    pub client_state: i32,
    /// From `<final_cpu_time>`; 0 if not finite.
    pub cpu_time: f64,
    /// From `<final_elapsed_time>`; 0 if not finite.
    pub elapsed_time: f64,
    /// From `<final_peak_working_set_size>`.
    pub peak_working_set_size: f64,
    /// From `<final_peak_swap_size>`.
    pub peak_swap_size: f64,
    /// From `<final_peak_disk_usage>`.
    pub peak_disk_usage: f64,
    /// 0 when the client did not send one.
    pub exit_status: i32,
    pub app_version_num: i32,
    /// The `<file_info>` elements of the uploaded output files,
    /// concatenated without separator, each with its contents stripped of
    /// surrounding whitespace.
    pub xml_doc_out: String,
    /// Raw contents of `<stderr_out>`: a `<core_client_version>` line
    /// followed by the task's stderr in a CDATA section. Stored as is,
    /// which is what the web pages expect.
    pub stderr_out: String,
}

impl ReportedResult {
    fn parse(xp: &mut XmlParser<'_>) -> Option<Self> {
        fn finite(x: f64) -> f64 {
            if x.is_finite() {
                x
            } else {
                0.0
            }
        }
        let mut r = ReportedResult::default();
        while xp.get_tag() {
            if xp.match_tag("/result") {
                return Some(r);
            }
            take!(xp.parse_str("name") => r.name);
            take!(xp.parse_int("state") => r.client_state);
            if let Some(x) = xp.parse_double("final_cpu_time") {
                r.cpu_time = finite(x);
                continue;
            }
            if let Some(x) = xp.parse_double("final_elapsed_time") {
                r.elapsed_time = finite(x);
                continue;
            }
            if let Some(x) = xp.parse_double("final_peak_working_set_size") {
                r.peak_working_set_size = finite(x);
                continue;
            }
            if let Some(x) = xp.parse_double("final_peak_swap_size") {
                r.peak_swap_size = finite(x);
                continue;
            }
            if let Some(x) = xp.parse_double("final_peak_disk_usage") {
                r.peak_disk_usage = finite(x);
                continue;
            }
            take!(xp.parse_int("exit_status") => r.exit_status);
            take!(xp.parse_int("app_version_num") => r.app_version_num);
            if xp.match_tag("file_info") {
                if let Ok(s) = xp.copy_element() {
                    r.xml_doc_out.push_str(&s);
                }
                continue;
            }
            if xp.match_tag("stderr_out") {
                if let Ok(s) = xp.copy_element_contents("</stderr_out>") {
                    r.stderr_out = s;
                }
                continue;
            }
            if xp.match_tag("file_ref") {
                let _ = xp.copy_element();
                continue;
            }
        }
        None
    }

    /// The output files described in [`xml_doc_out`](Self::xml_doc_out).
    pub fn output_files(&self) -> Vec<OutputFileInfo> {
        let mut out = Vec::new();
        let mut xp = XmlParser::new(&self.xml_doc_out);
        let mut cur: Option<OutputFileInfo> = None;
        while xp.get_tag() {
            if xp.match_tag("file_info") {
                cur = Some(OutputFileInfo::default());
                continue;
            }
            if xp.match_tag("/file_info") {
                out.extend(cur.take());
                continue;
            }
            let Some(fi) = cur.as_mut() else {
                continue;
            };
            take!(xp.parse_str("name") => fi.name);
            take!(xp.parse_double("nbytes") => fi.nbytes);
            take!(xp.parse_double("max_nbytes") => fi.max_nbytes);
            take!(xp.parse_str("md5_cksum") => fi.md5_cksum);
        }
        out
    }

    /// The task's stderr text: [`stderr_out`](Self::stderr_out) without
    /// the `<core_client_version>` line and the CDATA wrapper.
    pub fn stderr_text(&self) -> &str {
        let s = &self.stderr_out;
        match (s.find("<![CDATA["), s.rfind("]]>")) {
            (Some(a), Some(b)) if a + 9 <= b => {
                let inner = &s[a + 9..b];
                inner.strip_prefix('\n').unwrap_or(inner)
            }
            _ => "",
        }
    }

    /// The client version recorded in [`stderr_out`](Self::stderr_out),
    /// e.g. `8.2.4`.
    pub fn stderr_client_version(&self) -> Option<String> {
        line::parse_str(&self.stderr_out, "<core_client_version>")
    }
}

/// An entry of `<other_results>`: a result the client has and is not
/// reporting in this request (`OTHER_RESULT`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtherResult {
    pub name: String,
    /// Index into [`SchedulerRequest::client_app_versions`], -1 if absent.
    pub app_version: i32,
    pub plan_class: String,
    pub have_plan_class: bool,
    /// Set by the scheduler when it tells the client to abort the result.
    pub abort: bool,
    /// Set by the scheduler: abort only if the result has not started.
    pub abort_if_not_started: bool,
}

/// An entry of `<in_progress_results>` (`IP_RESULT`), sent when the reply
/// to an earlier request contained `<send_full_workload/>`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct IpResult {
    /// `ip0`, `ip1`, ... when the client sent no name.
    pub name: String,
    /// Absolute deadline, as sent.
    pub report_deadline: f64,
    /// From `<cpu_time_remaining>`, the tag upstream's scheduler reads.
    /// Current clients write `<time_remaining>` instead, so this is 0 for
    /// them.
    pub cpu_time_remaining: f64,
    /// From `<time_remaining>`, which upstream's scheduler ignores.
    pub time_remaining: f64,
}

/// A sticky file present on the client (`FILE_INFO` in
/// `sched/sched_types.h`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FileInfo {
    pub name: String,
    pub nbytes: f64,
    pub status: i32,
    pub sticky: bool,
}

/// A trickle-up message (`MSG_FROM_HOST_DESC`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MsgFromHost {
    pub variety: String,
    /// Every line of the element except the `<variety>` line, newlines
    /// included: normally `<result_name>`, `<time>` and the trickle text.
    pub msg_text: String,
}

impl MsgFromHost {
    /// `MSG_FROM_HOST_DESC::parse`: line based, to the line that holds
    /// `</msg_from_host>`.
    fn parse(xp: &mut XmlParser<'_>) -> Option<Self> {
        let mut m = MsgFromHost::default();
        while let Some(l) = xp.next_line() {
            if line::match_tag(l, "</msg_from_host>") {
                return Some(m);
            }
            if let Some(v) = line::parse_str(l, "<variety>") {
                m.variety = v;
                continue;
            }
            m.msg_text.push_str(l);
        }
        None
    }

    /// The `<result_name>` line of the message, if any.
    pub fn result_name(&self) -> Option<String> {
        line::parse_str(&self.msg_text, "<result_name>")
    }
}

/// Keyword preferences passed on from an account manager (`USER_KEYWORDS`
/// in `lib/keyword.cpp`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UserKeywords {
    pub yes: Vec<i32>,
    pub no: Vec<i32>,
}

/// The preferences the scheduler looks at (`GLOBAL_PREFS` in
/// `sched/sched_types.h`). A value of 0 means "not set".
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GlobalPrefs {
    pub mod_time: f64,
    pub disk_max_used_gb: f64,
    pub disk_max_used_pct: f64,
    pub disk_min_free_gb: f64,
    pub work_buf_min_days: f64,
    /// `<ram_max_used_busy_pct>` divided by 100.
    pub ram_max_used_busy_frac: f64,
    /// `<ram_max_used_idle_pct>` divided by 100.
    pub ram_max_used_idle_frac: f64,
    pub max_ncpus_pct: f64,
}

impl GlobalPrefs {
    /// Read preferences from a `<global_preferences>` text
    /// (`GLOBAL_PREFS::parse`). `venue` selects a `<venue name="...">`
    /// section; pass `""` to use the settings outside any venue.
    ///
    /// `mod_time` is returned as sent; upstream replaces a time in the
    /// future by the current time.
    pub fn parse(xml: &str, venue: &str) -> Self {
        let mut p = GlobalPrefs::default();
        if let Some(x) = line::parse_double(xml, "<mod_time>") {
            p.mod_time = x;
        }
        let body = extract_venue(xml, venue);
        let get = |tag: &str| line::parse_double(&body, tag);
        if let Some(x) = get("<disk_max_used_gb>") {
            p.disk_max_used_gb = x;
        }
        if let Some(x) = get("<disk_max_used_pct>") {
            p.disk_max_used_pct = x;
        }
        if let Some(x) = get("<disk_min_free_gb>") {
            p.disk_min_free_gb = x;
        }
        if let Some(x) = get("<work_buf_min_days>") {
            p.work_buf_min_days = x;
        }
        if let Some(x) = get("<ram_max_used_busy_pct>") {
            p.ram_max_used_busy_frac = x / 100.0;
        }
        if let Some(x) = get("<ram_max_used_idle_pct>") {
            p.ram_max_used_idle_frac = x / 100.0;
        }
        if let Some(x) = get("<max_ncpus_pct>") {
            p.max_ncpus_pct = x;
        }
        p
    }
}

/// A parsed scheduler request (`SCHEDULER_REQUEST`).
#[derive(Debug, Clone, PartialEq)]
pub struct SchedulerRequest {
    /// Account key, cut at the first quote character.
    pub authenticator: String,
    pub cross_project_id: String,
    /// 0 when the client has no host ID for this project yet.
    pub hostid: i64,
    pub rpc_seqno: i32,
    pub uptime: f64,
    pub previous_uptime: f64,
    /// Upstream `platform.name`; `anonymous` for the anonymous platform.
    pub platform_name: String,
    /// Names of the other platforms the client supports, in its order of
    /// preference.
    pub alt_platforms: Vec<String>,
    pub client_app_versions: Vec<ClientAppVersion>,
    pub core_client_major_version: i32,
    pub core_client_minor_version: i32,
    pub core_client_release: i32,
    /// `10000 * major + 100 * minor + release`.
    pub core_client_version: i32,
    /// Seconds of work requested, any resource; 0 means none.
    pub work_req_seconds: f64,
    pub cpu_req_secs: f64,
    pub cpu_req_instances: f64,
    /// Defaults to 1.
    pub resource_share_fraction: f64,
    /// Defaults to 1.
    pub rrs_fraction: f64,
    /// Defaults to 1.
    pub prrs_fraction: f64,
    /// From the top-level `<estimated_delay>`.
    pub cpu_estimated_delay: f64,
    /// Upstream `host.duration_correction_factor`.
    pub duration_correction_factor: f64,
    pub dont_send_work: bool,
    pub dont_use_docker: bool,
    pub dont_use_wsl: bool,
    /// `<global_preferences>` including its tags, or empty. Current
    /// clients do not send it.
    pub global_prefs_xml: String,
    /// Contents of `<working_global_preferences>`, without the tags.
    pub working_global_prefs_xml: String,
    /// [`GlobalPrefs`] read from `working_global_prefs_xml`.
    pub working_global_prefs: GlobalPrefs,
    pub global_prefs_source_email_hash: String,
    /// Contents of `<code_sign_key>`, stripped.
    pub code_sign_key: String,
    pub host_info: HostInfo,
    pub time_stats: TimeStats,
    pub net_stats: NetStats,
    pub disk_usage: DiskUsage,
    pub coprocs: Coprocs,
    /// Reported results, duplicates by name removed.
    pub results: Vec<ReportedResult>,
    /// Reported "results" whose name contains `download` or `upload`:
    /// outcomes of file transfer commands the scheduler sent earlier.
    pub file_xfer_results: Vec<ReportedResult>,
    pub msgs_from_host: Vec<MsgFromHost>,
    /// Sticky files present on the client.
    pub file_infos: Vec<FileInfo>,
    pub have_other_results_list: bool,
    pub other_results: Vec<OtherResult>,
    pub have_ip_results_list: bool,
    pub ip_results: Vec<IpResult>,
    pub have_time_stats_log: bool,
    /// Raw contents of `<time_stats_log>`.
    pub time_stats_log: String,
    pub client_cap_plan_class: bool,
    /// -1 when not sent.
    pub sandbox: i32,
    /// -1 when not sent.
    pub allow_multiple_clients: i32,
    pub user_keywords: UserKeywords,
    pub client_brand: String,
}

impl Default for SchedulerRequest {
    fn default() -> Self {
        SchedulerRequest {
            authenticator: String::new(),
            cross_project_id: String::new(),
            hostid: 0,
            rpc_seqno: 0,
            uptime: 0.0,
            previous_uptime: 0.0,
            platform_name: String::new(),
            alt_platforms: Vec::new(),
            client_app_versions: Vec::new(),
            core_client_major_version: 0,
            core_client_minor_version: 0,
            core_client_release: 0,
            core_client_version: 0,
            work_req_seconds: 0.0,
            cpu_req_secs: 0.0,
            cpu_req_instances: 0.0,
            resource_share_fraction: 1.0,
            rrs_fraction: 1.0,
            prrs_fraction: 1.0,
            cpu_estimated_delay: 0.0,
            duration_correction_factor: 0.0,
            dont_send_work: false,
            dont_use_docker: false,
            dont_use_wsl: false,
            global_prefs_xml: String::new(),
            working_global_prefs_xml: String::new(),
            working_global_prefs: GlobalPrefs::default(),
            global_prefs_source_email_hash: String::new(),
            code_sign_key: String::new(),
            host_info: HostInfo::default(),
            time_stats: TimeStats::default(),
            net_stats: NetStats::default(),
            disk_usage: DiskUsage::default(),
            coprocs: Coprocs::default(),
            results: Vec::new(),
            file_xfer_results: Vec::new(),
            msgs_from_host: Vec::new(),
            file_infos: Vec::new(),
            have_other_results_list: false,
            other_results: Vec::new(),
            have_ip_results_list: false,
            ip_results: Vec::new(),
            have_time_stats_log: false,
            time_stats_log: String::new(),
            client_cap_plan_class: false,
            sandbox: -1,
            allow_multiple_clients: -1,
            user_keywords: UserKeywords::default(),
            client_brand: String::new(),
        }
    }
}

/// Top-level elements that upstream names and ignores. Their children, if
/// any, are then read as top-level elements.
const IGNORED: &[&str] = &[
    "host_venue",
    "active_task_set",
    "app",
    "app_version",
    "duration_variability",
    "new_version_check_time",
    "newer_version",
    "project",
    "project_files",
    "proxy_info",
    "user_network_request",
    "user_run_request",
    "master_url",
    "project_name",
    "user_name",
    "team_name",
    "email_hash",
    "user_total_credit",
    "user_expavg_credit",
    "user_create_time",
    "host_total_credit",
    "host_expavg_credit",
    "host_create_time",
    "nrpc_failures",
    "master_fetch_failures",
    "min_rpc_time",
    "short_term_debt",
    "long_term_debt",
    "resource_share",
    "scheduler_url",
    "/project",
    "?xml",
];

/// Parse a scheduler request (`SCHEDULER_REQUEST::parse`).
///
/// Errors: [`ProtoError::Empty`] for input without any tag,
/// [`ProtoError::NoStartTag`] when the document is not a
/// `<scheduler_request>`, [`ProtoError::NoEndTag`] when
/// `</scheduler_request>` is missing (a truncated upload), and
/// [`ProtoError::Invalid`] when a preferences element is not closed.
pub fn parse(input: &str) -> Result<SchedulerRequest, ProtoError> {
    let mut xp = XmlParser::new(input);
    let mut r = SchedulerRequest::default();

    if !xp.get_tag() {
        return Err(ProtoError::Empty);
    }
    if xp.match_tag("?xml") && !xp.get_tag() {
        return Err(ProtoError::Empty);
    }
    if !xp.match_tag("scheduler_request") {
        return Err(ProtoError::NoStartTag("scheduler_request"));
    }
    let mut n_unnamed_ip_results = 0;
    while xp.get_tag() {
        if !xp.is_tag() {
            // Text left over from an element that was matched by name and
            // not read, such as <host_venue>.
            continue;
        }
        if xp.match_tag("/scheduler_request") {
            r.core_client_version = r
                .core_client_major_version
                .wrapping_mul(10000)
                .wrapping_add(r.core_client_minor_version.wrapping_mul(100))
                .wrapping_add(r.core_client_release);
            r.working_global_prefs = GlobalPrefs::parse(&r.working_global_prefs_xml, "");
            return Ok(r);
        }
        if let Some(mut s) = xp.parse_str("authenticator") {
            // remove_quotes
            if let Some(i) = s.find(['\'', '"']) {
                s.truncate(i);
            }
            r.authenticator = s;
            continue;
        }
        take!(xp.parse_str("cross_project_id") => r.cross_project_id);
        take!(xp.parse_long("hostid") => r.hostid);
        take!(xp.parse_int("rpc_seqno") => r.rpc_seqno);
        take!(xp.parse_double("uptime") => r.uptime);
        take!(xp.parse_double("previous_uptime") => r.previous_uptime);
        take!(xp.parse_str("platform_name") => r.platform_name);
        if xp.match_tag("alt_platform") {
            let mut name = String::new();
            while xp.get_tag() {
                if xp.match_tag("/alt_platform") {
                    r.alt_platforms.push(name);
                    break;
                }
                take!(xp.parse_str("name") => name);
            }
            continue;
        }
        if xp.match_tag("app_versions") {
            while xp.get_tag() {
                if xp.match_tag("/app_versions") {
                    break;
                }
                if xp.match_tag("app_version") {
                    r.client_app_versions.push(ClientAppVersion::parse(&mut xp));
                }
            }
            continue;
        }
        take!(xp.parse_int("core_client_major_version") => r.core_client_major_version);
        take!(xp.parse_int("core_client_minor_version") => r.core_client_minor_version);
        take!(xp.parse_int("core_client_release") => r.core_client_release);
        take!(xp.parse_double("work_req_seconds") => r.work_req_seconds);
        take!(xp.parse_double("cpu_req_secs") => r.cpu_req_secs);
        take!(xp.parse_double("cpu_req_instances") => r.cpu_req_instances);
        take!(xp.parse_double("resource_share_fraction") => r.resource_share_fraction);
        take!(xp.parse_double("rrs_fraction") => r.rrs_fraction);
        take!(xp.parse_double("prrs_fraction") => r.prrs_fraction);
        take!(xp.parse_double("estimated_delay") => r.cpu_estimated_delay);
        take!(xp.parse_double("duration_correction_factor") => r.duration_correction_factor);
        take!(xp.parse_bool("dont_send_work") => r.dont_send_work);
        take!(xp.parse_bool("dont_use_docker") => r.dont_use_docker);
        take!(xp.parse_bool("dont_use_wsl") => r.dont_use_wsl);
        if xp.match_tag("global_preferences") {
            let body = xp
                .element_contents("</global_preferences>")
                .map_err(|_| ProtoError::Invalid("error copying global prefs".to_string()))?;
            r.global_prefs_xml = format!("<global_preferences>\n{body}\n</global_preferences>\n");
            continue;
        }
        if xp.match_tag("working_global_preferences") {
            r.working_global_prefs_xml = xp
                .element_contents("</working_global_preferences>")
                .map_err(|_| {
                    ProtoError::Invalid("error copying working global prefs".to_string())
                })?;
            continue;
        }
        take!(xp.parse_str("global_prefs_source_email_hash") => r.global_prefs_source_email_hash);
        if xp.match_tag("host_info") {
            r.host_info.parse(&mut xp);
            continue;
        }
        if xp.match_tag("time_stats") {
            let t = &mut r.time_stats;
            while xp.get_tag() {
                if xp.match_tag("/time_stats") {
                    break;
                }
                take!(xp.parse_double("on_frac") => t.on_frac);
                take!(xp.parse_double("connected_frac") => t.connected_frac);
                take!(xp.parse_double("active_frac") => t.active_frac);
                take!(xp.parse_double("gpu_active_frac") => t.gpu_active_frac);
                take!(xp.parse_double("cpu_and_network_available_frac") => t.cpu_and_network_available_frac);
                take!(xp.parse_double("client_start_time") => t.client_start_time);
                take!(xp.parse_double("previous_uptime") => t.previous_uptime);
            }
            continue;
        }
        if xp.match_tag("time_stats_log") {
            if let Ok(s) = xp.copy_element_contents("</time_stats_log>") {
                r.time_stats_log = s;
                r.have_time_stats_log = true;
            }
            continue;
        }
        if xp.match_tag("net_stats") {
            while xp.get_tag() {
                if xp.match_tag("/net_stats") {
                    break;
                }
                take!(xp.parse_double("bwup") => r.net_stats.n_bwup);
                take!(xp.parse_double("bwdown") => r.net_stats.n_bwdown);
            }
            continue;
        }
        if xp.match_tag("disk_usage") {
            let d = &mut r.disk_usage;
            while xp.get_tag() {
                if xp.match_tag("/disk_usage") {
                    break;
                }
                take!(xp.parse_double("d_boinc_used_total") => d.d_boinc_used_total);
                take!(xp.parse_double("d_boinc_used_project") => d.d_boinc_used_project);
                take!(xp.parse_double("d_project_share") => d.d_boinc_max);
            }
            continue;
        }
        if xp.match_tag("result") {
            let Some(result) = ReportedResult::parse(&mut xp) else {
                continue;
            };
            if result.name.contains("download") || result.name.contains("upload") {
                r.file_xfer_results.push(result);
            } else if !r.results.iter().any(|x| x.name == result.name) {
                r.results.push(result);
            }
            continue;
        }
        if xp.match_tag("code_sign_key") {
            if let Ok(s) = xp.copy_element_contents("</code_sign_key>") {
                r.code_sign_key = strip_whitespace(&s).to_string();
            }
            continue;
        }
        if xp.match_tag("msg_from_host") {
            r.msgs_from_host.extend(MsgFromHost::parse(&mut xp));
            continue;
        }
        if xp.match_tag("file_info") {
            let mut fi = FileInfo::default();
            while xp.get_tag() {
                if xp.match_tag("/file_info") {
                    if !fi.name.is_empty() {
                        r.file_infos.push(fi);
                    }
                    break;
                }
                take!(xp.parse_str("name") => fi.name);
                take!(xp.parse_double("nbytes") => fi.nbytes);
                take!(xp.parse_int("status") => fi.status);
                take!(xp.parse_bool("sticky") => fi.sticky);
            }
            continue;
        }
        if xp.match_tag("other_results") {
            r.have_other_results_list = true;
            while xp.get_tag() {
                if xp.match_tag("/other_results") {
                    break;
                }
                if !xp.match_tag("other_result") {
                    continue;
                }
                let mut o = OtherResult {
                    name: String::new(),
                    app_version: -1,
                    plan_class: String::new(),
                    have_plan_class: false,
                    abort: false,
                    abort_if_not_started: false,
                };
                while xp.get_tag() {
                    if xp.match_tag("/other_result") {
                        if !o.name.is_empty() {
                            r.other_results.push(o);
                        }
                        break;
                    }
                    take!(xp.parse_str("name") => o.name);
                    take!(xp.parse_int("app_version") => o.app_version);
                    if let Some(s) = xp.parse_str("plan_class") {
                        o.plan_class = s;
                        o.have_plan_class = true;
                        continue;
                    }
                }
            }
            continue;
        }
        if xp.match_tag("in_progress_results") {
            r.have_ip_results_list = true;
            while xp.get_tag() {
                if xp.match_tag("/in_progress_results") {
                    break;
                }
                if !xp.match_tag("ip_result") {
                    continue;
                }
                let mut ir = IpResult::default();
                while xp.get_tag() {
                    if xp.match_tag("/ip_result") {
                        if ir.name.is_empty() {
                            ir.name = format!("ip{n_unnamed_ip_results}");
                            n_unnamed_ip_results += 1;
                        }
                        r.ip_results.push(ir);
                        break;
                    }
                    take!(xp.parse_str("name") => ir.name);
                    take!(xp.parse_double("report_deadline") => ir.report_deadline);
                    take!(xp.parse_double("cpu_time_remaining") => ir.cpu_time_remaining);
                    take!(xp.parse_double("time_remaining") => ir.time_remaining);
                }
            }
            continue;
        }
        if xp.match_tag("coprocs") {
            if let Ok(c) = Coprocs::parse(&mut xp) {
                r.coprocs = c;
            }
            continue;
        }
        take!(xp.parse_bool("client_cap_plan_class") => r.client_cap_plan_class);
        take!(xp.parse_int("sandbox") => r.sandbox);
        take!(xp.parse_int("allow_multiple_clients") => r.allow_multiple_clients);
        if xp.match_tag("user_keywords") {
            let mut k = UserKeywords::default();
            while xp.get_tag() {
                if xp.match_tag("/user_keywords") {
                    break;
                }
                if let Some(x) = xp.parse_int("yes") {
                    k.yes.push(x);
                } else if let Some(x) = xp.parse_int("no") {
                    k.no.push(x);
                }
            }
            r.user_keywords = k;
            continue;
        }
        take!(xp.parse_str("client_brand") => r.client_brand);
        if IGNORED.contains(&xp.tag()) {
            continue;
        }
        xp.skip_unexpected();
    }
    Err(ProtoError::NoEndTag("</scheduler_request>".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::{
        DOCKER_TYPE_DOCKER, DOCKER_TYPE_PODMAN, PROC_TYPE_NVIDIA_GPU, RESULT_COMPUTE_ERROR,
        RESULT_FILES_UPLOADED,
    };

    /// A Linux host with an NVIDIA GPU and Podman, reporting two results
    /// and asking for work, laid out as `make_scheduler_request` writes it.
    const LINUX_GPU: &str = r#"<scheduler_request>
    <authenticator>0123456789abcdef0123456789abcdef</authenticator>
    <hostid>4217</hostid>
    <rpc_seqno>88</rpc_seqno>
    <core_client_major_version>8</core_client_major_version>
    <core_client_minor_version>2</core_client_minor_version>
    <core_client_release>4</core_client_release>
    <resource_share_fraction>0.500000</resource_share_fraction>
    <rrs_fraction>0.750000</rrs_fraction>
    <prrs_fraction>0.250000</prrs_fraction>
    <duration_correction_factor>1.000000</duration_correction_factor>
    <allow_multiple_clients>0</allow_multiple_clients>
    <sandbox>1</sandbox>
    <dont_send_work>0</dont_send_work>
    <work_req_seconds>86400.000000</work_req_seconds>
    <cpu_req_secs>43200.000000</cpu_req_secs>
    <cpu_req_instances>4.000000</cpu_req_instances>
    <estimated_delay>120.000000</estimated_delay>
    <client_cap_plan_class>1</client_cap_plan_class>
    <platform_name>x86_64-pc-linux-gnu</platform_name>
    <alt_platform>
        <name>i686-pc-linux-gnu</name>
    </alt_platform>
    <code_sign_key>
1024
a2bd2b0f0e6b4d6e9f1c
0000000000000000000000000000000000000000000000000000000000010001
.
</code_sign_key>
<working_global_preferences>
<global_preferences>
   <source_project>https://example.org/proj/</source_project>
   <mod_time>1762800000.000000</mod_time>
   <battery_charge_min_pct>90.000000</battery_charge_min_pct>
   <work_buf_min_days>0.100000</work_buf_min_days>
   <work_buf_additional_days>0.500000</work_buf_additional_days>
   <max_ncpus_pct>75.000000</max_ncpus_pct>
   <disk_max_used_gb>100.000000</disk_max_used_gb>
   <disk_max_used_pct>90.000000</disk_max_used_pct>
   <disk_min_free_gb>1.000000</disk_min_free_gb>
   <ram_max_used_busy_pct>50.000000</ram_max_used_busy_pct>
   <ram_max_used_idle_pct>90.000000</ram_max_used_idle_pct>
</global_preferences>
</working_global_preferences>
<cross_project_id>fedcba9876543210fedcba9876543210</cross_project_id>
<time_stats>
    <on_frac>0.990000</on_frac>
    <connected_frac>0.980000</connected_frac>
    <cpu_and_network_available_frac>0.970000</cpu_and_network_available_frac>
    <active_frac>0.960000</active_frac>
    <gpu_active_frac>0.950000</gpu_active_frac>
    <client_start_time>1762850000.000000</client_start_time>
    <total_start_time>1700000000.000000</total_start_time>
    <total_duration>60000000.000000</total_duration>
    <total_active_duration>59000000.000000</total_active_duration>
    <total_gpu_active_duration>58000000.000000</total_gpu_active_duration>
    <now>1762860000.000000</now>
    <previous_uptime>3600.000000</previous_uptime>
    <session_active_duration>9000.000000</session_active_duration>
    <session_gpu_active_duration>8000.000000</session_gpu_active_duration>
</time_stats>
<net_stats>
    <bwup>1250000.000000</bwup>
    <avg_up>100000.000000</avg_up>
    <avg_time_up>1762850000.000000</avg_time_up>
    <bwdown>12500000.000000</bwdown>
    <avg_down>200000.000000</avg_down>
    <avg_time_down>1762850001.000000</avg_time_down>
</net_stats>
<host_info>
    <timezone>-18000</timezone>
    <domain_name>worker-7</domain_name>
    <ip_addr>192.0.2.17</ip_addr>
    <host_cpid>00112233445566778899aabbccddeeff</host_cpid>
    <p_ncpus>16</p_ncpus>
    <p_vendor>AuthenticAMD</p_vendor>
    <p_model>AMD Ryzen 7 5800X 8-Core Processor [Family 25 Model 33 Stepping 2]</p_model>
    <p_features>fpu vme de pse tsc msr pae mce cx8 apic sep mtrr pge mca cmov pat sse sse2 ht syscall nx avx avx2</p_features>
    <p_fpops>5123456789.000000</p_fpops>
    <p_iops>20123456789.000000</p_iops>
    <p_membw>1000000000.000000</p_membw>
    <p_calculated>1762000000.000000</p_calculated>
    <p_vm_extensions_disabled>0</p_vm_extensions_disabled>
    <m_nbytes>67368910848.000000</m_nbytes>
    <m_cache>524288.000000</m_cache>
    <m_swap>8589930496.000000</m_swap>
    <d_total>982141468672.000000</d_total>
    <d_free>712345678848.000000</d_free>
    <os_name>Linux Debian</os_name>
    <os_version>Debian GNU/Linux 12 (bookworm) [6.1.0-25-amd64|libc 2.36]</os_version>
    <n_usable_coprocs>1</n_usable_coprocs>
    <docker_version>4.3.1</docker_version>
    <docker_type>2</docker_type>
    <docker_compose_version>2.29.1</docker_compose_version>
    <docker_compose_type>1</docker_compose_type>
    <product_name>B550 AORUS &amp; friends</product_name>
    <mac_address>02:00:00:00:00:01</mac_address>
    <virtualbox_version>7.0.20</virtualbox_version>
    <opencl_cpu_prop>
        <platform_vendor>Portable Computing Language</platform_vendor>
        <opencl_cpu_info>
            <name>cpu-haswell-AMD Ryzen 7 5800X</name>
            <vendor>AuthenticAMD</vendor>
            <vendor_id>4098</vendor_id>
            <available>1</available>
            <global_mem_size>65221427200</global_mem_size>
            <max_compute_units>16</max_compute_units>
            <opencl_device_version>OpenCL 3.0 PoCL HSTR</opencl_device_version>
        </opencl_cpu_info>
    </opencl_cpu_prop>
</host_info>
    <disk_usage>
        <d_boinc_used_total>5000000000.000000</d_boinc_used_total>
        <d_boinc_used_project>1000000000.000000</d_boinc_used_project>
        <d_project_share>50000000000.000000</d_project_share>
    </disk_usage>
    <coprocs>
<coproc_cuda>
   <count>1</count>
   <name>NVIDIA GeForce RTX 2060 SUPER</name>
   <available_ram>8368685056.000000</available_ram>
   <have_cuda>1</have_cuda>
   <have_opencl>1</have_opencl>
   <req_secs>86400.000000</req_secs>
   <req_instances>1.000000</req_instances>
   <estimated_delay>0.000000</estimated_delay>
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
</coproc_cuda>
    </coprocs>
<result>
    <name>wu_0001_0</name>
    <final_cpu_time>1234.500000</final_cpu_time>
    <final_elapsed_time>1300.250000</final_elapsed_time>
    <exit_status>0</exit_status>
    <state>5</state>
    <platform>x86_64-pc-linux-gnu</platform>
    <version_num>103</version_num>
    <plan_class>cuda</plan_class>
    <final_peak_working_set_size>123456789</final_peak_working_set_size>
    <final_peak_swap_size>223456789</final_peak_swap_size>
    <final_peak_disk_usage>3456789</final_peak_disk_usage>
    <app_version_num>103</app_version_num>
<stderr_out>
<core_client_version>8.2.4</core_client_version>
<![CDATA[
<stderr_txt>
12:00:01 (4242): wrapper: running worker (--device 0)
a < b && c > d
12:21:41 (4242): called boinc_finish(0)

</stderr_txt>
]]>
</stderr_out>
<file_info>
    <name>wu_0001_0_r123_0</name>
    <nbytes>2048.000000</nbytes>
    <max_nbytes>1000000.000000</max_nbytes>
    <md5_cksum>d41d8cd98f00b204e9800998ecf8427e</md5_cksum>
    <upload_url>https://example.org/proj_cgi/file_upload_handler</upload_url>
</file_info>
<file_info>
    <name>wu_0001_0_r123_1</name>
    <nbytes>17.000000</nbytes>
    <max_nbytes>1000.000000</max_nbytes>
    <md5_cksum>0cc175b9c0f1b6a831c399e269772661</md5_cksum>
    <upload_url>https://example.org/proj_cgi/file_upload_handler</upload_url>
</file_info>
</result>
<result>
    <name>wu_0002_1</name>
    <final_cpu_time>3.000000</final_cpu_time>
    <final_elapsed_time>4.000000</final_elapsed_time>
    <exit_status>195</exit_status>
    <state>3</state>
    <platform>x86_64-pc-linux-gnu</platform>
    <version_num>103</version_num>
    <app_version_num>103</app_version_num>
<stderr_out>
<core_client_version>8.2.4</core_client_version>
<![CDATA[
<message>
process exited with code 195 (0xc3, -61)</message>
<stderr_txt>
SIGSEGV: segmentation violation
</stderr_txt>
]]>
</stderr_out>
</result>
  <msg_from_host>
      <result_name>wu_0003_0</result_name>
      <time>1762859000</time>
<variety>progress</variety>
<pct>41.5</pct>
  </msg_from_host>
    <file_info>
        <name>model_weights_v3.bin</name>
        <nbytes>4294967296.000000</nbytes>
        <status>1</status>
    </file_info>
<time_stats_log>
1762850000.000000 power_on
1762850060.000000 net_connected
</time_stats_log>
<app_versions>
<app_version>
    <app_name>worker</app_name>
    <version_num>103</version_num>
    <platform>x86_64-pc-linux-gnu</platform>
    <plan_class>cuda</plan_class>
    <api_version>8.1.0</api_version>
    <avg_ncpus>0.500000</avg_ncpus>
    <flops>123456789012.000000</flops>
    <coproc>
        <type>NVIDIA</type>
        <count>1.000000</count>
    </coproc>
    <gpu_ram>2147483648.000000</gpu_ram>
    <dont_throttle/>
</app_version>
<app_version>
    <app_name>worker</app_name>
    <version_num>102</version_num>
    <platform>x86_64-pc-linux-gnu</platform>
    <avg_ncpus>1.000000</avg_ncpus>
    <flops>5123456789.000000</flops>
</app_version>
</app_versions>
<other_results>
    <other_result>
        <name>wu_0003_0</name>
        <app_version>0</app_version>
        <plan_class>cuda</plan_class>
    </other_result>
    <other_result>
        <name>wu_0004_0</name>
        <app_version>1</app_version>
    </other_result>
</other_results>
<in_progress_results>
    <ip_result>
        <name>wu_0003_0</name>
        <report_deadline>1763000000</report_deadline>
        <time_remaining>1800.50</time_remaining>
        <avg_ncpus>0.500000</avg_ncpus>
        <ncudas>1.000000</ncudas>
    </ip_result>
    <ip_result>
        <report_deadline>1763100000</report_deadline>
        <time_remaining>7200.00</time_remaining>
        <avg_ncpus>1.000000</avg_ncpus>
    </ip_result>
</in_progress_results>
<client_opaque>
<![CDATA[
<anything> </client_opaque> <hostid>1</hostid>
]]>
</client_opaque>
    <client_brand>Example Brand</client_brand>
<user_keywords>
   <yes>3</yes>
   <yes>17</yes>
   <no>4</no>
</user_keywords>
</scheduler_request>
"#;

    #[test]
    fn linux_gpu_request_top_level() {
        let r = parse(LINUX_GPU).unwrap();
        assert_eq!(r.authenticator, "0123456789abcdef0123456789abcdef");
        assert_eq!(r.hostid, 4217);
        assert_eq!(r.rpc_seqno, 88);
        assert_eq!(
            (
                r.core_client_major_version,
                r.core_client_minor_version,
                r.core_client_release
            ),
            (8, 2, 4)
        );
        assert_eq!(r.core_client_version, 80204);
        assert_eq!(r.resource_share_fraction, 0.5);
        assert_eq!(r.rrs_fraction, 0.75);
        assert_eq!(r.prrs_fraction, 0.25);
        assert_eq!(r.duration_correction_factor, 1.0);
        assert_eq!(r.allow_multiple_clients, 0);
        assert_eq!(r.sandbox, 1);
        assert!(!r.dont_send_work);
        assert!(!r.dont_use_docker);
        assert_eq!(r.work_req_seconds, 86400.0);
        assert_eq!(r.cpu_req_secs, 43200.0);
        assert_eq!(r.cpu_req_instances, 4.0);
        assert_eq!(r.cpu_estimated_delay, 120.0);
        assert!(r.client_cap_plan_class);
        assert_eq!(r.platform_name, "x86_64-pc-linux-gnu");
        assert_eq!(r.alt_platforms, vec!["i686-pc-linux-gnu".to_string()]);
        assert!(r.code_sign_key.starts_with("1024\na2bd"));
        assert!(r.code_sign_key.ends_with("\n."));
        assert_eq!(r.cross_project_id, "fedcba9876543210fedcba9876543210");
        assert_eq!(r.client_brand, "Example Brand");
        assert_eq!(r.user_keywords.yes, vec![3, 17]);
        assert_eq!(r.user_keywords.no, vec![4]);
        assert_eq!(r.global_prefs_xml, "");
        // <client_opaque> is skipped; its CDATA must not leak fields.
        assert_eq!(r.hostid, 4217);
    }

    #[test]
    fn linux_gpu_request_prefs() {
        let r = parse(LINUX_GPU).unwrap();
        assert!(r
            .working_global_prefs_xml
            .starts_with("<global_preferences>"));
        assert!(r
            .working_global_prefs_xml
            .ends_with("</global_preferences>"));
        let p = &r.working_global_prefs;
        assert_eq!(p.mod_time, 1_762_800_000.0);
        assert_eq!(p.work_buf_min_days, 0.1);
        assert_eq!(p.max_ncpus_pct, 75.0);
        assert_eq!(p.disk_max_used_gb, 100.0);
        assert_eq!(p.disk_max_used_pct, 90.0);
        assert_eq!(p.disk_min_free_gb, 1.0);
        assert_eq!(p.ram_max_used_busy_frac, 0.5);
        assert_eq!(p.ram_max_used_idle_frac, 0.9);
    }

    #[test]
    fn linux_gpu_request_host() {
        let r = parse(LINUX_GPU).unwrap();
        let h = &r.host_info;
        assert_eq!(h.timezone, -18000);
        assert_eq!(h.domain_name, "worker-7");
        assert_eq!(h.last_ip_addr, "192.0.2.17");
        assert_eq!(h.host_cpid, "00112233445566778899aabbccddeeff");
        assert_eq!(h.p_ncpus, 16);
        assert_eq!(h.p_vendor, "AuthenticAMD");
        assert!(h.p_model.starts_with("AMD Ryzen 7 5800X"));
        assert!(h.p_features.contains("avx2"));
        assert_eq!(h.p_fpops, 5_123_456_789.0);
        assert_eq!(h.p_iops, 20_123_456_789.0);
        assert_eq!(h.p_membw, 1e9);
        assert!(!h.p_vm_extensions_disabled);
        assert_eq!(h.m_nbytes, 67_368_910_848.0);
        assert_eq!(h.m_cache, 524_288.0);
        assert_eq!(h.m_swap, 8_589_930_496.0);
        assert_eq!(h.d_total, 982_141_468_672.0);
        assert_eq!(h.d_free, 712_345_678_848.0);
        assert_eq!(h.os_name, "Linux Debian");
        assert!(h.os_version.contains("libc 2.36"));
        assert_eq!(h.docker_version, "4.3.1");
        assert_eq!(h.docker_type, DOCKER_TYPE_PODMAN);
        assert_eq!(h.docker_compose_version, "2.29.1");
        assert_eq!(h.docker_compose_type, DOCKER_TYPE_DOCKER);
        assert!(h.has_docker());
        assert_eq!(h.product_name, "B550 AORUS & friends");
        assert_eq!(h.virtualbox_version, "7.0.20");
        assert_eq!(h.opencl_cpu_prop.len(), 1);
        assert_eq!(
            h.opencl_cpu_prop[0].platform_vendor,
            "Portable Computing Language"
        );
        assert_eq!(h.opencl_cpu_prop[0].opencl_prop.max_compute_units, 16);
        assert_eq!(
            h.opencl_cpu_prop[0].opencl_prop.opencl_device_version_int,
            300
        );
        assert!(h.wsl_distros.is_empty());

        let t = &r.time_stats;
        assert_eq!(t.on_frac, 0.99);
        assert_eq!(t.connected_frac, 0.98);
        assert_eq!(t.cpu_and_network_available_frac, 0.97);
        assert_eq!(t.active_frac, 0.96);
        assert_eq!(t.gpu_active_frac, 0.95);
        assert_eq!(t.client_start_time, 1_762_850_000.0);
        assert_eq!(t.previous_uptime, 3600.0);
        assert_eq!(r.net_stats.n_bwup, 1_250_000.0);
        assert_eq!(r.net_stats.n_bwdown, 12_500_000.0);
        assert_eq!(r.disk_usage.d_boinc_used_total, 5e9);
        assert_eq!(r.disk_usage.d_boinc_used_project, 1e9);
        assert_eq!(r.disk_usage.d_boinc_max, 5e10);

        let nv = r.coprocs.nvidia.as_ref().unwrap();
        assert_eq!(nv.base.count, 1);
        assert_eq!(nv.base.req_secs, 86400.0);
        assert_eq!(nv.base.req_instances, 1.0);
        assert_eq!(nv.base.peak_flops, 4.728e12);
        assert_eq!(nv.base.opencl_prop.global_mem_size, 8_589_934_592);
        assert!(r.coprocs.ati.is_none());
    }

    #[test]
    fn linux_gpu_request_results() {
        let r = parse(LINUX_GPU).unwrap();
        assert_eq!(r.results.len(), 2);
        assert!(r.file_xfer_results.is_empty());

        let ok = &r.results[0];
        assert_eq!(ok.name, "wu_0001_0");
        assert_eq!(ok.client_state, RESULT_FILES_UPLOADED);
        assert_eq!(ok.cpu_time, 1234.5);
        assert_eq!(ok.elapsed_time, 1300.25);
        assert_eq!(ok.exit_status, 0);
        assert_eq!(ok.app_version_num, 103);
        assert_eq!(ok.peak_working_set_size, 123_456_789.0);
        assert_eq!(ok.peak_swap_size, 223_456_789.0);
        assert_eq!(ok.peak_disk_usage, 3_456_789.0);
        assert!(ok.stderr_out.starts_with(
            "\n<core_client_version>8.2.4</core_client_version>\n<![CDATA[\n<stderr_txt>\n"
        ));
        assert!(ok.stderr_out.ends_with("</stderr_txt>\n]]>\n"));
        assert!(ok.stderr_out.contains("a < b && c > d"));
        assert_eq!(ok.stderr_client_version().as_deref(), Some("8.2.4"));
        assert!(ok.stderr_text().starts_with("<stderr_txt>\n12:00:01"));
        assert!(ok.stderr_text().ends_with("</stderr_txt>\n"));
        assert!(ok
            .xml_doc_out
            .starts_with("<file_info><name>wu_0001_0_r123_0</name>\n"));
        assert!(ok.xml_doc_out.ends_with("</upload_url></file_info>"));
        assert_eq!(ok.xml_doc_out.matches("<file_info>").count(), 2);
        let files = ok.output_files();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "wu_0001_0_r123_0");
        assert_eq!(files[0].nbytes, 2048.0);
        assert_eq!(files[0].max_nbytes, 1e6);
        assert_eq!(files[0].md5_cksum, "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(files[1].name, "wu_0001_0_r123_1");
        assert_eq!(files[1].nbytes, 17.0);

        let bad = &r.results[1];
        assert_eq!(bad.name, "wu_0002_1");
        assert_eq!(bad.client_state, RESULT_COMPUTE_ERROR);
        assert_eq!(bad.exit_status, 195);
        assert_eq!(bad.xml_doc_out, "");
        assert!(bad.output_files().is_empty());
        assert!(bad.stderr_text().contains("SIGSEGV"));
        assert_eq!(bad.peak_disk_usage, 0.0);
    }

    #[test]
    fn linux_gpu_request_lists() {
        let r = parse(LINUX_GPU).unwrap();

        assert_eq!(r.msgs_from_host.len(), 1);
        let m = &r.msgs_from_host[0];
        assert_eq!(m.variety, "progress");
        assert_eq!(
            m.msg_text,
            "\n      <result_name>wu_0003_0</result_name>\n      <time>1762859000</time>\n<pct>41.5</pct>\n"
        );
        assert_eq!(m.result_name().as_deref(), Some("wu_0003_0"));

        assert_eq!(r.file_infos.len(), 1);
        assert_eq!(r.file_infos[0].name, "model_weights_v3.bin");
        assert_eq!(r.file_infos[0].nbytes, 4_294_967_296.0);
        assert_eq!(r.file_infos[0].status, 1);
        assert!(!r.file_infos[0].sticky);

        assert!(r.have_time_stats_log);
        assert_eq!(
            r.time_stats_log,
            "\n1762850000.000000 power_on\n1762850060.000000 net_connected\n"
        );

        assert_eq!(r.client_app_versions.len(), 2);
        let gpu = &r.client_app_versions[0];
        assert!(gpu.parsed);
        assert_eq!(gpu.app_name, "worker");
        assert_eq!(gpu.version_num, 103);
        assert_eq!(gpu.platform, "x86_64-pc-linux-gnu");
        assert_eq!(gpu.plan_class, "cuda");
        assert_eq!(gpu.avg_ncpus, 0.5);
        assert_eq!(gpu.flops, 123_456_789_012.0);
        assert_eq!(gpu.proc_type, PROC_TYPE_NVIDIA_GPU);
        assert_eq!(gpu.gpu_usage, 1.0);
        let cpu = &r.client_app_versions[1];
        assert_eq!(cpu.plan_class, "");
        assert_eq!(cpu.proc_type, PROC_TYPE_CPU);
        assert_eq!(cpu.gpu_usage, 0.0);

        assert!(r.have_other_results_list);
        assert_eq!(r.other_results.len(), 2);
        assert_eq!(r.other_results[0].name, "wu_0003_0");
        assert_eq!(r.other_results[0].app_version, 0);
        assert!(r.other_results[0].have_plan_class);
        assert_eq!(r.other_results[0].plan_class, "cuda");
        assert_eq!(r.other_results[1].app_version, 1);
        assert!(!r.other_results[1].have_plan_class);
        assert!(!r.other_results[1].abort);

        assert!(r.have_ip_results_list);
        assert_eq!(r.ip_results.len(), 2);
        assert_eq!(r.ip_results[0].name, "wu_0003_0");
        assert_eq!(r.ip_results[0].report_deadline, 1_763_000_000.0);
        assert_eq!(r.ip_results[0].time_remaining, 1800.5);
        assert_eq!(r.ip_results[0].cpu_time_remaining, 0.0);
        assert_eq!(r.ip_results[1].name, "ip0");
    }

    /// A Windows host with WSL distros, one of them running Docker, as
    /// `HOST_INFO::write` and `WSL_DISTRO::write_xml` produce it.
    const WINDOWS_WSL: &str = r#"<?xml version="1.0" encoding="UTF-8" ?>
<scheduler_request>
    <authenticator>aaaabbbbccccddddeeeeffff00001111</authenticator>
    <hostid>0</hostid>
    <rpc_seqno>0</rpc_seqno>
    <core_client_major_version>8</core_client_major_version>
    <core_client_minor_version>2</core_client_minor_version>
    <core_client_release>5</core_client_release>
    <dont_send_work>1</dont_send_work>
    <dont_use_docker/>
    <dont_use_wsl/>
    <work_req_seconds>0.000000</work_req_seconds>
    <platform_name>windows_x86_64</platform_name>
    <alt_platform>
        <name>windows_intelx86</name>
    </alt_platform>
<working_global_preferences>
<global_preferences>
</global_preferences>
</working_global_preferences>
<host_info>
    <timezone>3600</timezone>
    <domain_name>DESKTOP-EXAMPLE</domain_name>
    <ip_addr>198.51.100.4</ip_addr>
    <host_cpid>ffeeddccbbaa99887766554433221100</host_cpid>
    <p_ncpus>8</p_ncpus>
    <p_vendor>GenuineIntel</p_vendor>
    <p_model>Intel(R) Core(TM) i7-9700 CPU @ 3.00GHz [Family 6 Model 158 Stepping 13]</p_model>
    <p_features>fpu vme de pse tsc msr</p_features>
    <p_fpops>4000000000.000000</p_fpops>
    <p_iops>15000000000.000000</p_iops>
    <p_membw>1000000000.000000</p_membw>
    <p_calculated>1762000000.000000</p_calculated>
    <p_vm_extensions_disabled>1</p_vm_extensions_disabled>
    <m_nbytes>34359738368.000000</m_nbytes>
    <m_cache>262144.000000</m_cache>
    <m_swap>39728447488.000000</m_swap>
    <d_total>511101108224.000000</d_total>
    <d_free>211101108224.000000</d_free>
    <os_name>Microsoft Windows 11</os_name>
    <os_version>Professional x64 Edition, (10.00.26100.00)</os_version>
    <n_usable_coprocs>0</n_usable_coprocs>
    <wsl_version>2.4.13.0</wsl_version>
    <wsl>
        <distro>
            <distro_name>boinc-buda-runner</distro_name>
            <os_name>Alpine Linux</os_name>
            <os_version>3.21.0</os_version>
            <wsl_version>2</wsl_version>
            <distro_version>4</distro_version>
            <libc_version>1.2.5</libc_version>
            <docker_version>5.3.2</docker_version>
            <docker_type>2</docker_type>
            <wsl_gpu>
                <name>NVIDIA GeForce GTX 1660</name>
                <has_cuda/>
                <has_opencl/>
            </wsl_gpu>
            <wsl_gpu>
                <has_cuda/>
            </wsl_gpu>
        </distro>
        <distro>
            <distro_name>Ubuntu-24.04</distro_name>
            <os_name>Ubuntu</os_name>
            <os_version>24.04.1 LTS (Noble Numbat)</os_version>
            <wsl_version>2</wsl_version>
            <is_default/>
            <disallowed/>
            <libc_version>2.39</libc_version>
        </distro>
    </wsl>
    <product_name>OptiPlex 7070</product_name>
</host_info>
<app_versions>
</app_versions>
<other_results>
</other_results>
</scheduler_request>
"#;

    #[test]
    fn windows_wsl_request() {
        let r = parse(WINDOWS_WSL).unwrap();
        assert_eq!(r.hostid, 0);
        assert_eq!(r.core_client_version, 80205);
        assert!(r.dont_send_work);
        assert!(r.dont_use_docker);
        assert!(r.dont_use_wsl);
        assert_eq!(r.work_req_seconds, 0.0);
        assert_eq!(r.sandbox, -1);
        assert_eq!(r.allow_multiple_clients, -1);
        assert_eq!(r.resource_share_fraction, 1.0);
        assert_eq!(r.rrs_fraction, 1.0);
        assert_eq!(r.prrs_fraction, 1.0);
        assert_eq!(r.platform_name, "windows_x86_64");
        assert!(!r.client_cap_plan_class);
        assert!(r.have_other_results_list);
        assert!(r.other_results.is_empty());
        assert!(!r.have_ip_results_list);
        assert!(!r.have_time_stats_log);
        assert!(r.coprocs.none());
        assert_eq!(r.working_global_prefs, GlobalPrefs::default());

        let h = &r.host_info;
        assert!(h.p_vm_extensions_disabled);
        assert!(!h.has_docker());
        assert_eq!(h.wsl_version, "2.4.13.0");
        assert!(!h.use_wslc);
        assert_eq!(h.product_name, "OptiPlex 7070");
        assert_eq!(h.wsl_distros.len(), 2);
        let buda = &h.wsl_distros[0];
        assert_eq!(buda.distro_name, "boinc-buda-runner");
        assert_eq!(buda.boinc_buda_runner_version, 4);
        assert_eq!(buda.os_name, "Alpine Linux");
        assert_eq!(buda.wsl_version, 2);
        assert_eq!(buda.libc_version, "1.2.5");
        assert_eq!(buda.docker_version, "5.3.2");
        assert_eq!(buda.docker_type, DOCKER_TYPE_PODMAN);
        assert!(!buda.is_default);
        assert_eq!(
            buda.wsl_gpus,
            vec![WslGpu {
                name: "NVIDIA GeForce GTX 1660".to_string(),
                has_cuda: true,
                has_opencl: true,
            }]
        );
        let ubuntu = &h.wsl_distros[1];
        assert!(ubuntu.is_default);
        assert!(ubuntu.disallowed);
        assert_eq!(ubuntu.docker_version, "");
        assert_eq!(ubuntu.os_version, "24.04.1 LTS (Noble Numbat)");
    }

    fn wrap(body: &str) -> String {
        format!("<scheduler_request>\n{body}\n</scheduler_request>\n")
    }

    #[test]
    fn minimal_request_has_defaults() {
        let r = parse("<scheduler_request>\n</scheduler_request>").unwrap();
        assert_eq!(r, SchedulerRequest::default());
        assert_eq!(r.host_info.p_ncpus, 0);
        let r = parse(&wrap("<host_info>\n</host_info>")).unwrap();
        assert_eq!(r.host_info.p_ncpus, 1);
    }

    #[test]
    fn document_errors() {
        assert_eq!(parse(""), Err(ProtoError::Empty));
        assert_eq!(parse("   \n"), Err(ProtoError::Empty));
        assert_eq!(parse("no tags here"), Err(ProtoError::Empty));
        assert_eq!(parse("<?xml version=\"1.0\" ?>"), Err(ProtoError::Empty));
        assert_eq!(
            parse("<html><body/></html>"),
            Err(ProtoError::NoStartTag("scheduler_request"))
        );
        assert!(matches!(
            parse("<scheduler_request>\n<hostid>1</hostid>\n"),
            Err(ProtoError::NoEndTag(_))
        ));
        assert!(matches!(
            parse("<scheduler_request><global_preferences><a>1</a></scheduler_request>"),
            Err(ProtoError::Invalid(_))
        ));
        assert!(matches!(
            parse("<scheduler_request><working_global_preferences></scheduler_request>"),
            Err(ProtoError::Invalid(_))
        ));
        // A request cut in the middle of a result.
        let cut = &LINUX_GPU[..LINUX_GPU.find("<stderr_txt>").unwrap()];
        assert!(matches!(parse(cut), Err(ProtoError::NoEndTag(_))));
    }

    #[test]
    fn unknown_elements_are_skipped() {
        let r = parse(&wrap(
            "<hostid>7</hostid>
<future_thing>
    <hostid>999</hostid>
    <nested><deeper attr=\"1\">x</deeper><empty/></nested>
</future_thing>
<another_flag/>
<!-- a comment <hostid>998</hostid> -->
<job_log>
1762850000 ue 100.0 ct 90.0 fe 1e12 nm wu_1 et 95.0 es 0
</job_log>
<rpc_seqno>3</rpc_seqno>
<host_venue>home</host_venue>
<project_name>Example &amp; Co</project_name>
<proxy_info>
    <socks_server_name></socks_server_name>
</proxy_info>
<platform_name>x86_64-apple-darwin</platform_name>",
        ))
        .unwrap();
        assert_eq!(r.hostid, 7);
        assert_eq!(r.rpc_seqno, 3);
        assert_eq!(r.platform_name, "x86_64-apple-darwin");
    }

    #[test]
    fn malformed_values_are_ignored() {
        let r = parse(&wrap(
            "<hostid>abc</hostid>
<rpc_seqno>12</rpc_seqno>
<work_req_seconds>lots</work_req_seconds>
<cpu_req_secs>5.5</cpu_req_secs>
<sandbox></sandbox>
<authenticator>abc\"def</authenticator>
<client_brand><b>bold</b></client_brand>
<uptime>100.5</uptime>",
        ))
        .unwrap();
        assert_eq!(r.hostid, 0);
        assert_eq!(r.rpc_seqno, 12);
        assert_eq!(r.work_req_seconds, 0.0);
        assert_eq!(r.cpu_req_secs, 5.5);
        assert_eq!(r.sandbox, 0);
        assert_eq!(r.authenticator, "abc");
        assert_eq!(r.client_brand, "");
        assert_eq!(r.uptime, 100.5);
    }

    #[test]
    fn old_style_global_preferences() {
        let r = parse(&wrap(
            "<global_preferences>
<mod_time>1700000000</mod_time>
<disk_max_used_gb>10</disk_max_used_gb>
<venue name=\"work\">
<disk_max_used_gb>20</disk_max_used_gb>
<max_ncpus_pct>50</max_ncpus_pct>
</venue>
</global_preferences>
<global_prefs_source_email_hash>00ff</global_prefs_source_email_hash>",
        ))
        .unwrap();
        assert!(r
            .global_prefs_xml
            .starts_with("<global_preferences>\n<mod_time>"));
        assert!(r
            .global_prefs_xml
            .ends_with("</venue>\n</global_preferences>\n"));
        assert_eq!(r.global_prefs_source_email_hash, "00ff");
        let general = GlobalPrefs::parse(&r.global_prefs_xml, "");
        assert_eq!(general.mod_time, 1.7e9);
        assert_eq!(general.disk_max_used_gb, 10.0);
        assert_eq!(general.max_ncpus_pct, 0.0);
        let work = GlobalPrefs::parse(&r.global_prefs_xml, "work");
        assert_eq!(work.mod_time, 1.7e9);
        assert_eq!(work.disk_max_used_gb, 20.0);
        assert_eq!(work.max_ncpus_pct, 50.0);
        let home = GlobalPrefs::parse(&r.global_prefs_xml, "home");
        assert_eq!(home.disk_max_used_gb, 10.0);
    }

    #[test]
    fn results_are_filtered() {
        let r = parse(&wrap(
            "<result><name>a_0</name><state>5</state><final_cpu_time>nan</final_cpu_time>\
             <final_elapsed_time>inf</final_elapsed_time></result>
<result><name>a_0</name><state>3</state></result>
<result><name>proj_upload_17</name><state>5</state></result>
<result><name>proj_download_18</name><state>5</state></result>
<result><state>5</state></result>
<result><name>b_0</name><exit_status>-185</exit_status>
<file_ref><file_name>b_0_0</file_name><open_name>out</open_name></file_ref>
<wu_name>b</wu_name><report_deadline>1.0</report_deadline></result>
<result><name>cut_short</name>",
        ));
        // The last result is not closed, so the document has no end either.
        assert!(matches!(r, Err(ProtoError::NoEndTag(_))));

        let r = parse(&wrap(
            "<result><name>a_0</name><state>5</state><final_cpu_time>nan</final_cpu_time>\
             <final_elapsed_time>inf</final_elapsed_time></result>
<result><name>a_0</name><state>3</state></result>
<result><name>proj_upload_17</name><state>5</state></result>
<result><name>proj_download_18</name><state>5</state></result>
<result><state>5</state></result>
<result><name>b_0</name><exit_status>-185</exit_status>
<file_ref><file_name>b_0_0</file_name><open_name>out</open_name></file_ref>
<wu_name>b</wu_name><report_deadline>1.0</report_deadline></result>",
        ))
        .unwrap();
        let names: Vec<&str> = r.results.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["a_0", "", "b_0"]);
        assert_eq!(r.results[0].client_state, 5);
        assert_eq!(r.results[0].cpu_time, 0.0);
        assert_eq!(r.results[0].elapsed_time, 0.0);
        assert_eq!(r.results[2].exit_status, -185);
        assert_eq!(r.results[2].stderr_out, "");
        assert_eq!(r.results[2].stderr_text(), "");
        assert_eq!(r.results[2].stderr_client_version(), None);
        let xfer: Vec<&str> = r
            .file_xfer_results
            .iter()
            .map(|x| x.name.as_str())
            .collect();
        assert_eq!(xfer, vec!["proj_upload_17", "proj_download_18"]);
    }

    #[test]
    fn anonymous_platform_app_versions() {
        let r = parse(&wrap(
            "<platform_name>anonymous</platform_name>
<app_versions>
<app_version>
    <app_name>worker</app_name>
    <version_num>100</version_num>
    <avg_ncpus>-1</avg_ncpus>
    <flops>0</flops>
    <coproc><type>CUDA</type><count>0.5</count></coproc>
</app_version>
<app_version>
    <app_name>worker</app_name>
    <coproc><type>fpga</type><count>2</count></coproc>
    <coproc><count>2</count></coproc>
</app_version>
</app_versions>
<other_results>
<other_result><app_version>1</app_version></other_result>
<other_result><name>x_1</name></other_result>
</other_results>",
        ))
        .unwrap();
        assert_eq!(r.platform_name, "anonymous");
        assert_eq!(r.client_app_versions.len(), 2);
        let a = &r.client_app_versions[0];
        assert_eq!(a.avg_ncpus, 1.0);
        assert_eq!(a.flops, 0.0);
        assert_eq!(a.proc_type, PROC_TYPE_NVIDIA_GPU);
        assert_eq!(a.gpu_usage, 0.5);
        let b = &r.client_app_versions[1];
        assert_eq!(b.proc_type, PROC_TYPE_CPU);
        assert_eq!(b.gpu_usage, 0.0);
        assert!(b.parsed);
        assert_eq!(r.other_results.len(), 1);
        assert_eq!(r.other_results[0].name, "x_1");
        assert_eq!(r.other_results[0].app_version, -1);
    }

    #[test]
    fn unclosed_app_version_is_kept_unparsed() {
        let r = parse("<scheduler_request><app_versions><app_version><app_name>w</app_name>");
        assert!(matches!(r, Err(ProtoError::NoEndTag(_))));
        let mut xp = XmlParser::new("<app_name>w</app_name>");
        let v = ClientAppVersion::parse(&mut xp);
        assert!(!v.parsed);
        assert_eq!(v.app_name, "w");
    }

    #[test]
    fn trickle_messages() {
        let r = parse(&wrap(
            "  <msg_from_host>
      <result_name>r_1</result_name>
      <time>100</time>
<variety>score</variety>
<points>10 &lt; 20</points>
free text line
  </msg_from_host>
  <msg_from_host>
      <result_name>r_2</result_name>
      <time>200</time>

  </msg_from_host>
<hostid>5</hostid>",
        ))
        .unwrap();
        assert_eq!(r.hostid, 5);
        assert_eq!(r.msgs_from_host.len(), 2);
        assert_eq!(r.msgs_from_host[0].variety, "score");
        assert_eq!(
            r.msgs_from_host[0].msg_text,
            "\n      <result_name>r_1</result_name>\n      <time>100</time>\n<points>10 &lt; 20</points>\nfree text line\n"
        );
        assert_eq!(r.msgs_from_host[1].variety, "");
        assert_eq!(r.msgs_from_host[1].result_name().as_deref(), Some("r_2"));
    }

    #[test]
    fn sticky_file_infos() {
        let r = parse(&wrap(
            "<file_info><name>a.bin</name><nbytes>10.000000</nbytes><status>1</status><sticky/></file_info>
<file_info><nbytes>10.000000</nbytes></file_info>",
        ))
        .unwrap();
        assert_eq!(
            r.file_infos,
            vec![FileInfo {
                name: "a.bin".to_string(),
                nbytes: 10.0,
                status: 1,
                sticky: true,
            }]
        );
    }

    #[test]
    fn nul_byte_truncates_the_request() {
        let r = parse("<scheduler_request><hostid>1</hostid>\0</scheduler_request>");
        assert!(matches!(r, Err(ProtoError::NoEndTag(_))));
    }

    #[test]
    fn non_utf8_safe_text() {
        let r = parse(&wrap(
            "<host_info><domain_name>b\u{fc}ro-pc</domain_name><p_model>&#195;&#169;</p_model></host_info>",
        ))
        .unwrap();
        assert_eq!(r.host_info.domain_name, "b\u{fc}ro-pc");
        assert_eq!(r.host_info.p_model, "\u{e9}");
    }
}
