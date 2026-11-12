//! The stock server URL layout as a setting: what is stored, what the master
//! page and a scheduler reply advertise with it on and off, and that the
//! stock-style XML of imported work is read as before.

use std::cell::Cell;
use std::sync::OnceLock;

use cb_core::admin::{
    add_app_version, put_app, put_platform, NewApp, NewAppVersion, NewPlatform, VersionFile,
};
use cb_core::project::{put_setting, Project, SETTING_CODE_SIGN_KEY, SETTING_CONFIG};
use cb_core::scheduler::{handle_request, SchedCtx};
use cb_core::stock::filename_dir;
use cb_core::template::{file_infos, output_files, InputFile};
use cb_core::web::{create_account, master_page, project_config, NewAccount};
use cb_core::work::{create_work, register_job_file, JobSpec, Submission};
use cb_crypto::account::passwd_hash;
use cb_crypto::{md5_hex, PrivateKey, PublicKey, Signer};
use cb_db::{stmt, Db};
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;

const LINUX: &str = "x86_64-pc-linux-gnu";
const PEPPER: &[u8] = b"pepper for tests";
const INPUT_TEMPLATE: &str = "<input_template>\n<file_info>\n<number>0</number>\n</file_info>\n<workunit>\n<file_ref>\n<file_number>0</file_number>\n<open_name>in</open_name>\n</file_ref>\n<rsc_fpops_est>1e12</rsc_fpops_est>\n<delay_bound>3600</delay_bound>\n</workunit>\n</input_template>\n";
const RESULT_TEMPLATE: &str = "<output_template>\n<file_info>\n    <name><OUTFILE_0/></name>\n    <generated_locally/>\n    <upload_when_present/>\n    <max_nbytes>5000000</max_nbytes>\n    <url><UPLOAD_URL/></url>\n</file_info>\n<result>\n    <file_ref>\n        <file_name><OUTFILE_0/></file_name>\n        <open_name>out</open_name>\n    </file_ref>\n</result>\n</output_template>\n";

fn keys() -> &'static (PrivateKey, PublicKey) {
    static KEYS: OnceLock<(PrivateKey, PublicKey)> = OnceLock::new();
    KEYS.get_or_init(|| cb_crypto::generate_key_pair().unwrap())
}

fn config(extra: &str) -> String {
    config_at("https://p.example/", extra)
}

fn config_at(master: &str, extra: &str) -> String {
    format!(
        "<config>\n<long_name>Test Project</long_name>\n<master_url>{master}</master_url>\n<account_creation>1</account_creation>\n{extra}</config>\n"
    )
}

const STOCK: &str =
    "<stock_short_name>example</stock_short_name>\n<uldl_dir_fanout>1024</uldl_dir_fanout>\n";

#[test]
fn stock_settings_are_checked_when_stored() {
    let db = SqliteDb::with_schema().unwrap();
    for bad in [
        "<stock_short_name>a b</stock_short_name>",
        "<stock_cgi_prefix>/x_cgi</stock_cgi_prefix>",
        "<stock_short_name>x</stock_short_name><stock_page_prefix>/admin</stock_page_prefix>",
    ] {
        assert!(
            block_on(put_setting(&db, SETTING_CONFIG, &config(bad), 1)).is_err(),
            "{bad}"
        );
    }
    block_on(put_setting(&db, SETTING_CONFIG, &config(STOCK), 2)).unwrap();
    let p = block_on(Project::load(&db)).unwrap();
    let stock = p.config.stock.as_ref().unwrap();
    assert_eq!(stock.scheduler_path(), "/example_cgi/cgi");
    assert_eq!(p.scheduler_url(), "https://p.example/example_cgi/cgi");
}

/// Sets up a project with one app version, one job with an input file, and
/// returns what a client asking for work gets, with the stored documents.
struct Run {
    reply: String,
    app_version_doc: String,
    wu_doc: String,
    result_doc: String,
    master_page: String,
    project_config: String,
}

fn run(extra: &str) -> Run {
    run_with(&config(extra))
}

fn run_with(config_xml: &str) -> Run {
    let db = SqliteDb::with_schema().unwrap();
    let seq = Cell::new(0u32);
    let rand = || {
        seq.set(seq.get().wrapping_add(2_654_435_761));
        seq.get()
    };
    let (private, public) = keys();
    let signer = Signer::new(private).unwrap();
    block_on(put_setting(&db, SETTING_CONFIG, config_xml, 1)).unwrap();
    block_on(put_setting(
        &db,
        SETTING_CODE_SIGN_KEY,
        &public.to_key_string(),
        1,
    ))
    .unwrap();
    block_on(put_platform(
        &db,
        1,
        &NewPlatform {
            name: LINUX.into(),
            user_friendly_name: "Linux".into(),
            ..Default::default()
        },
    ))
    .unwrap();
    block_on(put_app(
        &db,
        1,
        &NewApp {
            name: "worker".into(),
            user_friendly_name: "Worker".into(),
            ..Default::default()
        },
    ))
    .unwrap();
    let body = b"#!/bin/sh\n";
    let md5 = md5_hex(body);
    let file = "worker_1.02_x86_64-pc-linux-gnu";
    block_on(register_job_file(
        &db,
        file,
        &md5,
        body.len() as u64,
        false,
        2,
    ))
    .unwrap();
    let project = block_on(Project::load(&db)).unwrap();
    let key = PublicKey::parse(&project.code_sign_key).unwrap();
    block_on(add_app_version(
        &db,
        &project.config,
        Some(&key),
        3,
        &NewAppVersion {
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
        },
    ))
    .unwrap();
    block_on(register_job_file(&db, "in_1", &md5_hex(b"x"), 1, false, 4)).unwrap();
    let project = block_on(Project::load(&db)).unwrap();
    let jobs = block_on(create_work(
        &db,
        &project.config,
        Some(&signer),
        200,
        &rand,
        &Submission {
            app: "worker".into(),
            input_template: INPUT_TEMPLATE.into(),
            result_template: RESULT_TEMPLATE.into(),
            jobs: vec![JobSpec {
                name: "job1".into(),
                input_files: vec![InputFile {
                    name: "in_1".into(),
                    ..Default::default()
                }],
                min_quorum: Some(1),
                target_nresults: Some(1),
                ..Default::default()
            }],
        },
    ))
    .unwrap();
    assert_eq!(jobs[0].error, None);

    let hash = passwd_hash("secret", "ada@example.com");
    let created = block_on(create_account(
        &db,
        &project.config,
        PEPPER,
        100,
        &rand,
        &NewAccount {
            email_addr: "ada@example.com",
            passwd_hash: &hash,
            user_name: "Ada",
            ..Default::default()
        },
    ))
    .unwrap();
    let auth = created
        .split("<authenticator>")
        .nth(1)
        .and_then(|r| r.split("</authenticator>").next())
        .unwrap()
        .to_string();
    let request = format!(
        "<scheduler_request>\n    <authenticator>{auth}</authenticator>\n    <hostid>0</hostid>\n    <rpc_seqno>1</rpc_seqno>\n    <core_client_major_version>8</core_client_major_version>\n    <core_client_minor_version>0</core_client_minor_version>\n    <core_client_release>4</core_client_release>\n    <work_req_seconds>1000</work_req_seconds>\n    <cpu_req_secs>1000</cpu_req_secs>\n    <cpu_req_instances>1</cpu_req_instances>\n    <estimated_delay>0</estimated_delay>\n    <platform_name>{LINUX}</platform_name>\n<host_info>\n    <domain_name>box</domain_name>\n    <host_cpid>c0ffee</host_cpid>\n    <p_ncpus>4</p_ncpus>\n    <p_vendor>GenuineIntel</p_vendor>\n    <p_model>cpu</p_model>\n    <p_fpops>2e9</p_fpops>\n    <m_nbytes>8e9</m_nbytes>\n    <d_total>1e11</d_total>\n    <d_free>5e10</d_free>\n    <os_name>Linux</os_name>\n    <os_version>6.1</os_version>\n</host_info>\n</scheduler_request>\n"
    );
    let ctx = SchedCtx {
        db: &db,
        config: &project.config,
        plan_classes: &project.plan_classes,
        catalog: &project.catalog,
        code_sign_key: &project.code_sign_key,
        now: 300,
        random: &rand,
        client_ip: "198.51.100.9",
    };
    let out = block_on(handle_request(&ctx, &request)).unwrap();
    assert_eq!(out.n_sent, 1);
    let doc = |sql: &str| {
        block_on(db.run(cb_db::Stmt::new(sql)))
            .unwrap()
            .rows
            .first()
            .and_then(|r| r.values().next())
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap()
    };
    Run {
        reply: out.reply.write().unwrap(),
        app_version_doc: doc("SELECT xml_doc FROM app_version"),
        wu_doc: doc("SELECT xml_doc FROM workunit"),
        result_doc: doc("SELECT xml_doc_in FROM result"),
        master_page: master_page(&project),
        project_config: project_config(&project),
    }
}

fn dir(name: &str) -> String {
    filename_dir(name, 1024).unwrap()
}

#[test]
fn a_stock_layout_is_advertised_by_default() {
    let r = run(STOCK);
    let app_file = "worker_1.02_x86_64-pc-linux-gnu";
    let app_url = format!(
        "https://p.example/example/download/{}/{app_file}",
        dir(app_file)
    );
    let input_url = format!("https://p.example/example/download/{}/in_1", dir("in_1"));
    let upload_url = "https://p.example/example_cgi/file_upload_handler";

    // The master page links the scheduler clients already hold.
    assert!(r
        .master_page
        .contains("<link rel=\"boinc_scheduler\" href=\"https://p.example/example_cgi/cgi\">"));
    assert!(r
        .master_page
        .contains("<scheduler>https://p.example/example_cgi/cgi</scheduler>"));
    // The master URL itself does not change.
    assert!(r
        .project_config
        .contains("<master_url>https://p.example/</master_url>"));

    // New work names the stock URLs, with the hash directory for downloads.
    assert!(r.app_version_doc.contains(&format!("<url>{app_url}</url>")));
    assert!(r.wu_doc.contains(&format!("<url>{input_url}</url>")));
    assert!(r.result_doc.contains(&format!("<url>{upload_url}</url>")));
    assert!(r.reply.contains(&format!("<url>{app_url}</url>")));
    assert!(r.reply.contains(&format!("<url>{input_url}</url>")));
    assert!(r.reply.contains(&format!("<url>{upload_url}</url>")));

    // The upload handler URL still marks the output as one to upload.
    let outs = output_files(&r.result_doc);
    assert_eq!(outs.len(), 1);
    assert!(outs[0].uploadable);
    assert_eq!(file_infos(&r.wu_doc).len(), 1);
}

#[test]
fn a_stock_layout_can_be_served_without_advertising_it() {
    let r = run(&format!("{STOCK}<stock_advertise>0</stock_advertise>\n"));
    assert!(r
        .master_page
        .contains("<link rel=\"boinc_scheduler\" href=\"https://p.example/sched\">"));
    assert!(r
        .app_version_doc
        .contains("<url>https://p.example/download/worker_1.02_x86_64-pc-linux-gnu</url>"));
    assert!(r
        .wu_doc
        .contains("<url>https://p.example/download/in_1</url>"));
    assert!(r
        .result_doc
        .contains("<url>https://p.example/upload/file_upload_handler</url>"));
}

/// The master URL of a migrated project is under its short name, but the
/// native paths are served at the host root, so a stock layout whose stock
/// URLs are not advertised advertises root URLs.
#[test]
fn native_urls_are_at_the_root_when_the_master_url_has_a_path() {
    let r = run_with(&config_at(
        "https://p.example/example/",
        &format!("{STOCK}<stock_advertise>0</stock_advertise>\n"),
    ));
    assert!(r
        .master_page
        .contains("<link rel=\"boinc_scheduler\" href=\"https://p.example/sched\">"));
    assert!(r
        .wu_doc
        .contains("<url>https://p.example/download/in_1</url>"));
    assert!(r
        .result_doc
        .contains("<url>https://p.example/upload/file_upload_handler</url>"));
    assert!(r
        .reply
        .contains("<url>https://p.example/upload/file_upload_handler</url>"));
    // The master URL is what clients attach to.
    assert!(r
        .project_config
        .contains("<master_url>https://p.example/example/</master_url>"));
}

#[test]
fn without_a_stock_setting_nothing_changes() {
    // Fanout alone is an upstream tag a copied config may hold; it does
    // nothing without a stock layout.
    let r = run("<uldl_dir_fanout>1024</uldl_dir_fanout>\n");
    assert!(r
        .master_page
        .contains("<link rel=\"boinc_scheduler\" href=\"https://p.example/sched\">"));
    assert!(r
        .wu_doc
        .contains("<url>https://p.example/download/in_1</url>"));
    assert!(r
        .reply
        .contains("<url>https://p.example/upload/file_upload_handler</url>"));
}

#[test]
fn imported_stock_xml_is_read_like_native_xml() {
    // The output file_info of a stock result: its upload URL names the stock
    // handler. A stock workunit's file_info has the stock download URL.
    let result = "<file_info>\n    <name>example_1_0</name>\n    <generated_locally/>\n    <upload_when_present/>\n    <max_nbytes>1000</max_nbytes>\n    <url>https://p.example/example_cgi/file_upload_handler</url>\n<xml_signature>\nabc\n</xml_signature>\n</file_info>\n<result>\n    <file_ref>\n        <file_name>example_1_0</file_name>\n        <open_name>out</open_name>\n    </file_ref>\n</result>\n";
    let outs = output_files(result);
    assert_eq!(outs.len(), 1);
    assert_eq!(outs[0].name, "example_1_0");
    assert!(outs[0].uploadable);
    let wu = format!(
        "<file_info>\n    <name>in_1</name>\n    <url>https://p.example/example/download/{}/in_1</url>\n    <md5_cksum>x</md5_cksum>\n    <nbytes>1</nbytes>\n    <no_delete/>\n</file_info>\n<workunit>\n</workunit>\n",
        dir("in_1")
    );
    let infos = file_infos(&wu);
    assert_eq!(infos.len(), 1);
    assert!(infos[0].no_delete);
}

// A scheduler POST to the stock path is the same request: routing is the
// Worker's, covered in cb_core::stock. This keeps the statement that the
// stored settings round trip through the database.
#[test]
fn the_stock_setting_survives_the_database() {
    let db = SqliteDb::with_schema().unwrap();
    block_on(put_setting(&db, SETTING_CONFIG, &config(STOCK), 1)).unwrap();
    let n = block_on(db.run(stmt!("SELECT count(*) FROM config WHERE name = 'config'")))
        .unwrap()
        .scalar_i64();
    assert_eq!(n, Some(1));
    let p = block_on(Project::load(&db)).unwrap();
    assert_eq!(p.config.stock.as_ref().unwrap().fanout, 1024);
}
