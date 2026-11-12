//! Settings, the admin operations and the account RPCs against SQLite, ending
//! with a client getting work from a version registered through them.

use std::cell::Cell;
use std::sync::OnceLock;

use cb_core::admin::{
    add_app_version, job_status, list_catalog, put_app, put_platform, set_app_version_deprecated,
    NewApp, NewAppVersion, NewPlatform, PutAction, VersionFile,
};
use cb_core::model::User;
use cb_core::project::{
    get_setting, put_setting, Project, SETTING_CODE_SIGN_KEY, SETTING_CONFIG, SETTING_UPLOAD_KEY,
};
use cb_core::scheduler::{handle_request, SchedCtx};
use cb_core::web::{
    create_account, lookup_account, master_page, project_config, server_status, server_status_xml,
    NewAccount, STATUS_COUNTS_TTL,
};
use cb_core::work::{create_work, register_job_file, JobSpec, Submission};
use cb_core::CoreError;
use cb_crypto::account::passwd_hash;
use cb_crypto::{md5_hex, PrivateKey, PublicKey, Signer};
use cb_db::{query_one, stmt, Db};
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;

const LINUX: &str = "x86_64-pc-linux-gnu";
const PEPPER: &[u8] = b"pepper for tests";
const CONFIG: &str = "<config>\n<long_name>Test Project</long_name>\n<master_url>https://p.example/</master_url>\n<download_url>https://p.example/download</download_url>\n<upload_url>https://p.example/upload/file_upload_handler</upload_url>\n<daily_result_quota>100</daily_result_quota>\n<account_creation>1</account_creation>\n</config>\n";
const INPUT_TEMPLATE: &str = "<input_template>\n<workunit>\n<rsc_fpops_est>1e12</rsc_fpops_est>\n<delay_bound>3600</delay_bound>\n</workunit>\n</input_template>\n";
const RESULT_TEMPLATE: &str = "<output_template>\n<file_info>\n    <name><OUTFILE_0/></name>\n    <generated_locally/>\n    <upload_when_present/>\n    <max_nbytes>5000000</max_nbytes>\n    <url><UPLOAD_URL/></url>\n</file_info>\n<result>\n    <file_ref>\n        <file_name><OUTFILE_0/></file_name>\n        <open_name>out</open_name>\n    </file_ref>\n</result>\n</output_template>\n";

/// One key pair for all tests; generating one takes a while.
fn keys() -> &'static (PrivateKey, PublicKey) {
    static KEYS: OnceLock<(PrivateKey, PublicKey)> = OnceLock::new();
    KEYS.get_or_init(|| cb_crypto::generate_key_pair().unwrap())
}

struct World {
    db: SqliteDb,
    seq: Cell<u32>,
}

impl World {
    fn new() -> World {
        let w = World {
            db: SqliteDb::with_schema().unwrap(),
            seq: Cell::new(0),
        };
        block_on(put_setting(&w.db, SETTING_CONFIG, CONFIG, 10)).unwrap();
        w
    }

    fn rand(&self) -> u32 {
        self.seq.set(self.seq.get().wrapping_add(2_654_435_761));
        self.seq.get()
    }

    fn project(&self) -> Project {
        block_on(Project::load(&self.db)).unwrap()
    }

    fn create(&self, email: &str, password: &str, name: &str) -> String {
        let hash = passwd_hash(password, &email.to_lowercase());
        let random = || self.rand();
        block_on(create_account(
            &self.db,
            &self.project().config,
            PEPPER,
            100,
            &random,
            &NewAccount {
                email_addr: email,
                passwd_hash: &hash,
                user_name: name,
                ..Default::default()
            },
        ))
        .unwrap()
    }

    fn lookup(&self, email: &str, password: Option<&str>) -> String {
        let hash = password
            .map(|p| passwd_hash(p, &email.to_lowercase()))
            .unwrap_or_default();
        block_on(lookup_account(&self.db, PEPPER, email, &hash)).unwrap()
    }
}

fn element<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    Some(&xml[start..end])
}

#[test]
fn settings_are_checked_and_loaded() {
    let w = World::new();
    assert!(matches!(
        block_on(put_setting(&w.db, "nope", "x", 1)),
        Err(CoreError::NotFound(_))
    ));
    assert!(block_on(put_setting(
        &w.db,
        SETTING_CONFIG,
        "<config><master_url>ftp://x/</master_url></config>",
        1
    ))
    .is_err());
    assert!(block_on(put_setting(&w.db, SETTING_UPLOAD_KEY, "not a key", 1)).is_err());
    let public = keys().1.to_key_string();
    block_on(put_setting(&w.db, SETTING_UPLOAD_KEY, &public, 2)).unwrap();
    block_on(put_setting(&w.db, SETTING_CODE_SIGN_KEY, &public, 2)).unwrap();
    assert_eq!(
        block_on(get_setting(&w.db, SETTING_CONFIG))
            .unwrap()
            .as_deref(),
        Some(CONFIG)
    );
    let p = w.project();
    assert_eq!(p.config.long_name, "Test Project");
    assert_eq!(p.scheduler_url(), "https://p.example/sched");
    assert!(p.upload_public_key.is_some());
    assert_eq!(p.code_sign_key, public);
}

#[test]
fn accounts_are_created_and_looked_up() {
    let w = World::new();
    let reply = w.create("Ada@Example.com", "secret", "Ada");
    let auth = element(&reply, "authenticator").unwrap().to_string();
    assert_eq!(auth.len(), 32);

    // The stored hash is not what the client sent.
    let user: User = block_on(query_one(
        &w.db,
        stmt!("SELECT * FROM user WHERE email_addr = 'ada@example.com'"),
    ))
    .unwrap()
    .unwrap();
    let stored = user.passwd_hash;
    assert_ne!(stored, passwd_hash("secret", "ada@example.com"));
    assert!(!stored.is_empty());

    assert_eq!(
        element(
            &w.lookup("ada@example.com", Some("secret")),
            "authenticator"
        ),
        Some(auth.as_str())
    );
    assert!(w.lookup("ada@example.com", None).contains("<success/>"));
    assert!(w
        .lookup("ada@example.com", Some("wrong"))
        .contains("<error_num>-206</error_num>"));
    assert!(w
        .lookup("nobody@example.com", Some("secret"))
        .contains("<error_num>-136</error_num>"));

    // The same request again is a lookup; another password is refused.
    let again = w.create("ada@example.com", "secret", "Ada");
    assert_eq!(element(&again, "authenticator"), Some(auth.as_str()));
    assert!(w
        .create("ada@example.com", "other", "Ada")
        .contains("<error_num>-137</error_num>"));
    assert!(w
        .create("not an address", "secret", "x")
        .contains("<error_num>-205</error_num>"));
    assert!(w
        .create("b@example.com", "secret", "<b>")
        .contains("<error_num>-188</error_num>"));

    block_on(put_setting(
        &w.db,
        SETTING_CONFIG,
        "<config><master_url>https://p.example/</master_url></config>",
        20,
    ))
    .unwrap();
    assert!(w
        .create("c@example.com", "secret", "c")
        .contains("not accepting new accounts"));
}

#[test]
fn a_registered_version_is_sent_to_a_client() {
    let w = World::new();
    let (private, public) = keys();
    let signer = Signer::new(private).unwrap();
    block_on(put_setting(
        &w.db,
        SETTING_CODE_SIGN_KEY,
        &public.to_key_string(),
        1,
    ))
    .unwrap();

    let platform = NewPlatform {
        name: LINUX.into(),
        user_friendly_name: "Linux x86-64 <glibc & musl>".into(),
        ..Default::default()
    };
    let pid = block_on(put_platform(&w.db, 1, &platform)).unwrap().id;
    assert_eq!(block_on(put_platform(&w.db, 2, &platform)).unwrap().id, pid);
    let app = NewApp {
        name: "worker".into(),
        user_friendly_name: "Worker".into(),
        ..Default::default()
    };
    let aid = block_on(put_app(&w.db, 1, &app)).unwrap();
    assert_eq!(block_on(put_app(&w.db, 2, &app)).unwrap(), aid);
    assert!(block_on(put_app(
        &w.db,
        2,
        &NewApp {
            name: "w2".into(),
            validator: "webhook".into(),
            ..Default::default()
        }
    ))
    .is_err());

    let body = b"#!/bin/sh\necho hi > out\n";
    let md5 = md5_hex(body);
    let file = "worker_1.02_x86_64-pc-linux-gnu";
    let mut version = NewAppVersion {
        app: "worker".into(),
        platform: LINUX.into(),
        version_num: 102,
        files: vec![VersionFile {
            name: file.into(),
            signature: signer.sign_md5(&md5).unwrap(),
            main_program: true,
            ..Default::default()
        }],
        ..Default::default()
    };
    let project = w.project();
    let key = PublicKey::parse(&project.code_sign_key).unwrap();

    // Not staged yet.
    assert!(matches!(
        block_on(add_app_version(
            &w.db,
            &project.config,
            Some(&key),
            5,
            &version
        )),
        Err(CoreError::NotFound(_))
    ));
    block_on(register_job_file(
        &w.db,
        file,
        &md5,
        body.len() as u64,
        false,
        4,
    ))
    .unwrap();

    // A signature over other content is refused.
    let good = version.files[0].signature.clone();
    version.files[0].signature = signer.sign_md5(&md5_hex(b"other")).unwrap();
    assert!(matches!(
        block_on(add_app_version(
            &w.db,
            &project.config,
            Some(&key),
            5,
            &version
        )),
        Err(CoreError::Invalid(_))
    ));
    version.files[0].signature = good;
    let avid = block_on(add_app_version(
        &w.db,
        &project.config,
        Some(&key),
        5,
        &version,
    ))
    .unwrap();
    assert!(avid > 0);
    assert!(matches!(
        block_on(add_app_version(
            &w.db,
            &project.config,
            Some(&key),
            6,
            &version
        )),
        Err(CoreError::Conflict(_))
    ));
    let kept = block_on(w.db.run(stmt!(
        "SELECT no_delete AS n FROM job_file WHERE name = ?",
        file
    )))
    .unwrap()
    .scalar_i64();
    assert_eq!(kept, Some(1));

    let project = w.project();
    let config_xml = project_config(&project);
    assert!(config_xml.contains("<name>Test Project</name>"));
    assert!(config_xml.contains(LINUX));
    assert!(config_xml.contains("Linux x86-64 &lt;glibc &amp; musl>"));
    let page = master_page(&project);
    assert!(page.contains("<link rel=\"boinc_scheduler\" href=\"https://p.example/sched\">"));
    assert!(page.contains("<scheduler>https://p.example/sched</scheduler>"));
    assert!(page.contains("1.02"));

    // A job, an account, and a client asking for work.
    let reply = w.create("ada@example.com", "secret", "Ada");
    let auth = element(&reply, "authenticator").unwrap().to_string();
    let random = || w.rand();
    let jobs = block_on(create_work(
        &w.db,
        &project.config,
        Some(&signer),
        200,
        &random,
        &Submission {
            app: "worker".into(),
            input_template: INPUT_TEMPLATE.into(),
            result_template: RESULT_TEMPLATE.into(),
            jobs: vec![JobSpec {
                name: "job1".into(),
                min_quorum: Some(1),
                target_nresults: Some(1),
                ..Default::default()
            }],
        },
    ))
    .unwrap();
    assert_eq!(jobs[0].error, None);

    let request = format!(
        "<scheduler_request>\n    <authenticator>{auth}</authenticator>\n    <hostid>0</hostid>\n    <rpc_seqno>1</rpc_seqno>\n    <core_client_major_version>8</core_client_major_version>\n    <core_client_minor_version>0</core_client_minor_version>\n    <core_client_release>4</core_client_release>\n    <work_req_seconds>1000</work_req_seconds>\n    <cpu_req_secs>1000</cpu_req_secs>\n    <cpu_req_instances>1</cpu_req_instances>\n    <estimated_delay>0</estimated_delay>\n    <platform_name>{LINUX}</platform_name>\n<host_info>\n    <domain_name>box</domain_name>\n    <host_cpid>c0ffee</host_cpid>\n    <p_ncpus>4</p_ncpus>\n    <p_vendor>GenuineIntel</p_vendor>\n    <p_model>cpu</p_model>\n    <p_fpops>2e9</p_fpops>\n    <m_nbytes>8e9</m_nbytes>\n    <d_total>1e11</d_total>\n    <d_free>5e10</d_free>\n    <os_name>Linux</os_name>\n    <os_version>6.1</os_version>\n</host_info>\n</scheduler_request>\n"
    );
    let ctx = SchedCtx {
        db: &w.db,
        config: &project.config,
        plan_classes: &project.plan_classes,
        catalog: &project.catalog,
        code_sign_key: &project.code_sign_key,
        now: 300,
        random: &random,
        client_ip: "198.51.100.9",
    };
    let out = block_on(handle_request(&ctx, &request)).unwrap();
    assert_eq!(out.n_sent, 1);
    let xml = out.reply.write().unwrap();
    assert!(xml.contains(&format!("<url>https://p.example/download/{file}</url>")));
    assert!(xml.contains("<executable/>"));
    assert!(xml.contains("<main_program/>"));
    assert!(xml.contains("<code_sign_key>"));

    let status = block_on(job_status(&w.db, "job1")).unwrap().unwrap();
    assert_eq!(status.results.len(), 1);
    assert_eq!(status.results[0].hostid, out.host_id);
    assert_eq!(status.results[0].app_version_id, avid);
    assert!(block_on(job_status(&w.db, "nope")).unwrap().is_none());

    let s = block_on(server_status(&w.db, 300)).unwrap();
    assert_eq!(s.results_in_progress, 1);
    assert_eq!(s.results_ready_to_send, 0);
    assert_eq!(s.users, 1);
    assert_eq!(s.hosts, 1);
    assert_eq!(s.users_with_recent_credit, 0);
    assert_eq!(s.hosts_with_recent_credit, 0);
    assert!(server_status_xml(&project, &s, 300)
        .contains("<results_in_progress>1</results_in_progress>"));

    assert!(block_on(set_app_version_deprecated(&w.db, avid, true)).unwrap());
    assert!(w.project().catalog.app_versions.is_empty());
    assert_eq!(block_on(list_catalog(&w.db)).unwrap().app_versions.len(), 1);
}

#[test]
fn status_counts_come_from_a_snapshot_until_it_expires() {
    let db = SqliteDb::with_schema().unwrap();
    let first = block_on(server_status(&db, 1000)).unwrap();
    assert_eq!((first.users, first.hosts), (0, 0));

    block_on(db.run(stmt!(
        "INSERT INTO host (create_time, userid, expavg_credit) VALUES (1, 0, 5)"
    )))
    .unwrap();

    // Within the TTL: the stored counts are used, and the aggregates do not run.
    let before = db.statements();
    let cached = block_on(server_status(&db, 1000 + STATUS_COUNTS_TTL - 1)).unwrap();
    assert_eq!((cached.hosts, cached.hosts_with_recent_credit), (0, 0));
    assert_eq!(db.statements() - before, 5);

    // Past the TTL: counts are recomputed and stored again.
    let fresh = block_on(server_status(&db, 1000 + STATUS_COUNTS_TTL)).unwrap();
    assert_eq!((fresh.hosts, fresh.hosts_with_recent_credit), (1, 1));
    let again = block_on(server_status(&db, 1000 + STATUS_COUNTS_TTL + 1)).unwrap();
    assert_eq!(again.hosts, 1);

    // The snapshot row does not disturb project settings.
    block_on(Project::load(&db)).unwrap();
}

#[test]
fn assigned_jobs_are_submitted_with_their_assignment() {
    let w = World::new();
    block_on(w.db.run(stmt!(
        "INSERT INTO app (create_time, name, user_friendly_name) VALUES (0, 'worker', 'Worker')"
    )))
    .unwrap();
    let signer = Signer::new(&keys().0).unwrap();
    // As the admin API receives it.
    let sub: Submission = serde_json::from_value(serde_json::json!({
        "app": "worker",
        "input_template": INPUT_TEMPLATE,
        "result_template": RESULT_TEMPLATE,
        "jobs": [
            {"name": "t1", "min_quorum": 1, "target_nresults": 1,
             "assign": {"target": "host", "id": 5}},
            {"name": "t2", "min_quorum": 2, "target_nresults": 2,
             "assign": {"target": "user", "id": 3}},
            {"name": "t3", "assign": {"target": "user", "id": 0}},
            {"name": "t4", "min_quorum": 2, "target_nresults": 2,
             "assign": {"target": "host", "id": 5}},
        ],
    }))
    .unwrap();
    let submit = |config: &cb_core::Config| {
        let random = || w.rand();
        block_on(create_work(
            &w.db,
            config,
            Some(&signer),
            300,
            &random,
            &sub,
        ))
        .unwrap()
    };
    let count = |sql: &str| {
        block_on(w.db.run(cb_db::Stmt::new(sql))).unwrap().rows[0]["n"]
            .as_i64()
            .unwrap()
    };

    // Refused while the project does not enable assignment.
    let jobs = submit(&w.project().config);
    for j in &jobs {
        assert_eq!(j.workunit_id, 0);
        assert!(j.error.as_deref().unwrap().contains("enable_assignment"));
    }
    assert_eq!(count("SELECT count(*) AS n FROM workunit"), 0);

    let config = CONFIG.replace(
        "</config>",
        "<enable_assignment>1</enable_assignment>\n</config>",
    );
    block_on(put_setting(&w.db, SETTING_CONFIG, &config, 11)).unwrap();
    let jobs = submit(&w.project().config);
    assert_eq!(jobs[0].error, None);
    assert_eq!(jobs[1].error, None);
    assert!(jobs[2].error.as_deref().unwrap().contains("nonzero"));
    assert!(jobs[3]
        .error
        .as_deref()
        .unwrap()
        .contains("target_nresults 1"));

    let rows = block_on(w.db.run(stmt!(
        "SELECT target_type, target_id, multi, workunitid FROM assignment ORDER BY id"
    )))
    .unwrap()
    .rows;
    let got: Vec<(i64, i64, i64, i64)> = rows
        .iter()
        .map(|r| {
            (
                r["target_type"].as_i64().unwrap(),
                r["target_id"].as_i64().unwrap(),
                r["multi"].as_i64().unwrap(),
                r["workunitid"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (1, 5, 0, jobs[0].workunit_id),
            (2, 3, 0, jobs[1].workunit_id)
        ]
    );
    assert_eq!(
        count("SELECT count(*) AS n FROM workunit WHERE transitioner_flags = 2"),
        2
    );
    assert_eq!(
        count("SELECT count(*) AS n FROM result WHERE server_state = 1"),
        3
    );
    assert_eq!(count("SELECT count(*) AS n FROM result"), 3);
}

/// What `tools/xadd` does for a `<platform>` of `project.xml`, and where
/// `put_platform` differs on purpose.
#[test]
fn put_platform_follows_xadd() {
    let w = World::new();
    let put = |now: i64, name: &str, friendly: &str, deprecated: Option<bool>| {
        block_on(put_platform(
            &w.db,
            now,
            &NewPlatform {
                name: name.into(),
                user_friendly_name: friendly.into(),
                deprecated,
            },
        ))
        .unwrap()
    };
    let row = |name: &str| -> (i64, String, i64, i64) {
        let r = block_on(w.db.run(stmt!(
            "SELECT id, user_friendly_name, deprecated, create_time FROM platform WHERE name = ?",
            name
        )))
        .unwrap()
        .rows
        .remove(0);
        (
            r["id"].as_i64().unwrap(),
            r["user_friendly_name"].as_str().unwrap().to_string(),
            r["deprecated"].as_i64().unwrap(),
            r["create_time"].as_i64().unwrap(),
        )
    };

    // A new platform is added with its user-friendly name and the time.
    let added = put(100, "windows_x86_64", "Windows on x86_64", None);
    assert_eq!(added.action, PutAction::Added);
    assert_eq!(
        row("windows_x86_64"),
        (added.id, "Windows on x86_64".into(), 0, 100)
    );

    // The same entry again is "already in DB": nothing changes, not even
    // the creation time.
    let again = put(200, "windows_x86_64", "Windows on x86_64", None);
    assert_eq!((again.id, again.action), (added.id, PutAction::Unchanged));
    assert_eq!(row("windows_x86_64").3, 100);

    // A different user-friendly name updates it and nothing else.
    let renamed = put(300, "windows_x86_64", "Microsoft Windows, 64 bit", None);
    assert_eq!((renamed.id, renamed.action), (added.id, PutAction::Updated));
    assert_eq!(
        row("windows_x86_64"),
        (added.id, "Microsoft Windows, 64 bit".into(), 0, 100)
    );

    // Differences from xadd. A platform an operator deprecated stays
    // deprecated when the entry is put again without saying so, and keeps
    // its user-friendly name when the entry has none; xadd does not touch
    // `deprecated` either, and would blank the name.
    let dep = put(400, "windows_x86_64", "", Some(true));
    assert_eq!((dep.id, dep.action), (added.id, PutAction::Updated));
    assert_eq!(
        row("windows_x86_64"),
        (added.id, "Microsoft Windows, 64 bit".into(), 1, 100)
    );
    let kept = put(500, "windows_x86_64", "", None);
    assert_eq!(kept.action, PutAction::Unchanged);
    let kept = put(500, "windows_x86_64", "Microsoft Windows, 64 bit", None);
    assert_eq!(kept.action, PutAction::Unchanged);
    assert_eq!(
        row("windows_x86_64"),
        (added.id, "Microsoft Windows, 64 bit".into(), 1, 100)
    );
    // A request that says so brings it back.
    let back = put(600, "windows_x86_64", "", Some(false));
    assert_eq!((back.id, back.action), (added.id, PutAction::Updated));
    assert_eq!(row("windows_x86_64").2, 0);

    // A new platform with no user-friendly name is called by its name
    // (xadd stores an empty string), and can be added deprecated.
    let bare = put(700, "arm-unknown-linux-gnueabihf", "", Some(true));
    assert_eq!(bare.action, PutAction::Added);
    assert_eq!(
        row("arm-unknown-linux-gnueabihf"),
        (bare.id, "arm-unknown-linux-gnueabihf".into(), 1, 700)
    );
    assert_ne!(bare.id, added.id);

    // A name is matched exactly (MySQL's collation makes xadd's lookup
    // ignore case), so a name in another case is another platform; never two
    // rows for one name.
    let upper = put(800, "Windows_x86_64", "Windows, capitalised", None);
    assert_eq!(upper.action, PutAction::Added);
    assert_ne!(upper.id, added.id);
    let count: i64 = block_on(w.db.run(stmt!(
        "SELECT count(*) AS n FROM platform WHERE lower(name) = 'windows_x86_64'"
    )))
    .unwrap()
    .scalar_i64()
    .unwrap();
    assert_eq!(count, 2);

    // Names that cannot be platform names are refused (xadd would put them
    // into SQL as they are).
    for bad in [
        "",
        "x'; DROP TABLE platform; --",
        "a b",
        "a/b",
        &"x".repeat(255),
    ] {
        assert!(
            block_on(put_platform(
                &w.db,
                900,
                &NewPlatform {
                    name: bad.into(),
                    ..Default::default()
                }
            ))
            .is_err(),
            "{bad:?}"
        );
    }
    // The names in upstream's `tools/project.xml` are fine, the dotted
    // Solaris one included.
    for ok in [
        "windows_intelx86",
        "x86_64-pc-linux-gnu",
        "sparc-sun-solaris2.7",
        "anonymous",
    ] {
        assert_ne!(put(1000, ok, ok, None).id, 0, "{ok}");
    }
}
