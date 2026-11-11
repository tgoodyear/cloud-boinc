//! Replies of the web RPCs a client uses to attach to a project, and the
//! error numbers of `lib/error_numbers.h`.
//!
//! The RPCs are `get_project_config.php`, `lookup_account.php` and
//! `create_account.php` in `html/user/`. The client reads their replies in
//! `PROJECT_CONFIG::parse` and `ACCOUNT_OUT::parse` (`lib/gui_rpc_client_ops.cpp`
//! and `client/acct_setup.cpp`). Every reply starts with [`XML_HEADER`]
//! and is served as `text/xml`.

use std::fmt::Write as _;

/// First line of every web RPC reply (`xml_header` in `html/inc/xml.inc`).
pub const XML_HEADER: &str = "<?xml version=\"1.0\" encoding=\"ISO-8859-1\" ?>\n";

/// Error numbers from `lib/error_numbers.h`.
pub mod err {
    pub const BOINC_SUCCESS: i32 = 0;
    pub const ERR_XML_PARSE: i32 = -112;
    pub const ERR_NULL: i32 = -116;
    pub const ERR_BUFFER_OVERFLOW: i32 = -118;
    pub const ERR_MD5_FAILED: i32 = -119;
    pub const ERR_RSA_FAILED: i32 = -120;
    pub const ERR_NO_SIGNATURE: i32 = -123;
    pub const ERR_UPLOAD_TRANSIENT: i32 = -127;
    pub const ERR_UPLOAD_PERMANENT: i32 = -128;
    pub const ERR_ALREADY_ATTACHED: i32 = -130;
    pub const ERR_FILE_TOO_BIG: i32 = -131;
    pub const ERR_DB_NOT_FOUND: i32 = -136;
    pub const ERR_DB_NOT_UNIQUE: i32 = -137;
    pub const ERR_DB_CANT_CONNECT: i32 = -138;
    pub const ERR_NOT_IMPLEMENTED: i32 = -150;
    pub const ERR_AUTHENTICATOR: i32 = -155;
    pub const ERR_BAD_RESULT_STATE: i32 = -158;
    pub const ERR_DB_CANT_INIT: i32 = -159;
    pub const ERR_NOT_UNIQUE: i32 = -160;
    pub const ERR_NOT_FOUND: i32 = -161;
    pub const ERR_NO_EXIT_STATUS: i32 = -162;
    pub const ERR_FILE_MISSING: i32 = -163;
    pub const ERR_INVALID_PARAM: i32 = -178;
    pub const ERR_TIMEOUT: i32 = -182;
    pub const ERR_PROJECT_DOWN: i32 = -183;
    pub const ERR_HTTP_TRANSIENT: i32 = -184;
    pub const ERR_RESULT_START: i32 = -185;
    pub const ERR_RESULT_DOWNLOAD: i32 = -186;
    pub const ERR_RESULT_UPLOAD: i32 = -187;
    pub const ERR_BAD_USER_NAME: i32 = -188;
    pub const ERR_INVALID_URL: i32 = -189;
    pub const ERR_MAJOR_VERSION: i32 = -190;
    pub const ERR_NO_APP_VERSION: i32 = -195;
    pub const ERR_WU_USER_RULE: i32 = -196;
    pub const ERR_ABORTED_VIA_GUI: i32 = -197;
    pub const ERR_INSUFFICIENT_RESOURCE: i32 = -198;
    pub const ERR_RETRY: i32 = -199;
    pub const ERR_WRONG_SIZE: i32 = -200;
    pub const ERR_USER_PERMISSION: i32 = -201;
    pub const ERR_IN_PROGRESS: i32 = -204;
    pub const ERR_BAD_EMAIL_ADDR: i32 = -205;
    pub const ERR_BAD_PASSWD: i32 = -206;
    pub const ERR_NONUNIQUE_EMAIL: i32 = -207;
    pub const ERR_ACCT_CREATION_DISABLED: i32 = -208;
    pub const ERR_ATTACH_FAIL_INIT: i32 = -209;
    pub const ERR_ATTACH_FAIL_DOWNLOAD: i32 = -210;
    pub const ERR_ATTACH_FAIL_PARSE: i32 = -211;
    pub const ERR_ATTACH_FAIL_BAD_KEY: i32 = -212;
    pub const ERR_ATTACH_FAIL_FILE_WRITE: i32 = -213;
    pub const ERR_ATTACH_FAIL_SERVER_ERROR: i32 = -214;
    pub const ERR_SIGNING_KEY: i32 = -215;
    pub const ERR_WRONG_URL: i32 = -219;
    pub const ERR_DUP_NAME: i32 = -220;
    pub const ERR_FILE_WRONG_SIZE: i32 = -221;
    pub const ERR_HTTP_PERMANENT: i32 = -224;
    pub const ERR_BAD_FILENAME: i32 = -225;
    pub const ERR_DB_CONN_LOST: i32 = -230;
    pub const ERR_CRYPTO: i32 = -231;
    pub const ERR_NEED_HTTPS: i32 = -238;
    pub const ERR_ACCT_REQUIRE_CONSENT: i32 = -242;
    pub const ERR_INVALID_STATE: i32 = -243;

    // Exit statuses a client reports for a result.
    pub const EXIT_STATEFILE_WRITE: i32 = 192;
    pub const EXIT_SIGNAL: i32 = 193;
    pub const EXIT_ABORTED_BY_CLIENT: i32 = 194;
    pub const EXIT_CHILD_FAILED: i32 = 195;
    pub const EXIT_DISK_LIMIT_EXCEEDED: i32 = 196;
    pub const EXIT_TIME_LIMIT_EXCEEDED: i32 = 197;
    pub const EXIT_MEM_LIMIT_EXCEEDED: i32 = 198;
    pub const EXIT_CLIENT_EXITING: i32 = 199;
    pub const EXIT_UNSTARTED_LATE: i32 = 200;
    pub const EXIT_MISSING_COPROC: i32 = 201;
    pub const EXIT_ABORTED_BY_PROJECT: i32 = 202;
    pub const EXIT_ABORTED_VIA_GUI: i32 = 203;
    pub const EXIT_UNKNOWN: i32 = 204;
    pub const EXIT_OUT_OF_MEMORY: i32 = 205;
    pub const EXIT_INIT_FAILURE: i32 = 206;
    pub const EXIT_NO_SUB_TASKS: i32 = 207;
    pub const EXIT_SUB_TASK_FAILURE: i32 = 208;
    pub const EXIT_OVERDUE_EXCEEDED: i32 = 209;
}

/// The message `xml_error` (`html/inc/util_basic.inc`) uses for an error
/// number when the caller gives none.
pub fn default_error_message(num: i32) -> &'static str {
    match num {
        -112 => "Invalid XML",
        -136 => "Not found",
        -137 => "Name or email address is not unique",
        -138 => "Can't access database",
        -183 => "Project is temporarily offline",
        -205 => "Email address has invalid syntax",
        -206 => "Invalid password",
        -207 => "Email address is not unique",
        -208 => "Account creation is disabled",
        -209 => "Invalid invitation code",
        -210 => "Invalid request method",
        _ => "Unknown error",
    }
}

/// An error reply (`xml_error`), header included. With `msg` `None` or
/// empty the default message for `num` is used. The message is written as
/// given.
pub fn write_error(num: i32, msg: Option<&str>) -> String {
    let msg = match msg {
        Some(m) if !m.is_empty() => m,
        _ => default_error_message(num),
    };
    format!(
        "{XML_HEADER}<error>\n    <error_num>{num}</error_num>\n    <error_msg>{msg}</error_msg>\n</error>\n"
    )
}

/// Successful reply of `lookup_account.php`, header included.
pub fn write_lookup_account(authenticator: &str) -> String {
    format!("{XML_HEADER}<account_out>\n<authenticator>{authenticator}</authenticator>\n</account_out>\n")
}

/// Reply of `lookup_account.php` to a request without `passwd_hash`: the
/// account exists.
pub fn write_lookup_account_exists() -> String {
    format!("{XML_HEADER}<account_out>\n   <success/>\n</account_out>\n")
}

/// Successful reply of `create_account.php`, header included.
pub fn write_create_account(authenticator: &str) -> String {
    format!("{XML_HEADER} <account_out>\n   <authenticator>{authenticator}</authenticator>\n</account_out>\n")
}

/// The failures of `create_account.php` and `lookup_account.php`, with the
/// numbers and messages upstream sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountError {
    /// `lookup_account`: no account with that email address.
    NotFound,
    /// `lookup_account`: wrong password hash.
    BadPassword,
    /// The project has disabled account creation.
    CreationDisabled,
    /// Accounts are created on the web site only.
    CreationOnWebSite,
    /// A missing or wrong invitation code.
    BadInviteCode,
    /// Upstream sends the reason `is_valid_user_name`
    /// (`html/inc/user_util.inc`) gives; [`AccountError::message`] holds
    /// one of them, and a caller with a more exact reason passes it to
    /// [`write_error`].
    BadUserName,
    BadEmailAddr,
    /// `passwd_hash` is not 32 characters.
    BadPasswdHashLength,
    /// An account with that email address exists, with another password.
    NotUnique,
    /// The project requires consent to its terms of use, which the
    /// request did not give.
    ConsentRequired,
    DbError,
}

impl AccountError {
    /// The `<error_num>` of the reply.
    pub fn num(self) -> i32 {
        match self {
            AccountError::NotFound => err::ERR_DB_NOT_FOUND,
            AccountError::BadPassword => err::ERR_BAD_PASSWD,
            AccountError::CreationDisabled
            | AccountError::CreationOnWebSite
            | AccountError::BadInviteCode
            | AccountError::BadPasswdHashLength => -1,
            AccountError::BadUserName => err::ERR_BAD_USER_NAME,
            AccountError::BadEmailAddr => err::ERR_BAD_EMAIL_ADDR,
            AccountError::NotUnique => err::ERR_DB_NOT_UNIQUE,
            AccountError::ConsentRequired => err::ERR_ACCT_REQUIRE_CONSENT,
            AccountError::DbError => err::ERR_DB_CANT_CONNECT,
        }
    }

    /// The `<error_msg>` of the reply.
    pub fn message(self) -> &'static str {
        match self {
            AccountError::CreationDisabled => "The project is not accepting new accounts.",
            AccountError::CreationOnWebSite => {
                "Please visit the project web site to create an account, then try again."
            }
            AccountError::BadInviteCode => "Invalid invitation code",
            AccountError::BadPasswdHashLength => "password hash length not 32",
            AccountError::BadUserName => "user name may not contain special characters",
            AccountError::ConsentRequired => {
                "This project requires you to consent to its terms of use. \
                 Please update your BOINC software \
                 or register via the project's website \
                 or contact your account manager's provider."
            }
            other => default_error_message(other.num()),
        }
    }

    /// The whole reply, header included.
    pub fn reply(self) -> String {
        write_error(self.num(), Some(self.message()))
    }
}

/// An entry of `<platforms>` in the project config.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfigPlatform {
    pub name: String,
    pub user_friendly_name: String,
    /// Written when not empty.
    pub plan_class: String,
}

/// What `get_project_config.php` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectConfig {
    /// The project's display name (`long_name`).
    pub name: String,
    pub master_url: String,
    /// Base URL of the web RPCs; clients append `lookup_account.php` and
    /// so on. Must be HTTPS for current clients to send passwords.
    pub web_rpc_url_base: String,
    /// `major.minor.release` of the server software.
    pub server_version: String,
    pub account_manager: bool,
    pub web_stopped: bool,
    pub account_creation_disabled: bool,
    pub invite_code_required: bool,
    pub min_passwd_length: u32,
    pub sched_stopped: bool,
    /// `<min_client_version>`, as `10000*major + 100*minor + release`.
    pub min_client_version: Option<i32>,
    pub platforms: Vec<ConfigPlatform>,
    /// Shown by the client before attaching; written when not empty.
    pub terms_of_use: String,
    /// Base64 of the account ownership public key file; written when not
    /// empty.
    pub account_ownership_public_key: String,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        ProjectConfig {
            name: String::new(),
            master_url: String::new(),
            web_rpc_url_base: String::new(),
            server_version: "8.3.0".to_string(),
            account_manager: false,
            web_stopped: false,
            account_creation_disabled: false,
            invite_code_required: false,
            min_passwd_length: 6,
            sched_stopped: false,
            min_client_version: None,
            platforms: Vec::new(),
            terms_of_use: String::new(),
            account_ownership_public_key: String::new(),
        }
    }
}

impl ProjectConfig {
    /// The reply of `get_project_config.php`, header included. Values are
    /// written as given, as upstream does.
    pub fn write(&self) -> String {
        let mut o = String::from(XML_HEADER);
        let _ = write!(
            o,
            "<project_config>\n    <name>{}</name>\n    <master_url>{}</master_url>\n    <web_rpc_url_base>{}</web_rpc_url_base>\n",
            self.name, self.master_url, self.web_rpc_url_base
        );
        let _ = writeln!(
            o,
            "<server_version>{}</server_version>",
            self.server_version
        );
        if self.account_manager {
            o.push_str("    <account_manager/>\n");
        }
        if self.web_stopped {
            let _ = write!(
                o,
                "\n        <error_num>{}</error_num>\n        <web_stopped>1</web_stopped>\n    ",
                err::ERR_PROJECT_DOWN
            );
        } else {
            o.push_str("    <web_stopped>0</web_stopped>\n");
        }
        if self.account_creation_disabled || self.invite_code_required {
            o.push_str("    <account_creation_disabled/>\n");
        }
        if self.invite_code_required {
            o.push_str("    <invite_code_required/>\n");
        }
        let _ = writeln!(
            o,
            "    <min_passwd_length>{}</min_passwd_length>",
            self.min_passwd_length
        );
        let _ = writeln!(
            o,
            "    <sched_stopped>{}</sched_stopped>",
            i32::from(self.sched_stopped)
        );
        if let Some(v) = self.min_client_version {
            let _ = writeln!(o, "<min_client_version>{v}</min_client_version>");
        }
        o.push_str("    <platforms>");
        for p in &self.platforms {
            let _ = write!(
                o,
                "\n            <platform>\n                <platform_name>{}</platform_name>\n                <user_friendly_name>{}</user_friendly_name>",
                p.name, p.user_friendly_name
            );
            if !p.plan_class.is_empty() {
                let _ = write!(
                    o,
                    "\n                <plan_class>{}</plan_class>\n",
                    p.plan_class
                );
            }
            o.push_str("\n            </platform>");
        }
        o.push_str("\n    </platforms>\n");
        let tou = self.terms_of_use.trim();
        if !tou.is_empty() {
            let _ = write!(o, "    <terms_of_use>\n{tou}\n</terms_of_use>\n");
        }
        if !self.account_ownership_public_key.is_empty() {
            let _ = writeln!(
                o,
                "    <account_ownership_public_key>{}</account_ownership_public_key>",
                self.account_ownership_public_key
            );
        }
        o.push_str("</project_config>");
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xml::XmlParser;

    /// `PROJECT_CONFIG::parse` in `lib/gui_rpc_client_ops.cpp`, the fields
    /// written here.
    #[derive(Debug, Default)]
    struct ClientConfig {
        ended: bool,
        name: String,
        master_url: String,
        web_rpc_url_base: String,
        error_num: i32,
        account_manager: bool,
        account_creation_disabled: bool,
        invite_code_required: bool,
        min_passwd_length: i32,
        sched_stopped: bool,
        web_stopped: bool,
        min_client_version: i32,
        platforms: Vec<(String, String, String)>,
        terms_of_use: String,
    }

    fn client_parse_config(text: &str) -> ClientConfig {
        let mut c = ClientConfig::default();
        let mut xp = XmlParser::new(text);
        assert!(xp.parse_start("project_config"));
        while xp.get_tag() {
            if !xp.is_tag() {
                continue;
            }
            if xp.match_tag("/project_config") {
                c.ended = true;
                break;
            }
            take!(xp.parse_str("name") => c.name);
            take!(xp.parse_str("master_url") => c.master_url);
            take!(xp.parse_str("web_rpc_url_base") => c.web_rpc_url_base);
            take!(xp.parse_int("error_num") => c.error_num);
            take!(xp.parse_bool("account_manager") => c.account_manager);
            take!(xp.parse_bool("account_creation_disabled") => c.account_creation_disabled);
            take!(xp.parse_bool("invite_code_required") => c.invite_code_required);
            take!(xp.parse_int("min_passwd_length") => c.min_passwd_length);
            take!(xp.parse_bool("sched_stopped") => c.sched_stopped);
            take!(xp.parse_bool("web_stopped") => c.web_stopped);
            take!(xp.parse_int("min_client_version") => c.min_client_version);
            if xp.match_tag("terms_of_use") {
                c.terms_of_use = xp.element_contents("</terms_of_use>").unwrap();
                continue;
            }
            if xp.match_tag("platforms") {
                let mut cur = (String::new(), String::new(), String::new());
                while xp.get_tag() {
                    if xp.match_tag("/platforms") {
                        break;
                    }
                    if xp.match_tag("platform") {
                        cur = Default::default();
                        continue;
                    }
                    if xp.match_tag("/platform") {
                        c.platforms.push(cur.clone());
                        continue;
                    }
                    take!(xp.parse_str("platform_name") => cur.0);
                    take!(xp.parse_str("user_friendly_name") => cur.1);
                    take!(xp.parse_str("plan_class") => cur.2);
                }
                continue;
            }
            xp.skip_unexpected();
        }
        c
    }

    fn config() -> ProjectConfig {
        ProjectConfig {
            name: "Example Project".to_string(),
            master_url: "https://example.org/proj/".to_string(),
            web_rpc_url_base: "https://example.org/proj/".to_string(),
            platforms: vec![
                ConfigPlatform {
                    name: "x86_64-pc-linux-gnu".to_string(),
                    user_friendly_name: "Linux running on an AMD x86_64 or Intel EM64T CPU"
                        .to_string(),
                    plan_class: String::new(),
                },
                ConfigPlatform {
                    name: "windows_x86_64".to_string(),
                    user_friendly_name:
                        "Microsoft Windows running on an AMD x86_64 or Intel EM64T CPU".to_string(),
                    plan_class: "cuda".to_string(),
                },
            ],
            ..ProjectConfig::default()
        }
    }

    #[test]
    fn project_config_bytes() {
        assert_eq!(
            config().write(),
            "<?xml version=\"1.0\" encoding=\"ISO-8859-1\" ?>\n\
<project_config>\n    <name>Example Project</name>\n    <master_url>https://example.org/proj/</master_url>\n    <web_rpc_url_base>https://example.org/proj/</web_rpc_url_base>\n\
<server_version>8.3.0</server_version>\n    <web_stopped>0</web_stopped>\n    <min_passwd_length>6</min_passwd_length>\n    <sched_stopped>0</sched_stopped>\n    <platforms>\n            <platform>\n                <platform_name>x86_64-pc-linux-gnu</platform_name>\n                <user_friendly_name>Linux running on an AMD x86_64 or Intel EM64T CPU</user_friendly_name>\n            </platform>\n            <platform>\n                <platform_name>windows_x86_64</platform_name>\n                <user_friendly_name>Microsoft Windows running on an AMD x86_64 or Intel EM64T CPU</user_friendly_name>\n                <plan_class>cuda</plan_class>\n\n            </platform>\n    </platforms>\n</project_config>"
        );
    }

    #[test]
    fn project_config_as_the_client_reads_it() {
        let c = client_parse_config(&config().write());
        assert!(c.ended);
        assert_eq!(c.name, "Example Project");
        assert_eq!(c.master_url, "https://example.org/proj/");
        assert_eq!(c.web_rpc_url_base, "https://example.org/proj/");
        assert_eq!(c.error_num, 0);
        assert!(!c.account_manager);
        assert!(!c.account_creation_disabled);
        assert!(!c.web_stopped);
        assert!(!c.sched_stopped);
        assert_eq!(c.min_passwd_length, 6);
        assert_eq!(c.platforms.len(), 2);
        assert_eq!(c.platforms[0].0, "x86_64-pc-linux-gnu");
        assert_eq!(c.platforms[0].2, "");
        assert_eq!(c.platforms[1].0, "windows_x86_64");
        assert_eq!(c.platforms[1].2, "cuda");
    }

    #[test]
    fn project_config_options() {
        let mut p = config();
        p.account_manager = true;
        p.web_stopped = true;
        p.sched_stopped = true;
        p.invite_code_required = true;
        p.min_passwd_length = 12;
        p.min_client_version = Some(70000);
        p.terms_of_use = "\nBe nice.\nSecond line.\n\n".to_string();
        p.account_ownership_public_key = "QUJD".to_string();
        p.platforms.clear();
        let text = p.write();
        assert!(text.contains("    <account_manager/>\n"));
        assert!(text.contains("<error_num>-183</error_num>"));
        assert!(text.contains("    <terms_of_use>\nBe nice.\nSecond line.\n</terms_of_use>\n"));
        assert!(text.contains(
            "    <account_ownership_public_key>QUJD</account_ownership_public_key>\n</project_config>"
        ));
        assert!(text.contains("    <platforms>\n    </platforms>\n"));
        let c = client_parse_config(&text);
        assert!(c.ended);
        assert!(c.account_manager);
        assert!(c.web_stopped);
        assert!(c.sched_stopped);
        assert_eq!(c.error_num, -183);
        assert!(c.account_creation_disabled);
        assert!(c.invite_code_required);
        assert_eq!(c.min_passwd_length, 12);
        assert_eq!(c.min_client_version, 70000);
        assert_eq!(c.terms_of_use, "Be nice.\nSecond line.");
        assert!(c.platforms.is_empty());
    }

    /// `ACCOUNT_OUT::parse` in `lib/gui_rpc_client_ops.cpp`.
    fn client_parse_account(text: &str) -> (i32, String, String) {
        let mut xp = XmlParser::new(text);
        let (mut num, mut msg, mut auth) = (0, String::new(), String::new());
        while xp.get_tag() {
            take!(xp.parse_int("error_num") => num);
            take!(xp.parse_str("error_msg") => msg);
            take!(xp.parse_str("authenticator") => auth);
        }
        (num, msg, auth)
    }

    #[test]
    fn account_replies() {
        assert_eq!(
            write_lookup_account("abc123"),
            "<?xml version=\"1.0\" encoding=\"ISO-8859-1\" ?>\n<account_out>\n<authenticator>abc123</authenticator>\n</account_out>\n"
        );
        assert_eq!(
            write_create_account("abc123"),
            "<?xml version=\"1.0\" encoding=\"ISO-8859-1\" ?>\n <account_out>\n   <authenticator>abc123</authenticator>\n</account_out>\n"
        );
        assert_eq!(
            write_lookup_account_exists(),
            "<?xml version=\"1.0\" encoding=\"ISO-8859-1\" ?>\n<account_out>\n   <success/>\n</account_out>\n"
        );
        assert_eq!(
            client_parse_account(&write_lookup_account("abc123")),
            (0, String::new(), "abc123".to_string())
        );
        assert_eq!(
            client_parse_account(&write_create_account("k1")),
            (0, String::new(), "k1".to_string())
        );
    }

    #[test]
    fn error_replies() {
        assert_eq!(
            write_error(-136, None),
            "<?xml version=\"1.0\" encoding=\"ISO-8859-1\" ?>\n<error>\n    <error_num>-136</error_num>\n    <error_msg>Not found</error_msg>\n</error>\n"
        );
        assert!(write_error(-1, Some("")).contains("<error_msg>Unknown error</error_msg>"));
        assert!(write_error(-1, Some("custom")).contains("<error_msg>custom</error_msg>"));
        assert_eq!(
            client_parse_account(&AccountError::BadPassword.reply()),
            (-206, "Invalid password".to_string(), String::new())
        );
        assert_eq!(
            client_parse_account(&AccountError::NotFound.reply()),
            (-136, "Not found".to_string(), String::new())
        );
        assert_eq!(AccountError::NotUnique.num(), -137);
        assert_eq!(
            AccountError::NotUnique.message(),
            "Name or email address is not unique"
        );
        assert_eq!(AccountError::BadEmailAddr.num(), -205);
        assert_eq!(AccountError::BadUserName.num(), -188);
        assert_eq!(AccountError::ConsentRequired.num(), -242);
        assert_eq!(AccountError::DbError.num(), -138);
        for e in [
            AccountError::CreationDisabled,
            AccountError::CreationOnWebSite,
            AccountError::BadInviteCode,
            AccountError::BadPasswdHashLength,
        ] {
            assert_eq!(e.num(), -1);
            assert_ne!(e.message(), "Unknown error");
        }
        assert_eq!(
            AccountError::CreationDisabled.message(),
            "The project is not accepting new accounts."
        );
    }

    #[test]
    fn default_messages() {
        assert_eq!(default_error_message(err::ERR_XML_PARSE), "Invalid XML");
        assert_eq!(
            default_error_message(err::ERR_PROJECT_DOWN),
            "Project is temporarily offline"
        );
        assert_eq!(
            default_error_message(err::ERR_NONUNIQUE_EMAIL),
            "Email address is not unique"
        );
        assert_eq!(default_error_message(12345), "Unknown error");
    }
}
