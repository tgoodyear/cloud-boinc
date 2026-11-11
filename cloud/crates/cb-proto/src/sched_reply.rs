//! The `<scheduler_reply>` document sent back to a client.
//!
//! [`SchedulerReply::write`] follows `SCHEDULER_REPLY::write` in
//! `sched/sched_types.cpp`, element for element and in the same order. The
//! client reads the reply in `SCHEDULER_REPLY::parse` in
//! `client/scheduler_op.cpp`; the tests check the output against what that
//! function expects.
//!
//! The reply body starts at `<scheduler_reply>`: no XML declaration (it
//! broke old clients upstream) and no HTTP headers. Scheduler replies have
//! no `<error_num>`; failures are reported with `<message>` elements and
//! `<request_delay>`.
//!
//! The helpers [`insert_wu_tags`], [`insert_name_tags`] and
//! [`insert_deadline_tag`] edit the stored workunit and result XML before it
//! is sent, as `sched/sched_send.cpp` does.

use crate::consts::{PROC_TYPE_CPU, PROC_TYPE_NVIDIA_GPU};
use crate::coproc::proc_type_name_xml;
use crate::xml::xml_escape;
use crate::{fmt_f, ProtoError};
use std::fmt::Write as _;

/// Value of `<scheduler_version>`: `BOINC_MAJOR_VERSION*100 +
/// BOINC_MINOR_VERSION` of the upstream release this crate follows.
pub const SCHEDULER_VERSION: i32 = 803;

/// A message shown to the user (`USER_MESSAGE`). `priority` is `low`,
/// `high` or `notice`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserMessage {
    pub message: String,
    pub priority: String,
}

impl UserMessage {
    pub fn new(message: impl Into<String>, priority: impl Into<String>) -> Self {
        UserMessage {
            message: message.into(),
            priority: priority.into(),
        }
    }
}

/// The account fields of a reply. Written only when `id` is nonzero.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReplyUser {
    pub id: u64,
    /// Escaped when written.
    pub name: String,
    pub total_credit: f64,
    pub expavg_credit: f64,
    pub create_time: i64,
    /// Written when not empty.
    pub email_hash: String,
    /// Written, with `external_cpid`, when not empty.
    pub cross_project_id: String,
    /// `md5(cross_project_id + email_addr)`, computed by the caller.
    pub external_cpid: String,
    /// The account's `<global_preferences>` XML, when the server's copy is
    /// newer than the client's (upstream `send_global_prefs`).
    pub global_prefs: Option<String>,
    /// The account's `<project_preferences>` XML; always sent.
    pub project_prefs: String,
}

/// The host fields of a reply; always written.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReplyHost {
    pub total_credit: f64,
    pub expavg_credit: f64,
    pub venue: String,
    pub create_time: i64,
}

/// An `<app>` element (`APP::write`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReplyApp {
    pub name: String,
    pub user_friendly_name: String,
    pub non_cpu_intensive: bool,
    pub fraction_done_exact: bool,
}

impl ReplyApp {
    fn write(&self, out: &mut String) {
        let _ = write!(
            out,
            "<app>\n    <name>{}</name>\n    <user_friendly_name>{}</user_friendly_name>\n    <non_cpu_intensive>{}</non_cpu_intensive>\n    <fraction_done_exact>{}</fraction_done_exact>\n</app>\n",
            self.name,
            self.user_friendly_name,
            i32::from(self.non_cpu_intensive),
            i32::from(self.fraction_done_exact),
        );
    }
}

/// An `<app_version>` element (`APP_VERSION::write`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReplyAppVersion {
    /// The stored XML of the version: `<app_version>` with app name,
    /// version number and file references, ending in `</app_version>`.
    pub xml_doc: String,
    pub platform: String,
    /// Written when not empty.
    pub plan_class: String,
    pub avg_ncpus: f64,
    pub flops: f64,
    /// Written when not empty.
    pub cmdline: String,
    /// `PROC_TYPE_*`; a `<coproc>` element is written unless this is
    /// `PROC_TYPE_CPU`.
    pub proc_type: i32,
    pub gpu_usage: f64,
    /// Written when nonzero.
    pub gpu_ram: f64,
}

impl ReplyAppVersion {
    fn write(&self, out: &mut String, core_client_version: i32) -> Result<(), ProtoError> {
        let end = self
            .xml_doc
            .find("</app_version>")
            .ok_or_else(|| ProtoError::NoEndTag("</app_version>".to_string()))?;
        out.push_str(&self.xml_doc[..end]);
        let _ = writeln!(out, "    <platform>{}</platform>", self.platform);
        if !self.plan_class.is_empty() {
            let _ = writeln!(out, "    <plan_class>{}</plan_class>", self.plan_class);
        }
        let _ = write!(
            out,
            "    <avg_ncpus>{}</avg_ncpus>\n    <flops>{}</flops>\n",
            fmt_f(self.avg_ncpus),
            fmt_f(self.flops)
        );
        if !self.cmdline.is_empty() {
            let _ = writeln!(out, "    <cmdline>{}</cmdline>", self.cmdline);
        }
        if self.proc_type != PROC_TYPE_CPU {
            // Clients before 7.0 know the NVIDIA type as "CUDA".
            let name = if self.proc_type == PROC_TYPE_NVIDIA_GPU && core_client_version < 70000 {
                "CUDA"
            } else {
                proc_type_name_xml(self.proc_type)
            };
            let _ = write!(
                out,
                "    <coproc>\n        <type>{}</type>\n        <count>{}</count>\n    </coproc>\n",
                name,
                fmt_f(self.gpu_usage)
            );
        }
        if self.gpu_ram != 0.0 {
            let _ = writeln!(out, "    <gpu_ram>{}</gpu_ram>", fmt_f(self.gpu_ram));
        }
        out.push_str("</app_version>\n");
        Ok(())
    }
}

/// A `<result>` element (`SCHED_DB_RESULT::write_to_client`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReplyResult {
    /// The stored XML of the result after [`insert_name_tags`] and
    /// [`insert_deadline_tag`], ending in `</result>`.
    pub xml_doc_in: String,
    /// Platform of the app version the job was assigned to.
    pub platform: String,
    pub version_num: i32,
    pub plan_class: String,
}

impl ReplyResult {
    fn write(&self, out: &mut String) -> Result<(), ProtoError> {
        let end = self
            .xml_doc_in
            .find("</result>")
            .ok_or_else(|| ProtoError::NoEndTag("</result>".to_string()))?;
        out.push_str(&self.xml_doc_in[..end]);
        out.push('\n');
        let _ = write!(
            out,
            "    <platform>{}</platform>\n    <version_num>{}</version_num>\n    <plan_class>{}</plan_class>\n</result>\n",
            self.platform, self.version_num, self.plan_class
        );
        Ok(())
    }
}

/// Everything a scheduler reply can carry (`SCHEDULER_REPLY`, plus the
/// project configuration values `SCHEDULER_REPLY::write` reads from
/// `config`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SchedulerReply {
    /// `core_client_version` of the request; selects the forms old clients
    /// need.
    pub core_client_version: i32,
    /// Written when not empty. The client warns the user when this
    /// differs from the URL it is attached to.
    pub master_url: String,
    /// Seconds the client must wait before the next request; written when
    /// positive. The caller applies `min_sendwork_interval` and
    /// [`crate::consts::DELAY_MAX`].
    pub request_delay: f64,
    pub messages: Vec<UserMessage>,
    /// The project's display name; not escaped, as upstream.
    pub project_name: String,
    pub send_time_stats_log: bool,
    /// Ends the reply after `<project_is_down/>`.
    pub project_is_down: bool,
    /// Ask the client to list its in-progress results in every request.
    pub send_full_workload: bool,
    /// Ends the reply before the account section (upstream `nucleus_only`):
    /// used when the request could not be authenticated or processed.
    pub nucleus_only: bool,
    /// Written when positive: the client contacts the scheduler at least
    /// this often.
    pub next_rpc_delay: f64,
    pub user: ReplyUser,
    /// Written when nonzero: tells the client its host ID.
    pub hostid: u64,
    pub host: ReplyHost,
    /// `(id, name)`; an empty `<team_name>` is written when `None`.
    pub team: Option<(u64, String)>,
    pub result_acks: Vec<String>,
    pub result_aborts: Vec<String>,
    pub result_abort_if_not_starteds: Vec<String>,
    pub apps: Vec<ReplyApp>,
    pub app_versions: Vec<ReplyAppVersion>,
    /// Workunit XML after [`insert_wu_tags`].
    pub wus: Vec<String>,
    pub results: Vec<ReplyResult>,
    /// Text of the code signing public key; written when not empty.
    pub code_sign_key: String,
    /// Written when not empty; must end in a newline.
    pub code_sign_key_signature: String,
    /// Acknowledge the trickle-up messages of the request.
    pub send_msg_ack: bool,
    /// Trickle-down messages, each a complete XML fragment.
    pub msgs_to_host: Vec<String>,
    pub non_cpu_intensive: bool,
    pub strict_memory_bound: bool,
    pub verify_files_on_app_start: bool,
    /// Names of sticky files the client should delete.
    pub file_deletes: Vec<String>,
    /// Raw XML of file transfer commands.
    pub file_transfer_requests: Vec<String>,
    /// Resources (names as [`proc_type_name_xml`] gives them) the project
    /// has no app versions for. The caller leaves out resources the client
    /// does not have, and sends nothing if that would cover all of the
    /// client's resources.
    pub no_rsc_apps: Vec<String>,
    /// `<gui_urls>` XML, written as is.
    pub gui_urls: String,
    /// `<project_files>` XML, written as is and followed by a newline.
    pub project_files: String,
}

impl SchedulerReply {
    /// Write the reply body (`SCHEDULER_REPLY::write`).
    ///
    /// Fails only when the XML of an app version or result has no end tag;
    /// upstream logs that and leaves the element out.
    pub fn write(&self) -> Result<String, ProtoError> {
        let mut o = String::new();
        let ccv = self.core_client_version;
        let _ = write!(
            o,
            "<scheduler_reply>\n<scheduler_version>{SCHEDULER_VERSION}</scheduler_version>\n"
        );
        if ccv >= 70028 {
            o.push_str("<dont_use_dcf/>\n");
        }
        if !self.master_url.is_empty() {
            let _ = writeln!(o, "<master_url>{}</master_url>", self.master_url);
        }
        if self.request_delay > 0.0 {
            let _ = writeln!(
                o,
                "<request_delay>{}</request_delay>",
                fmt_f(self.request_delay)
            );
        }
        self.write_messages(&mut o);
        let _ = writeln!(o, "<project_name>{}</project_name>", self.project_name);
        if self.send_time_stats_log {
            o.push_str("<send_time_stats_log>1</send_time_stats_log>\n");
        }
        if self.project_is_down {
            o.push_str("<project_is_down/>\n</scheduler_reply>\n");
            return Ok(o);
        }
        if self.send_full_workload {
            o.push_str("<send_full_workload/>\n");
        }
        if self.nucleus_only {
            o.push_str("</scheduler_reply>\n");
            return Ok(o);
        }
        if self.next_rpc_delay > 0.0 {
            let _ = writeln!(
                o,
                "<next_rpc_delay>{}</next_rpc_delay>",
                fmt_f(self.next_rpc_delay)
            );
        }
        let u = &self.user;
        if u.id != 0 {
            let _ = write!(
                o,
                "<userid>{}</userid>\n<user_name>{}</user_name>\n<user_total_credit>{}</user_total_credit>\n<user_expavg_credit>{}</user_expavg_credit>\n<user_create_time>{}</user_create_time>\n",
                u.id,
                xml_escape(&u.name),
                fmt_f(u.total_credit),
                fmt_f(u.expavg_credit),
                u.create_time
            );
            if !u.email_hash.is_empty() {
                let _ = writeln!(o, "<email_hash>{}</email_hash>", u.email_hash);
            }
            if !u.cross_project_id.is_empty() {
                let _ = write!(
                    o,
                    "<cross_project_id>{}</cross_project_id>\n<external_cpid>{}</external_cpid>\n",
                    u.cross_project_id, u.external_cpid
                );
            }
            if let Some(p) = &u.global_prefs {
                o.push_str(p);
                o.push('\n');
            }
            o.push_str(&u.project_prefs);
            o.push('\n');
        }
        if self.hostid != 0 {
            let _ = writeln!(o, "<hostid>{}</hostid>", self.hostid);
        }
        let _ = write!(
            o,
            "<host_total_credit>{}</host_total_credit>\n<host_expavg_credit>{}</host_expavg_credit>\n<host_venue>{}</host_venue>\n<host_create_time>{}</host_create_time>\n",
            fmt_f(self.host.total_credit),
            fmt_f(self.host.expavg_credit),
            self.host.venue,
            self.host.create_time
        );
        match &self.team {
            Some((id, name)) if *id != 0 => {
                let _ = write!(
                    o,
                    "<teamid>{}</teamid>\n<team_name>{}</team_name>\n",
                    id,
                    xml_escape(name)
                );
            }
            _ => o.push_str("<team_name></team_name>\n"),
        }
        for (tag, names) in [
            ("result_ack", &self.result_acks),
            ("result_abort", &self.result_aborts),
            (
                "result_abort_if_not_started",
                &self.result_abort_if_not_starteds,
            ),
        ] {
            for name in names {
                let _ = write!(o, "<{tag}>\n    <name>{name}</name>\n</{tag}>\n");
            }
        }
        for a in &self.apps {
            a.write(&mut o);
        }
        for av in &self.app_versions {
            av.write(&mut o, ccv)?;
        }
        for wu in &self.wus {
            o.push_str(wu);
            o.push('\n');
        }
        for r in &self.results {
            r.write(&mut o)?;
        }
        if !self.code_sign_key.is_empty() {
            let _ = write!(
                o,
                "<code_sign_key>\n{}\n</code_sign_key>\n",
                self.code_sign_key
            );
        }
        if !self.code_sign_key_signature.is_empty() {
            let _ = write!(
                o,
                "<code_sign_key_signature>\n{}</code_sign_key_signature>\n",
                self.code_sign_key_signature
            );
        }
        if self.send_msg_ack {
            o.push_str("<message_ack/>\n");
        }
        for m in &self.msgs_to_host {
            o.push_str(m);
            o.push('\n');
        }
        if self.non_cpu_intensive {
            o.push_str("<non_cpu_intensive/>\n");
        }
        if self.strict_memory_bound {
            o.push_str("<strict_memory_bound/>\n");
        }
        if self.verify_files_on_app_start {
            o.push_str("<verify_files_on_app_start/>\n");
        }
        for f in &self.file_deletes {
            let _ = writeln!(o, "<delete_file_info>{f}</delete_file_info>");
        }
        for s in &self.file_transfer_requests {
            o.push_str(s);
        }
        for r in &self.no_rsc_apps {
            let _ = writeln!(o, "<no_rsc_apps>{r}</no_rsc_apps>");
        }
        o.push_str(&self.gui_urls);
        if !self.project_files.is_empty() {
            o.push_str(&self.project_files);
            o.push('\n');
        }
        o.push_str("</scheduler_reply>\n");
        Ok(o)
    }

    fn write_messages(&self, o: &mut String) {
        let ccv = self.core_client_version;
        if ccv <= 41900 {
            // One message, no newlines.
            if self.messages.is_empty() {
                return;
            }
            let mut msg = String::new();
            let mut pri = "low";
            for m in &self.messages {
                msg.push_str(&m.message);
                msg.push(' ');
                if m.priority == "notice" {
                    pri = "notice";
                }
            }
            let msg = msg.replace('\n', " ");
            let _ = writeln!(o, "<message priority=\"{pri}\">{msg}</message>");
            return;
        }
        for m in &self.messages {
            let pri = if ccv <= 61100 && m.priority == "notice" {
                "high"
            } else {
                m.priority.as_str()
            };
            let _ = writeln!(o, "<message priority=\"{pri}\">{}</message>", m.message);
        }
    }
}

/// The reply for a project that is down or cannot process the request
/// (`send_message` in `sched/sched_main.cpp`). `extra` is appended raw
/// before the end tag and may be empty.
pub fn write_project_down(message: &str, delay: i32, extra: &str) -> String {
    format!(
        "<scheduler_reply>\n    <message priority=\"low\">{message}</message>\n    <request_delay>{delay}</request_delay>\n    <project_is_down/>\n{extra}</scheduler_reply>\n"
    )
}

/// Insert `text` right after the first occurrence of `after` (`insert_after`
/// in `sched/sched_send.cpp`).
pub fn insert_after(xml: &str, after: &str, text: &str) -> Result<String, ProtoError> {
    let i = xml
        .find(after)
        .ok_or_else(|| ProtoError::MarkerNotFound(after.to_string()))?
        + after.len();
    let mut out = String::with_capacity(xml.len() + text.len());
    out.push_str(&xml[..i]);
    out.push_str(text);
    out.push_str(&xml[i..]);
    Ok(out)
}

/// The fields `insert_wu_tags` adds to a workunit's XML.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WuTags<'a> {
    pub rsc_fpops_est: f64,
    pub rsc_fpops_bound: f64,
    pub rsc_memory_bound: f64,
    pub rsc_disk_bound: f64,
    pub name: &'a str,
    pub app_name: &'a str,
    /// Written when not empty.
    pub job_keyword_ids: &'a str,
}

/// Add the database fields the client needs to a workunit's XML
/// (`insert_wu_tags`). The XML must contain `<workunit>\n`.
pub fn insert_wu_tags(wu_xml: &str, t: &WuTags<'_>) -> Result<String, ProtoError> {
    let mut buf = format!(
        "    <rsc_fpops_est>{}</rsc_fpops_est>\n    <rsc_fpops_bound>{}</rsc_fpops_bound>\n    <rsc_memory_bound>{}</rsc_memory_bound>\n    <rsc_disk_bound>{}</rsc_disk_bound>\n    <name>{}</name>\n    <app_name>{}</app_name>\n",
        fmt_f(t.rsc_fpops_est),
        fmt_f(t.rsc_fpops_bound),
        fmt_f(t.rsc_memory_bound),
        fmt_f(t.rsc_disk_bound),
        t.name,
        t.app_name
    );
    if !t.job_keyword_ids.is_empty() {
        let _ = writeln!(
            buf,
            "    <job_keyword_ids>{}</job_keyword_ids>",
            t.job_keyword_ids
        );
    }
    insert_after(wu_xml, "<workunit>\n", &buf)
}

/// Add `<name>` and `<wu_name>` to a result's XML (`insert_name_tags`).
/// The XML must contain `<result>\n`. `<wu_name>` ends up first.
pub fn insert_name_tags(result_xml: &str, name: &str, wu_name: &str) -> Result<String, ProtoError> {
    let s = insert_after(result_xml, "<result>\n", &format!("<name>{name}</name>\n"))?;
    insert_after(&s, "<result>\n", &format!("<wu_name>{wu_name}</wu_name>\n"))
}

/// Add `<report_deadline>` to a result's XML (`insert_deadline_tag`).
pub fn insert_deadline_tag(result_xml: &str, report_deadline: i64) -> Result<String, ProtoError> {
    insert_after(
        result_xml,
        "<result>\n",
        &format!("<report_deadline>{report_deadline}</report_deadline>\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::PROC_TYPE_AMD_GPU;
    use crate::xml::XmlParser;

    /// The parts of `SCHEDULER_REPLY::parse` in `client/scheduler_op.cpp`
    /// that matter here.
    #[derive(Debug, Default)]
    struct ClientView {
        ended: bool,
        scheduler_version: i32,
        dont_use_dcf: bool,
        master_url: String,
        request_delay: f64,
        next_rpc_delay: f64,
        messages: Vec<(String, String)>,
        project_name: String,
        project_is_down: bool,
        send_full_workload: bool,
        send_time_stats_log: i32,
        user_name: String,
        user_total_credit: f64,
        hostid: i32,
        host_venue: String,
        team_name: Option<String>,
        email_hash: String,
        cross_project_id: String,
        external_cpid: String,
        global_prefs: Option<String>,
        project_prefs: Option<String>,
        result_acks: Vec<String>,
        result_aborts: Vec<String>,
        result_abort_if_not_starteds: Vec<String>,
        apps: Vec<String>,
        app_versions: Vec<String>,
        workunits: Vec<String>,
        results: Vec<String>,
        code_sign_key: String,
        code_sign_key_signature: String,
        message_ack: bool,
        trickles: Vec<String>,
        non_cpu_intensive: bool,
        verify_files_on_app_start: bool,
        delete_file_infos: Vec<String>,
        no_rsc_apps: Vec<String>,
        gui_urls: Option<String>,
        project_files: Option<String>,
    }

    fn named(xp: &mut XmlParser<'_>, tag: &str) -> String {
        // e.g. <result_ack> <name>x</name> </result_ack>
        let body = xp.element_contents(&format!("</{tag}>")).unwrap();
        crate::xml::line::parse_str(&body, "<name>").unwrap_or_default()
    }

    fn client_parse(reply: &str) -> Result<ClientView, &'static str> {
        let mut v = ClientView::default();
        let mut xp = XmlParser::new(reply);
        // The client first looks for the start tag, skipping other lines.
        let mut started = false;
        while xp.get_tag() {
            if xp.match_tag("scheduler_reply") {
                started = true;
                break;
            }
        }
        if !started {
            return Err("no start tag");
        }
        while xp.get_tag() {
            if xp.match_tag("/scheduler_reply") {
                v.ended = true;
                return Ok(v);
            }
            if !xp.is_tag() {
                continue;
            }
            take!(xp.parse_int("scheduler_version") => v.scheduler_version);
            take!(xp.parse_bool("dont_use_dcf") => v.dont_use_dcf);
            take!(xp.parse_str("master_url") => v.master_url);
            take!(xp.parse_double("request_delay") => v.request_delay);
            take!(xp.parse_double("next_rpc_delay") => v.next_rpc_delay);
            take!(xp.parse_str("project_name") => v.project_name);
            take!(xp.parse_bool("project_is_down") => v.project_is_down);
            take!(xp.parse_bool("send_full_workload") => v.send_full_workload);
            take!(xp.parse_int("send_time_stats_log") => v.send_time_stats_log);
            take!(xp.parse_str("user_name") => v.user_name);
            take!(xp.parse_double("user_total_credit") => v.user_total_credit);
            take!(xp.parse_int("hostid") => v.hostid);
            take!(xp.parse_str("host_venue") => v.host_venue);
            take!(xp.parse_str("email_hash") => v.email_hash);
            take!(xp.parse_str("cross_project_id") => v.cross_project_id);
            take!(xp.parse_str("external_cpid") => v.external_cpid);
            take!(xp.parse_bool("message_ack") => v.message_ack);
            take!(xp.parse_bool("non_cpu_intensive") => v.non_cpu_intensive);
            take!(xp.parse_bool("verify_files_on_app_start") => v.verify_files_on_app_start);
            if let Some(s) = xp.parse_str("team_name") {
                v.team_name = Some(s);
                continue;
            }
            if xp.match_tag("message") {
                let pri = xp.attrs().to_string();
                let body = xp.element_contents("</message>").unwrap();
                v.messages.push((pri, body));
                continue;
            }
            if xp.match_tag("global_preferences") {
                v.global_prefs = Some(xp.element_contents("</global_preferences>").unwrap());
                continue;
            }
            if xp.match_tag("project_preferences") {
                v.project_prefs = Some(xp.element_contents("</project_preferences>").unwrap());
                continue;
            }
            if xp.match_tag("gui_urls") {
                v.gui_urls = Some(xp.element_contents("</gui_urls>").unwrap());
                continue;
            }
            if xp.match_tag("project_files") {
                v.project_files = Some(xp.element_contents("</project_files>").unwrap());
                continue;
            }
            if xp.match_tag("result_ack") {
                v.result_acks.push(named(&mut xp, "result_ack"));
                continue;
            }
            if xp.match_tag("result_abort") {
                v.result_aborts.push(named(&mut xp, "result_abort"));
                continue;
            }
            if xp.match_tag("result_abort_if_not_started") {
                v.result_abort_if_not_starteds
                    .push(named(&mut xp, "result_abort_if_not_started"));
                continue;
            }
            if xp.match_tag("app") {
                v.apps.push(xp.element_contents("</app>").unwrap());
                continue;
            }
            if xp.match_tag("app_version") {
                v.app_versions
                    .push(xp.element_contents("</app_version>").unwrap());
                continue;
            }
            if xp.match_tag("workunit") {
                v.workunits
                    .push(xp.element_contents("</workunit>").unwrap());
                continue;
            }
            if xp.match_tag("result") {
                v.results.push(xp.element_contents("</result>").unwrap());
                continue;
            }
            if xp.match_tag("code_sign_key") {
                v.code_sign_key = xp.element_contents("</code_sign_key>").unwrap();
                continue;
            }
            if xp.match_tag("code_sign_key_signature") {
                v.code_sign_key_signature =
                    xp.element_contents("</code_sign_key_signature>").unwrap();
                continue;
            }
            if xp.match_tag("trickle_down") {
                v.trickles
                    .push(xp.element_contents("</trickle_down>").unwrap());
                continue;
            }
            if let Some(s) = xp.parse_str("delete_file_info") {
                v.delete_file_infos.push(s);
                continue;
            }
            if let Some(s) = xp.parse_str("no_rsc_apps") {
                v.no_rsc_apps.push(s);
                continue;
            }
            xp.skip_unexpected();
        }
        Err("no end tag")
    }

    fn field(body: &str, tag: &str) -> String {
        crate::xml::line::parse_str(body, tag).unwrap_or_else(|| panic!("no {tag} in {body}"))
    }

    const WU_XML: &str = "<workunit>\n    <command_line>--n 5</command_line>\n    <file_ref>\n        <file_name>in_1</file_name>\n        <open_name>in</open_name>\n    </file_ref>\n</workunit>\n";
    const RESULT_XML: &str = "<result>\n    <file_ref>\n        <file_name>wu_1_0_r1_0</file_name>\n        <open_name>out</open_name>\n    </file_ref>\n</result>\n";
    const AV_XML: &str = "<app_version>\n    <app_name>worker</app_name>\n    <version_num>103</version_num>\n    <file_ref>\n        <file_name>worker_103</file_name>\n        <main_program/>\n    </file_ref>\n</app_version>\n";

    fn full_reply() -> SchedulerReply {
        let wu = insert_wu_tags(
            WU_XML,
            &WuTags {
                rsc_fpops_est: 1e12,
                rsc_fpops_bound: 1e14,
                rsc_memory_bound: 5e8,
                rsc_disk_bound: 1e9,
                name: "wu_1",
                app_name: "worker",
                job_keyword_ids: "",
            },
        )
        .unwrap();
        let result = insert_name_tags(RESULT_XML, "wu_1_0", "wu_1").unwrap();
        let result = insert_deadline_tag(&result, 1_763_000_000).unwrap();
        SchedulerReply {
            core_client_version: 80204,
            master_url: "https://example.org/proj/".to_string(),
            request_delay: 7.0,
            messages: vec![
                UserMessage::new("No work available", "low"),
                UserMessage::new("Please update", "notice"),
            ],
            project_name: "Example Project".to_string(),
            send_full_workload: true,
            next_rpc_delay: 3600.0,
            user: ReplyUser {
                id: 12,
                name: "Ada <L>".to_string(),
                total_credit: 1234.5,
                expavg_credit: 12.25,
                create_time: 1_700_000_000,
                email_hash: "aa11".to_string(),
                cross_project_id: "bb22".to_string(),
                external_cpid: "cc33".to_string(),
                global_prefs: Some(
                    "<global_preferences>\n<mod_time>5</mod_time>\n</global_preferences>".to_string(),
                ),
                project_prefs: "<project_preferences>\n<resource_share>100</resource_share>\n</project_preferences>".to_string(),
            },
            hostid: 4217,
            host: ReplyHost {
                total_credit: 99.0,
                expavg_credit: 1.5,
                venue: "home".to_string(),
                create_time: 1_700_000_100,
            },
            team: Some((3, "A & B".to_string())),
            result_acks: vec!["wu_0_0".to_string(), "wu_0_1".to_string()],
            result_aborts: vec!["wu_9_0".to_string()],
            result_abort_if_not_starteds: vec!["wu_8_0".to_string()],
            apps: vec![ReplyApp {
                name: "worker".to_string(),
                user_friendly_name: "Worker".to_string(),
                non_cpu_intensive: false,
                fraction_done_exact: true,
            }],
            app_versions: vec![ReplyAppVersion {
                xml_doc: AV_XML.to_string(),
                platform: "x86_64-pc-linux-gnu".to_string(),
                plan_class: "cuda".to_string(),
                avg_ncpus: 0.5,
                flops: 1e11,
                cmdline: "--gpu".to_string(),
                proc_type: PROC_TYPE_NVIDIA_GPU,
                gpu_usage: 1.0,
                gpu_ram: 2e9,
            }],
            wus: vec![wu],
            results: vec![ReplyResult {
                xml_doc_in: result,
                platform: "x86_64-pc-linux-gnu".to_string(),
                version_num: 103,
                plan_class: "cuda".to_string(),
            }],
            code_sign_key: "1024\nabcd\n.".to_string(),
            code_sign_key_signature: "ffee\n.\n".to_string(),
            send_msg_ack: true,
            msgs_to_host: vec![
                "<trickle_down>\n    <result_name>wu_3_0</result_name>\n    <time>5</time>\n<x>1</x>\n</trickle_down>".to_string(),
            ],
            non_cpu_intensive: true,
            verify_files_on_app_start: true,
            file_deletes: vec!["old.bin".to_string()],
            no_rsc_apps: vec!["ATI".to_string()],
            gui_urls: "<gui_urls>\n    <gui_url>\n        <name>Home</name>\n    </gui_url>\n</gui_urls>\n".to_string(),
            project_files: "<project_files>\n</project_files>".to_string(),
            ..SchedulerReply::default()
        }
    }

    #[test]
    fn full_reply_reads_back_as_the_client_reads_it() {
        let text = full_reply().write().unwrap();
        assert!(text.starts_with(
            "<scheduler_reply>\n<scheduler_version>803</scheduler_version>\n<dont_use_dcf/>\n<master_url>https://example.org/proj/</master_url>\n<request_delay>7.000000</request_delay>\n<message priority=\"low\">No work available</message>\n<message priority=\"notice\">Please update</message>\n<project_name>Example Project</project_name>\n<send_full_workload/>\n<next_rpc_delay>3600.000000</next_rpc_delay>\n<userid>12</userid>\n<user_name>Ada &lt;L></user_name>\n"
        ));
        assert!(text.ends_with("</project_files>\n</scheduler_reply>\n"));

        let v = client_parse(&text).unwrap();
        assert!(v.ended);
        assert_eq!(v.scheduler_version, 803);
        assert!(v.dont_use_dcf);
        assert_eq!(v.master_url, "https://example.org/proj/");
        assert_eq!(v.request_delay, 7.0);
        assert_eq!(v.next_rpc_delay, 3600.0);
        assert_eq!(v.messages.len(), 2);
        assert_eq!(v.messages[1].1, "Please update");
        assert!(v.messages[1].0.contains("priority=\"notice\""));
        assert_eq!(v.project_name, "Example Project");
        assert!(!v.project_is_down);
        assert!(v.send_full_workload);
        assert_eq!(v.send_time_stats_log, 0);
        assert_eq!(v.user_name, "Ada <L>");
        assert_eq!(v.user_total_credit, 1234.5);
        assert_eq!(v.hostid, 4217);
        assert_eq!(v.host_venue, "home");
        assert_eq!(v.team_name.as_deref(), Some("A & B"));
        assert_eq!(v.email_hash, "aa11");
        assert_eq!(v.cross_project_id, "bb22");
        assert_eq!(v.external_cpid, "cc33");
        assert_eq!(v.global_prefs.as_deref(), Some("<mod_time>5</mod_time>"));
        assert_eq!(
            v.project_prefs.as_deref(),
            Some("<resource_share>100</resource_share>")
        );
        assert_eq!(v.result_acks, vec!["wu_0_0", "wu_0_1"]);
        assert_eq!(v.result_aborts, vec!["wu_9_0"]);
        assert_eq!(v.result_abort_if_not_starteds, vec!["wu_8_0"]);

        assert_eq!(v.apps.len(), 1);
        assert_eq!(field(&v.apps[0], "<name>"), "worker");
        assert_eq!(field(&v.apps[0], "<user_friendly_name>"), "Worker");
        assert_eq!(field(&v.apps[0], "<non_cpu_intensive>"), "0");
        assert_eq!(field(&v.apps[0], "<fraction_done_exact>"), "1");

        assert_eq!(v.app_versions.len(), 1);
        let av = &v.app_versions[0];
        assert_eq!(field(av, "<app_name>"), "worker");
        assert_eq!(field(av, "<version_num>"), "103");
        assert_eq!(field(av, "<platform>"), "x86_64-pc-linux-gnu");
        assert_eq!(field(av, "<plan_class>"), "cuda");
        assert_eq!(field(av, "<avg_ncpus>"), "0.500000");
        assert_eq!(field(av, "<flops>"), "100000000000.000000");
        assert_eq!(field(av, "<cmdline>"), "--gpu");
        assert_eq!(field(av, "<type>"), "NVIDIA");
        assert_eq!(field(av, "<count>"), "1.000000");
        assert_eq!(field(av, "<gpu_ram>"), "2000000000.000000");
        assert!(av.contains("<main_program/>"));

        assert_eq!(v.workunits.len(), 1);
        let wu = &v.workunits[0];
        assert_eq!(field(wu, "<name>"), "wu_1");
        assert_eq!(field(wu, "<app_name>"), "worker");
        assert_eq!(field(wu, "<rsc_fpops_est>"), "1000000000000.000000");
        assert_eq!(field(wu, "<rsc_fpops_bound>"), "100000000000000.000000");
        assert_eq!(field(wu, "<rsc_memory_bound>"), "500000000.000000");
        assert_eq!(field(wu, "<rsc_disk_bound>"), "1000000000.000000");
        assert_eq!(field(wu, "<command_line>"), "--n 5");

        assert_eq!(v.results.len(), 1);
        let r = &v.results[0];
        assert_eq!(field(r, "<name>"), "wu_1_0");
        assert_eq!(field(r, "<wu_name>"), "wu_1");
        assert_eq!(field(r, "<report_deadline>"), "1763000000");
        assert_eq!(field(r, "<platform>"), "x86_64-pc-linux-gnu");
        assert_eq!(field(r, "<version_num>"), "103");
        assert_eq!(field(r, "<plan_class>"), "cuda");
        assert_eq!(field(r, "<open_name>"), "out");

        assert_eq!(v.code_sign_key, "1024\nabcd\n.");
        assert_eq!(v.code_sign_key_signature, "ffee\n.");
        assert!(v.message_ack);
        assert_eq!(v.trickles.len(), 1);
        assert!(v.trickles[0].contains("<result_name>wu_3_0</result_name>"));
        assert!(v.non_cpu_intensive);
        assert!(v.verify_files_on_app_start);
        assert_eq!(v.delete_file_infos, vec!["old.bin"]);
        assert_eq!(v.no_rsc_apps, vec!["ATI"]);
        assert!(v.gui_urls.unwrap().contains("<name>Home</name>"));
        assert_eq!(v.project_files.as_deref(), Some(""));
    }

    #[test]
    fn exact_bytes_of_a_small_reply() {
        let r = SchedulerReply {
            core_client_version: 80204,
            project_name: "P".to_string(),
            user: ReplyUser {
                id: 1,
                name: "n".to_string(),
                project_prefs: "<project_preferences>\n</project_preferences>".to_string(),
                ..ReplyUser::default()
            },
            hostid: 2,
            result_acks: vec!["r_0".to_string()],
            ..SchedulerReply::default()
        };
        assert_eq!(
            r.write().unwrap(),
            "<scheduler_reply>\n\
<scheduler_version>803</scheduler_version>\n\
<dont_use_dcf/>\n\
<project_name>P</project_name>\n\
<userid>1</userid>\n\
<user_name>n</user_name>\n\
<user_total_credit>0.000000</user_total_credit>\n\
<user_expavg_credit>0.000000</user_expavg_credit>\n\
<user_create_time>0</user_create_time>\n\
<project_preferences>\n\
</project_preferences>\n\
<hostid>2</hostid>\n\
<host_total_credit>0.000000</host_total_credit>\n\
<host_expavg_credit>0.000000</host_expavg_credit>\n\
<host_venue></host_venue>\n\
<host_create_time>0</host_create_time>\n\
<team_name></team_name>\n\
<result_ack>\n    <name>r_0</name>\n</result_ack>\n\
</scheduler_reply>\n"
        );
    }

    #[test]
    fn project_down_and_nucleus_only_end_early() {
        let mut r = full_reply();
        r.project_is_down = true;
        r.send_time_stats_log = true;
        let text = r.write().unwrap();
        assert!(text.ends_with(
            "<project_name>Example Project</project_name>\n<send_time_stats_log>1</send_time_stats_log>\n<project_is_down/>\n</scheduler_reply>\n"
        ));
        let v = client_parse(&text).unwrap();
        assert!(v.project_is_down);
        assert_eq!(v.send_time_stats_log, 1);
        assert_eq!(v.hostid, 0);
        assert!(v.results.is_empty());

        let mut r = full_reply();
        r.nucleus_only = true;
        let text = r.write().unwrap();
        assert!(text.ends_with("<send_full_workload/>\n</scheduler_reply>\n"));
        let v = client_parse(&text).unwrap();
        assert_eq!(v.messages.len(), 2);
        assert_eq!(v.request_delay, 7.0);
        assert!(v.team_name.is_none());
        assert!(!text.contains("<userid>"));
    }

    #[test]
    fn old_clients_get_old_forms() {
        let mut r = full_reply();
        r.core_client_version = 61000;
        let text = r.write().unwrap();
        assert!(!text.contains("<dont_use_dcf/>"));
        assert!(text.contains("<message priority=\"high\">Please update</message>\n"));
        assert!(text.contains("<type>CUDA</type>"));

        r.core_client_version = 41900;
        r.messages[0].message = "line one\nline two".to_string();
        let text = r.write().unwrap();
        assert!(text
            .contains("<message priority=\"notice\">line one line two Please update </message>\n"));
        assert_eq!(text.matches("<message ").count(), 1);

        r.core_client_version = 70028;
        r.app_versions[0].proc_type = PROC_TYPE_AMD_GPU;
        let text = r.write().unwrap();
        assert!(text.contains("<dont_use_dcf/>"));
        assert!(text.contains("<type>ATI</type>"));
    }

    #[test]
    fn cpu_app_version_has_no_coproc() {
        let mut out = String::new();
        ReplyAppVersion {
            xml_doc: AV_XML.to_string(),
            platform: "windows_x86_64".to_string(),
            avg_ncpus: 1.0,
            flops: 3e9,
            ..ReplyAppVersion::default()
        }
        .write(&mut out, 80000)
        .unwrap();
        assert!(out.ends_with(
            "    </file_ref>\n    <platform>windows_x86_64</platform>\n    <avg_ncpus>1.000000</avg_ncpus>\n    <flops>3000000000.000000</flops>\n</app_version>\n"
        ));
    }

    #[test]
    fn missing_end_tags_are_errors() {
        let mut r = full_reply();
        r.app_versions[0].xml_doc = "<app_version>\n".to_string();
        assert_eq!(
            r.write(),
            Err(ProtoError::NoEndTag("</app_version>".to_string()))
        );
        let mut r = full_reply();
        r.results[0].xml_doc_in = "<result>\n".to_string();
        assert_eq!(
            r.write(),
            Err(ProtoError::NoEndTag("</result>".to_string()))
        );
    }

    #[test]
    fn insert_helpers() {
        assert_eq!(
            insert_name_tags("<result>\n    <x/>\n</result>\n", "r", "w").unwrap(),
            "<result>\n<wu_name>w</wu_name>\n<name>r</name>\n    <x/>\n</result>\n"
        );
        assert_eq!(
            insert_deadline_tag("<result>\n</result>\n", 77).unwrap(),
            "<result>\n<report_deadline>77</report_deadline>\n</result>\n"
        );
        let wu = insert_wu_tags(
            "<workunit>\n</workunit>\n",
            &WuTags {
                rsc_fpops_est: 1.0,
                rsc_fpops_bound: 2.0,
                rsc_memory_bound: 3.0,
                rsc_disk_bound: 4.0,
                name: "w",
                app_name: "a",
                job_keyword_ids: "3 17",
            },
        )
        .unwrap();
        assert_eq!(
            wu,
            "<workunit>\n    <rsc_fpops_est>1.000000</rsc_fpops_est>\n    <rsc_fpops_bound>2.000000</rsc_fpops_bound>\n    <rsc_memory_bound>3.000000</rsc_memory_bound>\n    <rsc_disk_bound>4.000000</rsc_disk_bound>\n    <name>w</name>\n    <app_name>a</app_name>\n    <job_keyword_ids>3 17</job_keyword_ids>\n</workunit>\n"
        );
        // The marker includes the newline.
        assert_eq!(
            insert_deadline_tag("<result></result>", 1),
            Err(ProtoError::MarkerNotFound("<result>\n".to_string()))
        );
        assert!(matches!(
            insert_wu_tags("<result>\n</result>", &WuTags::default()),
            Err(ProtoError::MarkerNotFound(_))
        ));
    }

    #[test]
    fn project_down_message() {
        let text = write_project_down("Project is down for maintenance", 3600, "");
        assert_eq!(
            text,
            "<scheduler_reply>\n    <message priority=\"low\">Project is down for maintenance</message>\n    <request_delay>3600</request_delay>\n    <project_is_down/>\n</scheduler_reply>\n"
        );
        let v = client_parse(&text).unwrap();
        assert!(v.project_is_down);
        assert_eq!(v.request_delay, 3600.0);
        assert_eq!(v.messages[0].1, "Project is down for maintenance");
        let text = write_project_down("x", 1, "<ended>1</ended>\n");
        assert!(text.ends_with("<project_is_down/>\n<ended>1</ended>\n</scheduler_reply>\n"));
    }

    #[test]
    fn truncated_reply_is_rejected_by_the_client() {
        let text = full_reply().write().unwrap();
        let cut = &text[..text.len() - "</scheduler_reply>\n".len()];
        assert_eq!(client_parse(cut).unwrap_err(), "no end tag");
        assert_eq!(client_parse("<html></html>").unwrap_err(), "no start tag");
    }
}
