//! Replies of the web RPCs a client uses to attach, the upload handler and
//! the master page.

use std::cell::Cell;

use cb_core::project::{put_setting, Project, SETTING_CONFIG};
use cb_core::web::{create_account, lookup_account, master_page, project_config, NewAccount};
use cb_core::Config;
use cb_crypto::account::passwd_hash;
use cb_db::{stmt, Db};
use cb_proto::upload::{
    incomplete_error, reply_error, reply_file_size, reply_success, ClientVersion, UploadError,
    UploadHeader,
};
use cb_proto::web_rpc::{err, AccountError, ConfigPlatform, ProjectConfig};
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;

use crate::harness::{Expect, Harness};

const PEPPER: &[u8] = b"pepper for tests";
const MASTER: &str = "https://p.example/";
const CONFIG: &str = "<config>\n<long_name>Oracle Project &amp; Friends</long_name>\n<master_url>https://p.example</master_url>\n<account_creation>1</account_creation>\n<min_core_client_version>70000</min_core_client_version>\n</config>\n";

struct World {
    db: SqliteDb,
    seq: Cell<u32>,
}

impl World {
    fn new(config: &str) -> World {
        let db = SqliteDb::with_schema().unwrap();
        block_on(put_setting(&db, SETTING_CONFIG, config, 10)).unwrap();
        for sql in [
            "INSERT INTO platform (id, create_time, name, user_friendly_name) VALUES (1, 1, 'x86_64-pc-linux-gnu', 'Linux (64 bit)')",
            "INSERT INTO platform (id, create_time, name, user_friendly_name) VALUES (2, 1, 'windows_x86_64', 'Windows <64 bit>')",
            "INSERT INTO app (id, create_time, name, user_friendly_name) VALUES (1, 1, 'worker', 'Worker')",
            "INSERT INTO app_version (id, create_time, appid, version_num, platformid, xml_doc, plan_class) VALUES (1, 1, 1, 102, 1, '', '')",
            "INSERT INTO app_version (id, create_time, appid, version_num, platformid, xml_doc, plan_class) VALUES (2, 1, 1, 102, 1, '', 'cuda118')",
            "INSERT INTO app_version (id, create_time, appid, version_num, platformid, xml_doc, plan_class) VALUES (3, 1, 1, 102, 2, '', '')",
        ] {
            block_on(db.run(stmt!(sql))).unwrap();
        }
        World {
            db,
            seq: Cell::new(0),
        }
    }

    fn rand(&self) -> u32 {
        self.seq.set(self.seq.get().wrapping_add(2_654_435_761));
        self.seq.get()
    }

    fn project(&self) -> Project {
        block_on(Project::load(&self.db)).unwrap()
    }
}

fn check_config(
    h: &Harness,
    case: &str,
    file: &str,
    text: &str,
    want: &ProjectConfig,
    unescaped_name: &str,
) {
    let path = h.save(file, text);
    if !h.ready() {
        return;
    }
    let d = h.run("project_config", &path, &[]);
    let mut e = Expect::new(case, &d);
    e.rc("parse.rc", 0);
    e.s("config.name", unescaped_name);
    e.s("config.master_url", &want.master_url);
    e.s("config.web_rpc_url_base", &want.web_rpc_url_base);
    e.n(
        "config.min_passwd_length",
        f64::from(want.min_passwd_length),
    );
    e.b("config.account_manager", want.account_manager);
    e.b(
        "config.account_creation_disabled",
        want.account_creation_disabled || want.invite_code_required,
    );
    e.b("config.sched_stopped", want.sched_stopped);
    e.b("config.web_stopped", want.web_stopped);
    // A stopped web site reports the error number the client then shows.
    e.n(
        "config.error_num",
        if want.web_stopped {
            f64::from(err::ERR_PROJECT_DOWN)
        } else {
            0.0
        },
    );
    e.n(
        "config.min_client_version",
        f64::from(want.min_client_version.unwrap_or(0)),
    );
    e.n("config.platform.count", want.platforms.len() as f64);
    for (i, p) in want.platforms.iter().enumerate() {
        e.s(&format!("config.platform[{i}]"), &p.name);
    }
    let tou = want.terms_of_use.trim();
    let got = d.get("config.terms_of_use").unwrap_or_default().trim();
    if got != tou {
        e.fail(format!(
            "config.terms_of_use: client read {got:?}, server meant {tou:?}"
        ));
    }
    e.finish();
}

/// The platform entries of the project in [`World::new`]: one per
/// platform and plan class, as `get_project_config.php` lists them, and the
/// client reads one `platform_name` for each.
fn platforms() -> Vec<ConfigPlatform> {
    let entry = |name: &str, plan_class: &str| ConfigPlatform {
        name: name.into(),
        plan_class: plan_class.into(),
        ..Default::default()
    };
    vec![
        entry("x86_64-pc-linux-gnu", ""),
        entry("x86_64-pc-linux-gnu", "cuda118"),
        entry("windows_x86_64", ""),
    ]
}

fn unescape_name(s: &str) -> String {
    s.replace("&amp;", "&")
}

#[test]
fn project_config_of_the_server() {
    let h = Harness::new("web");
    let w = World::new(CONFIG);
    let p = w.project();
    let text = project_config(&p);
    let want = ProjectConfig {
        name: "Oracle Project &amp; Friends".into(),
        master_url: MASTER.into(),
        web_rpc_url_base: MASTER.into(),
        min_client_version: Some(70000),
        platforms: platforms(),
        ..Default::default()
    };
    check_config(
        &h,
        "project config, accounts open",
        "project_config_open.xml",
        &text,
        &want,
        &unescape_name(&want.name),
    );
}

#[test]
fn project_config_with_account_creation_off() {
    let h = Harness::new("web");
    let config = CONFIG.replace("<account_creation>1", "<account_creation>0");
    let w = World::new(&config);
    let text = project_config(&w.project());
    let want = ProjectConfig {
        name: "Oracle Project &amp; Friends".into(),
        master_url: MASTER.into(),
        web_rpc_url_base: MASTER.into(),
        account_creation_disabled: true,
        min_client_version: Some(70000),
        platforms: platforms(),
        ..Default::default()
    };
    check_config(
        &h,
        "project config, account creation off",
        "project_config_closed.xml",
        &text,
        &want,
        &unescape_name(&want.name),
    );
}

#[test]
fn project_config_of_an_ended_project() {
    let h = Harness::new("web");
    let config = CONFIG.replace(
        "<account_creation>1",
        "<ended>1</ended><dont_send_jobs>1</dont_send_jobs><account_creation>1",
    );
    let w = World::new(&config);
    let text = project_config(&w.project());
    let want = ProjectConfig {
        name: "Oracle Project &amp; Friends".into(),
        master_url: MASTER.into(),
        web_rpc_url_base: MASTER.into(),
        sched_stopped: true,
        min_client_version: Some(70000),
        platforms: platforms(),
        ..Default::default()
    };
    check_config(
        &h,
        "project config, scheduler stopped",
        "project_config_ended.xml",
        &text,
        &want,
        &unescape_name(&want.name),
    );
}

/// The variants of the config reply the project settings do not reach:
/// every field of `ProjectConfig`.
#[test]
fn project_config_variants() {
    let h = Harness::new("web");
    let linux = || ConfigPlatform {
        name: "x86_64-pc-linux-gnu".into(),
        user_friendly_name: "Linux".into(),
        plan_class: "cuda118".into(),
    };
    let base = || ProjectConfig {
        name: "Variant".into(),
        master_url: MASTER.into(),
        web_rpc_url_base: "https://rpc.p.example/".into(),
        platforms: vec![linux()],
        ..Default::default()
    };
    let cases: Vec<(&str, ProjectConfig)> = vec![
        ("plain", base()),
        (
            "web_stopped",
            ProjectConfig {
                web_stopped: true,
                ..base()
            },
        ),
        (
            "sched_stopped",
            ProjectConfig {
                sched_stopped: true,
                ..base()
            },
        ),
        (
            "account_manager",
            ProjectConfig {
                account_manager: true,
                ..base()
            },
        ),
        (
            "invite_code",
            ProjectConfig {
                invite_code_required: true,
                ..base()
            },
        ),
        (
            "long_passwords",
            ProjectConfig {
                min_passwd_length: 12,
                ..base()
            },
        ),
        (
            "terms_of_use",
            ProjectConfig {
                terms_of_use: "Be kind to the machines.\nSecond line.\n".into(),
                ..base()
            },
        ),
        (
            "ownership_key",
            ProjectConfig {
                account_ownership_public_key: "MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQC".into(),
                min_client_version: Some(80300),
                platforms: vec![linux(), linux()],
                ..base()
            },
        ),
        (
            "no_platforms",
            ProjectConfig {
                platforms: Vec::new(),
                ..base()
            },
        ),
    ];
    for (name, want) in cases {
        check_config(
            &h,
            &format!("project config variant {name}"),
            &format!("project_config_{name}.xml"),
            &want.write(),
            &want,
            &want.name,
        );
    }
}

fn check_account(
    h: &Harness,
    kind: &str,
    case: &str,
    file: &str,
    text: &str,
    error: Option<(i32, &str)>,
    authenticator: &str,
) {
    let path = h.save(file, text);
    if !h.ready() {
        return;
    }
    let d = h.run(kind, &path, &[]);
    let mut e = Expect::new(case, &d);
    e.rc("parse.rc", 0);
    match error {
        Some((num, msg)) => {
            e.n("account.error_num", f64::from(num));
            e.s("account.error_msg", msg);
        }
        None => {
            e.n("account.error_num", 0.0);
            e.s("account.error_msg", "");
        }
    }
    e.s("account.authenticator", authenticator);
    e.finish();
}

#[test]
fn lookup_account_replies() {
    let h = Harness::new("web");
    let w = World::new(CONFIG);
    let hash = passwd_hash("s3cret!", "ada@example.com");
    let random = || w.rand();
    let auth = block_on(cb_core::web::make_user(
        &w.db,
        PEPPER,
        100,
        &random,
        &NewAccount {
            email_addr: "ada@example.com",
            passwd_hash: &hash,
            user_name: "Ada",
        },
    ))
    .unwrap()
    .unwrap();

    let ok = block_on(lookup_account(&w.db, PEPPER, "ada@example.com", &hash)).unwrap();
    check_account(
        &h,
        "lookup_account",
        "lookup_account, found",
        "lookup_account_ok.xml",
        &ok,
        None,
        &auth,
    );

    let wrong = passwd_hash("other", "ada@example.com");
    let bad = block_on(lookup_account(&w.db, PEPPER, "ada@example.com", &wrong)).unwrap();
    check_account(
        &h,
        "lookup_account",
        "lookup_account, wrong password",
        "lookup_account_bad_password.xml",
        &bad,
        Some((err::ERR_BAD_PASSWD, "Invalid password")),
        "",
    );

    let missing = block_on(lookup_account(&w.db, PEPPER, "nobody@example.com", &hash)).unwrap();
    check_account(
        &h,
        "lookup_account",
        "lookup_account, no such account",
        "lookup_account_not_found.xml",
        &missing,
        Some((err::ERR_DB_NOT_FOUND, "Not found")),
        "",
    );

    let invalid = block_on(lookup_account(&w.db, PEPPER, "not an address", &hash)).unwrap();
    check_account(
        &h,
        "lookup_account",
        "lookup_account, malformed address",
        "lookup_account_bad_address.xml",
        &invalid,
        Some((err::ERR_DB_NOT_FOUND, "Not found")),
        "",
    );

    // Without a password hash the server only says whether the account
    // exists; a client never asks that way, but it must read the reply.
    let exists = block_on(lookup_account(&w.db, PEPPER, "ada@example.com", "")).unwrap();
    check_account(
        &h,
        "lookup_account",
        "lookup_account, existence only",
        "lookup_account_exists.xml",
        &exists,
        None,
        "",
    );
}

#[test]
fn create_account_replies() {
    let h = Harness::new("web");
    let w = World::new(CONFIG);
    let config = Config::parse(CONFIG).unwrap();
    let random = || w.rand();
    let hash = passwd_hash("s3cret!", "new@example.com");
    let create = |config: &Config, email: &str, hash: &str, name: &str| {
        block_on(create_account(
            &w.db,
            config,
            PEPPER,
            200,
            &random,
            &NewAccount {
                email_addr: email,
                passwd_hash: hash,
                user_name: name,
            },
        ))
        .unwrap()
    };

    let created = create(&config, "new@example.com", &hash, "Newcomer");
    #[derive(serde::Deserialize)]
    struct Key {
        authenticator: String,
    }
    let auth = block_on(w.db.run(stmt!(
        "SELECT authenticator FROM user WHERE email_addr = 'new@example.com'"
    )))
    .unwrap()
    .first::<Key>()
    .unwrap()
    .unwrap()
    .authenticator;
    check_account(
        &h,
        "create_account",
        "create_account, new account",
        "create_account_ok.xml",
        &created,
        None,
        &auth,
    );

    // The same request again is a lookup and returns the same key.
    let again = create(&config, "new@example.com", &hash, "Newcomer");
    check_account(
        &h,
        "create_account",
        "create_account, repeated",
        "create_account_repeat.xml",
        &again,
        None,
        &auth,
    );

    let other = passwd_hash("different", "new@example.com");
    let taken = create(&config, "new@example.com", &other, "Newcomer");
    check_account(
        &h,
        "create_account",
        "create_account, address in use",
        "create_account_not_unique.xml",
        &taken,
        Some((
            AccountError::NotUnique.num(),
            AccountError::NotUnique.message(),
        )),
        "",
    );

    let bad_email = create(&config, "nonsense", &hash, "Newcomer");
    check_account(
        &h,
        "create_account",
        "create_account, malformed address",
        "create_account_bad_email.xml",
        &bad_email,
        Some((
            AccountError::BadEmailAddr.num(),
            AccountError::BadEmailAddr.message(),
        )),
        "",
    );

    let bad_name = create(&config, "x@example.com", &hash, "bad<name>");
    check_account(
        &h,
        "create_account",
        "create_account, bad user name",
        "create_account_bad_name.xml",
        &bad_name,
        Some((
            AccountError::BadUserName.num(),
            AccountError::BadUserName.message(),
        )),
        "",
    );

    let short_hash = create(&config, "y@example.com", "abc", "Newcomer");
    check_account(
        &h,
        "create_account",
        "create_account, short password hash",
        "create_account_bad_hash.xml",
        &short_hash,
        Some((
            AccountError::BadPasswdHashLength.num(),
            AccountError::BadPasswdHashLength.message(),
        )),
        "",
    );

    let closed =
        Config::parse(&CONFIG.replace("<account_creation>1", "<account_creation>0")).unwrap();
    let disabled = create(&closed, "z@example.com", &hash, "Newcomer");
    check_account(
        &h,
        "create_account",
        "create_account, creation disabled",
        "create_account_disabled.xml",
        &disabled,
        Some((
            AccountError::CreationDisabled.num(),
            AccountError::CreationDisabled.message(),
        )),
        "",
    );
}

/// Every refusal the account RPCs can send, as the client reads it.
#[test]
fn account_error_replies() {
    let h = Harness::new("web");
    let all = [
        ("not_found", AccountError::NotFound),
        ("bad_password", AccountError::BadPassword),
        ("creation_disabled", AccountError::CreationDisabled),
        ("creation_on_web_site", AccountError::CreationOnWebSite),
        ("bad_invite_code", AccountError::BadInviteCode),
        ("bad_user_name", AccountError::BadUserName),
        ("bad_email_addr", AccountError::BadEmailAddr),
        ("bad_passwd_hash_length", AccountError::BadPasswdHashLength),
        ("not_unique", AccountError::NotUnique),
        ("consent_required", AccountError::ConsentRequired),
        ("db_error", AccountError::DbError),
    ];
    for (name, e) in all {
        for kind in ["lookup_account", "create_account"] {
            check_account(
                &h,
                kind,
                &format!("{kind}, {name}"),
                &format!("{kind}_error_{name}.xml"),
                &e.reply(),
                Some((e.num(), e.message())),
                "",
            );
        }
    }
    // A bare error reply with a message of the server's own.
    check_account(
        &h,
        "lookup_account",
        "lookup_account, custom message",
        "lookup_account_error_custom.xml",
        &cb_proto::web_rpc::write_error(err::ERR_DB_NOT_FOUND, Some("No such user here")),
        Some((err::ERR_DB_NOT_FOUND, "No such user here")),
        "",
    );
}

/// What the client's upload code makes of one reply of the upload handler.
fn check_upload(h: &Harness, name: &str, text: &str, want: Upload<'_>) {
    let path = h.save(&format!("upload_{name}.xml"), text);
    if !h.ready() {
        return;
    }
    let d = h.run("upload_reply", &path, &[]);
    let mut e = Expect::new(&format!("upload reply {name}"), &d);
    match want {
        Upload::Done => {
            e.rc("parse.rc", 0);
            e.n("file_size", -1.0);
            e.quiet(&[]);
        }
        Upload::Size(n) => {
            e.rc("parse.rc", 0);
            e.n("file_size", n as f64);
            e.quiet(&[]);
        }
        Upload::Permanent(msg) => {
            e.rc("parse.rc", err::ERR_UPLOAD_PERMANENT);
            e.n("file_size", -1.0);
            e.logged(&format!("Error reported by file upload server: {msg}"));
        }
        Upload::Transient(msg) => {
            e.rc("parse.rc", err::ERR_UPLOAD_TRANSIENT);
            e.n("file_size", -1.0);
            e.logged(&format!("Error reported by file upload server: {msg}"));
        }
    }
    e.finish();
}

enum Upload<'a> {
    Done,
    Size(u64),
    Permanent(&'a str),
    Transient(&'a str),
}

#[test]
fn upload_handler_replies() {
    let h = Harness::new("upload");
    check_upload(&h, "success", &reply_success(), Upload::Done);
    check_upload(&h, "size_zero", &reply_file_size(0), Upload::Size(0));
    check_upload(&h, "size", &reply_file_size(123_456), Upload::Size(123_456));
    check_upload(
        &h,
        "size_large",
        &reply_file_size(9_007_199_254_740_992),
        Upload::Size(9_007_199_254_740_992),
    );

    // The refusals of cb_proto::upload and of the files Worker.
    let permanent = [
        "Missing name",
        "missing signature",
        "missing max_nbytes",
        "invalid signature",
        "nbytes missing or negative",
        "Bad filename",
        "Missing <data> tag",
        "invalid filename",
        "the request header is too long",
        "file result_0_r1_0 exists with another size",
        "file size (6000 KB) exceeds limit (4882 KB)",
        "file size (200000 KB) exceeds the project limit (97656 KB)",
        "file_upload_handler: invalid filename: ../etc/passwd",
    ];
    for (i, msg) in permanent.iter().enumerate() {
        let e = UploadError::permanent(*msg);
        check_upload(
            &h,
            &format!("permanent_{i}"),
            &e.reply(),
            Upload::Permanent(msg),
        );
    }
    let transient = [
        "no command",
        "the file could not be stored",
        "the project has no upload key",
        "partial uploads are not kept; send the file from the start",
        "the project is temporarily unavailable",
    ];
    for (i, msg) in transient.iter().enumerate() {
        check_upload(
            &h,
            &format!("transient_{i}"),
            &reply_error(true, msg),
            Upload::Transient(msg),
        );
    }
    // The errors the handler derives from a request that ends early.
    for (i, body) in [
        &b"<file_upload>\n<file_info>\n"[..],
        &b"<file_upload>\n<file_info>\n<name>x</name>\n"[..],
        &b"<data_server_request>\n"[..],
    ]
    .iter()
    .enumerate()
    {
        let e = incomplete_error(body);
        let want = if e.transient {
            Upload::Transient(&e.message)
        } else {
            Upload::Permanent(&e.message)
        };
        check_upload(&h, &format!("incomplete_{i}"), &e.reply(), want);
    }
    // `UploadHeader::invalid_signature` is the same text as above; keep the
    // type in use so a change of the text is noticed here.
    assert_eq!(
        UploadHeader::invalid_signature().message,
        "invalid signature"
    );
    let _ = ClientVersion::default();
}

#[test]
fn master_page_lists_the_scheduler() {
    let h = Harness::new("web");
    let w = World::new(CONFIG);
    let p = w.project();
    let text = master_page(&p);
    let path = h.save("master.html", &text);
    if !h.ready() {
        return;
    }
    let d = h.run("master", &path, &["--master-url", MASTER]);
    let mut e = Expect::new("master page", &d);
    e.n("scheduler_url.count", 1.0);
    e.s("scheduler_url[0]", p.scheduler_url());
    e.n("master_fetch_failures", 0.0);
    e.b("master_url_fetch_pending", false);
    e.quiet(&[]);
    e.finish();
}

#[test]
fn master_page_of_a_project_with_unusual_urls() {
    let h = Harness::new("web");
    let config = "<config>\n<long_name>Odd</long_name>\n<master_url>http://[2001:db8::1]:8080/boinc/</master_url>\n</config>\n";
    let w = World::new(config);
    let p = w.project();
    let path = h.save("master_odd.html", &master_page(&p));
    if !h.ready() {
        return;
    }
    let d = h.run(
        "master",
        &path,
        &["--master-url", "http://[2001:db8::1]:8080/boinc/"],
    );
    let mut e = Expect::new("master page with an address URL", &d);
    e.n("scheduler_url.count", 1.0);
    e.s("scheduler_url[0]", p.scheduler_url());
    e.quiet(&[]);
    e.finish();
}
