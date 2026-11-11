//! The `<coprocs>` element of a scheduler request.
//!
//! Follows `COPROCS::parse`, `COPROC::parse` and the vendor parsers in
//! `lib/coproc.cpp`, and `OPENCL_DEVICE_PROP::parse` in
//! `lib/opencl_boinc.cpp`. Upstream tag names are mixed case
//! (`totalGlobalMem`, `CALVersion`); the Rust fields are their snake_case
//! forms.

use crate::consts::{
    PROC_TYPE_AMD_GPU, PROC_TYPE_APPLE_GPU, PROC_TYPE_CPU, PROC_TYPE_INTEL_GPU,
    PROC_TYPE_NVIDIA_GPU,
};
use crate::xml::{c_atoi, XmlParser};
use crate::ProtoError;

/// Largest number of instances of one coprocessor (`MAX_COPROC_INSTANCES`).
pub const MAX_COPROC_INSTANCES: i32 = 64;

/// Largest number of resource types, the CPU included (`MAX_RSC`).
pub const MAX_RSC: usize = 8;

const MEGA: f64 = 1_048_576.0;

/// Name used in XML for a processor type (`proc_type_name_xml`).
pub fn proc_type_name_xml(proc_type: i32) -> &'static str {
    match proc_type {
        PROC_TYPE_CPU => "CPU",
        PROC_TYPE_NVIDIA_GPU => "NVIDIA",
        PROC_TYPE_AMD_GPU => "ATI",
        PROC_TYPE_INTEL_GPU => "intel_gpu",
        PROC_TYPE_APPLE_GPU => "apple_gpu",
        _ => "unknown",
    }
}

/// Processor type for a coprocessor name, or `None` for names that are not
/// one of the known GPU types (`coproc_type_name_to_num`). `CUDA` is the old
/// name for NVIDIA.
pub fn coproc_type_name_to_num(name: &str) -> Option<i32> {
    match name {
        "CUDA" | "NVIDIA" => Some(PROC_TYPE_NVIDIA_GPU),
        "ATI" => Some(PROC_TYPE_AMD_GPU),
        "intel_gpu" => Some(PROC_TYPE_INTEL_GPU),
        "apple_gpu" => Some(PROC_TYPE_APPLE_GPU),
        _ => None,
    }
}

/// OpenCL properties of a device (`OPENCL_DEVICE_PROP`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OpenclDeviceProp {
    pub name: String,
    pub vendor: String,
    pub vendor_id: u64,
    pub available: i32,
    pub half_fp_config: u64,
    pub single_fp_config: u64,
    pub double_fp_config: u64,
    pub endian_little: i32,
    pub execution_capabilities: u64,
    pub extensions: String,
    pub global_mem_size: u64,
    pub local_mem_size: u64,
    pub max_clock_frequency: i32,
    pub max_compute_units: i32,
    pub nv_compute_capability_major: i32,
    pub nv_compute_capability_minor: i32,
    pub amd_simd_per_compute_unit: i32,
    pub amd_simd_width: i32,
    pub amd_simd_instruction_width: i32,
    pub opencl_platform_version: String,
    pub opencl_device_version: String,
    /// `100 * major + minor`, derived from `opencl_device_version`
    /// (`OpenCL 1.2 ...` gives 102); 0 when the version does not parse.
    pub opencl_device_version_int: i32,
    pub opencl_driver_version: String,
    pub device_num: i32,
    pub peak_flops: f64,
    pub opencl_available_ram: f64,
    pub opencl_device_index: i32,
    pub warn_bad_cuda: bool,
}

/// `sscanf(s, "OpenCL %d.%d", ...)`.
fn opencl_version_int(s: &str) -> i32 {
    let Some(rest) = s.trim_start().strip_prefix("OpenCL") else {
        return 0;
    };
    let rest = rest.trim_start();
    let Some((major, tail)) = rest.split_once('.') else {
        return 0;
    };
    if major.is_empty() || !major.bytes().all(|c| c.is_ascii_digit()) {
        return 0;
    }
    let minor_len = tail.bytes().take_while(|c| c.is_ascii_digit()).count();
    if minor_len == 0 {
        return 0;
    }
    c_atoi(major)
        .saturating_mul(100)
        .saturating_add(c_atoi(&tail[..minor_len]))
}

impl OpenclDeviceProp {
    /// Parse up to `end_tag` (`/coproc_opencl` or `/opencl_cpu_info`).
    /// Returns `None` if the input ends first.
    pub fn parse(xp: &mut XmlParser<'_>, end_tag: &str) -> Option<Self> {
        let mut p = OpenclDeviceProp::default();
        while xp.get_tag() {
            if xp.match_tag(end_tag) {
                p.opencl_device_version_int = opencl_version_int(&p.opencl_device_version);
                return Some(p);
            }
            take!(xp.parse_str("name") => p.name);
            take!(xp.parse_str("vendor") => p.vendor);
            take!(xp.parse_u64("vendor_id") => p.vendor_id);
            take!(xp.parse_int("available") => p.available);
            take!(xp.parse_u64("half_fp_config") => p.half_fp_config);
            take!(xp.parse_u64("single_fp_config") => p.single_fp_config);
            take!(xp.parse_u64("double_fp_config") => p.double_fp_config);
            take!(xp.parse_int("endian_little") => p.endian_little);
            take!(xp.parse_u64("execution_capabilities") => p.execution_capabilities);
            take!(xp.parse_str("extensions") => p.extensions);
            take!(xp.parse_u64("global_mem_size") => p.global_mem_size);
            take!(xp.parse_u64("local_mem_size") => p.local_mem_size);
            take!(xp.parse_int("max_clock_frequency") => p.max_clock_frequency);
            take!(xp.parse_int("max_compute_units") => p.max_compute_units);
            take!(xp.parse_int("nv_compute_capability_major") => p.nv_compute_capability_major);
            take!(xp.parse_int("nv_compute_capability_minor") => p.nv_compute_capability_minor);
            take!(xp.parse_int("amd_simd_per_compute_unit") => p.amd_simd_per_compute_unit);
            take!(xp.parse_int("amd_simd_width") => p.amd_simd_width);
            take!(xp.parse_int("amd_simd_instruction_width") => p.amd_simd_instruction_width);
            take!(xp.parse_str("opencl_platform_version") => p.opencl_platform_version);
            take!(xp.parse_str("opencl_device_version") => p.opencl_device_version);
            take!(xp.parse_str("opencl_driver_version") => p.opencl_driver_version);
            take!(xp.parse_int("device_num") => p.device_num);
            take!(xp.parse_double("peak_flops") => p.peak_flops);
            take!(xp.parse_double("opencl_available_ram") => p.opencl_available_ram);
            take!(xp.parse_int("opencl_device_index") => p.opencl_device_index);
            take!(xp.parse_bool("warn_bad_cuda") => p.warn_bad_cuda);
        }
        None
    }
}

/// Fields common to every coprocessor (`COPROC`). Also the whole
/// description of a coprocessor reported with a generic `<coproc>` element.
#[derive(Debug, Clone, PartialEq)]
pub struct Coproc {
    /// Upstream `type`: `NVIDIA`, `ATI`, `intel_gpu`, `apple_gpu`, or the
    /// name the client gave a generic coprocessor.
    pub coproc_type: String,
    /// Number of instances, between 1 and [`MAX_COPROC_INSTANCES`].
    pub count: i32,
    /// Set for coprocessors that are not GPUs (generic coprocessors only).
    pub non_gpu: bool,
    /// Peak floating point operations per second of one instance.
    pub peak_flops: f64,
    /// Bytes of RAM available on one instance.
    pub available_ram: f64,
    /// Seconds of work requested for this resource.
    pub req_secs: f64,
    /// Idle instances the client wants to fill.
    pub req_instances: f64,
    /// Seconds until an instance becomes free; -1 when not reported.
    pub estimated_delay: f64,
    pub have_opencl: bool,
    pub opencl_prop: OpenclDeviceProp,
    /// Device numbers of the instances (generic coprocessors only);
    /// defaults to `0..64`.
    pub device_nums: Vec<i32>,
}

impl Default for Coproc {
    fn default() -> Self {
        Coproc {
            coproc_type: String::new(),
            count: 0,
            non_gpu: false,
            peak_flops: 0.0,
            available_ram: 0.0,
            req_secs: 0.0,
            req_instances: 0.0,
            estimated_delay: -1.0,
            have_opencl: false,
            opencl_prop: OpenclDeviceProp::default(),
            device_nums: (0..MAX_COPROC_INSTANCES).collect(),
        }
    }
}

impl Coproc {
    fn with_type(name: &str) -> Self {
        Coproc {
            coproc_type: name.to_string(),
            ..Coproc::default()
        }
    }

    /// `COPROC::check_count`: the count is forced into `1..=64`.
    fn check_count(&mut self) {
        self.count = self.count.clamp(1, MAX_COPROC_INSTANCES);
    }

    /// The processor type of this coprocessor, if it is a known GPU type.
    pub fn proc_type(&self) -> Option<i32> {
        coproc_type_name_to_num(&self.coproc_type)
    }

    /// Parse a generic `<coproc>` element (`COPROC::parse`); the start tag
    /// has been read. Fails when the input ends or no `<type>` was given.
    pub fn parse(xp: &mut XmlParser<'_>) -> Result<Self, ProtoError> {
        let mut c = Coproc::default();
        while xp.get_tag() {
            if !xp.is_tag() {
                continue;
            }
            if xp.match_tag("/coproc") {
                if c.coproc_type.is_empty() {
                    return Err(ProtoError::Missing("coproc type"));
                }
                c.check_count();
                return Ok(c);
            }
            if xp.match_tag("coproc_opencl") {
                if let Some(p) = OpenclDeviceProp::parse(xp, "/coproc_opencl") {
                    c.opencl_prop = p;
                }
                continue;
            }
            take!(xp.parse_str("type") => c.coproc_type);
            take!(xp.parse_int("count") => c.count);
            take!(xp.parse_double("req_secs") => c.req_secs);
            take!(xp.parse_double("req_instances") => c.req_instances);
            take!(xp.parse_double("peak_flops") => c.peak_flops);
            if let Some(s) = xp.parse_str("device_nums") {
                let nums = s
                    .split(' ')
                    .filter(|p| !p.is_empty())
                    .take(MAX_COPROC_INSTANCES as usize)
                    .map(c_atoi);
                for (slot, n) in c.device_nums.iter_mut().zip(nums) {
                    *slot = n;
                }
                continue;
            }
            take!(xp.parse_bool("non_gpu") => c.non_gpu);
        }
        Err(ProtoError::NoEndTag("</coproc>".to_string()))
    }
}

/// Three integers separated by single spaces, read the way upstream reads
/// `maxThreadsDim` and `maxGridSize`.
fn three_ints(s: &str) -> [i32; 3] {
    let mut out = [0; 3];
    let mut rest = s;
    for (i, slot) in out.iter_mut().enumerate() {
        if i > 0 {
            match rest.split_once(' ') {
                Some((_, tail)) => rest = tail,
                None => break,
            }
        }
        *slot = c_atoi(rest);
    }
    out
}

/// An NVIDIA GPU (`COPROC_NVIDIA`, element `<coproc_cuda>`).
#[derive(Debug, Clone, PartialEq)]
pub struct CoprocNvidia {
    pub base: Coproc,
    pub have_cuda: bool,
    pub cuda_version: i32,
    /// Upstream tag `drvVersion`, e.g. 53529 for driver 535.29.
    pub display_driver_version: i32,
    pub name: String,
    pub total_global_mem: f64,
    pub shared_mem_per_block: f64,
    pub regs_per_block: i32,
    pub warp_size: i32,
    pub mem_pitch: f64,
    pub max_threads_per_block: i32,
    pub max_threads_dim: [i32; 3],
    pub max_grid_size: [i32; 3],
    pub clock_rate: i32,
    pub total_const_mem: f64,
    pub major: i32,
    pub minor: i32,
    pub texture_alignment: f64,
    pub device_overlap: i32,
    pub multi_processor_count: i32,
}

impl Default for CoprocNvidia {
    fn default() -> Self {
        CoprocNvidia {
            base: Coproc::with_type("NVIDIA"),
            have_cuda: false,
            cuda_version: 0,
            display_driver_version: 0,
            name: String::new(),
            total_global_mem: 0.0,
            shared_mem_per_block: 0.0,
            regs_per_block: 0,
            warp_size: 0,
            mem_pitch: 0.0,
            max_threads_per_block: 0,
            max_threads_dim: [0; 3],
            max_grid_size: [0; 3],
            clock_rate: 0,
            total_const_mem: 0.0,
            major: 0,
            minor: 0,
            texture_alignment: 0.0,
            device_overlap: 0,
            multi_processor_count: 0,
        }
    }
}

impl CoprocNvidia {
    /// Estimate of peak FLOPS from the hardware description
    /// (`COPROC_NVIDIA::set_peak_flops`). Zero when the description is not
    /// enough to tell; the scheduler must cope with that.
    pub fn estimated_peak_flops(&self) -> f64 {
        let ocl = &self.base.opencl_prop;
        let (mut major, mut minor) = (self.major, self.minor);
        if ocl.nv_compute_capability_major != 0 {
            major = ocl.nv_compute_capability_major;
        }
        if ocl.nv_compute_capability_minor != 0 {
            minor = ocl.nv_compute_capability_minor;
        }
        let mut x = 0.0;
        if major != 0 {
            let (flops_per_clock, cores_per_proc) = match major {
                1 => (3.0, 8.0),
                2 => (2.0, if minor == 0 { 32.0 } else { 48.0 }),
                3 => (2.0, 192.0),
                5 => (2.0, 128.0),
                6 => (2.0, if minor == 0 { 64.0 } else { 128.0 }),
                7 => (2.0, 64.0),
                8 => (2.0, if minor == 0 { 64.0 } else { 128.0 }),
                _ => (2.0, 128.0),
            };
            if self.clock_rate != 0 {
                x = 1000.0
                    * f64::from(self.clock_rate)
                    * f64::from(self.multi_processor_count)
                    * cores_per_proc
                    * flops_per_clock;
            } else if ocl.nv_compute_capability_major != 0 {
                x = f64::from(ocl.max_compute_units)
                    * cores_per_proc
                    * flops_per_clock
                    * (f64::from(ocl.max_clock_frequency) * 1e6);
            }
        } else if ocl.max_compute_units != 0 {
            x = f64::from(ocl.max_compute_units)
                * 48.0
                * 2.0
                * (f64::from(ocl.max_clock_frequency) * 1e6);
        }
        x
    }

    /// Parse `<coproc_cuda>`; the start tag has been read. `None` if the
    /// input ends before the end tag.
    pub fn parse(xp: &mut XmlParser<'_>) -> Option<Self> {
        let mut c = CoprocNvidia::default();
        while xp.get_tag() {
            if xp.match_tag("/coproc_cuda") {
                if c.base.peak_flops == 0.0 {
                    c.base.peak_flops = c.estimated_peak_flops();
                }
                if c.base.available_ram == 0.0 {
                    c.base.available_ram = c.total_global_mem;
                }
                c.base.check_count();
                return Some(c);
            }
            take!(xp.parse_int("count") => c.base.count);
            take!(xp.parse_double("peak_flops") => c.base.peak_flops);
            take!(xp.parse_bool("have_cuda") => c.have_cuda);
            take!(xp.parse_bool("have_opencl") => c.base.have_opencl);
            take!(xp.parse_double("available_ram") => c.base.available_ram);
            take!(xp.parse_double("req_secs") => c.base.req_secs);
            take!(xp.parse_double("req_instances") => c.base.req_instances);
            take!(xp.parse_double("estimated_delay") => c.base.estimated_delay);
            take!(xp.parse_int("cudaVersion") => c.cuda_version);
            take!(xp.parse_int("drvVersion") => c.display_driver_version);
            take!(xp.parse_str("name") => c.name);
            take!(xp.parse_double("totalGlobalMem") => c.total_global_mem);
            take!(xp.parse_double("sharedMemPerBlock") => c.shared_mem_per_block);
            take!(xp.parse_int("regsPerBlock") => c.regs_per_block);
            take!(xp.parse_int("warpSize") => c.warp_size);
            take!(xp.parse_double("memPitch") => c.mem_pitch);
            take!(xp.parse_int("maxThreadsPerBlock") => c.max_threads_per_block);
            if let Some(s) = xp.parse_str("maxThreadsDim") {
                c.max_threads_dim = three_ints(&s);
                continue;
            }
            if let Some(s) = xp.parse_str("maxGridSize") {
                c.max_grid_size = three_ints(&s);
                continue;
            }
            take!(xp.parse_int("clockRate") => c.clock_rate);
            take!(xp.parse_double("totalConstMem") => c.total_const_mem);
            take!(xp.parse_int("major") => c.major);
            take!(xp.parse_int("minor") => c.minor);
            take!(xp.parse_double("textureAlignment") => c.texture_alignment);
            take!(xp.parse_int("deviceOverlap") => c.device_overlap);
            take!(xp.parse_int("multiProcessorCount") => c.multi_processor_count);
            if xp.match_tag("coproc_opencl") {
                c.base.opencl_prop = OpenclDeviceProp::parse(xp, "/coproc_opencl")?;
                continue;
            }
        }
        None
    }
}

/// An AMD/ATI GPU (`COPROC_ATI`, element `<coproc_ati>`).
#[derive(Debug, Clone, PartialEq)]
pub struct CoprocAti {
    pub base: Coproc,
    pub have_cal: bool,
    pub name: String,
    /// Upstream tag `CALVersion`, e.g. `1.4.1848`.
    pub version: String,
    /// `major * 10000000 + minor * 10000 + release`, from `version`.
    pub version_num: i32,
    pub amdrt_detected: bool,
    pub atirt_detected: bool,
    pub target: i32,
    /// Megabytes (upstream tag `localRAM`).
    pub local_ram: i32,
    pub uncached_remote_ram: i32,
    pub cached_remote_ram: i32,
    pub engine_clock: i32,
    pub memory_clock: i32,
    pub wavefront_size: i32,
    pub number_of_simd: i32,
    pub double_precision: i32,
    pub pitch_alignment: i32,
    pub surface_alignment: i32,
    pub max_resource_1d_width: i32,
    pub max_resource_2d_width: i32,
    pub max_resource_2d_height: i32,
}

impl Default for CoprocAti {
    fn default() -> Self {
        CoprocAti {
            base: Coproc::with_type("ATI"),
            have_cal: false,
            name: String::new(),
            version: String::new(),
            version_num: 0,
            amdrt_detected: false,
            atirt_detected: false,
            target: 0,
            local_ram: 0,
            uncached_remote_ram: 0,
            cached_remote_ram: 0,
            engine_clock: 0,
            memory_clock: 0,
            wavefront_size: 0,
            number_of_simd: 0,
            double_precision: 0,
            pitch_alignment: 0,
            surface_alignment: 0,
            max_resource_1d_width: 0,
            max_resource_2d_width: 0,
            max_resource_2d_height: 0,
        }
    }
}

/// `sscanf(s, "%d.%d.%d", ...)` requiring all three numbers.
fn cal_version_num(s: &str) -> i32 {
    let mut parts = s.trim().splitn(3, '.');
    let mut nums = [0i32; 3];
    for slot in nums.iter_mut() {
        let Some(part) = parts.next() else {
            return 0;
        };
        let digits = part.bytes().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return 0;
        }
        *slot = c_atoi(&part[..digits]);
    }
    nums[0]
        .saturating_mul(10_000_000)
        .saturating_add(nums[1].saturating_mul(10_000))
        .saturating_add(nums[2])
}

impl CoprocAti {
    /// Estimate of peak FLOPS (`COPROC_ATI::set_peak_flops`).
    pub fn estimated_peak_flops(&self) -> f64 {
        let ocl = &self.base.opencl_prop;
        let mut x = 0.0;
        if self.number_of_simd != 0 {
            x = f64::from(self.number_of_simd)
                * f64::from(self.wavefront_size)
                * 5.0
                * f64::from(self.engine_clock)
                * 1e6;
        } else if ocl.amd_simd_per_compute_unit != 0 {
            x = f64::from(ocl.max_compute_units)
                * f64::from(ocl.amd_simd_per_compute_unit)
                * f64::from(ocl.amd_simd_width)
                * f64::from(ocl.amd_simd_instruction_width)
                * 2.0
                * (f64::from(ocl.max_clock_frequency) * 1e6);
        } else if ocl.max_compute_units != 0 {
            x = f64::from(ocl.max_compute_units)
                * 16.0
                * 5.0
                * (f64::from(ocl.max_clock_frequency) * 1e6);
        }
        x
    }

    /// Parse `<coproc_ati>`; the start tag has been read.
    pub fn parse(xp: &mut XmlParser<'_>) -> Option<Self> {
        let mut c = CoprocAti::default();
        while xp.get_tag() {
            if xp.match_tag("/coproc_ati") {
                c.version_num = cal_version_num(&c.version);
                if c.base.peak_flops == 0.0 {
                    c.base.peak_flops = c.estimated_peak_flops();
                }
                if c.base.available_ram == 0.0 {
                    c.base.available_ram = f64::from(c.local_ram) * MEGA;
                }
                c.base.check_count();
                return Some(c);
            }
            take!(xp.parse_int("count") => c.base.count);
            take!(xp.parse_double("peak_flops") => c.base.peak_flops);
            take!(xp.parse_bool("have_cal") => c.have_cal);
            take!(xp.parse_bool("have_opencl") => c.base.have_opencl);
            take!(xp.parse_double("available_ram") => c.base.available_ram);
            take!(xp.parse_double("req_secs") => c.base.req_secs);
            take!(xp.parse_double("req_instances") => c.base.req_instances);
            take!(xp.parse_double("estimated_delay") => c.base.estimated_delay);
            take!(xp.parse_str("name") => c.name);
            take!(xp.parse_str("CALVersion") => c.version);
            take!(xp.parse_bool("amdrt_detected") => c.amdrt_detected);
            take!(xp.parse_bool("atirt_detected") => c.atirt_detected);
            take!(xp.parse_int("target") => c.target);
            take!(xp.parse_int("localRAM") => c.local_ram);
            take!(xp.parse_int("uncachedRemoteRAM") => c.uncached_remote_ram);
            take!(xp.parse_int("cachedRemoteRAM") => c.cached_remote_ram);
            take!(xp.parse_int("engineClock") => c.engine_clock);
            take!(xp.parse_int("memoryClock") => c.memory_clock);
            take!(xp.parse_int("wavefrontSize") => c.wavefront_size);
            take!(xp.parse_int("numberOfSIMD") => c.number_of_simd);
            take!(xp.parse_int("doublePrecision") => c.double_precision);
            take!(xp.parse_int("pitch_alignment") => c.pitch_alignment);
            take!(xp.parse_int("surface_alignment") => c.surface_alignment);
            take!(xp.parse_int("maxResource1DWidth") => c.max_resource_1d_width);
            take!(xp.parse_int("maxResource2DWidth") => c.max_resource_2d_width);
            take!(xp.parse_int("maxResource2DHeight") => c.max_resource_2d_height);
            if xp.match_tag("coproc_opencl") {
                c.base.opencl_prop = OpenclDeviceProp::parse(xp, "/coproc_opencl")?;
                continue;
            }
        }
        None
    }
}

/// An Intel GPU (`COPROC_INTEL`, element `<coproc_intel_gpu>`).
#[derive(Debug, Clone, PartialEq)]
pub struct CoprocIntel {
    pub base: Coproc,
}

impl Default for CoprocIntel {
    fn default() -> Self {
        CoprocIntel {
            base: Coproc::with_type("intel_gpu"),
        }
    }
}

impl CoprocIntel {
    /// Estimate of peak FLOPS (`COPROC_INTEL::set_peak_flops`).
    pub fn estimated_peak_flops(&self) -> f64 {
        let ocl = &self.base.opencl_prop;
        f64::from(ocl.max_compute_units) * 8.0 * f64::from(ocl.max_clock_frequency) * 1e6
    }

    /// Parse `<coproc_intel_gpu>`; the start tag has been read.
    pub fn parse(xp: &mut XmlParser<'_>) -> Option<Self> {
        let mut c = CoprocIntel::default();
        while xp.get_tag() {
            if xp.match_tag("/coproc_intel_gpu") {
                if c.base.peak_flops == 0.0 {
                    c.base.peak_flops = c.estimated_peak_flops();
                }
                if c.base.available_ram == 0.0 {
                    c.base.available_ram = c.base.opencl_prop.global_mem_size as f64;
                }
                c.base.check_count();
                return Some(c);
            }
            take!(xp.parse_int("count") => c.base.count);
            take!(xp.parse_double("peak_flops") => c.base.peak_flops);
            take!(xp.parse_bool("have_opencl") => c.base.have_opencl);
            take!(xp.parse_double("available_ram") => c.base.available_ram);
            take!(xp.parse_double("req_secs") => c.base.req_secs);
            take!(xp.parse_double("req_instances") => c.base.req_instances);
            take!(xp.parse_double("estimated_delay") => c.base.estimated_delay);
            if xp.match_tag("coproc_opencl") {
                c.base.opencl_prop = OpenclDeviceProp::parse(xp, "/coproc_opencl")?;
                continue;
            }
        }
        None
    }
}

/// An Apple GPU (`COPROC_APPLE`, element `<coproc_apple_gpu>`).
#[derive(Debug, Clone, PartialEq)]
pub struct CoprocApple {
    pub base: Coproc,
    pub model: String,
    pub have_metal: bool,
    pub ncores: i32,
    pub metal_support: i32,
}

impl Default for CoprocApple {
    fn default() -> Self {
        CoprocApple {
            base: Coproc::with_type("apple_gpu"),
            model: String::new(),
            have_metal: false,
            ncores: 0,
            metal_support: 0,
        }
    }
}

impl CoprocApple {
    /// Estimate of peak FLOPS (`COPROC_APPLE::set_peak_flops`): from the
    /// OpenCL description when there is one, else 100 GFLOPS.
    pub fn estimated_peak_flops(&self) -> f64 {
        let ocl = &self.base.opencl_prop;
        if ocl.max_compute_units != 0 {
            f64::from(ocl.max_compute_units) * 8.0 * f64::from(ocl.max_clock_frequency) * 1e6
        } else {
            1e11
        }
    }

    /// Parse `<coproc_apple_gpu>`; the start tag has been read.
    pub fn parse(xp: &mut XmlParser<'_>) -> Option<Self> {
        let mut c = CoprocApple::default();
        while xp.get_tag() {
            if xp.match_tag("/coproc_apple_gpu") {
                if c.base.peak_flops == 0.0 {
                    c.base.peak_flops = c.estimated_peak_flops();
                }
                if c.base.available_ram == 0.0 {
                    c.base.available_ram = c.base.opencl_prop.global_mem_size as f64;
                }
                c.base.check_count();
                return Some(c);
            }
            take!(xp.parse_int("count") => c.base.count);
            take!(xp.parse_str("model") => c.model);
            take!(xp.parse_double("peak_flops") => c.base.peak_flops);
            take!(xp.parse_bool("have_opencl") => c.base.have_opencl);
            take!(xp.parse_bool("have_metal") => c.have_metal);
            take!(xp.parse_double("available_ram") => c.base.available_ram);
            take!(xp.parse_int("ncores") => c.ncores);
            take!(xp.parse_int("metal_support") => c.metal_support);
            take!(xp.parse_double("req_secs") => c.base.req_secs);
            take!(xp.parse_double("req_instances") => c.base.req_instances);
            take!(xp.parse_double("estimated_delay") => c.base.estimated_delay);
            if xp.match_tag("coproc_opencl") {
                c.base.opencl_prop = OpenclDeviceProp::parse(xp, "/coproc_opencl")?;
                continue;
            }
        }
        None
    }
}

/// The coprocessors of a host (`COPROCS`).
///
/// A vendor field is `None` when the host did not report that kind of GPU,
/// which upstream expresses as a count of zero.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Coprocs {
    pub nvidia: Option<CoprocNvidia>,
    pub ati: Option<CoprocAti>,
    pub intel_gpu: Option<CoprocIntel>,
    pub apple_gpu: Option<CoprocApple>,
    /// Coprocessors reported with a generic `<coproc>` element.
    pub other: Vec<Coproc>,
}

impl Coprocs {
    /// Parse `<coprocs>`; the start tag has been read (`COPROCS::parse`).
    ///
    /// A vendor element that is not closed leaves that vendor unset. At
    /// most `MAX_RSC - 1` coprocessors are kept. Fails only when the input
    /// ends before `</coprocs>`; what was parsed until then is lost, as it
    /// is upstream (the scheduler ignores the error and carries on with
    /// what it has, which is nothing).
    pub fn parse(xp: &mut XmlParser<'_>) -> Result<Self, ProtoError> {
        let mut c = Coprocs::default();
        let mut n_rsc = 1usize;
        while xp.get_tag() {
            if xp.match_tag("/coprocs") {
                return Ok(c);
            }
            let room = n_rsc < MAX_RSC;
            if xp.match_tag("coproc_cuda") {
                c.nvidia = CoprocNvidia::parse(xp).filter(|_| room);
                n_rsc += usize::from(c.nvidia.is_some());
            } else if xp.match_tag("coproc_ati") {
                c.ati = CoprocAti::parse(xp).filter(|_| room);
                n_rsc += usize::from(c.ati.is_some());
            } else if xp.match_tag("coproc_intel_gpu") {
                c.intel_gpu = CoprocIntel::parse(xp).filter(|_| room);
                n_rsc += usize::from(c.intel_gpu.is_some());
            } else if xp.match_tag("coproc_apple_gpu") {
                c.apple_gpu = CoprocApple::parse(xp).filter(|_| room);
                n_rsc += usize::from(c.apple_gpu.is_some());
            } else if xp.match_tag("coproc") {
                if let Ok(cp) = Coproc::parse(xp) {
                    if room {
                        c.other.push(cp);
                        n_rsc += 1;
                    }
                }
            }
        }
        Err(ProtoError::NoEndTag("</coprocs>".to_string()))
    }

    /// Whether the host reported no coprocessors (`COPROCS::none`).
    pub fn none(&self) -> bool {
        self.count() == 0
    }

    /// Number of coprocessor kinds reported.
    pub fn count(&self) -> usize {
        usize::from(self.nvidia.is_some())
            + usize::from(self.ati.is_some())
            + usize::from(self.intel_gpu.is_some())
            + usize::from(self.apple_gpu.is_some())
            + self.other.len()
    }

    /// Common fields of the GPU of the given processor type
    /// (`PROC_TYPE_NVIDIA_GPU` and so on), if the host has one.
    pub fn proc_type(&self, proc_type: i32) -> Option<&Coproc> {
        match proc_type {
            PROC_TYPE_NVIDIA_GPU => self.nvidia.as_ref().map(|c| &c.base),
            PROC_TYPE_AMD_GPU => self.ati.as_ref().map(|c| &c.base),
            PROC_TYPE_INTEL_GPU => self.intel_gpu.as_ref().map(|c| &c.base),
            PROC_TYPE_APPLE_GPU => self.apple_gpu.as_ref().map(|c| &c.base),
            _ => None,
        }
    }

    /// Common fields of every coprocessor, vendor GPUs first.
    pub fn all(&self) -> Vec<&Coproc> {
        let mut v: Vec<&Coproc> = Vec::new();
        v.extend(self.nvidia.as_ref().map(|c| &c.base));
        v.extend(self.ati.as_ref().map(|c| &c.base));
        v.extend(self.intel_gpu.as_ref().map(|c| &c.base));
        v.extend(self.apple_gpu.as_ref().map(|c| &c.base));
        v.extend(self.other.iter());
        v
    }

    /// Look a coprocessor up by its type name (`COPROCS::lookup_type`).
    pub fn lookup_type(&self, name: &str) -> Option<&Coproc> {
        self.all().into_iter().find(|c| c.coproc_type == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Result<Coprocs, ProtoError> {
        let mut xp = XmlParser::new(src);
        assert!(xp.parse_start("coprocs"));
        Coprocs::parse(&mut xp)
    }

    /// As written by `COPROC_NVIDIA::write_xml` in the client.
    const NVIDIA: &str = "    <coprocs>
<coproc_cuda>
   <count>2</count>
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
   <sharedMemPerBlock>49152.000000</sharedMemPerBlock>
   <regsPerBlock>65536</regsPerBlock>
   <warpSize>32</warpSize>
   <memPitch>2147483647.000000</memPitch>
   <maxThreadsPerBlock>1024</maxThreadsPerBlock>
   <maxThreadsDim>1024 1024 64</maxThreadsDim>
   <maxGridSize>2147483647 65535 65535</maxGridSize>
   <clockRate>1000000</clockRate>
   <totalConstMem>65536.000000</totalConstMem>
   <major>7</major>
   <minor>5</minor>
   <textureAlignment>512.000000</textureAlignment>
   <deviceOverlap>1</deviceOverlap>
   <multiProcessorCount>34</multiProcessorCount>
   <pci_info>
      <bus_id>1</bus_id>
      <device_id>0</device_id>
      <domain_id>0</domain_id>
   </pci_info>
   <coproc_opencl>
      <name>NVIDIA GeForce RTX 2060 SUPER</name>
      <vendor>NVIDIA Corporation</vendor>
      <vendor_id>4318</vendor_id>
      <available>1</available>
      <half_fp_config>0</half_fp_config>
      <single_fp_config>191</single_fp_config>
      <double_fp_config>63</double_fp_config>
      <endian_little>1</endian_little>
      <execution_capabilities>1</execution_capabilities>
      <extensions>cl_khr_global_int32_base_atomics cl_khr_fp64 cl_nv_device_attribute_query</extensions>
      <global_mem_size>8589934592</global_mem_size>
      <local_mem_size>32768</local_mem_size>
      <max_clock_frequency>1000</max_clock_frequency>
      <max_compute_units>34</max_compute_units>
      <nv_compute_capability_major>7</nv_compute_capability_major>
      <nv_compute_capability_minor>5</nv_compute_capability_minor>
      <amd_simd_per_compute_unit>0</amd_simd_per_compute_unit>
      <amd_simd_width>0</amd_simd_width>
      <amd_simd_instruction_width>0</amd_simd_instruction_width>
      <opencl_platform_version>OpenCL 3.0 CUDA 12.2.138</opencl_platform_version>
      <opencl_device_version>OpenCL 3.0 CUDA</opencl_device_version>
      <opencl_driver_version>535.129.03</opencl_driver_version>
   </coproc_opencl>
</coproc_cuda>
    </coprocs>
";

    #[test]
    fn nvidia_as_client_writes_it() {
        let c = parse(NVIDIA).unwrap();
        assert_eq!(c.count(), 1);
        assert!(!c.none());
        let nv = c.nvidia.as_ref().unwrap();
        assert_eq!(nv.base.coproc_type, "NVIDIA");
        assert_eq!(nv.base.count, 2);
        assert_eq!(nv.name, "NVIDIA GeForce RTX 2060 SUPER");
        assert_eq!(nv.base.available_ram, 8_368_685_056.0);
        assert!(nv.have_cuda);
        assert!(nv.base.have_opencl);
        assert_eq!(nv.base.req_secs, 86400.0);
        assert_eq!(nv.base.req_instances, 1.0);
        assert_eq!(nv.base.estimated_delay, 0.0);
        assert_eq!(nv.base.peak_flops, 4.728e12);
        assert_eq!(nv.cuda_version, 12020);
        assert_eq!(nv.display_driver_version, 53529);
        assert_eq!(nv.total_global_mem, 8_589_934_592.0);
        assert_eq!(nv.max_threads_dim, [1024, 1024, 64]);
        assert_eq!(nv.max_grid_size, [2_147_483_647, 65535, 65535]);
        assert_eq!((nv.major, nv.minor), (7, 5));
        assert_eq!(nv.multi_processor_count, 34);
        let ocl = &nv.base.opencl_prop;
        assert_eq!(ocl.vendor_id, 4318);
        assert_eq!(ocl.global_mem_size, 8_589_934_592);
        assert_eq!(ocl.opencl_device_version_int, 300);
        assert_eq!(ocl.opencl_driver_version, "535.129.03");
        assert!(ocl.extensions.contains("cl_khr_fp64"));
        assert_eq!(c.proc_type(PROC_TYPE_NVIDIA_GPU).unwrap().count, 2);
        assert!(c.proc_type(PROC_TYPE_AMD_GPU).is_none());
        assert_eq!(c.lookup_type("NVIDIA").unwrap().count, 2);
    }

    #[test]
    fn nvidia_defaults_are_derived() {
        let c = parse(
            "<coprocs><coproc_cuda><count>0</count><totalGlobalMem>1024.0</totalGlobalMem>\
             <clockRate>1000000</clockRate><major>7</major><minor>5</minor>\
             <multiProcessorCount>34</multiProcessorCount></coproc_cuda></coprocs>",
        )
        .unwrap();
        let nv = c.nvidia.unwrap();
        assert_eq!(nv.base.count, 1);
        assert_eq!(nv.base.available_ram, 1024.0);
        assert_eq!(nv.base.estimated_delay, -1.0);
        // 1000 * clockRate * processors * 64 cores * 2 flops per clock
        assert_eq!(nv.base.peak_flops, 1000.0 * 1e6 * 34.0 * 64.0 * 2.0);
    }

    #[test]
    fn nvidia_flops_from_opencl_only() {
        let c = parse(
            "<coprocs><coproc_cuda><count>1</count><coproc_opencl>\
             <max_compute_units>10</max_compute_units>\
             <max_clock_frequency>1000</max_clock_frequency>\
             <nv_compute_capability_major>2</nv_compute_capability_major>\
             <nv_compute_capability_minor>1</nv_compute_capability_minor>\
             </coproc_opencl></coproc_cuda></coprocs>",
        )
        .unwrap();
        assert_eq!(c.nvidia.unwrap().base.peak_flops, 10.0 * 48.0 * 2.0 * 1e9);
    }

    #[test]
    fn count_is_clamped() {
        let c = parse("<coprocs><coproc_cuda><count>1000</count></coproc_cuda></coprocs>").unwrap();
        let nv = c.nvidia.unwrap();
        assert_eq!(nv.base.count, 64);
        assert_eq!(nv.base.peak_flops, 0.0);
    }

    #[test]
    fn ati_as_client_writes_it() {
        let c = parse(
            "<coprocs>
<coproc_ati>
   <count>1</count>
   <name>AMD Radeon HD 6900 series (Cayman)</name>
   <available_ram>2122317824.000000</available_ram>
   <have_cal>1</have_cal>
   <have_opencl>0</have_opencl>
   <req_secs>0.000000</req_secs>
   <req_instances>0.000000</req_instances>
   <estimated_delay>120.500000</estimated_delay>
   <CALVersion>1.4.1848</CALVersion>
   <target>20</target>
   <localRAM>2048</localRAM>
   <uncachedRemoteRAM>2047</uncachedRemoteRAM>
   <cachedRemoteRAM>508</cachedRemoteRAM>
   <engineClock>880</engineClock>
   <memoryClock>1375</memoryClock>
   <wavefrontSize>64</wavefrontSize>
   <numberOfSIMD>24</numberOfSIMD>
   <doublePrecision>1</doublePrecision>
   <pitch_alignment>256</pitch_alignment>
   <surface_alignment>4096</surface_alignment>
   <maxResource1DWidth>16384</maxResource1DWidth>
   <maxResource2DWidth>16384</maxResource2DWidth>
   <maxResource2DHeight>16384</maxResource2DHeight>
    <atirt_detected/>
</coproc_ati>
</coprocs>",
        )
        .unwrap();
        let ati = c.ati.unwrap();
        assert_eq!(ati.base.coproc_type, "ATI");
        assert!(ati.have_cal);
        assert!(!ati.base.have_opencl);
        assert_eq!(ati.version, "1.4.1848");
        assert_eq!(ati.version_num, 10_041_848);
        assert!(ati.atirt_detected);
        assert!(!ati.amdrt_detected);
        assert_eq!(ati.local_ram, 2048);
        assert_eq!(ati.base.estimated_delay, 120.5);
        assert_eq!(ati.base.peak_flops, 24.0 * 64.0 * 5.0 * 880.0 * 1e6);
        assert_eq!(ati.max_resource_2d_height, 16384);
    }

    #[test]
    fn ati_ram_defaults_to_local_ram() {
        let c =
            parse("<coprocs><coproc_ati><localRAM>512</localRAM></coproc_ati></coprocs>").unwrap();
        assert_eq!(c.ati.unwrap().base.available_ram, 512.0 * 1_048_576.0);
    }

    #[test]
    fn intel_and_apple() {
        let c = parse(
            "<coprocs>
<coproc_intel_gpu>
   <count>1</count>
   <name>Intel(R) UHD Graphics 630</name>
   <have_opencl>1</have_opencl>
   <req_secs>10.000000</req_secs>
   <req_instances>1.000000</req_instances>
   <estimated_delay>0.000000</estimated_delay>
   <coproc_opencl>
      <name>Intel(R) UHD Graphics 630</name>
      <global_mem_size>6841753600</global_mem_size>
      <max_clock_frequency>1150</max_clock_frequency>
      <max_compute_units>24</max_compute_units>
      <opencl_device_version>OpenCL 2.1 NEO </opencl_device_version>
   </coproc_opencl>
</coproc_intel_gpu>
<coproc_apple_gpu>
   <count>1</count>
   <model>Apple M2</model>
   <available_ram>17179869184.000000</available_ram>
   <have_metal>1</have_metal>
   <have_opencl>1</have_opencl>
   <ncores>10</ncores>
   <metal_support>3</metal_support>
   <req_secs>0.000000</req_secs>
   <req_instances>0.000000</req_instances>
   <estimated_delay>0.000000</estimated_delay>
   <peak_flops>0.000000</peak_flops>
</coproc_apple_gpu>
</coprocs>",
        )
        .unwrap();
        assert_eq!(c.count(), 2);
        let intel = c.intel_gpu.as_ref().unwrap();
        assert_eq!(intel.base.available_ram, 6_841_753_600.0);
        assert_eq!(intel.base.peak_flops, 24.0 * 8.0 * 1150.0 * 1e6);
        assert_eq!(intel.base.opencl_prop.opencl_device_version_int, 201);
        assert_eq!(intel.base.req_secs, 10.0);
        let apple = c.apple_gpu.as_ref().unwrap();
        assert_eq!(apple.model, "Apple M2");
        assert!(apple.have_metal);
        assert_eq!(apple.ncores, 10);
        assert_eq!(apple.metal_support, 3);
        assert_eq!(apple.base.peak_flops, 1e11);
        assert_eq!(c.all().len(), 2);
        assert_eq!(
            c.proc_type(PROC_TYPE_APPLE_GPU).unwrap().coproc_type,
            "apple_gpu"
        );
    }

    #[test]
    fn generic_coproc() {
        let c = parse(
            "<coprocs>
<coproc>
   <type>miner_asic</type>
   <count>3</count>
   <peak_flops>1000.000000</peak_flops>
   <device_nums>4 5 6</device_nums>
   <non_gpu/>
</coproc>
<coproc>
   <count>1</count>
</coproc>
</coprocs>",
        )
        .unwrap();
        assert_eq!(c.other.len(), 1);
        let cp = &c.other[0];
        assert_eq!(cp.coproc_type, "miner_asic");
        assert_eq!(cp.count, 3);
        assert!(cp.non_gpu);
        assert_eq!(&cp.device_nums[..4], &[4, 5, 6, 3]);
        assert_eq!(cp.proc_type(), None);
        assert_eq!(c.lookup_type("miner_asic").unwrap().peak_flops, 1000.0);
    }

    #[test]
    fn empty_and_truncated() {
        assert!(parse("<coprocs>\n</coprocs>").unwrap().none());
        assert!(parse("<coprocs><coproc_cuda><count>1</count>").is_err());
        assert!(parse("<coprocs>").is_err());
    }

    #[test]
    fn at_most_seven_coprocessors() {
        let mut src = String::from("<coprocs>");
        for i in 0..10 {
            src.push_str(&format!(
                "<coproc><type>t{i}</type><count>1</count></coproc>"
            ));
        }
        src.push_str("</coprocs>");
        assert_eq!(parse(&src).unwrap().other.len(), MAX_RSC - 1);
    }

    #[test]
    fn type_names() {
        assert_eq!(coproc_type_name_to_num("CUDA"), Some(PROC_TYPE_NVIDIA_GPU));
        assert_eq!(
            coproc_type_name_to_num("NVIDIA"),
            Some(PROC_TYPE_NVIDIA_GPU)
        );
        assert_eq!(coproc_type_name_to_num("ATI"), Some(PROC_TYPE_AMD_GPU));
        assert_eq!(
            coproc_type_name_to_num("intel_gpu"),
            Some(PROC_TYPE_INTEL_GPU)
        );
        assert_eq!(
            coproc_type_name_to_num("apple_gpu"),
            Some(PROC_TYPE_APPLE_GPU)
        );
        assert_eq!(coproc_type_name_to_num("fpga"), None);
        assert_eq!(proc_type_name_xml(PROC_TYPE_CPU), "CPU");
        assert_eq!(proc_type_name_xml(PROC_TYPE_INTEL_GPU), "intel_gpu");
    }

    #[test]
    fn helper_parsers() {
        assert_eq!(three_ints("1 2 3"), [1, 2, 3]);
        assert_eq!(three_ints("1 2"), [1, 2, 0]);
        assert_eq!(three_ints(""), [0, 0, 0]);
        assert_eq!(cal_version_num("1.4.1848"), 10_041_848);
        assert_eq!(cal_version_num("1.4"), 0);
        assert_eq!(opencl_version_int("OpenCL 1.2 AMD-APP (1800.8)"), 102);
        assert_eq!(opencl_version_int("something else"), 0);
    }
}
