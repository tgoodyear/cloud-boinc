//! Rows of the tables in `cloud/migrations`. Field names are column names.
//!
//! Every struct tolerates missing columns, so a query may select only what
//! it needs.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Platform {
    pub id: i64,
    pub name: String,
    pub user_friendly_name: String,
    pub deprecated: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct App {
    pub id: i64,
    pub create_time: i64,
    pub name: String,
    pub min_version: i32,
    pub deprecated: i32,
    pub user_friendly_name: String,
    pub weight: f64,
    pub beta: i32,
    pub target_nresults: i32,
    pub non_cpu_intensive: i32,
    pub fraction_done_exact: i32,
    pub validator: String,
    pub validator_url: String,
    pub assimilate_url: String,
    /// The homogeneous redundancy type: 0 for none, 1 fine, 2 coarse. When
    /// 0, the project's `<homogeneous_redundancy>` applies.
    pub homogeneous_redundancy: i32,
    /// Nonzero: a workunit's results all go out with one app version.
    pub homogeneous_app_version: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppVersion {
    pub id: i64,
    pub create_time: i64,
    pub appid: i64,
    pub version_num: i32,
    pub platformid: i64,
    pub xml_doc: String,
    pub min_core_version: i32,
    pub max_core_version: i32,
    pub deprecated: i32,
    pub plan_class: String,
    pub pfc_n: f64,
    pub pfc_avg: f64,
    pub pfc_scale: f64,
    pub expavg_credit: f64,
    pub expavg_time: f64,
    pub beta: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct User {
    pub id: i64,
    pub create_time: i64,
    pub email_addr: String,
    pub name: String,
    pub authenticator: String,
    pub country: String,
    pub postal_code: String,
    pub total_credit: f64,
    pub expavg_credit: f64,
    pub expavg_time: f64,
    pub global_prefs: String,
    pub project_prefs: String,
    pub teamid: i64,
    pub venue: String,
    pub url: String,
    pub send_email: i32,
    pub show_hosts: i32,
    pub cross_project_id: String,
    pub passwd_hash: String,
    pub email_validated: i32,
    pub blocked: i32,
    /// The address before the last change, while the previous address can
    /// still undo it; empty otherwise.
    pub previous_email_addr: String,
    /// When the address was last changed through the site with email on.
    pub email_addr_change_time: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Host {
    pub id: i64,
    pub create_time: i64,
    pub userid: i64,
    pub rpc_seqno: i64,
    pub rpc_time: i64,
    pub total_credit: f64,
    pub expavg_credit: f64,
    pub expavg_time: f64,
    pub timezone: i32,
    pub domain_name: String,
    pub serialnum: String,
    pub last_ip_addr: String,
    pub nsame_ip_addr: i32,
    pub on_frac: f64,
    pub connected_frac: f64,
    pub active_frac: f64,
    pub cpu_efficiency: f64,
    pub duration_correction_factor: f64,
    pub p_ncpus: i32,
    pub p_vendor: String,
    pub p_model: String,
    pub p_fpops: f64,
    pub p_iops: f64,
    pub p_membw: f64,
    pub os_name: String,
    pub os_version: String,
    pub m_nbytes: f64,
    pub m_cache: f64,
    pub m_swap: f64,
    pub d_total: f64,
    pub d_free: f64,
    pub d_boinc_used_total: f64,
    pub d_boinc_used_project: f64,
    pub d_boinc_max: f64,
    pub n_bwup: f64,
    pub n_bwdown: f64,
    pub credit_per_cpu_sec: f64,
    pub venue: String,
    pub nresults_today: i32,
    pub avg_turnaround: f64,
    pub host_cpid: String,
    pub external_ip_addr: String,
    pub max_results_day: i32,
    pub error_rate: f64,
    pub product_name: String,
    pub gpu_active_frac: f64,
    pub p_ngpus: i32,
    pub p_gpu_fpops: f64,
    pub misc: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostAppVersion {
    pub host_id: i64,
    /// An app version ID. For anonymous platform it is the generalized ID
    /// `appid * 1000000 - resource type` instead (`anon_gavid`).
    pub app_version_id: i64,
    pub pfc_n: f64,
    pub pfc_avg: f64,
    pub et_n: f64,
    pub et_avg: f64,
    pub et_var: f64,
    pub et_q: f64,
    pub max_jobs_per_day: i32,
    pub n_jobs_today: i32,
    pub turnaround_n: f64,
    pub turnaround_avg: f64,
    pub turnaround_var: f64,
    pub turnaround_q: f64,
    pub consecutive_valid: i32,
}

impl HostAppVersion {
    pub fn et(&self) -> crate::avg::AverageVar {
        crate::avg::AverageVar {
            n: self.et_n,
            avg: self.et_avg,
            var: self.et_var,
            q: self.et_q,
        }
    }

    pub fn set_et(&mut self, a: crate::avg::AverageVar) {
        self.et_n = a.n;
        self.et_avg = a.avg;
        self.et_var = a.var;
        self.et_q = a.q;
    }

    pub fn turnaround(&self) -> crate::avg::AverageVar {
        crate::avg::AverageVar {
            n: self.turnaround_n,
            avg: self.turnaround_avg,
            var: self.turnaround_var,
            q: self.turnaround_q,
        }
    }

    pub fn set_turnaround(&mut self, a: crate::avg::AverageVar) {
        self.turnaround_n = a.n;
        self.turnaround_avg = a.avg;
        self.turnaround_var = a.var;
        self.turnaround_q = a.q;
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Workunit {
    pub id: i64,
    pub create_time: i64,
    pub appid: i64,
    pub name: String,
    pub xml_doc: String,
    pub batch: i64,
    pub rsc_fpops_est: f64,
    pub rsc_fpops_bound: f64,
    pub rsc_memory_bound: f64,
    pub rsc_disk_bound: f64,
    pub need_validate: i32,
    pub canonical_resultid: i64,
    pub canonical_credit: f64,
    pub transition_time: i64,
    pub delay_bound: i64,
    pub error_mask: i32,
    pub file_delete_state: i32,
    pub assimilate_state: i32,
    pub opaque: f64,
    pub min_quorum: i32,
    pub target_nresults: i32,
    pub max_error_results: i32,
    pub max_total_results: i32,
    pub max_success_results: i32,
    pub result_template: String,
    pub priority: i32,
    pub mod_time: i64,
    pub rsc_bandwidth_bound: f64,
    pub app_version_id: i64,
    pub transitioner_flags: i32,
    pub keywords: String,
    pub app_version_num: i32,
    pub n_sent: i32,
    pub rev: i64,
    /// The homogeneous redundancy class the workunit is committed to; 0 when
    /// it is not committed.
    pub hr_class: i32,
}

/// A row of `result`. Named `JobResult` to keep `Result` free.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobResult {
    pub id: i64,
    pub create_time: i64,
    pub workunitid: i64,
    pub server_state: i32,
    pub outcome: i32,
    pub client_state: i32,
    pub hostid: i64,
    pub userid: i64,
    pub report_deadline: i64,
    pub sent_time: i64,
    pub received_time: i64,
    pub name: String,
    pub cpu_time: f64,
    pub xml_doc_in: String,
    pub xml_doc_out: String,
    pub stderr_out: String,
    pub batch: i64,
    pub file_delete_state: i32,
    pub validate_state: i32,
    pub claimed_credit: f64,
    pub granted_credit: f64,
    pub opaque: f64,
    pub random: i64,
    pub app_version_num: i32,
    pub appid: i64,
    pub exit_status: i32,
    pub teamid: i64,
    pub priority: i32,
    pub mod_time: i64,
    pub elapsed_time: f64,
    pub flops_estimate: f64,
    pub app_version_id: i64,
    pub runtime_outlier: i32,
    pub peak_working_set_size: f64,
    pub peak_swap_size: f64,
    pub peak_disk_usage: f64,
}

/// Every `result` column except the two large client-supplied ones.
pub const RESULT_COLS_LIGHT: &str = "id, create_time, workunitid, server_state, outcome, client_state, \
     hostid, userid, report_deadline, sent_time, received_time, name, cpu_time, xml_doc_in, batch, \
     file_delete_state, validate_state, claimed_credit, granted_credit, opaque, random, app_version_num, \
     appid, exit_status, teamid, priority, mod_time, elapsed_time, flops_estimate, app_version_id, \
     runtime_outlier, peak_working_set_size, peak_swap_size, peak_disk_usage";

/// `generalized_app_version_id` in `sched/sched_util.h`: the key of
/// `host_app_version` for a result. Anonymous-platform results carry a
/// negative `app_version_id` naming the resource type, and share one
/// `host_app_version` row per app and resource type:
/// `appid * 1000000 - app_version_id`, so `appid * 1000000 + 2` for CPU.
pub fn generalized_app_version_id(app_version_id: i64, appid: i64) -> i64 {
    if app_version_id < 0 {
        appid * 1_000_000 - app_version_id
    } else {
        app_version_id
    }
}

/// `HOST_USAGE::resource_type` in `sched/sched_types.h`: the
/// `result.app_version_id` of a result an anonymous-platform client runs on
/// this kind of processor.
pub fn anon_platform_id(proc_type: i32) -> i64 {
    use cb_proto::consts::*;
    match proc_type {
        PROC_TYPE_NVIDIA_GPU => ANON_PLATFORM_NVIDIA,
        PROC_TYPE_AMD_GPU => ANON_PLATFORM_ATI,
        PROC_TYPE_INTEL_GPU => ANON_PLATFORM_INTEL_GPU,
        PROC_TYPE_APPLE_GPU => ANON_PLATFORM_APPLE_GPU,
        _ => ANON_PLATFORM_CPU,
    }
}

/// The processor type an anonymous-platform result's `app_version_id` names;
/// `None` for anything else (a real version, or the unknown type).
pub fn anon_proc_type(app_version_id: i64) -> Option<i32> {
    use cb_proto::consts::*;
    match app_version_id {
        ANON_PLATFORM_CPU => Some(PROC_TYPE_CPU),
        ANON_PLATFORM_NVIDIA => Some(PROC_TYPE_NVIDIA_GPU),
        ANON_PLATFORM_ATI => Some(PROC_TYPE_AMD_GPU),
        ANON_PLATFORM_INTEL_GPU => Some(PROC_TYPE_INTEL_GPU),
        ANON_PLATFORM_APPLE_GPU => Some(PROC_TYPE_APPLE_GPU),
        _ => None,
    }
}

/// `host_usage_to_gavid` in `sched/sched_version.cpp`: the
/// `host_app_version.app_version_id` of an app for anonymous-platform
/// clients, by the processor type their version uses.
pub fn anon_gavid(appid: i64, proc_type: i32) -> i64 {
    generalized_app_version_id(anon_platform_id(proc_type), appid)
}

/// Whether a result's `app_version_id` means it runs on a GPU: an
/// anonymous-platform GPU resource type.
pub fn anon_uses_gpu(app_version_id: i64) -> bool {
    app_version_id <= cb_proto::consts::ANON_PLATFORM_NVIDIA
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_proto::consts::*;

    #[test]
    fn anonymous_platform_ids_follow_upstream() {
        // `ANON_PLATFORM_*` in `db/boinc_db_types.h`
        assert_eq!(anon_platform_id(PROC_TYPE_CPU), -2);
        assert_eq!(anon_platform_id(PROC_TYPE_NVIDIA_GPU), -3);
        assert_eq!(anon_platform_id(PROC_TYPE_AMD_GPU), -4);
        assert_eq!(anon_platform_id(PROC_TYPE_INTEL_GPU), -5);
        assert_eq!(anon_platform_id(PROC_TYPE_APPLE_GPU), -6);
        for pt in [
            PROC_TYPE_CPU,
            PROC_TYPE_NVIDIA_GPU,
            PROC_TYPE_AMD_GPU,
            PROC_TYPE_INTEL_GPU,
            PROC_TYPE_APPLE_GPU,
        ] {
            assert_eq!(anon_proc_type(anon_platform_id(pt)), Some(pt));
        }
        assert_eq!(anon_proc_type(ANON_PLATFORM_UNKNOWN), None);
        assert_eq!(anon_proc_type(7), None);
        // `appid * 1000000 - app_version_id`; what `av_desc` in
        // `html/user/host_app_versions.php` takes apart again.
        assert_eq!(anon_gavid(3, PROC_TYPE_CPU), 3_000_002);
        assert_eq!(anon_gavid(3, PROC_TYPE_NVIDIA_GPU), 3_000_003);
        assert_eq!(generalized_app_version_id(-2, 12), 12_000_002);
        // A real version keeps its own ID, and so does "unknown".
        assert_eq!(generalized_app_version_id(7, 12), 7);
        assert_eq!(generalized_app_version_id(0, 12), 0);
        assert_eq!(generalized_app_version_id(-1, 12), 12_000_001);
    }

    #[test]
    fn gpu_results_are_told_apart_by_their_id() {
        assert!(!anon_uses_gpu(7));
        assert!(!anon_uses_gpu(0));
        assert!(!anon_uses_gpu(ANON_PLATFORM_UNKNOWN));
        assert!(!anon_uses_gpu(ANON_PLATFORM_CPU));
        assert!(anon_uses_gpu(ANON_PLATFORM_NVIDIA));
        assert!(anon_uses_gpu(ANON_PLATFORM_APPLE_GPU));
    }
}
