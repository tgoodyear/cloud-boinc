//! Scheduler replies: what the client's `SCHEDULER_REPLY::parse` and
//! `CLIENT_STATE::handle_scheduler_reply` make of them.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::OnceLock;

use cb_core::admin::{
    add_app_version, put_app, put_platform, NewApp, NewAppVersion, NewPlatform, VersionFile,
};
use cb_core::plan_class::PlanClassSpecs;
use cb_core::scheduler::{handle_request, Catalog, SchedCtx, SchedOutcome};
use cb_core::template::InputFile;
use cb_core::work::{create_work, register_job_file, JobSpec, Submission};
use cb_core::Config;
use cb_crypto::{email_hash, generate_key_pair, md5_hex, user_cpid, PrivateKey, PublicKey, Signer};
use cb_db::{stmt, Db};
use cb_proto::consts::PROC_TYPE_NVIDIA_GPU;
use cb_proto::sched_reply::{
    insert_deadline_tag, insert_name_tags, insert_wu_tags, write_project_down, ReplyApp,
    ReplyAppVersion, ReplyHost, ReplyResult, ReplyUser, SchedulerReply, UserMessage, WuTags,
    SCHEDULER_VERSION,
};
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;

use crate::harness::{Expect, Harness};

const LINUX: &str = "x86_64-pc-linux-gnu";
const MASTER: &str = "https://p.example/";
const DOWNLOAD: &str = "https://dl.p.example/download";
const UPLOAD: &str = "https://up.p.example/upload/file_upload_handler";
const NOW: i64 = 1_800_000_000;
const DELAY_BOUND: i64 = 7200;

const CONFIG: &str = "<config>\n<long_name>Oracle Project</long_name>\n<master_url>https://p.example</master_url>\n<download_url>https://dl.p.example/download</download_url>\n<upload_url>https://up.p.example/upload/file_upload_handler</upload_url>\n<next_rpc_delay>7200</next_rpc_delay>\n<daily_result_quota>100</daily_result_quota>\n<version_select_random_factor>0</version_select_random_factor>\n</config>\n";

const USER_NAME: &str = "Ada <&> Lov\u{e9}lace";
const GLOBAL_PREFS: &str = "<global_preferences>\n    <mod_time>1750000000</mod_time>\n    <cpu_usage_limit>50</cpu_usage_limit>\n    <work_buf_min_days>0.5</work_buf_min_days>\n    <run_on_batteries>1</run_on_batteries>\n    <max_ncpus_pct>50</max_ncpus_pct>\n    <disk_max_used_gb>10</disk_max_used_gb>\n</global_preferences>";
const PROJECT_PREFS: &str = "<project_preferences>\n    <resource_share>150</resource_share>\n    <allow_non_preferred_apps>1</allow_non_preferred_apps>\n</project_preferences>";

/// Files of the app version: `(name, contents, open name, main program)`.
const APP_FILES: [(&str, &[u8], &str, bool); 2] = [
    (
        "worker_1.02_x86_64-pc-linux-gnu",
        b"#!/bin/sh\nexit 0\n",
        "",
        true,
    ),
    (
        "worker_data_1.02.dat",
        b"0123456789abcdef",
        "data.dat",
        false,
    ),
];

/// Input files: `(name, contents)`. The last is the constant file of the
/// input template; the others are per job.
const INPUT_FILES: [(&str, &[u8]); 3] = [
    ("input_a.dat", b"aaaaaaaaaaaaaaaaaaaa"),
    ("input_b.dat", b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
    ("common_params.xml", b"<params/>\n"),
];

const INPUT_TEMPLATE: &str = "<input_template>\n    <file_info>\n        <number>0</number>\n    </file_info>\n    <file_info>\n        <physical_name>common_params.xml</physical_name>\n    </file_info>\n    <workunit>\n        <file_ref>\n            <file_number>0</file_number>\n            <open_name>in</open_name>\n        </file_ref>\n        <file_ref>\n            <open_name>params.xml</open_name>\n            <copy_file/>\n        </file_ref>\n        <command_line>--steps 100</command_line>\n        <rsc_fpops_est>1e12</rsc_fpops_est>\n        <rsc_fpops_bound>1e14</rsc_fpops_bound>\n        <rsc_memory_bound>5e8</rsc_memory_bound>\n        <rsc_disk_bound>1e9</rsc_disk_bound>\n        <delay_bound>7200</delay_bound>\n        <min_quorum>1</min_quorum>\n        <target_nresults>1</target_nresults>\n    </workunit>\n</input_template>\n";
const RESULT_TEMPLATE: &str = "<output_template>\n    <file_info>\n        <name><OUTFILE_0/></name>\n        <generated_locally/>\n        <upload_when_present/>\n        <max_nbytes>5000000</max_nbytes>\n        <url><UPLOAD_URL/></url>\n    </file_info>\n    <file_info>\n        <name><OUTFILE_1/></name>\n        <generated_locally/>\n        <upload_when_present/>\n        <max_nbytes>100000</max_nbytes>\n        <url><UPLOAD_URL/></url>\n    </file_info>\n    <result>\n        <file_ref>\n            <file_name><OUTFILE_0/></file_name>\n            <open_name>result.dat</open_name>\n        </file_ref>\n        <file_ref>\n            <file_name><OUTFILE_1/></file_name>\n            <open_name>log.txt</open_name>\n            <optional/>\n        </file_ref>\n    </result>\n</output_template>\n";

/// The upload key and the code-signing key; generating keys takes a while,
/// so the tests share them.
struct Keys {
    upload: Signer,
    upload_public: PublicKey,
    code_private: PrivateKey,
    code_public: PublicKey,
}

fn keys() -> &'static Keys {
    static KEYS: OnceLock<Keys> = OnceLock::new();
    KEYS.get_or_init(|| {
        let (upload_private, upload_public) = generate_key_pair().unwrap();
        let (code_private, code_public) = generate_key_pair().unwrap();
        Keys {
            upload: Signer::new(&upload_private).unwrap(),
            upload_public,
            code_private,
            code_public,
        }
    })
}

/// What a client sends.
#[derive(Clone)]
struct Req {
    auth: String,
    hostid: i64,
    seqno: i32,
    platform: String,
    work_secs: f64,
    /// The code-signing key the client holds, once it has one.
    code_sign_key: Option<String>,
    /// `(name, client state, exit status)`
    reports: Vec<(String, i32, i32)>,
}

impl Req {
    fn new(hostid: i64) -> Req {
        Req {
            auth: "auth1".into(),
            hostid,
            seqno: 1,
            platform: LINUX.into(),
            work_secs: 3000.0,
            code_sign_key: None,
            reports: Vec::new(),
        }
    }

    fn xml(&self) -> String {
        let mut s = format!(
            "<scheduler_request>\n    <authenticator>{}</authenticator>\n    <hostid>{}</hostid>\n    <rpc_seqno>{}</rpc_seqno>\n    <core_client_major_version>8</core_client_major_version>\n    <core_client_minor_version>3</core_client_minor_version>\n    <core_client_release>0</core_client_release>\n    <work_req_seconds>{w}</work_req_seconds>\n    <cpu_req_secs>{w}</cpu_req_secs>\n    <cpu_req_instances>{}</cpu_req_instances>\n    <estimated_delay>0</estimated_delay>\n    <platform_name>{}</platform_name>\n",
            self.auth,
            self.hostid,
            self.seqno,
            if self.work_secs > 0.0 { 1 } else { 0 },
            self.platform,
            w = self.work_secs,
        );
        s.push_str("<host_info>\n    <domain_name>oracle-box</domain_name>\n    <ip_addr>192.0.2.17</ip_addr>\n    <host_cpid>c0ffee</host_cpid>\n    <p_ncpus>4</p_ncpus>\n    <p_vendor>GenuineIntel</p_vendor>\n    <p_model>cpu</p_model>\n    <p_fpops>2e9</p_fpops>\n    <m_nbytes>8e9</m_nbytes>\n    <d_total>1e11</d_total>\n    <d_free>5e10</d_free>\n    <os_name>Linux</os_name>\n    <os_version>6.1</os_version>\n</host_info>\n");
        if let Some(key) = &self.code_sign_key {
            s.push_str(&format!("<code_sign_key>\n{key}\n</code_sign_key>\n"));
        }
        for (name, state, exit) in &self.reports {
            s.push_str(&format!(
                "<result>\n    <name>{name}</name>\n    <final_cpu_time>90</final_cpu_time>\n    <final_elapsed_time>100</final_elapsed_time>\n    <exit_status>{exit}</exit_status>\n    <state>{state}</state>\n    <app_version_num>102</app_version_num>\n</result>\n"
            ));
        }
        s.push_str("</scheduler_request>\n");
        s
    }
}

/// A project with one app, one signed app version, staged input files and
/// an account.
struct World {
    db: SqliteDb,
    config: Config,
    plan_classes: PlanClassSpecs,
    seq: Cell<u32>,
}

impl World {
    fn new() -> World {
        let db = SqliteDb::with_schema().unwrap();
        let config = Config::parse(CONFIG).unwrap();
        let k = keys();
        block_on(async {
            put_platform(
                &db,
                1,
                &NewPlatform {
                    name: LINUX.into(),
                    user_friendly_name: "Linux (64 bit)".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            put_app(
                &db,
                1,
                &NewApp {
                    name: "worker".into(),
                    user_friendly_name: "Worker App".into(),
                    fraction_done_exact: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            let mut files = Vec::new();
            for (name, bytes, open_name, main) in APP_FILES {
                let md5 = md5_hex(bytes);
                register_job_file(&db, name, &md5, bytes.len() as u64, true, 1)
                    .await
                    .unwrap();
                files.push(VersionFile {
                    name: name.into(),
                    signature: cb_crypto::sign_md5(&md5, &k.code_private).unwrap(),
                    main_program: main,
                    executable: main,
                    open_name: open_name.into(),
                    copy_file: !main,
                });
            }
            add_app_version(
                &db,
                &config,
                Some(&k.code_public),
                1,
                &NewAppVersion {
                    app: "worker".into(),
                    platform: LINUX.into(),
                    version_num: 102,
                    api_version: "7.7.0".into(),
                    files,
                    needs_network: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            for (name, bytes) in INPUT_FILES {
                register_job_file(&db, name, &md5_hex(bytes), bytes.len() as u64, false, 1)
                    .await
                    .unwrap();
            }
            db.run(stmt!(
                "INSERT INTO user (id, create_time, email_addr, name, authenticator, cross_project_id, \
                 global_prefs, project_prefs, total_credit, expavg_credit) \
                 VALUES (1, 1700000000, 'ada@example.com', ?, 'auth1', 'cpid1', ?, ?, 1234.5, 12.25)",
                USER_NAME,
                GLOBAL_PREFS,
                PROJECT_PREFS
            ))
            .await
            .unwrap();
        });
        World {
            db,
            config,
            plan_classes: PlanClassSpecs::default(),
            seq: Cell::new(0),
        }
    }

    fn rand(&self) -> u32 {
        self.seq.set(self.seq.get().wrapping_add(2_654_435_761));
        self.seq.get()
    }

    fn submit(&self, job: &str, input: &str) {
        let sub = Submission {
            app: "worker".into(),
            input_template: INPUT_TEMPLATE.into(),
            result_template: RESULT_TEMPLATE.into(),
            jobs: vec![JobSpec {
                name: job.into(),
                input_files: vec![InputFile {
                    name: input.into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let random = || self.rand();
        let out = block_on(create_work(
            &self.db,
            &self.config,
            Some(&keys().upload),
            NOW - 100,
            &random,
            &sub,
        ))
        .unwrap();
        assert_eq!(out[0].error, None);
    }

    fn rpc(&self, req: &Req, now: i64) -> SchedOutcome {
        let catalog = block_on(Catalog::load(&self.db)).unwrap();
        let random = || self.rand();
        let key = keys().code_public.to_key_string();
        let ctx = SchedCtx {
            db: &self.db,
            config: &self.config,
            plan_classes: &self.plan_classes,
            catalog: &catalog,
            code_sign_key: &key,
            now,
            random: &random,
            client_ip: "198.51.100.9",
        };
        block_on(handle_request(&ctx, &req.xml())).unwrap()
    }
}

/// What the client should find for a file of the reply.
struct FileWant<'a> {
    name: &'a str,
    url: String,
    nbytes: f64,
    md5: Option<String>,
    executable: bool,
    /// The signature block the server wrote, for an application file.
    signature: Option<String>,
}

/// Checks the file named `want.name` among `list` (`file_info` or
/// `state.file_info`).
fn expect_file(e: &mut Expect<'_>, list: &str, want: &FileWant<'_>) {
    let Some(i) = e.dump().index_of(list, "name", want.name) else {
        e.fail(format!("{list}: the client found no file {}", want.name));
        return;
    };
    let b = format!("{list}[{i}]");
    e.s(&format!("{b}.download_urls"), &want.url);
    e.n(&format!("{b}.nbytes"), want.nbytes);
    e.s(
        &format!("{b}.md5_cksum"),
        want.md5.as_deref().unwrap_or_default(),
    );
    e.b(&format!("{b}.executable"), want.executable);
    e.s(
        &format!("{b}.file_signature"),
        want.signature.as_deref().map(str::trim).unwrap_or_default(),
    );
    e.s(&format!("{b}.upload_urls"), "");
}

/// The files of the app version as the client should find them.
fn app_files() -> Vec<FileWant<'static>> {
    let k = keys();
    APP_FILES
        .iter()
        .map(|(name, bytes, _, main)| FileWant {
            name,
            url: format!("{DOWNLOAD}/{name}"),
            nbytes: bytes.len() as f64,
            md5: None,
            executable: *main,
            signature: Some(cb_crypto::sign_md5(&md5_hex(bytes), &k.code_private).unwrap()),
        })
        .collect()
}

fn input_file(name: &'static str) -> FileWant<'static> {
    let (_, bytes) = INPUT_FILES.iter().find(|(n, _)| *n == name).unwrap();
    FileWant {
        name,
        url: format!("{DOWNLOAD}/{name}"),
        nbytes: bytes.len() as f64,
        md5: Some(md5_hex(bytes)),
        executable: false,
        signature: None,
    }
}

/// What the account part of every reply from this world says.
fn expect_account(e: &mut Expect<'_>, out: &SchedOutcome) {
    let u = &out.reply.user;
    e.s("project.name", "Oracle Project");
    e.n("project.userid", u.id as f64);
    e.s("project.user_name", USER_NAME);
    e.n("project.user_total_credit", 1234.5);
    e.n("project.user_expavg_credit", 12.25);
    e.n("project.user_create_time", 1_700_000_000.0);
    e.n("project.cpid_time", 1_700_000_000.0);
    e.s("project.email_hash", email_hash("ada@example.com"));
    e.s("project.cross_project_id", "cpid1");
    e.s(
        "project.external_cpid",
        user_cpid("cpid1", "ada@example.com"),
    );
    e.s("project.team_name", "");
    e.n("project.teamid", 0.0);
    e.n(
        "project.host_create_time",
        out.reply.host.create_time as f64,
    );
    e.n("project.host_total_credit", 0.0);
    e.n("project.host_expavg_credit", 0.0);
    e.s("master_url", MASTER);
    e.n("scheduler_version", f64::from(SCHEDULER_VERSION));
    e.b("dont_use_dcf", true);
}

fn expect_messages(e: &mut Expect<'_>, out: &SchedOutcome) {
    e.n("message.count", out.reply.messages.len() as f64);
    for (i, m) in out.reply.messages.iter().enumerate() {
        e.s(&format!("message[{i}].priority"), &m.priority);
        e.s(&format!("message[{i}].text"), &m.message);
    }
}

/// The oracle arguments every scheduler reply case shares.
fn base_args() -> Vec<String> {
    ["--master-url", MASTER, "--platform", LINUX]
        .map(String::from)
        .to_vec()
}

fn run(h: &Harness, kind_file: &std::path::Path, args: &[String]) -> crate::harness::Dump {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    h.run("scheduler_reply", kind_file, &refs)
}

fn path_arg(p: &std::path::Path) -> String {
    p.to_str().unwrap().to_string()
}

/// A new host asks for work and gets two jobs.
struct Work {
    path: PathBuf,
    out: SchedOutcome,
    host: i64,
}

fn work_reply(h: &Harness, w: &World) -> Work {
    w.submit("job_a", "input_a.dat");
    w.submit("job_b", "input_b.dat");
    let out = w.rpc(&Req::new(0), NOW);
    assert_eq!(out.n_sent, 2);
    let path = h.save("scheduler_work.xml", &out.reply.write().unwrap());
    let host = out.host_id;
    Work { path, out, host }
}

#[test]
fn work_assigned() {
    let h = Harness::new("scheduler");
    let w = World::new();
    let work = work_reply(&h, &w);
    if !h.ready() {
        return;
    }
    let out = &work.out;
    let k = keys();
    let key_text = k.code_public.to_key_string();
    let mut args = base_args();
    for (name, bytes, _, _) in APP_FILES {
        args.push("--file-md5".into());
        args.push(format!("{name}={}", md5_hex(bytes)));
    }
    let d = run(&h, &work.path, &args);
    let mut e = Expect::new("scheduler reply with work", &d);

    // Reply level.
    e.rc("parse.rc", 0);
    e.n("hostid", work.host as f64);
    e.n("request_delay", 0.0);
    e.n("next_rpc_delay", 7200.0);
    expect_account(&mut e, out);
    expect_messages(&mut e, out);
    e.b("project_prefs.present", true);
    e.s(
        "project_prefs.xml",
        "\n    <resource_share>150</resource_share>\n    <allow_non_preferred_apps>1</allow_non_preferred_apps>\n",
    );
    e.b("global_prefs.present", true);
    e.n("global_prefs.mod_time", 1_750_000_000.0);
    e.b("code_sign_key.present", true);
    e.s("code_sign_key.text", key_text.trim());

    // Application.
    e.n("app.count", 1.0);
    e.s("app[0].name", "worker");
    e.s("app[0].user_friendly_name", "Worker App");
    e.b("app[0].non_cpu_intensive", false);
    e.b("app[0].fraction_done_exact", true);

    // Files: two of the app version, then for each of the two jobs the two
    // inputs and the two outputs.
    e.n("file_info.count", 10.0);
    for f in app_files() {
        expect_file(&mut e, "file_info", &f);
        let i = d.index_of("file_info", "name", f.name).unwrap();
        e.b(&format!("file_info[{i}].signature_verified"), true);
        e.n(&format!("file_info[{i}].signature_check_rc"), 0.0);
    }
    for name in ["input_a.dat", "input_b.dat", "common_params.xml"] {
        expect_file(&mut e, "file_info", &input_file(name));
    }

    // App version.
    e.n("app_version.count", 1.0);
    e.s("app_version[0].app_name", "worker");
    e.n("app_version[0].version_num", 102.0);
    e.s("app_version[0].platform", LINUX);
    e.s("app_version[0].plan_class", "");
    e.s("app_version[0].api_version", "7.7.0");
    e.n(
        "app_version[0].avg_ncpus",
        out.reply.app_versions[0].avg_ncpus,
    );
    e.n("app_version[0].flops", out.reply.app_versions[0].flops);
    e.b("app_version[0].needs_network", true);
    e.n("app_version[0].rsc_type", 0.0);
    e.n("app_version[0].file_ref.count", 2.0);
    e.s("app_version[0].file_ref[0].file_name", APP_FILES[0].0);
    e.b("app_version[0].file_ref[0].main_program", true);
    e.s("app_version[0].file_ref[1].file_name", APP_FILES[1].0);
    e.s("app_version[0].file_ref[1].open_name", "data.dat");
    e.b("app_version[0].file_ref[1].copy_file", true);
    e.b("app_version[0].file_ref[1].main_program", false);

    // Workunits and results.
    e.n("workunit.count", 2.0);
    e.n("result.count", 2.0);
    for (job, input) in [("job_a", "input_a.dat"), ("job_b", "input_b.dat")] {
        let Some(i) = d.index_of("workunit", "name", job) else {
            e.fail(format!("the client found no workunit {job}"));
            continue;
        };
        let b = format!("workunit[{i}]");
        e.s(&format!("{b}.app_name"), "worker");
        e.s(&format!("{b}.command_line"), "--steps 100");
        e.n(&format!("{b}.rsc_fpops_est"), 1e12);
        e.n(&format!("{b}.rsc_fpops_bound"), 1e14);
        e.n(&format!("{b}.rsc_memory_bound"), 5e8);
        e.n(&format!("{b}.rsc_disk_bound"), 1e9);
        e.n(&format!("{b}.input_file.count"), 2.0);
        e.s(&format!("{b}.input_file[0].file_name"), input);
        e.s(&format!("{b}.input_file[0].open_name"), "in");
        e.s(&format!("{b}.input_file[1].file_name"), "common_params.xml");
        e.s(&format!("{b}.input_file[1].open_name"), "params.xml");
        e.b(&format!("{b}.input_file[1].copy_file"), true);

        let rname = format!("{job}_0");
        let Some(i) = d.index_of("result", "name", &rname) else {
            e.fail(format!("the client found no result {rname}"));
            continue;
        };
        let b = format!("result[{i}]");
        e.s(&format!("{b}.wu_name"), job);
        e.n(&format!("{b}.report_deadline"), (NOW + DELAY_BOUND) as f64);
        e.s(&format!("{b}.platform"), LINUX);
        e.n(&format!("{b}.version_num"), 102.0);
        e.s(&format!("{b}.plan_class"), "");
        e.n(&format!("{b}.output_file.count"), 2.0);
        for (n, open, optional, max) in [
            (0, "result.dat", false, 5_000_000.0),
            (1, "log.txt", true, 100_000.0),
        ] {
            let key = format!("{b}.output_file[{n}]");
            let fname = d
                .get(&format!("{key}.file_name"))
                .unwrap_or_default()
                .to_string();
            if !fname.starts_with(&format!("{rname}_r")) || !fname.ends_with(&format!("_{n}")) {
                e.fail(format!(
                    "{key}.file_name is {fname:?}, not {rname}_r<random>_{n}"
                ));
            }
            e.s(&format!("{key}.open_name"), open);
            e.b(&format!("{key}.optional"), optional);
            // The upload side of the output file, with the certificate the
            // server signed for its name and size limit.
            let Some(fi) = d.index_of("file_info", "name", &fname) else {
                e.fail(format!("the client found no file_info for {fname}"));
                continue;
            };
            let fb = format!("file_info[{fi}]");
            e.s(&format!("{fb}.upload_urls"), UPLOAD);
            e.s(&format!("{fb}.download_urls"), "");
            e.n(&format!("{fb}.max_nbytes"), max);
            let cert = k.upload.sign_upload_certificate(&fname, max).unwrap();
            e.s(&format!("{fb}.xml_signature"), cert.trim());
        }
    }

    // The same reply through the client's own handler.
    e.rc("handle.rc", 0);
    e.n("state.app.count", 1.0);
    e.n("state.app_version.count", 1.0);
    e.b("state.app_version[0].linked_to_app", true);
    e.b("state.app_version[0].file_ref[0].linked", true);
    e.b("state.app_version[0].file_ref[1].linked", true);
    e.n("state.workunit.count", 2.0);
    e.n("state.result.count", 2.0);
    for i in 0..2 {
        e.b(&format!("state.workunit[{i}].linked_to_app"), true);
        e.b(&format!("state.workunit[{i}].input_file[0].linked"), true);
        e.b(&format!("state.workunit[{i}].input_file[1].linked"), true);
        e.b(&format!("state.result[{i}].linked_to_workunit"), true);
        e.b(&format!("state.result[{i}].linked_to_app_version"), true);
        e.b(&format!("state.result[{i}].output_file[0].linked"), true);
        e.b(&format!("state.result[{i}].output_file[1].linked"), true);
        e.n(&format!("state.result[{i}].state"), 0.0);
    }
    for f in app_files() {
        expect_file(&mut e, "state.file_info", &f);
    }
    for name in ["input_a.dat", "input_b.dat", "common_params.xml"] {
        expect_file(&mut e, "state.file_info", &input_file(name));
    }
    e.s("state.master_url", MASTER);
    e.n("state.rpc_pending_hostid", work.host as f64);
    e.s(
        "state.project_prefs",
        "\n    <resource_share>150</resource_share>\n    <allow_non_preferred_apps>1</allow_non_preferred_apps>\n",
    );
    // The preferences the account holds, as the client applied them.
    e.n("state.global_prefs.mod_time", 1_750_000_000.0);
    e.n("state.global_prefs.cpu_usage_limit", 50.0);
    e.n("state.global_prefs.work_buf_min_days", 0.5);
    e.b("state.global_prefs.run_on_batteries", true);
    e.n("state.global_prefs.max_ncpus_pct", 50.0);
    e.n("state.global_prefs.disk_max_used_gb", 10.0);
    e.s("state.global_prefs.source_project", MASTER);

    // The upload request the client builds for each output file is what the
    // upload handler accepts: it names the file, carries the certificate
    // the server signed, and passes the handler's checks.
    e.n("upload_request.count", 4.0);
    for i in 0..d.count("upload_request") {
        let b = format!("upload_request[{i}]");
        e.rc(&format!("{b}.rc"), 0);
        let file = d.get(&format!("{b}.file")).unwrap_or_default().to_string();
        let text = d.get(&format!("{b}.text")).unwrap_or_default();
        match cb_proto::upload::parse_request(text.as_bytes()) {
            Ok(cb_proto::upload::UploadRequest::FileUpload(h)) => {
                if h.name != file {
                    e.fail(format!("{b}: the request names {:?}, not {file:?}", h.name));
                }
                if let Err(err) = h.check_signature_fields(false) {
                    e.fail(format!("{b}: the handler refuses the request: {err}"));
                }
                if let Err(err) = h.check_limits(false, w.config.max_upload_nbytes) {
                    e.fail(format!("{b}: the handler refuses the request: {err}"));
                }
                if !cb_crypto::verify_upload_certificate(
                    &h.name,
                    h.max_nbytes,
                    &h.xml_signature,
                    &k.upload_public,
                ) {
                    e.fail(format!(
                        "{b}: the certificate the client sent for {file} does not verify"
                    ));
                }
            }
            other => e.fail(format!(
                "{b}: the handler cannot read the request: {other:?}"
            )),
        }
    }

    e.quiet(&[]);
    e.finish();
}

#[test]
fn no_work_available() {
    let h = Harness::new("scheduler");
    let w = World::new();
    let work = work_reply(&h, &w);
    let mut again = Req::new(work.host);
    again.seqno = 2;
    again.code_sign_key = Some(keys().code_public.to_key_string().trim().to_string());
    let out = w.rpc(&again, NOW + 100);
    assert_eq!(out.n_sent, 0);
    let path = h.save("scheduler_no_work.xml", &out.reply.write().unwrap());
    if !h.ready() {
        return;
    }
    let d = run(&h, &path, &base_args());
    let mut e = Expect::new("scheduler reply without work", &d);
    e.rc("parse.rc", 0);
    e.n("hostid", 0.0);
    e.n("request_delay", out.reply.request_delay);
    expect_account(&mut e, &out);
    expect_messages(&mut e, &out);
    e.logged("Project has no tasks available");
    e.n("result.count", 0.0);
    e.n("workunit.count", 0.0);
    e.rc("handle.rc", 0);
    if out.reply.request_delay > 0.0 {
        e.logged(&format!(
            "Project requested delay of {:.0} seconds",
            out.reply.request_delay
        ));
    }
    e.quiet(&[]);
    e.finish();
}

#[test]
fn results_acknowledged() {
    let h = Harness::new("scheduler");
    let w = World::new();
    let work = work_reply(&h, &w);
    let mut report = Req::new(work.host);
    report.seqno = 2;
    report.work_secs = 0.0;
    report.code_sign_key = Some(keys().code_public.to_key_string().trim().to_string());
    report.reports = vec![
        ("job_a_0".into(), cb_proto::consts::RESULT_FILES_UPLOADED, 0),
        (
            "job_b_0".into(),
            cb_proto::consts::RESULT_COMPUTE_ERROR,
            195,
        ),
        (
            "nonesuch_0".into(),
            cb_proto::consts::RESULT_FILES_UPLOADED,
            0,
        ),
    ];
    let out = w.rpc(&report, NOW + 3000);
    assert_eq!(out.reply.result_acks.len(), 3);
    let path = h.save("scheduler_acks.xml", &out.reply.write().unwrap());
    if !h.ready() {
        return;
    }
    let mut args = base_args();
    args.push("--prior-reply".into());
    args.push(path_arg(&work.path));
    let d = run(&h, &path, &args);
    let mut e = Expect::new("scheduler reply with result acknowledgements", &d);
    e.rc("parse.rc", 0);
    expect_messages(&mut e, &out);
    e.n("result_ack.count", 3.0);
    for (i, name) in out.reply.result_acks.iter().enumerate() {
        e.s(&format!("result_ack[{i}]"), name);
    }
    e.rc("prior[0].handle.rc", 0);
    e.rc("handle.rc", 0);
    // The client matched the acknowledgements to the tasks it holds, which
    // is where it would report "can't find it" for a name it does not know.
    e.logged("got ack for task job_a_0");
    e.logged("got ack for task job_b_0");
    // The server also acknowledges a result it never sent; the client
    // reports that one. Nothing else may be reported.
    e.quiet(&["Got ack for task nonesuch_0, but can't find it"]);
    e.finish();
}

#[test]
fn account_key_refused() {
    let h = Harness::new("scheduler");
    let w = World::new();
    let mut req = Req::new(0);
    req.auth = "wrong".into();
    let out = w.rpc(&req, NOW);
    assert!(out.reply.nucleus_only);
    let path = h.save("scheduler_bad_key.xml", &out.reply.write().unwrap());
    if !h.ready() {
        return;
    }
    let d = run(&h, &path, &base_args());
    let mut e = Expect::new("scheduler reply to an unknown account key", &d);
    e.rc("parse.rc", 0);
    e.n("request_delay", out.reply.request_delay);
    expect_messages(&mut e, &out);
    e.s("message[0].priority", "notice");
    e.s("project.name", "Oracle Project");
    e.n("result.count", 0.0);
    e.n("app.count", 0.0);
    e.rc("handle.rc", 0);
    e.quiet(&[]);
    e.finish();
}

#[test]
fn platform_not_supported() {
    let h = Harness::new("scheduler");
    let w = World::new();
    let mut req = Req::new(0);
    req.platform = "sparc-sun-solaris".into();
    let out = w.rpc(&req, NOW);
    let path = h.save("scheduler_bad_platform.xml", &out.reply.write().unwrap());
    if !h.ready() {
        return;
    }
    let d = run(&h, &path, &base_args());
    let mut e = Expect::new("scheduler reply to an unsupported platform", &d);
    e.rc("parse.rc", 0);
    e.n("request_delay", out.reply.request_delay);
    expect_messages(&mut e, &out);
    e.logged("doesn't support computers of type sparc-sun-solaris");
    e.rc("handle.rc", 0);
    e.quiet(&[]);
    e.finish();
}

#[test]
fn project_down() {
    let h = Harness::new("scheduler");
    let text = write_project_down("The project is down for maintenance.", 1800, "");
    let path = h.save("scheduler_project_down.xml", &text);
    if !h.ready() {
        return;
    }
    let d = run(&h, &path, &base_args());
    let mut e = Expect::new("scheduler reply of a project that is down", &d);
    e.rc("parse.rc", 0);
    e.b("project_is_down", true);
    e.n("request_delay", 1800.0);
    e.n("message.count", 1.0);
    e.s("message[0].priority", "low");
    e.s("message[0].text", "The project is down for maintenance.");
    // The client treats the reply as an error so that it backs off.
    e.rc("handle.rc", -183);
    e.quiet(&[]);
    e.finish();
}

/// A reply that carries every optional element the server can write, on
/// top of the work of [`work_assigned`] so that its references resolve.
#[test]
fn optional_elements() {
    let h = Harness::new("scheduler");
    let w = World::new();
    let work = work_reply(&h, &w);
    let k = keys();
    let key_text = k.code_public.to_key_string();
    let key_signature = cb_crypto::sign_text(key_text.trim(), &k.code_private).unwrap();
    let reply = SchedulerReply {
        core_client_version: 80300,
        master_url: MASTER.to_string(),
        request_delay: 25.0,
        messages: vec![
            UserMessage::new("A low priority message", "low"),
            UserMessage::new("A high priority message", "high"),
            UserMessage::new("A notice for the user", "notice"),
        ],
        project_name: "Oracle Project".to_string(),
        send_time_stats_log: true,
        send_full_workload: true,
        next_rpc_delay: 3600.0,
        user: ReplyUser {
            id: 1,
            name: USER_NAME.to_string(),
            total_credit: 99.5,
            expavg_credit: 4.0,
            create_time: 1_700_000_000,
            email_hash: email_hash("ada@example.com"),
            cross_project_id: "cpid1".to_string(),
            external_cpid: user_cpid("cpid1", "ada@example.com"),
            global_prefs: Some(
                "<global_preferences>\n    <mod_time>1750000100</mod_time>\n    <cpu_usage_limit>75</cpu_usage_limit>\n    <idle_time_to_run>7</idle_time_to_run>\n</global_preferences>"
                    .to_string(),
            ),
            project_prefs: PROJECT_PREFS.to_string(),
        },
        hostid: 0,
        host: ReplyHost {
            total_credit: 7.5,
            expavg_credit: 1.25,
            venue: "home".to_string(),
            create_time: 1_700_000_100,
        },
        team: Some((3, "Team <A> & B".to_string())),
        result_aborts: vec!["job_a_0".to_string()],
        result_abort_if_not_starteds: vec!["job_b_0".to_string()],
        code_sign_key: key_text.trim().to_string(),
        code_sign_key_signature: key_signature.clone(),
        send_msg_ack: true,
        non_cpu_intensive: true,
        strict_memory_bound: true,
        verify_files_on_app_start: true,
        file_deletes: vec!["input_a.dat".to_string()],
        no_rsc_apps: vec!["ATI".to_string()],
        gui_urls: "<gui_urls>\n    <gui_url>\n        <name>Project home</name>\n        <description>The project web site</description>\n        <url>https://p.example/</url>\n    </gui_url>\n</gui_urls>\n".to_string(),
        project_files: "<file_info>\n    <name>logo.png</name>\n    <url>https://dl.p.example/download/logo.png</url>\n    <md5_cksum>0123456789abcdef0123456789abcdef</md5_cksum>\n    <nbytes>4096</nbytes>\n</file_info>\n<project_files>\n    <file_ref>\n        <file_name>logo.png</file_name>\n        <open_name>logo.png</open_name>\n    </file_ref>\n</project_files>".to_string(),
        ..SchedulerReply::default()
    };
    let path = h.save("scheduler_optional.xml", &reply.write().unwrap());
    if !h.ready() {
        return;
    }
    let mut args = base_args();
    args.extend(["--gpu".into(), "amd".into()]);
    args.push("--prior-reply".into());
    args.push(path_arg(&work.path));
    args.push("--code-sign-key".into());
    let key_file = h.save("code_sign_key.txt", &key_text);
    args.push(path_arg(&key_file));
    let d = run(&h, &path, &args);
    let mut e = Expect::new("scheduler reply with every optional element", &d);
    e.rc("parse.rc", 0);
    e.n("request_delay", 25.0);
    e.n("next_rpc_delay", 3600.0);
    e.s("master_url", MASTER);
    e.s("host_venue", "home");
    e.b("send_full_workload", true);
    e.n("send_time_stats_log", 1.0);
    e.b("message_ack", true);
    e.b("dont_use_dcf", true);
    expect_msg_list(&mut e, &reply.messages);
    e.s("project.team_name", "Team <A> & B");
    e.n("project.teamid", 3.0);
    e.n("project.user_total_credit", 99.5);
    e.n("project.user_expavg_credit", 4.0);
    e.n("project.host_total_credit", 7.5);
    e.n("project.host_expavg_credit", 1.25);
    e.n("project.host_create_time", 1_700_000_100.0);
    e.b("project.non_cpu_intensive", true);
    e.b("project.strict_memory_bound", true);
    e.b("project.no_rsc_apps.CPU", false);
    e.s(
        "project.gui_urls",
        "<gui_urls>\n\n    <gui_url>\n        <name>Project home</name>\n        <description>The project web site</description>\n        <url>https://p.example/</url>\n    </gui_url>\n</gui_urls>\n",
    );
    e.n("result_abort.count", 1.0);
    e.s("result_abort[0]", "job_a_0");
    e.n("result_abort_if_not_started.count", 1.0);
    e.s("result_abort_if_not_started[0]", "job_b_0");
    e.n("file_delete.count", 1.0);
    e.s("file_delete[0]", "input_a.dat");
    e.n("project_file.count", 1.0);
    e.s("project_file[0].file_name", "logo.png");
    e.s("project_file[0].open_name", "logo.png");
    e.s("code_sign_key.text", key_text.trim());
    e.s_trim("code_sign_key_signature.text", &key_signature);
    e.b("code_sign_key_signature.valid", true);
    e.n("code_sign_key_signature.check_rc", 0.0);
    e.n("file_info.count", 1.0);
    e.s("file_info[0].name", "logo.png");

    e.rc("prior[0].handle.rc", 0);
    e.rc("handle.rc", 0);
    e.b("state.send_full_workload", true);
    e.n("state.send_time_stats_log", 1.0);
    e.s("state.host_venue", "home");
    e.n("state.project_file.count", 1.0);
    e.n("state.global_prefs.mod_time", 1_750_000_100.0);
    e.n("state.global_prefs.cpu_usage_limit", 75.0);
    e.n("state.global_prefs.idle_time_to_run", 7.0);
    e.s("state.team_name", "Team <A> & B");
    // Tags the stock server writes and the stock client does not read:
    // `verify_files_on_app_start` has no parser in the client.
    e.quiet(&["verify_files_on_app_start"]);
    e.finish();
}

fn expect_msg_list(e: &mut Expect<'_>, messages: &[UserMessage]) {
    e.n("message.count", messages.len() as f64);
    for (i, m) in messages.iter().enumerate() {
        e.s(&format!("message[{i}].priority"), &m.priority);
        e.s(&format!("message[{i}].text"), &m.message);
    }
}

/// A job for a GPU app version: the `<coproc>` element and the resource
/// fields the client reads.
#[test]
fn gpu_app_version() {
    let h = Harness::new("scheduler");
    let app_version = "<file_info>\n    <name>gpuapp_1.00_x86_64-pc-linux-gnu__cuda118</name>\n    <url>https://dl.p.example/download/gpuapp_1.00_x86_64-pc-linux-gnu__cuda118</url>\n    <executable/>\n    <file_signature>\n00ff\n.\n</file_signature>\n    <nbytes>1048576</nbytes>\n</file_info>\n<app_version>\n    <app_name>gpuapp</app_name>\n    <version_num>100</version_num>\n    <file_ref>\n        <file_name>gpuapp_1.00_x86_64-pc-linux-gnu__cuda118</file_name>\n        <main_program/>\n    </file_ref>\n</app_version>\n";
    let wu = insert_wu_tags(
        "<workunit>\n    <command_line>--gpu 1</command_line>\n</workunit>\n",
        &WuTags {
            rsc_fpops_est: 1e13,
            rsc_fpops_bound: 1e15,
            rsc_memory_bound: 1e9,
            rsc_disk_bound: 2e9,
            name: "gpu_job",
            app_name: "gpuapp",
            job_keyword_ids: "",
        },
    )
    .unwrap();
    let result = insert_name_tags("<result>\n</result>\n", "gpu_job_0", "gpu_job").unwrap();
    let result = insert_deadline_tag(&result, NOW + 86400).unwrap();
    let reply = SchedulerReply {
        core_client_version: 80300,
        master_url: MASTER.to_string(),
        project_name: "Oracle Project".to_string(),
        apps: vec![ReplyApp {
            name: "gpuapp".to_string(),
            user_friendly_name: "GPU App".to_string(),
            non_cpu_intensive: false,
            fraction_done_exact: false,
        }],
        app_versions: vec![ReplyAppVersion {
            xml_doc: app_version.to_string(),
            platform: LINUX.to_string(),
            plan_class: "cuda118".to_string(),
            avg_ncpus: 0.5,
            flops: 3e11,
            cmdline: "--device 0".to_string(),
            proc_type: PROC_TYPE_NVIDIA_GPU,
            gpu_usage: 1.0,
            gpu_ram: 2e9,
        }],
        wus: vec![wu],
        results: vec![ReplyResult {
            xml_doc_in: result,
            platform: LINUX.to_string(),
            version_num: 100,
            plan_class: "cuda118".to_string(),
        }],
        ..SchedulerReply::default()
    };
    let path = h.save("scheduler_gpu.xml", &reply.write().unwrap());
    if !h.ready() {
        return;
    }
    let mut args = base_args();
    args.extend(["--gpu".into(), "nvidia".into()]);
    let d = run(&h, &path, &args);
    let mut e = Expect::new("scheduler reply with a GPU app version", &d);
    e.rc("parse.rc", 0);
    e.n("app_version.count", 1.0);
    e.s("app_version[0].plan_class", "cuda118");
    e.n("app_version[0].avg_ncpus", 0.5);
    e.n("app_version[0].flops", 3e11);
    e.s("app_version[0].cmdline", "--device 0");
    // Index 1 is the NVIDIA entry of the client's resource list.
    e.n("app_version[0].rsc_type", 1.0);
    e.n("app_version[0].coproc_usage", 1.0);
    e.b("app_version[0].missing_coproc", false);
    // The scheduler writes <gpu_ram> as the stock one does; the client
    // reads the element under another name and ignores this one.
    e.n("app_version[0].gpu_ram", 0.0);
    e.s("workunit[0].command_line", "--gpu 1");
    e.n("workunit[0].rsc_fpops_est", 1e13);
    e.s("result[0].plan_class", "cuda118");
    e.n("result[0].report_deadline", (NOW + 86400) as f64);
    e.rc("handle.rc", 0);
    e.n("state.result.count", 1.0);
    e.b("state.result[0].linked_to_app_version", true);
    e.n("state.app_version[0].rsc_type", 1.0);
    e.quiet(&["gpu_ram"]);
    e.finish();
}
