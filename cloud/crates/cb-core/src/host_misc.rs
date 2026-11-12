//! The JSON stored in `host.misc`: what `host_info_json` in
//! `sched/sched_host.cpp` writes on every request (the client's version, its
//! GPUs, WSL and Docker, VirtualBox, and the client's own settings), for tools
//! that read the host table.
//!
//! The layout, key names and the order of the keys are upstream's. Upstream
//! writes strings as they come, so a GPU name with a quote in it makes its
//! JSON invalid; here every string is escaped. It also stores whatever
//! length comes out, up to the column's 64 KiB; here each string is cut at
//! `MAX_TEXT` bytes and the whole text is kept to `MAX_MISC`, so the value is
//! always complete JSON, and the number of GPUs is bounded by the request's
//! parser (at most seven resource types).

use cb_proto::sched_request::{HostInfo, SchedulerRequest};

/// `BOINC_WSL_DISTRO_NAME` in `lib/wslinfo.h`.
const BOINC_WSL_DISTRO_NAME: &str = "boinc-buda-runner";
/// Longest string stored, in bytes.
const MAX_TEXT: usize = 200;
/// Longest value stored, in bytes. Upstream's column holds 65536.
pub const MAX_MISC: usize = 8 * 1024;
const MEGA: f64 = 1_048_576.0;

fn text(s: &str) -> String {
    let mut end = s.len().min(MAX_TEXT);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    serde_json::to_string(&s[..end]).unwrap_or_else(|_| "\"\"".into())
}

fn gpu(kind: &str, model: &str, count: i32, ram_mb: i64, driver: &str, opencl: i32) -> String {
    format!(
        "        {{\n            \"type\": {},\n            \"model\": {},\n            \"count\": {count},\n            \"ram_mb\": {ram_mb},\n            \"driver_version\": {},\n            \"opencl_version\": \"{opencl}\"\n        }}",
        text(kind),
        text(model),
        text(driver)
    )
}

/// `COPROCS::summary_string_json`: one object per kind of GPU the host has.
fn gpus(req: &SchedulerRequest) -> Vec<String> {
    let c = &req.coprocs;
    let mut out = Vec::new();
    if let Some(n) = c.nvidia.as_ref().filter(|n| n.base.count != 0) {
        out.push(gpu(
            "nvidia",
            &n.name,
            n.base.count,
            (n.total_global_mem / MEGA) as i64,
            &n.display_driver_version.to_string(),
            n.base.opencl_prop.opencl_device_version_int,
        ));
    }
    if let Some(a) = c.ati.as_ref().filter(|a| a.base.count != 0) {
        out.push(gpu(
            "amd",
            &a.name,
            a.base.count,
            i64::from(a.local_ram),
            &a.version,
            a.base.opencl_prop.opencl_device_version_int,
        ));
    }
    if let Some(i) = c.intel_gpu.as_ref().filter(|i| i.base.count != 0) {
        out.push(gpu(
            "intel",
            &i.base.opencl_prop.name,
            i.base.count,
            (i.base.opencl_prop.global_mem_size as f64 / MEGA) as i64,
            &i.base.opencl_prop.opencl_driver_version,
            i.base.opencl_prop.opencl_device_version_int,
        ));
    }
    if let Some(a) = c.apple_gpu.as_ref().filter(|a| a.base.count != 0) {
        out.push(gpu(
            "apple",
            &a.model,
            a.base.count,
            (a.base.opencl_prop.global_mem_size as f64 / MEGA) as i64,
            &a.metal_support.to_string(),
            a.base.opencl_prop.opencl_device_version_int,
        ));
    }
    for o in &c.other {
        if o.opencl_prop.name.is_empty() {
            continue;
        }
        out.push(gpu(
            &o.opencl_prop.vendor,
            &o.opencl_prop.name,
            o.count,
            (o.opencl_prop.global_mem_size as f64 / MEGA) as i64,
            &o.opencl_prop.opencl_device_version,
            o.opencl_prop.opencl_device_version_int,
        ));
    }
    out
}

/// What `DOCKER_INFO::get_docker_info` finds: on Windows the WSL version and
/// the distro with Docker, elsewhere Docker itself.
#[derive(Default)]
struct DockerInfo {
    docker_version: String,
    docker_type: i32,
    wsl_version: String,
    use_wslc: bool,
    wsl_distro: String,
    bbr_version: i32,
}

fn docker_info(h: &HostInfo) -> DockerInfo {
    let mut d = DockerInfo::default();
    if h.os_name.contains("Windows") {
        d.wsl_version = h.wsl_version.clone();
        d.use_wslc = h.use_wslc;
        if !h.use_wslc {
            // `WSL_DISTROS::find_docker`: the BOINC distro, else any allowed
            // distro with Docker or Podman.
            let found = h
                .wsl_distros
                .iter()
                .find(|w| w.distro_name == BOINC_WSL_DISTRO_NAME)
                .or_else(|| {
                    h.wsl_distros
                        .iter()
                        .find(|w| !w.disallowed && !w.docker_version.is_empty())
                });
            if let Some(w) = found {
                d.docker_version = w.docker_version.clone();
                d.docker_type = w.docker_type;
                d.wsl_distro = w.distro_name.clone();
                d.bbr_version = w.boinc_buda_runner_version;
            }
        }
    } else {
        d.docker_version = h.docker_version.clone();
        d.docker_type = h.docker_type;
    }
    d
}

/// `host_info_json`.
pub fn host_info_json(req: &SchedulerRequest) -> String {
    let full = build(req, true);
    // Past the cap the GPU list, the one part that can grow, is dropped.
    if full.len() > MAX_MISC {
        return build(req, false);
    }
    full
}

fn build(req: &SchedulerRequest, with_gpus: bool) -> String {
    let h = &req.host_info;
    let mut out = format!(
        "{{\n    \"client_version\": \"{}.{}.{}\"",
        req.core_client_major_version, req.core_client_minor_version, req.core_client_release
    );
    let gpus = if with_gpus { gpus(req) } else { Vec::new() };
    if !gpus.is_empty() {
        out.push_str(",\n    \"gpus\": [\n");
        out.push_str(&gpus.join(",\n"));
        out.push_str("\n    ]");
    }
    let d = docker_info(h);
    if !d.wsl_version.is_empty() {
        out.push_str(&format!(
            ",\n    \"wsl\": {{\n        \"version\": {},\n        \"use_wslc\": {}\n    }}",
            text(&d.wsl_version),
            d.use_wslc
        ));
    }
    if !d.docker_version.is_empty() {
        out.push_str(&format!(
            ",\n    \"docker\": {{\n        \"version\": {},\n        \"type\": {}",
            text(&d.docker_version),
            d.docker_type
        ));
        if !d.wsl_distro.is_empty() {
            out.push_str(&format!(
                ",\n        \"wsl_distro\": {}",
                text(&d.wsl_distro)
            ));
        }
        if d.bbr_version != 0 {
            out.push_str(&format!(
                ",\n        \"boinc_buda_runner_version\": {}",
                d.bbr_version
            ));
        }
        out.push_str("\n    }");
    }
    if !h.virtualbox_version.is_empty() {
        let hw_accel = h.p_features.contains("vmx") || h.p_features.contains("svm");
        out.push_str(&format!(
            ",\n    \"vbox\": {{\n        \"version\": {},\n        \"hw_accel\": {hw_accel},\n        \"hw_accel_enabled\": {}\n    }}",
            text(&h.virtualbox_version),
            !h.p_vm_extensions_disabled
        ));
    }
    if req.dont_use_docker || req.dont_use_wsl {
        out.push_str(&format!(
            ",\n    \"config\": {{\n        \"dont_use_docker\": {},\n        \"dont_use_wsl\": {}\n    }}",
            req.dont_use_docker, req.dont_use_wsl
        ));
    }
    out.push_str("\n}");
    out
}
