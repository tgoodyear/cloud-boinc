//! Runs the importer against a stock MySQL or MariaDB server with synthetic
//! rows. Skipped unless `CB_IMPORT_TEST_ADMIN_URL` names a server
//! (`mysql://user:password@host:port/`) on which the test may create and drop
//! databases and a SELECT-only account. In CI a `mariadb` service provides it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cb_core::stock::filename_dir;
use cb_crypto::{md5_hex, pepper_passwd_hash};
use cb_import::config_xml;
use cb_import::error::{ImportError, Kind};
use cb_import::import::{preflight, run, Finished, Options};
use cb_import::schema::{all_tables, apply_migrations};
use cb_import::sink::{load_sql_dir, SqlDirSink, SqliteSink};
use cb_import::source::{is_read_only_statement, Source};
use cb_import::summary::Summary;
use cb_import::verify::verify;
use mysql::prelude::Queryable;
use mysql::{Conn, Opts, Params, Value as My};
use rusqlite::Connection;

const MASTER: &str = "http://project.test/proj/";
const PEPPER: &[u8] = b"synthetic-pepper-for-tests-only-0123456789";

// Values that must never appear in anything the tool prints.
const SENTINELS: &[&str] = &[
    "Alice Sentinel",
    "alice.sentinel@example.org",
    "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a",
    "c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1",
    "$2y$10$sentinelbcrypthashvalue",
    "sentinel-host.local",
    "203.0.113.77",
    "d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3",
    "Sentinel Team Name",
];

/// id, name, workunit, server_state, outcome, validate_state, host, user
type ResultSeed = (i64, &'static str, i64, i64, i64, i64, i64, i64);

fn admin_url() -> Option<String> {
    std::env::var("CB_IMPORT_TEST_ADMIN_URL")
        .ok()
        .filter(|s| !s.is_empty())
}

struct Fx {
    admin: Conn,
    db: String,
    dir: tempfile::TempDir,
    dsn_file: PathBuf,
    project: PathBuf,
}

fn my_i(i: i64) -> My {
    My::Int(i)
}
fn my_s(s: &str) -> My {
    My::Bytes(s.as_bytes().to_vec())
}
fn my_f(f: f64) -> My {
    My::Double(f)
}

fn insert(c: &mut Conn, table: &str, cols: &[(&str, My)]) {
    let names: Vec<String> = cols.iter().map(|(n, _)| format!("`{n}`")).collect();
    let marks = vec!["?"; cols.len()].join(", ");
    let sql = format!(
        "INSERT INTO `{table}` ({}) VALUES ({marks})",
        names.join(", ")
    );
    let vals: Vec<My> = cols.iter().map(|(_, v)| v.clone()).collect();
    c.exec_drop(sql, Params::Positional(vals))
        .unwrap_or_else(|e| panic!("insert into {table}: {e}"));
}

impl Fx {
    fn new(label: &str) -> Option<Fx> {
        let url = admin_url()?;
        let mut admin = Conn::new(Opts::from_url(&url).unwrap()).expect("admin connection");
        let db = format!("cbimp_{label}_{}", std::process::id());
        admin
            .query_drop(format!("DROP DATABASE IF EXISTS `{db}`"))
            .unwrap();
        admin
            .query_drop(format!("CREATE DATABASE `{db}` CHARACTER SET utf8mb4"))
            .unwrap();
        admin.query_drop(format!("USE `{db}`")).unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../db");
        for f in ["schema.sql", "constraints.sql"] {
            let sql = std::fs::read_to_string(root.join(f))
                .unwrap_or_else(|_| panic!("db/{f} of the repository root is needed"));
            admin
                .query_drop(sql)
                .unwrap_or_else(|e| panic!("loading {f}: {e}"));
        }
        admin.query_drop("SET SESSION sql_mode = ''").unwrap();
        // A SELECT-only account, as a replica would give.
        let hostport = url
            .split('@')
            .nth(1)
            .unwrap()
            .trim_end_matches('/')
            .to_string();
        admin
            .query_drop(
                "CREATE USER IF NOT EXISTS 'cbimp_ro'@'%' IDENTIFIED BY 'cbimp-ro-synthetic'",
            )
            .unwrap();
        admin
            .query_drop(format!("GRANT SELECT ON `{db}`.* TO 'cbimp_ro'@'%'"))
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let dsn_file = dir.path().join("dsn");
        std::fs::write(
            &dsn_file,
            format!("mysql://cbimp_ro:cbimp-ro-synthetic@{hostport}/{db}\n"),
        )
        .unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join("templates")).unwrap();
        std::fs::write(
            project.join("templates/upper_out"),
            "<output_template>\n  <file_info/>\n</output_template>\n",
        )
        .unwrap();
        let mut fx = Fx {
            admin,
            db,
            dir,
            dsn_file,
            project,
        };
        fx.seed();
        Some(fx)
    }

    fn put_file(&self, tree: &str, name: &str, body: &str) -> (String, usize) {
        let d = self
            .project
            .join(tree)
            .join(filename_dir(name, 1024).unwrap());
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(name), body).unwrap();
        (md5_hex(body.as_bytes()), body.len())
    }

    fn seed(&mut self) {
        let c = &mut self.admin;
        insert(
            c,
            "platform",
            &[
                ("id", my_i(1)),
                ("create_time", my_i(100)),
                ("name", my_s("x86_64-pc-linux-gnu")),
                ("user_friendly_name", my_s("Linux")),
            ],
        );
        insert(
            c,
            "platform",
            &[
                ("id", my_i(2)),
                ("create_time", my_i(100)),
                ("name", my_s("windows_x86_64")),
                ("user_friendly_name", my_s("Windows")),
            ],
        );
        insert(
            c,
            "app",
            &[
                ("id", my_i(1)),
                ("create_time", my_i(100)),
                ("name", my_s("upper")),
                ("user_friendly_name", my_s("Upper")),
                ("min_avg_pfc", my_f(1.0)),
                ("locality_scheduling", my_i(0)),
                ("homogeneous_redundancy", my_i(2)),
            ],
        );
        insert(
            c,
            "app",
            &[
                ("id", my_i(2)),
                ("create_time", my_i(100)),
                ("name", my_s("other")),
                ("user_friendly_name", my_s("Other")),
            ],
        );

        // Files and documents. The app file is signed with a made-up block.
        std::fs::create_dir_all(self.project.join("download")).unwrap();
        let app_name = "upper_1.00_x86_64-pc-linux-gnu";
        let (app_md5, app_len) = {
            let d = self
                .project
                .join("download")
                .join(filename_dir(app_name, 1024).unwrap());
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(app_name), "binary").unwrap();
            (md5_hex(b"binary"), 6)
        };
        let dir = filename_dir(app_name, 1024).unwrap();
        let av_doc = format!(
            "<file_info>\n    <name>{app_name}</name>\n    <url>http://project.test/proj/download/{dir}/{app_name}</url>\n    <executable/>\n    <md5_cksum>{app_md5}</md5_cksum>\n    <nbytes>{app_len}</nbytes>\n    <file_signature>\n00\n.\n    </file_signature>\n</file_info>\n<app_version>\n    <app_name>upper</app_name>\n</app_version>\n"
        );
        // A second version the way `update_versions` writes it: no MD5 in the document, the
        // file at the top of the download directory, not in a hash directory.
        let flat = "upper_2.00_windows_x86_64.exe";
        std::fs::write(self.project.join("download").join(flat), "flat file").unwrap();
        let flat_doc = format!(
            "<file_info>\n    <name>{flat}</name>\n    <url>http://project.test/proj/download/{flat}</url>\n    <executable/>\n    <nbytes>9</nbytes>\n</file_info>\n<app_version>\n</app_version>\n"
        );
        let c = &mut self.admin;
        insert(
            c,
            "app_version",
            &[
                ("id", my_i(2)),
                ("create_time", my_i(101)),
                ("appid", my_i(1)),
                ("version_num", my_i(200)),
                ("platformid", my_i(2)),
                ("xml_doc", my_s(&flat_doc)),
            ],
        );
        insert(
            c,
            "app_version",
            &[
                ("id", my_i(1)),
                ("create_time", my_i(100)),
                ("appid", my_i(1)),
                ("version_num", my_i(100)),
                ("platformid", my_i(1)),
                ("xml_doc", my_s(&av_doc)),
                ("pfc_avg", my_f(2.5)),
            ],
        );

        insert(
            c,
            "team",
            &[
                ("id", my_i(1)),
                ("create_time", my_i(200)),
                ("userid", my_i(1)),
                ("name", my_s("Sentinel Team Name")),
                ("name_lc", My::NULL),
                ("type", my_i(3)),
                ("description", My::NULL),
                ("nusers", my_i(99)),
                ("total_credit", my_f(500.0)),
                ("expavg_credit", my_f(10.0)),
                ("expavg_time", my_f(1.7e9)),
            ],
        );
        insert(
            c,
            "team",
            &[
                ("id", my_i(2)),
                ("create_time", my_i(200)),
                ("userid", my_i(7)),
                ("name", my_s("Second")),
                ("description", my_s("two\nlines")),
                ("nusers", my_i(0)),
            ],
        );
        let users: [(i64, &str, &str, &str, i64, &str); 4] = [
            (
                1,
                "Alice.Sentinel@Example.ORG",
                "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a",
                "c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1",
                1,
                "Alice Sentinel",
            ),
            (
                2,
                "bob@example.org",
                "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2",
                "$2y$10$sentinelbcrypthashvalue",
                1,
                "Bob",
            ),
            (
                3,
                "carol@example.org",
                "e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5",
                "",
                9,
                "Carol",
            ),
            (
                7,
                "dave@example.org",
                "f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6",
                "weird-form",
                2,
                "Dave",
            ),
        ];
        for (id, email, auth, hash, team, name) in users {
            insert(
                c,
                "user",
                &[
                    ("id", my_i(id)),
                    ("create_time", my_i(300 + id)),
                    ("email_addr", my_s(email)),
                    ("name", my_s(name)),
                    ("authenticator", my_s(auth)),
                    ("country", my_s("United States")),
                    ("total_credit", my_f(1000.0 * id as f64)),
                    ("expavg_credit", my_f(50.0)),
                    ("expavg_time", my_f(1.7e9)),
                    ("global_prefs", My::NULL),
                    ("project_prefs", my_s("<p/>")),
                    ("teamid", my_i(team)),
                    ("venue", my_s("")),
                    ("send_email", my_i(1)),
                    ("show_hosts", my_i(1)),
                    ("cross_project_id", my_s("d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3")),
                    (
                        "passwd_hash",
                        if id == 1 {
                            my_s("c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1")
                        } else {
                            my_s(hash)
                        },
                    ),
                    ("email_validated", my_i(1)),
                    ("posts", my_i(4)),
                    ("signature", my_s("sig")),
                    ("previous_email_addr", my_s("OLD@Example.org")),
                    ("email_addr_change_time", my_f(1.7e9)),
                ],
            );
        }
        c.query_drop("ALTER TABLE user AUTO_INCREMENT = 20")
            .unwrap();
        c.query_drop("ALTER TABLE team AUTO_INCREMENT = 5").unwrap();
        for (id, user) in [(1, 1), (2, 2), (3, 7), (11, 7)] {
            insert(
                c,
                "host",
                &[
                    ("id", my_i(id)),
                    ("create_time", my_i(400)),
                    ("userid", my_i(user)),
                    ("rpc_seqno", my_i(5 + id)),
                    ("rpc_time", my_i(1_700_000_000 + id)),
                    ("total_credit", my_f(10.0)),
                    ("expavg_credit", my_f(3.0)),
                    ("expavg_time", my_f(1.7e9)),
                    ("domain_name", my_s("sentinel-host.local")),
                    ("last_ip_addr", my_s("203.0.113.77")),
                    ("host_cpid", my_s("d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3")),
                    ("p_ncpus", my_i(4)),
                    ("p_vendor", my_s("GenuineIntel")),
                    ("os_name", my_s("Linux")),
                    ("misc", my_s("{\"gpus\":[]}")),
                ],
            );
        }
        c.query_drop("ALTER TABLE host AUTO_INCREMENT = 40")
            .unwrap();
        for (h, av) in [(1, 1), (2, 1), (11, 1)] {
            insert(
                c,
                "host_app_version",
                &[
                    ("host_id", my_i(h)),
                    ("app_version_id", my_i(av)),
                    ("pfc_n", my_f(7.0)),
                    ("pfc_avg", my_f(1.5)),
                    ("max_jobs_per_day", my_i(33)),
                    ("n_jobs_today", my_i(2)),
                    ("consecutive_valid", my_i(4)),
                ],
            );
        }
        insert(
            c,
            "batch",
            &[
                ("id", my_i(3)),
                ("user_id", my_i(1)),
                ("create_time", my_i(500)),
                ("name", my_s("batch three")),
                ("app_id", my_i(1)),
                ("state", my_i(1)),
                ("description", my_s("d")),
                ("njobs", my_i(2)),
            ],
        );
        insert(
            c,
            "batch",
            &[
                ("id", my_i(4)),
                ("user_id", my_i(1)),
                ("create_time", my_i(500)),
                ("name", my_s("batch four")),
                ("app_id", my_i(1)),
                ("state", my_i(1)),
            ],
        );

        // Workunits. 1: finished long ago (left behind). 2: open, one result in
        // progress, one unsent. 3: a result uploaded and not yet validated.
        // 4: finished, holds the highest result ID. 10: finished, the highest workunit ID.
        let (in2_md5, in2_len) = self.put_file("download", "wu2_in", "hello");
        let (out5_md5, out5_len) = self.put_file("upload", "wu3_r5_0", "out");
        let c = &mut self.admin;
        let in_dir = filename_dir("wu2_in", 1024).unwrap();
        let wus: [(i64, &str, i64, i64, i64, i64, i64); 5] = [
            (1, "wu1", 2, 2, 1, 0, 0),
            (2, "wu2", 0, 0, 0, 0, 3),
            (3, "wu3", 0, 0, 0, 1, 0),
            (4, "wu4", 2, 2, 2, 0, 0),
            (10, "wu10", 2, 2, 3, 0, 0),
        ];
        for (id, name, assim, fds, canon, nv, batch) in wus {
            let doc = if id == 2 {
                format!("<file_info>\n    <name>wu2_in</name>\n    <url>http://project.test/proj/download/{in_dir}/wu2_in</url>\n    <md5_cksum>{in2_md5}</md5_cksum>\n    <nbytes>{in2_len}</nbytes>\n</file_info>\n<file_info>\n    <name>external</name>\n    <url>http://elsewhere.test/external</url>\n    <md5_cksum>{in2_md5}</md5_cksum>\n    <nbytes>5</nbytes>\n</file_info>\n<workunit>\n  <file_ref><file_name>wu2_in</file_name></file_ref>\n</workunit>\n")
            } else {
                "<workunit>\n</workunit>\n".to_string()
            };
            insert(
                c,
                "workunit",
                &[
                    ("id", my_i(id)),
                    ("create_time", my_i(1_700_000_000 + id)),
                    ("appid", my_i(1)),
                    ("name", my_s(name)),
                    ("xml_doc", my_s(&doc)),
                    ("batch", my_i(batch)),
                    ("rsc_fpops_est", my_f(1e12)),
                    ("rsc_fpops_bound", my_f(1e13)),
                    ("rsc_memory_bound", my_f(1e8)),
                    ("rsc_disk_bound", my_f(1e8)),
                    ("need_validate", my_i(nv)),
                    ("canonical_resultid", my_i(canon)),
                    ("transition_time", my_i(2_147_483_647)),
                    ("delay_bound", my_i(86400)),
                    ("file_delete_state", my_i(fds)),
                    ("assimilate_state", my_i(assim)),
                    ("hr_class", my_i(if id == 2 { 5 } else { 0 })),
                    ("min_quorum", my_i(1)),
                    ("target_nresults", my_i(1)),
                    ("max_error_results", my_i(3)),
                    ("max_total_results", my_i(5)),
                    ("max_success_results", my_i(2)),
                    ("result_template_file", my_s("templates/upper_out")),
                    ("priority", my_i(0)),
                    ("rsc_bandwidth_bound", my_f(0.0)),
                    ("fileset_id", my_i(0)),
                    ("app_version_id", my_i(0)),
                    ("transitioner_flags", my_i(0)),
                    ("size_class", my_i(-1)),
                    ("keywords", my_s("")),
                    ("app_version_num", my_i(0)),
                    ("n_sent", my_i(1)),
                ],
            );
        }
        c.query_drop("UPDATE workunit SET mod_time = '2026-01-02 03:04:05'")
            .unwrap();
        c.query_drop("ALTER TABLE workunit AUTO_INCREMENT = 15")
            .unwrap();
        // Results: name, wu, server_state, outcome, validate_state, host, user
        let results: [ResultSeed; 7] = [
            (1, "wu1_0", 1, 5, 1, 1, 1, 1),
            (2, "wu1_1", 1, 5, 1, 1, 2, 2),
            (3, "wu2_0", 2, 4, 0, 0, 1, 1),
            (4, "wu2_1", 2, 2, 0, 0, 0, 0),
            (5, "wu3_r5", 3, 5, 1, 0, 2, 2),
            (6, "wu10_0", 10, 5, 1, 1, 1, 1),
            (30, "wu4_0", 4, 5, 1, 1, 1, 1),
        ];
        for (id, name, wu, ss, outcome, vs, host, user) in results {
            // The certificate text is stored with odd white space on purpose.
            let doc_in = format!("<result>\n    <name>{name}</name>\n    <file_info>\n        <name>{name}_0</name>\n        <generated_locally/>\n        <upload_when_present/>\n        <max_nbytes>1e6</max_nbytes>\n        <url>http://project.test/proj_cgi/file_upload_handler</url>\n    </file_info>\n    <xml_signature>\n  ab cd\n\tef\n.\n    </xml_signature>\n</result>\n\n");
            let doc_out = if id == 5 {
                format!("<file_info>\n<name>wu3_r5_0</name>\n<nbytes>{out5_len}</nbytes>\n<md5_cksum>{out5_md5}</md5_cksum>\n</file_info>\n")
            } else {
                String::new()
            };
            insert(
                c,
                "result",
                &[
                    ("id", my_i(id)),
                    ("create_time", my_i(1_700_000_100 + id)),
                    ("workunitid", my_i(wu)),
                    ("server_state", my_i(ss)),
                    ("outcome", my_i(outcome)),
                    ("client_state", my_i(if ss == 4 { 2 } else { 5 })),
                    ("hostid", my_i(host)),
                    ("userid", my_i(user)),
                    ("report_deadline", my_i(1_800_000_000)),
                    ("sent_time", my_i(if ss == 2 { 0 } else { 1_700_000_500 })),
                    (
                        "received_time",
                        my_i(if ss == 5 { 1_700_000_900 } else { 0 }),
                    ),
                    ("name", my_s(name)),
                    ("cpu_time", my_f(12.5)),
                    ("xml_doc_in", my_s(&doc_in)),
                    ("xml_doc_out", my_s(&doc_out)),
                    ("stderr_out", my_s("")),
                    ("batch", my_i(0)),
                    (
                        "file_delete_state",
                        my_i(if ss == 5 && vs == 0 {
                            0
                        } else if ss == 5 {
                            2
                        } else {
                            0
                        }),
                    ),
                    ("validate_state", my_i(vs)),
                    ("granted_credit", my_f(if vs == 1 { 20.0 } else { 0.0 })),
                    ("random", my_i(77)),
                    ("app_version_num", my_i(100)),
                    ("appid", my_i(1)),
                    ("teamid", my_i(if user == 1 { 1 } else { 0 })),
                    ("priority", my_i(0)),
                    ("elapsed_time", my_f(13.0)),
                    ("flops_estimate", my_f(1e9)),
                    ("app_version_id", my_i(1)),
                    ("peak_working_set_size", my_f(0.0)),
                    ("peak_swap_size", my_f(0.0)),
                    ("peak_disk_usage", my_f(0.0)),
                    ("size_class", my_i(-1)),
                ],
            );
        }
        c.query_drop("UPDATE result SET mod_time = '2026-01-02 03:04:06'")
            .unwrap();
        c.query_drop("ALTER TABLE result AUTO_INCREMENT = 100")
            .unwrap();
        insert(
            c,
            "assignment",
            &[
                ("id", my_i(1)),
                ("create_time", my_i(600)),
                ("target_id", my_i(1)),
                ("target_type", my_i(1)),
                ("multi", my_i(0)),
                ("workunitid", my_i(2)),
                ("resultid", my_i(3)),
            ],
        );
        insert(
            c,
            "assignment",
            &[
                ("id", my_i(2)),
                ("create_time", my_i(600)),
                ("target_id", my_i(1)),
                ("target_type", my_i(1)),
                ("multi", my_i(0)),
                ("workunitid", my_i(1)),
                ("resultid", my_i(1)),
            ],
        );
    }

    fn options(&self) -> Options {
        let cfg = format!("<boinc><config>\n<master_url>{MASTER}</master_url>\n<uldl_dir_fanout>1024</uldl_dir_fanout>\n<send_result_abort/>\n<db_passwd>synthetic-secret</db_passwd></config>\n<daemons><daemon><cmd>sample_trivial_validator -d 3 --app upper</cmd></daemon></daemons></boinc>");
        Options {
            project_dir: Some(self.project.clone()),
            config: Some(config_xml::parse(&cfg).unwrap()),
            now: 1_800_000_000,
            ..Options::default()
        }
    }

    fn import_sqlite(
        &self,
        opts: &Options,
        name: &str,
    ) -> Result<(Summary, PathBuf, Vec<String>), ImportError> {
        let path = self.dir.path().join(name);
        let mut src = Source::connect(&self.dsn_file)?;
        let mut summary = Summary::default();
        let pre = preflight(&mut src, opts, &mut summary)?;
        let mem = Connection::open_in_memory().unwrap();
        apply_migrations(&mem).unwrap();
        let tables = all_tables(&mem).unwrap();
        let mut sink = SqliteSink::create(&path)?;
        run(&mut src, &mut sink, opts, &pre, &tables, &mut summary)?;
        Ok((summary, path, src.statements().to_vec()))
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        let _ = self
            .admin
            .query_drop(format!("DROP DATABASE IF EXISTS `{}`", self.db));
    }
}

fn q_i(c: &Connection, sql: &str) -> i64 {
    c.query_row(sql, [], |r| r.get(0)).unwrap()
}
fn q_s(c: &Connection, sql: &str) -> String {
    c.query_row(sql, [], |r| r.get(0)).unwrap()
}

macro_rules! fx {
    ($label:expr) => {
        match Fx::new($label) {
            Some(f) => f,
            None => {
                eprintln!("CB_IMPORT_TEST_ADMIN_URL is not set; skipped");
                return;
            }
        }
    };
}

#[test]
fn imports_the_open_work_and_all_accounts() {
    let fx = fx!("main");
    let mut opts = fx.options();
    opts.pepper = Some(PEPPER.to_vec());
    let (summary, path, _) = fx.import_sqlite(&opts, "out.sqlite").unwrap();
    let t = Connection::open(&path).unwrap();

    assert_eq!(q_i(&t, "SELECT count(*) FROM platform"), 2);
    assert_eq!(q_i(&t, "SELECT count(*) FROM app"), 2);
    assert_eq!(q_i(&t, "SELECT count(*) FROM user"), 4);
    assert_eq!(q_i(&t, "SELECT count(*) FROM host"), 4);
    assert_eq!(q_i(&t, "SELECT count(*) FROM host_app_version"), 3);
    // Open: wu2, wu3. Kept for the highest ids: wu10, wu4. Left behind: wu1.
    assert_eq!(
        q_s(
            &t,
            "SELECT group_concat(name) FROM (SELECT name FROM workunit ORDER BY id)"
        ),
        "wu2,wu3,wu4,wu10"
    );
    assert_eq!(
        q_s(
            &t,
            "SELECT group_concat(name) FROM (SELECT name FROM result ORDER BY id)"
        ),
        "wu2_0,wu2_1,wu3_r5,wu10_0,wu4_0"
    );
    // Only the assignment of the carried workunit.
    assert_eq!(q_i(&t, "SELECT count(*) FROM assignment"), 1);
    // Only the batch a carried workunit names.
    assert_eq!(q_i(&t, "SELECT count(*) FROM batch"), 1);

    // D25: the address is lower-cased; the account key and CPID are as they were.
    assert_eq!(
        q_s(&t, "SELECT email_addr FROM user WHERE id = 1"),
        "alice.sentinel@example.org"
    );
    assert_eq!(
        q_s(&t, "SELECT authenticator FROM user WHERE id = 1"),
        "0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a"
    );
    assert_eq!(
        q_s(&t, "SELECT cross_project_id FROM user WHERE id = 1"),
        "d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3"
    );
    assert_eq!(
        q_s(&t, "SELECT previous_email_addr FROM user WHERE id = 1"),
        "old@example.org"
    );
    // D23/D24: the bare MD5 is converted with the pepper; the others are carried as they are.
    assert_eq!(
        q_s(&t, "SELECT passwd_hash FROM user WHERE id = 1"),
        pepper_passwd_hash(PEPPER, "c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1")
    );
    assert_eq!(
        q_s(&t, "SELECT passwd_hash FROM user WHERE id = 2"),
        "$2y$10$sentinelbcrypthashvalue"
    );
    assert_eq!(q_s(&t, "SELECT passwd_hash FROM user WHERE id = 3"), "");
    assert_eq!(
        q_s(&t, "SELECT passwd_hash FROM user WHERE id = 7"),
        "weird-form"
    );
    assert_eq!(
        summary.counters["password hashes: bare MD5, converted with the pepper"],
        1
    );
    assert_eq!(
        summary.counters["password hashes: PHP password_hash form, carried unchanged"],
        1
    );
    assert_eq!(summary.counters["password hashes: empty"], 1);
    assert_eq!(
        summary.counters["password hashes: other form, carried unchanged"],
        1
    );

    // Teams: the stock count (99) is not carried; the triggers count the members.
    assert_eq!(q_i(&t, "SELECT nusers FROM team WHERE id = 1"), 2);
    assert_eq!(q_i(&t, "SELECT nusers FROM team WHERE id = 2"), 1);
    assert_eq!(
        q_s(&t, "SELECT name_lc FROM team WHERE id = 1"),
        "sentinel team name"
    );
    assert_eq!(
        summary.counters["accounts whose team is not in the team table"],
        1
    );
    // Id floors: above the highest imported id, from AUTO_INCREMENT.
    assert_eq!(
        q_i(&t, "SELECT last_id FROM id_floor WHERE name = 'user'"),
        19
    );
    assert_eq!(
        q_i(&t, "SELECT last_id FROM id_floor WHERE name = 'host'"),
        39
    );
    assert_eq!(
        q_i(&t, "SELECT last_id FROM id_floor WHERE name = 'team'"),
        4
    );

    // Hosts keep what the scheduler reads.
    assert_eq!(q_i(&t, "SELECT rpc_seqno FROM host WHERE id = 11"), 16);
    assert_eq!(
        q_i(&t, "SELECT rpc_time FROM host WHERE id = 1"),
        1_700_000_001
    );
    assert_eq!(
        q_s(&t, "SELECT host_cpid FROM host WHERE id = 1"),
        "d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3"
    );
    assert_eq!(
        q_s(&t, "SELECT misc FROM host WHERE id = 1"),
        "{\"gpus\":[]}"
    );
    assert_eq!(q_i(&t, "SELECT max_jobs_per_day FROM host_app_version WHERE host_id = 1 AND app_version_id = 1"), 33);
    // Valid results: counted from the source, which still has the left-behind workunit's results.
    assert_eq!(q_i(&t, "SELECT valid_results FROM user WHERE id = 1"), 3);
    assert_eq!(q_i(&t, "SELECT valid_results FROM host WHERE id = 1"), 3);
    assert!(
        q_s(&t, "SELECT cast(recent_key AS text) FROM user WHERE id = 1")
            .parse::<f64>()
            .unwrap()
            > -1e8
    );

    // Documents are byte for byte, the template is the file's text, mod_time is seconds.
    let doc: String = t
        .query_row(
            "SELECT xml_doc_in FROM result WHERE name = 'wu2_0'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(doc.contains("\n  ab cd\n\tef\n.\n    </xml_signature>\n</result>\n\n"));
    assert!(doc.ends_with("\n\n"));
    assert_eq!(
        q_s(&t, "SELECT result_template FROM workunit WHERE id = 2"),
        "<output_template>\n  <file_info/>\n</output_template>\n"
    );
    assert_eq!(
        q_i(&t, "SELECT mod_time FROM workunit WHERE id = 2"),
        1_767_323_045
    );
    assert_eq!(
        q_i(&t, "SELECT mod_time FROM result WHERE id = 3"),
        1_767_323_046
    );
    assert_eq!(q_i(&t, "SELECT hr_class FROM workunit WHERE id = 2"), 5);
    assert_eq!(
        q_i(&t, "SELECT homogeneous_redundancy FROM app WHERE id = 1"),
        2
    );
    assert_eq!(q_s(&t, "SELECT validator FROM app WHERE id = 1"), "trivial");

    // job_file: the app file (kept) and the staged input; the external file is not staged.
    assert_eq!(
        q_s(
            &t,
            "SELECT group_concat(name) FROM (SELECT name FROM job_file ORDER BY name)"
        ),
        "upper_1.00_x86_64-pc-linux-gnu,upper_2.00_windows_x86_64.exe,wu2_in"
    );
    assert_eq!(
        q_s(
            &t,
            "SELECT md5 FROM job_file WHERE name = 'upper_2.00_windows_x86_64.exe'"
        ),
        md5_hex(b"flat file")
    );
    assert_eq!(
        q_i(
            &t,
            "SELECT no_delete FROM job_file WHERE name = 'upper_1.00_x86_64-pc-linux-gnu'"
        ),
        1
    );
    assert_eq!(
        q_i(&t, "SELECT nbytes FROM job_file WHERE name = 'wu2_in'"),
        5
    );
    assert_eq!(
        q_s(&t, "SELECT md5 FROM job_file WHERE name = 'wu2_in'"),
        md5_hex(b"hello")
    );

    // Left behind is said.
    assert_eq!(
        summary.counters["workunits not carried (finished, left on the stock server)"],
        1
    );
    assert!(summary
        .notes
        .iter()
        .any(|n| n.contains("stay on the stock server")));
    // Columns with no target column are listed.
    let app = summary.tables.iter().find(|t| t.name == "app").unwrap();
    assert!(
        app.dropped.contains(&"min_avg_pfc".to_string())
            && app.dropped.contains(&"locality_scheduling".to_string())
    );
    let user = summary.tables.iter().find(|t| t.name == "user").unwrap();
    assert!(
        user.dropped.contains(&"posts".to_string())
            && user.dropped.contains(&"signature".to_string())
    );
}

#[test]
fn bare_md5_is_carried_unchanged_without_a_pepper() {
    let fx = fx!("nopepper");
    let (summary, path, _) = fx.import_sqlite(&fx.options(), "o.sqlite").unwrap();
    let t = Connection::open(&path).unwrap();
    assert_eq!(
        q_s(&t, "SELECT passwd_hash FROM user WHERE id = 1"),
        "c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1"
    );
    assert_eq!(
        summary.counters["password hashes: bare MD5, carried unchanged"],
        1
    );
    assert!(summary
        .notes
        .iter()
        .any(|n| n.contains("no --passwd-pepper-file")));
}

#[test]
fn only_selects_are_sent_and_the_session_is_read_only() {
    let fx = fx!("ro");
    let (_, _, sent) = fx.import_sqlite(&fx.options(), "o.sqlite").unwrap();
    assert!(!sent.is_empty());
    for s in &sent {
        assert!(is_read_only_statement(s), "{s}");
        let w = s.split_whitespace().next().unwrap().to_ascii_uppercase();
        assert!(
            ["SELECT", "SET", "START", "SHOW"].contains(&w.as_str()),
            "{s}"
        );
    }
    assert!(sent
        .iter()
        .any(|s| s == "SET SESSION TRANSACTION READ ONLY"));
    assert!(sent
        .iter()
        .any(|s| s.starts_with("START TRANSACTION READ ONLY")));
    assert!(sent.iter().filter(|s| s.starts_with("SELECT")).count() > 10);
}

#[test]
fn the_import_works_with_a_select_only_account_and_cannot_write() {
    let mut fx = fx!("grant");
    // The DSN's account has SELECT only: a write by it fails, so an importer that
    // wrote would fail too.
    let url = std::fs::read_to_string(&fx.dsn_file).unwrap();
    let mut c = Conn::new(Opts::from_url(url.trim()).unwrap()).unwrap();
    assert!(c.query_drop("UPDATE user SET name = 'x'").is_err());
    assert!(fx.import_sqlite(&fx.options(), "o.sqlite").is_ok());
    let n: i64 = fx
        .admin
        .query_first("SELECT count(*) FROM user WHERE name = 'x'")
        .unwrap()
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn addresses_that_differ_only_in_case_are_reported_not_imported() {
    let mut fx = fx!("collide");
    fx.admin.query_drop("ALTER TABLE user MODIFY email_addr VARCHAR(191) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL").unwrap();
    insert(
        &mut fx.admin,
        "user",
        &[
            ("id", my_i(8)),
            ("create_time", my_i(1)),
            ("email_addr", my_s("BOB@example.org")),
            ("name", my_s("Bob Two")),
            ("authenticator", my_s("9999999999999999999999999999999a")),
        ],
    );
    let e = fx.import_sqlite(&fx.options(), "o.sqlite").unwrap_err();
    assert_eq!(e.kind, Kind::Refused);
    assert!(e.message.contains("[2, 8]"), "{}", e.message);
    assert!(e.message.contains("D25"));
    for s in SENTINELS {
        assert!(!e.message.contains(s));
    }
    assert!(!e.message.to_lowercase().contains("bob@example"));
}

#[test]
fn sql_files_give_the_same_database_as_the_sqlite_file() {
    let mut fx = fx!("sqlfiles");
    // A document over D1's statement limit, split into chunks.
    let big: String = (0..40_000).map(|i| format!("<l>{i}</l>\n")).collect();
    insert(
        &mut fx.admin,
        "app_version",
        &[
            ("id", my_i(9)),
            ("create_time", my_i(1)),
            ("appid", my_i(2)),
            ("version_num", my_i(100)),
            ("platformid", my_i(2)),
            ("xml_doc", my_s(&big)),
        ],
    );
    let mut opts = fx.options();
    opts.pepper = Some(PEPPER.to_vec());
    let (_, direct, _) = fx.import_sqlite(&opts, "direct.sqlite").unwrap();

    let sql_dir = fx.dir.path().join("sql");
    let mut src = Source::connect(&fx.dsn_file).unwrap();
    let mut summary = Summary::default();
    let pre = preflight(&mut src, &opts, &mut summary).unwrap();
    let mem = Connection::open_in_memory().unwrap();
    apply_migrations(&mem).unwrap();
    let tables = all_tables(&mem).unwrap();
    // Small files, so that several are written.
    let mut sink = SqlDirSink::create(&sql_dir, 20_000).unwrap();
    run(&mut src, &mut sink, &opts, &pre, &tables, &mut summary).unwrap();
    assert!(sink.files.len() > 5);
    assert!(sink.split_rows >= 1);
    assert!(
        sink.max_statement_seen <= cb_import::sink::D1_MAX_STATEMENT_BYTES,
        "{}",
        sink.max_statement_seen
    );
    let names: Vec<String> = sink
        .files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(names[0].starts_with("0001_platform"));
    for f in &sink.files {
        let text = std::fs::read_to_string(f).unwrap();
        for line in text.lines() {
            assert!(
                line.starts_with("INSERT INTO ") || line.starts_with("UPDATE "),
                "{}",
                &line[..20.min(line.len())]
            );
            assert!(line.ends_with(';'));
        }
        assert!(!text.contains("BEGIN") && !text.contains("COMMIT"));
    }
    let via = fx.dir.path().join("via.sqlite");
    load_sql_dir(&sql_dir, &via).unwrap();

    let a = Connection::open(&direct).unwrap();
    let b = Connection::open(&via).unwrap();
    for t in [
        "platform",
        "app",
        "app_version",
        "team",
        "user",
        "host",
        "host_app_version",
        "workunit",
        "result",
        "assignment",
        "batch",
        "job_file",
        "id_floor",
    ] {
        assert_eq!(dump(&a, t), dump(&b, t), "table {t}");
    }
    assert_eq!(
        q_i(&b, "SELECT length(xml_doc) FROM app_version WHERE id = 9"),
        big.len() as i64
    );
}

fn dump(c: &Connection, table: &str) -> Vec<String> {
    let mut st = c
        .prepare(&format!("SELECT * FROM {table} ORDER BY 1, 2"))
        .unwrap();
    let n = st.column_count();
    let rows = st
        .query_map([], |r| {
            let mut s = String::new();
            for i in 0..n {
                s.push_str(&format!("{:?}|", r.get_ref(i).unwrap()));
            }
            Ok(s)
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

#[test]
fn verify_passes_and_finds_a_changed_column_without_printing_values() {
    let fx = fx!("verify");
    let mut opts = fx.options();
    opts.pepper = Some(PEPPER.to_vec());
    let (_, path, _) = fx.import_sqlite(&opts, "o.sqlite").unwrap();
    let mut src = Source::connect(&fx.dsn_file).unwrap();
    let mut s = Summary::default();
    let r = verify(&mut src, &path, &opts, 1000, &mut s).unwrap();
    assert!(r.ok(), "{}", r.render());
    assert!(r.render().contains("verify: ok"));
    for (t, (a, b)) in &r.counts {
        assert_eq!(a, b, "{t}");
    }

    let t = Connection::open(&path).unwrap();
    t.execute(
        "UPDATE host SET domain_name = 'changed-value-xyz' WHERE id = 2",
        [],
    )
    .unwrap();
    t.execute("DELETE FROM assignment", []).unwrap();
    t.execute("UPDATE team SET nusers = 7 WHERE id = 2", [])
        .unwrap();
    drop(t);
    let mut src = Source::connect(&fx.dsn_file).unwrap();
    let r = verify(&mut src, &path, &opts, 1000, &mut s).unwrap();
    assert!(!r.ok());
    let text = r.render();
    assert!(text.contains("host.domain_name"), "{text}");
    assert!(text.contains("assignment") && text.contains("row count differs"));
    assert!(text.contains("team.nusers is wrong"));
    assert!(!text.contains("changed-value-xyz"));
    for s in SENTINELS {
        assert!(!text.contains(s));
    }
}

#[test]
fn nothing_private_is_printed() {
    let fx = fx!("privacy");
    let mut opts = fx.options();
    opts.pepper = Some(PEPPER.to_vec());
    let (summary, _, _) = fx.import_sqlite(&opts, "o.sqlite").unwrap();
    let out = summary.render();
    for s in SENTINELS {
        assert!(!out.contains(s), "summary shows {s}");
    }
    assert!(!out.contains("synthetic-secret"));
    assert!(!out.contains("cbimp-ro-synthetic"));
}

#[test]
fn error_paths_do_not_print_values() {
    let mut fx = fx!("errors");
    // A connection that fails: neither the account, the host nor the password is shown.
    let bad = fx.dir.path().join("bad-dsn");
    std::fs::write(
        &bad,
        "mysql://sentinel_user:sentinel-password@127.0.0.1:1/sentinel_db\n",
    )
    .unwrap();
    let e = Source::connect(&bad).err().unwrap();
    for w in [
        "sentinel_user",
        "sentinel-password",
        "sentinel_db",
        "127.0.0.1",
    ] {
        assert!(!e.message.contains(w), "{}", e.message);
    }
    std::fs::write(&bad, "not a url sentinel-password\n").unwrap();
    let e = Source::connect(&bad).err().unwrap();
    assert!(!e.message.contains("sentinel-password"));
    // A text column that is not UTF-8.
    fx.admin
        .query_drop("UPDATE app_version SET xml_doc = UNHEX('ff61') WHERE id = 1")
        .unwrap();
    let e = fx.import_sqlite(&fx.options(), "o1.sqlite").unwrap_err();
    assert!(
        e.message.contains("app_version.xml_doc of row 1"),
        "{}",
        e.message
    );
    fx.admin
        .query_drop("UPDATE app_version SET xml_doc = '<file_info></file_info>' WHERE id = 1")
        .unwrap();
    // A missing template file.
    std::fs::remove_file(fx.project.join("templates/upper_out")).unwrap();
    let e = fx.import_sqlite(&fx.options(), "o2.sqlite").unwrap_err();
    assert!(e.message.contains("templates/upper_out"));
    for s in SENTINELS {
        assert!(!e.message.contains(s));
    }
    // And without --project-dir at all.
    let mut o = fx.options();
    o.project_dir = None;
    let e = fx.import_sqlite(&o, "o3.sqlite").unwrap_err();
    assert_eq!(e.kind, Kind::Usage);
}

#[test]
fn size_classes_and_filesets_are_refused_unless_allowed() {
    let mut fx = fx!("features");
    fx.admin
        .query_drop("UPDATE workunit SET size_class = 1 WHERE id = 2")
        .unwrap();
    let e = fx.import_sqlite(&fx.options(), "o.sqlite").unwrap_err();
    assert_eq!(e.kind, Kind::Refused);
    assert!(
        e.message.contains("size classes")
            && e.message.contains("--allow-size-class")
            && e.message.contains(" 2"),
        "{}",
        e.message
    );
    // The partial output is not left for the caller: the CLI deletes it; here the file may exist.
    let mut o = fx.options();
    o.allow_size_class = true;
    let (_, path, _) = fx.import_sqlite(&o, "ok.sqlite").unwrap();
    assert_eq!(
        q_i(
            &Connection::open(path).unwrap(),
            "SELECT count(*) FROM workunit"
        ),
        4
    );
    fx.admin
        .query_drop("UPDATE workunit SET fileset_id = 4 WHERE id = 3")
        .unwrap();
    let e = fx.import_sqlite(&o, "o2.sqlite").unwrap_err();
    assert!(e.message.contains("fileset_id") && e.message.contains("--allow-fileset-id"));
    o.allow_fileset_id = true;
    assert!(fx.import_sqlite(&o, "o3.sqlite").is_ok());
}

#[test]
fn finished_all_and_recent() {
    let fx = fx!("finished");
    let mut o = fx.options();
    o.finished = Finished::All;
    let (_, p, _) = fx.import_sqlite(&o, "all.sqlite").unwrap();
    let t = Connection::open(p).unwrap();
    assert_eq!(q_i(&t, "SELECT count(*) FROM workunit"), 5);
    assert_eq!(q_i(&t, "SELECT count(*) FROM result"), 7);
    assert_eq!(q_i(&t, "SELECT count(*) FROM assignment"), 2);
    assert_eq!(q_i(&t, "SELECT count(*) FROM batch"), 2, "every batch");

    // wu1 was created at 1_700_000_001; a window that reaches back to it carries it.
    let mut o = fx.options();
    o.finished = Finished::Recent;
    o.now = 1_700_000_001 + 5 * 86400;
    o.finished_days = 10;
    let (_, p, _) = fx.import_sqlite(&o, "recent.sqlite").unwrap();
    assert_eq!(
        q_i(
            &Connection::open(p).unwrap(),
            "SELECT count(*) FROM workunit"
        ),
        5
    );
    o.finished_days = 1;
    let (_, p, _) = fx.import_sqlite(&o, "recent1.sqlite").unwrap();
    assert_eq!(
        q_i(
            &Connection::open(p).unwrap(),
            "SELECT count(*) FROM workunit"
        ),
        4
    );
}

#[test]
fn manifest_lists_the_files_to_copy() {
    let fx = fx!("manifest");
    let mut o = fx.options();
    let manifest = fx.dir.path().join("manifest.tsv");
    o.files_manifest = Some(manifest.clone());
    let (summary, _, _) = fx.import_sqlite(&o, "o.sqlite").unwrap();
    let text = std::fs::read_to_string(&manifest).unwrap();
    let lines: BTreeMap<String, Vec<String>> = text
        .lines()
        .map(|l| {
            let f: Vec<String> = l.split('\t').map(String::from).collect();
            (f[1].clone(), f)
        })
        .collect();
    assert_eq!(lines.len(), 4, "{text}");
    let flat = &lines["download/upper_2.00_windows_x86_64.exe"];
    assert_eq!(
        flat[0], "download/upper_2.00_windows_x86_64.exe",
        "a file at the top of the directory"
    );
    assert_eq!(flat[3], md5_hex(b"flat file"));
    let app = &lines["download/upper_1.00_x86_64-pc-linux-gnu"];
    assert!(
        app[0].starts_with("download/") && app[0].ends_with("/upper_1.00_x86_64-pc-linux-gnu"),
        "{app:?}"
    );
    assert_eq!(app[2], "6");
    assert_eq!(app[3], md5_hex(b"binary"));
    assert_eq!(lines["download/wu2_in"][3], md5_hex(b"hello"));
    let up = &lines["upload/wu3_r5_0"];
    assert_eq!(up[2], "3");
    assert_eq!(up[3], md5_hex(b"out"));
    assert!(summary.warnings.is_empty(), "{:?}", summary.warnings);
    assert_eq!(summary.counters["manifest upload files"], 1);
}

#[test]
fn manifest_reports_missing_and_changed_files() {
    let fx = fx!("manifest2");
    let d = fx
        .project
        .join("upload")
        .join(filename_dir("wu3_r5_0", 1024).unwrap());
    std::fs::remove_file(d.join("wu3_r5_0")).unwrap();
    let i = fx
        .project
        .join("download")
        .join(filename_dir("wu2_in", 1024).unwrap())
        .join("wu2_in");
    std::fs::write(&i, "changed!").unwrap();
    let mut o = fx.options();
    o.files_manifest = Some(fx.dir.path().join("m.tsv"));
    let (summary, _, _) = fx.import_sqlite(&o, "o.sqlite").unwrap();
    let w = summary.warnings.join("\n");
    assert!(
        w.contains("not in the stock directories") && w.contains("wu3_r5_0"),
        "{w}"
    );
    assert!(
        w.contains("MD5 that differs") && w.contains("wu2_in"),
        "{w}"
    );
}

#[test]
fn a_wrong_code_signing_key_is_a_mismatch() {
    let fx = fx!("codekey");
    let (_priv, public) = cb_crypto::generate_key_pair().unwrap();
    let mut o = fx.options();
    o.code_sign_public_key = Some(public.to_key_string());
    let e = fx.import_sqlite(&o, "o.sqlite").unwrap_err();
    assert_eq!(e.kind, Kind::Mismatch, "{}", e.message);
    assert!(e.message.contains("code-signing public key"));
}

/// `cargo test -p cb-import --test mariadb -- --ignored --nocapture throughput`
/// prints rows per second for a larger synthetic table.
#[test]
#[ignore]
fn throughput() {
    let mut fx = fx!("speed");
    let n: i64 = std::env::var("CB_IMPORT_SPEED_ROWS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300_000);
    fx.admin.query_drop(format!(
        "INSERT INTO workunit (id, create_time, appid, name, xml_doc, batch, rsc_fpops_est, rsc_fpops_bound, rsc_memory_bound, rsc_disk_bound, need_validate, canonical_resultid, canonical_credit, transition_time, delay_bound, error_mask, file_delete_state, assimilate_state, hr_class, opaque, min_quorum, target_nresults, max_error_results, max_total_results, max_success_results, result_template_file, priority, rsc_bandwidth_bound, fileset_id, app_version_id, transitioner_flags, size_class, keywords, app_version_num, n_sent) \
         SELECT seq + 1000, 1, 1, concat('swu', seq), '<workunit></workunit>', 0, 1e12, 1e13, 1e8, 1e8, 0, 0, 0, 2147483647, 86400, 0, 2, 2, 0, 0, 1, 1, 3, 5, 2, 'templates/upper_out', 0, 0, 0, 0, 0, -1, '', 0, 1 FROM seq_1_to_{n}"
    )).unwrap();
    fx.admin.query_drop(format!(
        "INSERT INTO result (id, create_time, workunitid, server_state, outcome, client_state, hostid, userid, report_deadline, sent_time, received_time, name, cpu_time, xml_doc_in, xml_doc_out, stderr_out, batch, file_delete_state, validate_state, claimed_credit, granted_credit, opaque, random, app_version_num, appid, exit_status, teamid, priority, elapsed_time, flops_estimate, app_version_id, runtime_outlier, size_class, peak_working_set_size, peak_swap_size, peak_disk_usage) \
         SELECT seq + 1000, 1, seq + 1000, 5, 1, 5, 1, 1, 1800000000, 1700000000, 1700000900, concat('sres', seq), 12.5, concat('<result><name>sres', seq, '</name><xml_signature>0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef</xml_signature></result>\n'), '<file_info><name>x</name></file_info>', 'stderr text of a finished task', 0, 2, 1, 0, 20, 0, 77, 100, 1, 0, 1, 0, 13, 1e9, 1, 0, -1, 0, 0, 0 FROM seq_1_to_{n}"
    )).unwrap();
    let mut o = fx.options();
    o.finished = Finished::All;
    o.project_dir = Some(fx.project.clone());
    let (summary, _, _) = fx.import_sqlite(&o, "speed.sqlite").unwrap();
    println!("SQLite target\n{}", summary.render());

    let mut src = Source::connect(&fx.dsn_file).unwrap();
    let mut summary = Summary::default();
    let pre = preflight(&mut src, &o, &mut summary).unwrap();
    let mem = Connection::open_in_memory().unwrap();
    apply_migrations(&mem).unwrap();
    let tables = all_tables(&mem).unwrap();
    let mut sink = SqlDirSink::create(
        &fx.dir.path().join("speedsql"),
        cb_import::sink::DEFAULT_MAX_FILE_BYTES,
    )
    .unwrap();
    run(&mut src, &mut sink, &o, &pre, &tables, &mut summary).unwrap();
    println!(
        "SQL files target ({} files)\n{}",
        sink.files.len(),
        summary.render()
    );
}

#[test]
fn stderr_with_invalid_utf8_is_carried_with_replacement_characters() {
    let mut fx = fx!("stderr");
    fx.admin
        .query_drop("UPDATE result SET stderr_out = UNHEX('ff61') WHERE id = 3")
        .unwrap();
    let (summary, path, _) = fx.import_sqlite(&fx.options(), "o.sqlite").unwrap();
    let t = Connection::open(path).unwrap();
    assert_eq!(
        q_s(&t, "SELECT stderr_out FROM result WHERE id = 3"),
        "\u{fffd}a"
    );
    assert_eq!(
        summary.counters["result.stderr_out values with invalid UTF-8 (bytes replaced)"],
        1
    );
}
