//! Project configuration: the `<config>` element of upstream `config.xml`,
//! limited to the options cloud-boinc acts on. Tag names and defaults follow
//! `SCHED_CONFIG::parse` in `sched/sched_config.cpp`.

use cb_proto::xml::XmlParser;

use crate::{CoreError, CoreResult};

/// Upper bound on the CPU count used to scale per-host limits.
pub const MAX_NCPUS: i32 = 64;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub master_url: String,
    pub long_name: String,
    pub download_url: String,
    pub upload_url: String,
    pub daily_result_quota: i32,
    pub max_wus_to_send: i32,
    /// Per-CPU limit on CPU jobs in progress; 0 is no limit.
    pub max_wus_in_progress: i32,
    /// Per-GPU limit on GPU jobs in progress; 0 is no limit.
    pub max_wus_in_progress_gpu: i32,
    pub max_ncpus: i32,
    pub gpu_multiplier: i32,
    pub min_sendwork_interval: i32,
    pub next_rpc_delay: f64,
    pub min_core_client_version: i32,
    pub one_result_per_user_per_wu: bool,
    pub one_result_per_host_per_wu: bool,
    pub resend_lost_results: bool,
    pub send_result_abort: bool,
    pub report_grace_period: i32,
    pub report_max: i32,
    pub max_results_accepted: i32,
    /// Seconds between assimilation and file deletion.
    pub delete_delay: f64,
    pub dont_generate_upload_certificates: bool,
    pub ignore_upload_certificates: bool,
    pub dont_store_success_stderr: bool,
    pub dont_send_jobs: bool,
    pub ended: bool,
    pub non_cpu_intensive: bool,
    pub verify_files_on_app_start: bool,
    pub strict_memory_bound: bool,
    pub ignore_delay_bound: bool,
    pub enable_assignment: bool,
    pub default_disk_min_free_gb: f64,
    pub reliable_priority_on_over: i32,
    pub reliable_priority_on_over_except_error: i32,
    pub version_select_random_factor: f64,
    pub prefer_primary_platform: bool,
    pub user_url: bool,
    pub user_country: bool,
    /// Sender address of the email the site sends (`EMAIL_FROM` in upstream's
    /// `project.inc`). Empty: the site sends no email.
    pub email_from: String,
    /// Sender name (`EMAIL_FROM_NAME`).
    pub email_from_name: String,
    // cloud-boinc options without an upstream counterpart.
    /// Whether `create_account` is open to anyone.
    pub account_creation: bool,
    /// Upper bound on the credit of one job; 0 is no bound.
    pub max_granted_credit: f64,
    /// Upper bound in bytes on one uploaded file, whatever its certificate says.
    pub max_upload_nbytes: f64,
    /// Days to keep finished workunits before they are purged; 0 keeps them.
    pub purge_days: f64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            master_url: String::new(),
            long_name: String::new(),
            download_url: String::new(),
            upload_url: String::new(),
            daily_result_quota: 0,
            max_wus_to_send: 10,
            max_wus_in_progress: 0,
            max_wus_in_progress_gpu: 0,
            max_ncpus: MAX_NCPUS,
            gpu_multiplier: 0,
            min_sendwork_interval: 0,
            next_rpc_delay: 0.0,
            min_core_client_version: 0,
            one_result_per_user_per_wu: false,
            one_result_per_host_per_wu: false,
            resend_lost_results: false,
            send_result_abort: false,
            report_grace_period: 0,
            report_max: 0,
            max_results_accepted: 0,
            delete_delay: 0.0,
            dont_generate_upload_certificates: false,
            ignore_upload_certificates: false,
            dont_store_success_stderr: false,
            dont_send_jobs: false,
            ended: false,
            non_cpu_intensive: false,
            verify_files_on_app_start: false,
            strict_memory_bound: false,
            ignore_delay_bound: false,
            enable_assignment: false,
            default_disk_min_free_gb: 1.0,
            reliable_priority_on_over: 0,
            reliable_priority_on_over_except_error: 0,
            version_select_random_factor: 1.0,
            prefer_primary_platform: false,
            user_url: true,
            user_country: true,
            email_from: String::new(),
            email_from_name: String::new(),
            account_creation: false,
            max_granted_credit: 200.0 * 1000.0 * 365.0,
            max_upload_nbytes: 100e6,
            purge_days: 0.0,
        }
    }
}

macro_rules! take {
    ($xp:ident . $method:ident ( $name:literal ) => $dst:expr) => {
        if let Some(v) = $xp.$method($name) {
            $dst = v;
            continue;
        }
    };
}

impl Config {
    /// Parses `<boinc><config>...</config></boinc>` or a bare `<config>`.
    pub fn parse(xml: &str) -> CoreResult<Config> {
        let mut c = Config::default();
        let mut xp = XmlParser::new(xml);
        let mut in_config = false;
        while xp.get_tag() {
            if !xp.is_tag() {
                continue;
            }
            if !in_config {
                if xp.match_tag("config") {
                    in_config = true;
                }
                continue;
            }
            if xp.match_tag("/config") {
                c.finish();
                return Ok(c);
            }
            take!(xp.parse_str("master_url") => c.master_url);
            take!(xp.parse_str("long_name") => c.long_name);
            take!(xp.parse_str("download_url") => c.download_url);
            take!(xp.parse_str("upload_url") => c.upload_url);
            take!(xp.parse_int("daily_result_quota") => c.daily_result_quota);
            take!(xp.parse_int("max_wus_to_send") => c.max_wus_to_send);
            take!(xp.parse_int("max_wus_in_progress") => c.max_wus_in_progress);
            take!(xp.parse_int("max_wus_in_progress_gpu") => c.max_wus_in_progress_gpu);
            take!(xp.parse_int("max_ncpus") => c.max_ncpus);
            take!(xp.parse_int("gpu_multiplier") => c.gpu_multiplier);
            take!(xp.parse_int("min_sendwork_interval") => c.min_sendwork_interval);
            take!(xp.parse_double("next_rpc_delay") => c.next_rpc_delay);
            if let Some(v) = xp.parse_int("min_core_client_version") {
                // Accept both 70000 and the older 700 form, as upstream does.
                c.min_core_client_version = if v < 10000 { v * 100 } else { v };
                continue;
            }
            take!(xp.parse_bool("one_result_per_user_per_wu") => c.one_result_per_user_per_wu);
            take!(xp.parse_bool("one_result_per_host_per_wu") => c.one_result_per_host_per_wu);
            take!(xp.parse_bool("resend_lost_results") => c.resend_lost_results);
            take!(xp.parse_bool("send_result_abort") => c.send_result_abort);
            if let Some(v) = xp.parse_double("grace_period_hours") {
                c.report_grace_period = (v * 3600.0) as i32;
                continue;
            }
            take!(xp.parse_int("report_grace_period") => c.report_grace_period);
            take!(xp.parse_int("report_max") => c.report_max);
            take!(xp.parse_int("max_results_accepted") => c.max_results_accepted);
            if let Some(v) = xp.parse_double("delete_delay_hours") {
                c.delete_delay = v * 3600.0;
                continue;
            }
            take!(xp.parse_bool("dont_generate_upload_certificates") => c.dont_generate_upload_certificates);
            take!(xp.parse_bool("ignore_upload_certificates") => c.ignore_upload_certificates);
            take!(xp.parse_bool("dont_store_success_stderr") => c.dont_store_success_stderr);
            take!(xp.parse_bool("dont_send_jobs") => c.dont_send_jobs);
            take!(xp.parse_bool("ended") => c.ended);
            take!(xp.parse_bool("non_cpu_intensive") => c.non_cpu_intensive);
            take!(xp.parse_bool("verify_files_on_app_start") => c.verify_files_on_app_start);
            take!(xp.parse_bool("strict_memory_bound") => c.strict_memory_bound);
            take!(xp.parse_bool("ignore_delay_bound") => c.ignore_delay_bound);
            take!(xp.parse_bool("enable_assignment") => c.enable_assignment);
            take!(xp.parse_double("default_disk_min_free_gb") => c.default_disk_min_free_gb);
            take!(xp.parse_int("reliable_priority_on_over") => c.reliable_priority_on_over);
            take!(xp.parse_int("reliable_priority_on_over_except_error") => c.reliable_priority_on_over_except_error);
            take!(xp.parse_double("version_select_random_factor") => c.version_select_random_factor);
            take!(xp.parse_bool("prefer_primary_platform") => c.prefer_primary_platform);
            take!(xp.parse_bool("user_url") => c.user_url);
            take!(xp.parse_bool("user_country") => c.user_country);
            take!(xp.parse_str("email_from") => c.email_from);
            take!(xp.parse_str("email_from_name") => c.email_from_name);
            take!(xp.parse_bool("account_creation") => c.account_creation);
            take!(xp.parse_double("max_granted_credit") => c.max_granted_credit);
            take!(xp.parse_double("max_upload_nbytes") => c.max_upload_nbytes);
            take!(xp.parse_double("purge_days") => c.purge_days);
            // There are many upstream tags cloud-boinc has no use for.
            xp.skip_unexpected();
        }
        Err(CoreError::Xml(
            "config: no <config> element, or it is not closed".into(),
        ))
    }

    fn finish(&mut self) {
        for url in [
            &mut self.master_url,
            &mut self.download_url,
            &mut self.upload_url,
        ] {
            while url.ends_with('/') {
                url.pop();
            }
        }
        if !self.master_url.is_empty() {
            // The master URL is an identity: clients compare it as text, with
            // the trailing slash.
            self.master_url.push('/');
            let base = self.master_url.clone();
            if self.download_url.is_empty() {
                self.download_url = format!("{base}download");
            }
            if self.upload_url.is_empty() {
                // A client takes a `<url>` for an upload URL only when it
                // contains `file_upload_handler`.
                self.upload_url = format!("{base}upload/file_upload_handler");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_upstream_style_config() {
        let c = Config::parse(
            "<boinc>\n<config>\n<master_url>https://example.org/</master_url>\n\
             <long_name>Example</long_name>\n<daily_result_quota>500</daily_result_quota>\n\
             <db_passwd>x</db_passwd>\n<one_result_per_user_per_wu/>\n\
             <delete_delay_hours>2</delete_delay_hours>\n<resend_lost_results>1</resend_lost_results>\n\
             <min_core_client_version>709</min_core_client_version>\n</config>\n<daemons>\n</daemons>\n</boinc>\n",
        )
        .unwrap();
        assert_eq!(c.master_url, "https://example.org/");
        assert_eq!(
            c.upload_url,
            "https://example.org/upload/file_upload_handler"
        );
        assert_eq!(c.download_url, "https://example.org/download");
        assert_eq!(c.daily_result_quota, 500);
        assert!(c.one_result_per_user_per_wu);
        assert!(c.resend_lost_results);
        assert_eq!(c.delete_delay, 7200.0);
        assert_eq!(c.min_core_client_version, 70900);
        assert_eq!(c.max_wus_to_send, 10);
    }

    #[test]
    fn parses_the_sender_settings() {
        let c = Config::parse(
            "<config><master_url>https://example.org/</master_url>\n\
             <email_from>boinc@example.org</email_from>\n\
             <email_from_name>Example Project</email_from_name></config>",
        )
        .unwrap();
        assert_eq!(c.email_from, "boinc@example.org");
        assert_eq!(c.email_from_name, "Example Project");
        let none = Config::parse("<config><master_url>https://example.org/</master_url></config>")
            .unwrap();
        assert_eq!(
            (none.email_from.as_str(), none.email_from_name.as_str()),
            ("", "")
        );
    }

    #[test]
    fn unclosed_config_is_an_error() {
        assert!(Config::parse("<config><long_name>x</long_name>").is_err());
    }
}
