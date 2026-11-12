//! Project configuration: the `<config>` element of upstream `config.xml`,
//! limited to the options cloud-boinc acts on. Tag names and defaults follow
//! `SCHED_CONFIG::parse` in `sched/sched_config.cpp`.

use cb_proto::xml::XmlParser;

use crate::hr::HR_NTYPES;
use crate::stock::{comparable_url, url_origin, url_path, FileUrls, StockLayout, StockSpec};
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
    /// The homogeneous redundancy type of apps that set none of their own:
    /// 0 for none, 1 fine, 2 coarse.
    pub homogeneous_redundancy: i32,
    /// Keep a workunit committed to its class and app version after all its
    /// results have failed.
    pub hr_class_static: bool,
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
    /// Most messages the site sends in a UTC day, all kinds together; 0
    /// sends none. A cloud-boinc option, for cost and abuse.
    pub email_max_per_day: i32,
    // cloud-boinc options without an upstream counterpart.
    /// Whether accounts may be created at all (the sign-up page and
    /// `create_account.php`). Resolved by [`Config::parse`]: `<disable_account_creation>`
    /// closes it; otherwise an explicit `<account_creation>` decides; otherwise
    /// a config that states `<disable_account_creation>0` (as the one
    /// `make_project` writes) opens it; with none of them it is closed.
    pub account_creation: bool,
    /// `disable_account_creation_rpc`: `create_account.php` refuses and tells
    /// the client to use the web site; the sign-up page stays open.
    pub disable_account_creation_rpc: bool,
    /// `no_web_account_creation` (the tag upstream's PHP reads) or
    /// `disable_web_account_creation` (the tag `py/Boinc/setup_project.py`
    /// writes): the sign-up page is closed; `create_account.php` stays open
    /// unless `disable_account_creation_rpc` is also set.
    pub disable_web_account_creation: bool,
    /// What new accounts get for `show_hosts`: 1 only when the config says
    /// `<enable_privacy_by_default>0` (upstream's `default_show_hosts` gives 1
    /// unless the option is set; an unstated option keeps accounts private
    /// here, as before).
    pub default_show_hosts: bool,
    /// `show_results`: `results.php` and the task links of computer pages.
    /// On unless the config says `<show_results>0` (upstream: off unless set).
    pub show_results: bool,
    /// `enable_delete_account`: 0 off, 1 obfuscate, 2 wipe. Upstream's 3
    /// (project-defined) and other values are off here.
    pub enable_delete_account: i32,
    /// Value of an `X-Robots-Tag` header on account pages, empty for none.
    /// Not in upstream, which sets none.
    pub robots_tag: String,
    /// Upper bound on the credit of one job; 0 is no bound.
    pub max_granted_credit: f64,
    /// Upper bound in bytes on one uploaded file, whatever its certificate says.
    pub max_upload_nbytes: f64,
    /// Days to keep finished workunits before they are purged; 0 keeps them.
    pub purge_days: f64,
    /// Upstream's `uldl_dir_fanout`: the number of hash directories of a
    /// stock server's download hierarchy, 0 for none. Used only with
    /// [`Config::stock`], which is where it takes effect.
    pub uldl_dir_fanout: i32,
    /// The URL layout of a stock server, served in addition to the native
    /// one. Set by the `stock_*` tags; see [`crate::stock`].
    pub stock: Option<StockLayout>,
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
            homogeneous_redundancy: 0,
            hr_class_static: false,
            default_disk_min_free_gb: 1.0,
            reliable_priority_on_over: 0,
            reliable_priority_on_over_except_error: 0,
            version_select_random_factor: 1.0,
            prefer_primary_platform: false,
            user_url: true,
            user_country: true,
            email_from: String::new(),
            email_from_name: String::new(),
            email_max_per_day: 200,
            account_creation: false,
            disable_account_creation_rpc: false,
            disable_web_account_creation: false,
            default_show_hosts: false,
            show_results: true,
            enable_delete_account: 0,
            robots_tag: String::new(),
            max_granted_credit: 200.0 * 1000.0 * 365.0,
            max_upload_nbytes: 100e6,
            purge_days: 0.0,
            uldl_dir_fanout: 0,
            stock: None,
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
        let mut spec = StockSpec::default();
        // What the three tags say about account creation, until `finish`.
        let mut disable: Option<bool> = None;
        let mut explicit: Option<bool> = None;
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
                c.stock = spec.resolve(c.uldl_dir_fanout)?;
                c.finish();
                c.account_creation = match (disable, explicit) {
                    (Some(true), _) => false,
                    (_, Some(open)) => open,
                    (Some(false), None) => true,
                    (None, None) => false,
                };
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
            if let Some(v) = xp.parse_int("homogeneous_redundancy") {
                // Upstream's feeder refuses to start with a type it does not know.
                if !(0..HR_NTYPES).contains(&v) {
                    return Err(CoreError::Invalid(format!(
                        "config: homogeneous_redundancy {v} is not 0, 1 or 2"
                    )));
                }
                c.homogeneous_redundancy = v;
                continue;
            }
            take!(xp.parse_bool("hr_class_static") => c.hr_class_static);
            take!(xp.parse_double("default_disk_min_free_gb") => c.default_disk_min_free_gb);
            take!(xp.parse_int("reliable_priority_on_over") => c.reliable_priority_on_over);
            take!(xp.parse_int("reliable_priority_on_over_except_error") => c.reliable_priority_on_over_except_error);
            take!(xp.parse_double("version_select_random_factor") => c.version_select_random_factor);
            take!(xp.parse_bool("prefer_primary_platform") => c.prefer_primary_platform);
            take!(xp.parse_bool("user_url") => c.user_url);
            take!(xp.parse_bool("user_country") => c.user_country);
            take!(xp.parse_str("email_from") => c.email_from);
            take!(xp.parse_str("email_from_name") => c.email_from_name);
            take!(xp.parse_int("email_max_per_day") => c.email_max_per_day);
            if let Some(v) = xp.parse_bool("account_creation") {
                explicit = Some(v);
                continue;
            }
            if let Some(v) = xp.parse_bool("disable_account_creation") {
                disable = Some(v);
                continue;
            }
            take!(xp.parse_bool("disable_account_creation_rpc") => c.disable_account_creation_rpc);
            if let Some(v) = xp
                .parse_bool("no_web_account_creation")
                .or_else(|| xp.parse_bool("disable_web_account_creation"))
            {
                c.disable_web_account_creation |= v;
                continue;
            }
            if let Some(v) = xp.parse_bool("enable_privacy_by_default") {
                c.default_show_hosts = !v;
                continue;
            }
            take!(xp.parse_bool("show_results") => c.show_results);
            if let Some(v) = xp.parse_int("enable_delete_account") {
                c.enable_delete_account = if (1..=2).contains(&v) { v } else { 0 };
                continue;
            }
            take!(xp.parse_str("robots_tag") => c.robots_tag);
            take!(xp.parse_double("max_granted_credit") => c.max_granted_credit);
            take!(xp.parse_double("max_upload_nbytes") => c.max_upload_nbytes);
            take!(xp.parse_double("purge_days") => c.purge_days);
            take!(xp.parse_int("uldl_dir_fanout") => c.uldl_dir_fanout);
            take!(xp.parse_str("stock_short_name") => spec.short_name);
            take!(xp.parse_str("stock_cgi_prefix") => spec.cgi_prefix);
            take!(xp.parse_str("stock_upload_prefix") => spec.upload_prefix);
            take!(xp.parse_str("stock_download_prefix") => spec.download_prefix);
            if let Some(v) = xp.parse_str("stock_page_prefix") {
                spec.page_prefix = Some(v);
                continue;
            }
            if let Some(v) = xp.parse_bool("stock_advertise") {
                spec.advertise = Some(v);
                continue;
            }
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
            let base = self.native_base();
            let stock = self.stock_urls();
            let advertise = self.stock.as_ref().is_some_and(|s| s.advertise);
            if self.download_url.is_empty() {
                self.download_url = match &stock {
                    Some((download, _)) if advertise => download.clone(),
                    _ => format!("{base}download"),
                };
            }
            if self.upload_url.is_empty() {
                // A client takes a `<url>` for an upload URL only when it
                // contains `file_upload_handler`.
                self.upload_url = match &stock {
                    Some((_, upload)) if advertise => upload.clone(),
                    _ => format!("{base}upload/file_upload_handler"),
                };
            }
        }
    }

    /// Where the native paths (`/sched`, `/download/`, `/upload/`) are. The
    /// Workers' routes are at the root of the master URL's host, so with a
    /// stock layout, whose master URL is usually `https://host/<short>/`, they
    /// are under the origin, not under the master URL's directory. Without
    /// one the master URL is used as before.
    fn native_base(&self) -> String {
        match (&self.stock, url_origin(&self.master_url)) {
            (Some(_), Some(origin)) => format!("{origin}/"),
            _ => self.master_url.clone(),
        }
    }

    /// The stock download and upload URLs, on the master URL's host.
    fn stock_urls(&self) -> Option<(String, String)> {
        let stock = self.stock.as_ref()?;
        let origin = url_origin(&self.master_url)?;
        Some((stock.download_url(origin), stock.upload_url(origin)))
    }

    /// Where clients send scheduler requests: the master page's
    /// `boinc_scheduler` link. With a stock layout whose URLs are advertised
    /// it is the stock scheduler URL, which a client attached to a stock
    /// server already holds; a client reads the master page again only after
    /// repeated scheduler failures (`SCHEDULER_OP::project_rpc_backoff` in
    /// `client/scheduler_op.cpp`) and then replaces its scheduler URLs with
    /// the page's.
    pub fn scheduler_url(&self) -> String {
        if let (Some(stock), Some(origin)) = (
            self.stock.as_ref().filter(|s| s.advertise),
            url_origin(&self.master_url),
        ) {
            return stock.scheduler_url(origin);
        }
        format!("{}sched", self.native_base())
    }

    /// The URL of a download file is `<base>/<name>`, with a hash directory
    /// between them when the base is the stock download URL of a layout with
    /// fanout.
    pub fn download_urls(&self) -> FileUrls {
        let fanout = self
            .stock
            .as_ref()
            .filter(|s| url_path(&self.download_url) == s.download_prefix)
            .map_or(0, |s| s.fanout);
        FileUrls {
            base: self.download_url.clone(),
            fanout,
        }
    }

    /// Whether a file's `<url>` in stored XML points at a download directory
    /// of this project: the configured one, the native one, or the stock one.
    /// Work imported from a stock server carries the stock URL whatever
    /// `download_url` is. The scheme does not matter, so a project that moved
    /// from `http` to `https` still owns its work.
    pub fn is_own_download_url(&self, url: &str) -> bool {
        let url = comparable_url(url);
        let mut bases = vec![self.download_url.clone()];
        if !self.master_url.is_empty() {
            bases.push(format!("{}download", self.master_url));
        }
        // The native directory is at the origin's root whatever path the
        // master URL has.
        if let Some(origin) = url_origin(&self.master_url) {
            bases.push(format!("{origin}/download"));
        }
        if let Some((download, _)) = self.stock_urls() {
            bases.push(download);
        }
        bases.iter().filter(|b| !b.is_empty()).any(|b| {
            url.strip_prefix(&comparable_url(b))
                .is_some_and(|rest| rest.starts_with('/'))
        })
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

    fn stock(extra: &str) -> Config {
        Config::parse(&format!(
            "<config><master_url>https://example.org/testproj</master_url>{extra}</config>"
        ))
        .unwrap()
    }

    #[test]
    fn no_stock_tag_leaves_the_native_layout() {
        let c = stock("<uldl_dir_fanout>1024</uldl_dir_fanout>");
        assert_eq!(c.stock, None);
        assert_eq!(c.scheduler_url(), "https://example.org/testproj/sched");
        assert_eq!(c.download_url, "https://example.org/testproj/download");
        assert_eq!(
            c.upload_url,
            "https://example.org/testproj/upload/file_upload_handler"
        );
        assert_eq!(c.download_urls().fanout, 0);
    }

    #[test]
    fn stock_layout_advertises_stock_urls_by_default() {
        let c = Config::parse(
            "<config><master_url>https://example.org/</master_url>\
             <stock_short_name>testproj</stock_short_name>\
             <uldl_dir_fanout>1024</uldl_dir_fanout></config>",
        )
        .unwrap();
        let s = c.stock.as_ref().unwrap();
        assert_eq!(s.fanout, 1024);
        assert_eq!(c.scheduler_url(), "https://example.org/testproj_cgi/cgi");
        assert_eq!(c.download_url, "https://example.org/testproj/download");
        assert_eq!(
            c.upload_url,
            "https://example.org/testproj_cgi/file_upload_handler"
        );
        assert_eq!(
            c.download_urls().download("wu_1_0"),
            "https://example.org/testproj/download/319/wu_1_0"
        );
    }

    #[test]
    fn stock_layout_without_advertising_keeps_the_native_urls() {
        let c = Config::parse(
            "<config><master_url>https://example.org/</master_url>\
             <stock_short_name>testproj</stock_short_name>\
             <stock_advertise>0</stock_advertise><uldl_dir_fanout>1024</uldl_dir_fanout></config>",
        )
        .unwrap();
        assert!(c.stock.is_some());
        assert_eq!(c.scheduler_url(), "https://example.org/sched");
        assert_eq!(c.download_url, "https://example.org/download");
        assert_eq!(
            c.upload_url,
            "https://example.org/upload/file_upload_handler"
        );
        // The native download URL has no hash directory, whatever the fanout.
        assert_eq!(
            c.download_urls().download("wu_1_0"),
            "https://example.org/download/wu_1_0"
        );
        // The stock URLs still count as this project's.
        assert!(c.is_own_download_url("https://example.org/testproj/download/319/wu_1_0"));
    }

    /// The native paths are served at the host root, so with a stock layout
    /// and a master URL under the short name the native URLs are not under it.
    #[test]
    fn native_urls_are_at_the_origin_root_with_a_stock_layout() {
        let c = stock(
            "<stock_short_name>testproj</stock_short_name><stock_advertise>0</stock_advertise>\
             <uldl_dir_fanout>1024</uldl_dir_fanout>\
             <stock_download_prefix>/testproj_download</stock_download_prefix>",
        );
        assert_eq!(c.scheduler_url(), "https://example.org/sched");
        assert_eq!(c.download_url, "https://example.org/download");
        assert_eq!(
            c.upload_url,
            "https://example.org/upload/file_upload_handler"
        );
        assert_eq!(
            c.download_urls().download("wu_1_0"),
            "https://example.org/download/wu_1_0"
        );
        assert!(c.is_own_download_url("https://example.org/download/input"));
        assert!(c.is_own_download_url("https://example.org/testproj_download/319/input"));
        // Advertised stock URLs: the native directory is still the project's.
        let c = stock("<stock_short_name>testproj</stock_short_name>");
        assert_eq!(c.download_url, "https://example.org/testproj/download");
        assert!(c.is_own_download_url("https://example.org/download/input"));
    }

    /// A download URL set to the stock one gets the hash directory, because
    /// that is the only path under it that the files Worker serves.
    #[test]
    fn a_download_url_on_the_stock_path_gets_the_directory() {
        let c = stock(
            "<stock_short_name>testproj</stock_short_name><stock_advertise>0</stock_advertise>\
             <uldl_dir_fanout>1024</uldl_dir_fanout>\
             <download_url>https://example.org/testproj/download</download_url>",
        );
        assert_eq!(c.download_url, "https://example.org/testproj/download");
        assert_eq!(
            c.download_urls().download("wu_1_0"),
            "https://example.org/testproj/download/319/wu_1_0"
        );
    }

    #[test]
    fn explicit_urls_win_over_the_stock_ones() {
        let c = stock(
            "<stock_short_name>testproj</stock_short_name>\
             <download_url>https://dl.example.org/files</download_url>\
             <upload_url>https://up.example.org/x/file_upload_handler</upload_url>",
        );
        assert_eq!(c.download_url, "https://dl.example.org/files");
        assert_eq!(c.upload_url, "https://up.example.org/x/file_upload_handler");
        assert_eq!(c.download_urls().fanout, 0);
        // The scheduler link is still the stock one: it is the URL clients hold.
        assert_eq!(c.scheduler_url(), "https://example.org/testproj_cgi/cgi");
        assert!(c.is_own_download_url("https://dl.example.org/files/a"));
        assert!(c.is_own_download_url("https://example.org/testproj/download/0/a"));
        assert!(!c.is_own_download_url("https://elsewhere.example/testproj/download/0/a"));
    }

    #[test]
    fn own_download_urls() {
        let c = Config::parse(
            "<config><master_url>https://example.org/</master_url>\
             <stock_short_name>testproj</stock_short_name></config>",
        )
        .unwrap();
        for own in [
            "https://example.org/testproj/download/a",
            "http://EXAMPLE.org/testproj/download/a",
            "https://example.org/download/a",
            "https://example.org/testproj/download/ab/a",
        ] {
            assert!(c.is_own_download_url(own), "{own}");
        }
        for foreign in [
            "https://example.org/testproj/downloads/a",
            "https://example.org/testproj/download",
            "https://example.org/other/download/a",
            "https://mirror.example/testproj/download/a",
            "",
        ] {
            assert!(!c.is_own_download_url(foreign), "{foreign}");
        }
    }

    #[test]
    fn bad_stock_settings_make_the_config_unparseable() {
        for extra in [
            "<stock_short_name>a b</stock_short_name>",
            "<stock_short_name>x</stock_short_name><stock_cgi_prefix>x</stock_cgi_prefix>",
            "<stock_cgi_prefix>/x_cgi</stock_cgi_prefix>",
            "<stock_short_name>x</stock_short_name><uldl_dir_fanout>-1</uldl_dir_fanout>",
        ] {
            let xml =
                format!("<config><master_url>https://e.example/</master_url>{extra}</config>");
            assert!(Config::parse(&xml).is_err(), "{extra}");
        }
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
        assert_eq!(c.email_max_per_day, 200);
        let capped = Config::parse(
            "<config><master_url>https://example.org/</master_url>\n\
             <email_max_per_day>5</email_max_per_day></config>",
        )
        .unwrap();
        assert_eq!(capped.email_max_per_day, 5);
        let none = Config::parse("<config><master_url>https://example.org/</master_url></config>")
            .unwrap();
        assert_eq!(
            (none.email_from.as_str(), none.email_from_name.as_str()),
            ("", "")
        );
    }

    fn account_creation_of(tags: &str) -> bool {
        Config::parse(&format!(
            "<config><master_url>https://example.org/</master_url>{tags}</config>"
        ))
        .unwrap()
        .account_creation
    }

    #[test]
    fn account_creation_follows_the_stock_tags() {
        // Nothing stated: closed, as before this option was read.
        assert!(!account_creation_of(""));
        // A stock config states it.
        assert!(account_creation_of(
            "<disable_account_creation>0</disable_account_creation>"
        ));
        assert!(!account_creation_of("<disable_account_creation/>"));
        assert!(!account_creation_of(
            "<disable_account_creation>1</disable_account_creation>"
        ));
        // The fork's own option, when the stock one is not there.
        assert!(account_creation_of(
            "<account_creation>1</account_creation>"
        ));
        assert!(!account_creation_of(
            "<account_creation>0</account_creation>"
        ));
        // The stock disable wins over an open `account_creation`.
        assert!(!account_creation_of(
            "<account_creation>1</account_creation><disable_account_creation>1</disable_account_creation>"
        ));
        // And an explicit `account_creation` of 0 stays closed whatever the
        // order and the stock tag at 0.
        assert!(!account_creation_of(
            "<disable_account_creation>0</disable_account_creation><account_creation>0</account_creation>"
        ));
    }

    #[test]
    fn parses_the_site_options() {
        let c = Config::parse(
            "<config><master_url>https://example.org/</master_url>\n\
             <disable_account_creation_rpc>1</disable_account_creation_rpc>\n\
             <no_web_account_creation/><enable_privacy_by_default>0</enable_privacy_by_default>\n\
             <show_results>0</show_results><enable_delete_account>2</enable_delete_account>\n\
             <robots_tag>noindex, nofollow</robots_tag></config>",
        )
        .unwrap();
        assert!(c.disable_account_creation_rpc && c.disable_web_account_creation);
        assert!(c.default_show_hosts && !c.show_results);
        assert_eq!(
            (c.enable_delete_account, c.robots_tag.as_str()),
            (2, "noindex, nofollow")
        );
        let c = Config::parse(
            "<config><master_url>https://example.org/</master_url>\n\
             <disable_web_account_creation>1</disable_web_account_creation>\n\
             <enable_privacy_by_default/><enable_delete_account>3</enable_delete_account></config>",
        )
        .unwrap();
        assert!(c.disable_web_account_creation && !c.default_show_hosts);
        assert_eq!(c.enable_delete_account, 0);
        let c = Config::parse("<config><master_url>https://example.org/</master_url></config>")
            .unwrap();
        assert!(!c.disable_account_creation_rpc && !c.disable_web_account_creation);
        assert!(!c.default_show_hosts && c.show_results && c.robots_tag.is_empty());
    }

    #[test]
    fn homogeneous_redundancy_options() {
        let c = Config::parse(
            "<config><homogeneous_redundancy>2</homogeneous_redundancy><hr_class_static/></config>",
        )
        .unwrap();
        assert_eq!(c.homogeneous_redundancy, 2);
        assert!(c.hr_class_static);
        let c = Config::parse("<config></config>").unwrap();
        assert_eq!(c.homogeneous_redundancy, 0);
        assert!(!c.hr_class_static);
        // Upstream's feeder refuses to start with a type it does not know.
        for t in ["3", "-1"] {
            let xml =
                format!("<config><homogeneous_redundancy>{t}</homogeneous_redundancy></config>");
            assert!(Config::parse(&xml).is_err(), "{t}");
        }
    }

    #[test]
    fn unclosed_config_is_an_error() {
        assert!(Config::parse("<config><long_name>x</long_name>").is_err());
    }
}
