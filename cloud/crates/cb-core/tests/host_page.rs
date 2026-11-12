//! What the computer pages say about a host's GPUs, Docker or Podman, VirtualBox and client
//! version (difference D17 in `docs/compatibility.md`): read from `host.misc`, which the
//! scheduler stores from the request, with `host.serialnum` for a row that has no usable
//! `misc`. The hosts of the first three tests are made the way the server makes them: a request
//! from `tests/client-corpus/` goes through `handle_request`, then the pages are rendered from
//! the row it wrote.

use std::path::{Path, PathBuf};

use cb_core::plan_class::PlanClassSpecs;
use cb_core::project::{put_setting, Project, SETTING_CONFIG};
use cb_core::scheduler::{handle_request, Catalog, SchedCtx};
use cb_core::site::{site_page, Params, SiteCtx};
use cb_core::Config;
use cb_db::{stmt, Db};
use cb_proto::sched_request::parse;
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;

const CONFIG: &str = "<config>\n<long_name>Test Project</long_name>\n<master_url>https://p.example/</master_url>\n<download_url>https://p.example/download</download_url>\n<upload_url>https://p.example/upload/file_upload_handler</upload_url>\n<user_country>1</user_country>\n</config>\n";
const AUTH: &str = "0123456789abcdef0123456789abcdef";
const NOW: i64 = 1_800_000_000;

fn fixture(file: &str) -> String {
    let dir: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/client-corpus");
    std::fs::read_to_string(dir.join(file))
        .unwrap_or_else(|e| panic!("{file}: {e}"))
        .replace("fixture-authenticator", AUTH)
}

struct World {
    db: SqliteDb,
    project: Project,
}

impl World {
    /// A project with the platforms the request names and one account.
    fn new(request: &str) -> World {
        let req = parse(request).unwrap();
        let db = SqliteDb::with_schema().unwrap();
        block_on(put_setting(&db, SETTING_CONFIG, CONFIG, 10)).unwrap();
        let mut names = vec![req.platform_name.clone()];
        names.extend(req.alt_platforms.iter().cloned());
        for (i, n) in names.iter().enumerate() {
            block_on(db.run(stmt!(
                "INSERT INTO platform (id, create_time, name, user_friendly_name) VALUES (?, 0, ?, ?)",
                i as i64 + 1,
                n.as_str(),
                n.as_str()
            )))
            .unwrap();
        }
        block_on(db.run(stmt!(
            "INSERT INTO user (id, create_time, email_addr, name, authenticator, cross_project_id, show_hosts) \
             VALUES (1, 0, 'tester@example.org', 'tester', ?, 'fixture-cross-project-id', 1)",
            AUTH
        )))
        .unwrap();
        let project = block_on(Project::load(&db)).unwrap();
        World { db, project }
    }

    /// Sends the request to the scheduler; returns the host it made.
    fn rpc(&self, request: &str) -> i64 {
        let config =
            Config::parse("<config><master_url>https://p.example/</master_url></config>").unwrap();
        let plan_classes = PlanClassSpecs::default();
        let catalog = block_on(Catalog::load(&self.db)).unwrap();
        let random = || 7;
        let ctx = SchedCtx {
            db: &self.db,
            config: &config,
            plan_classes: &plan_classes,
            catalog: &catalog,
            code_sign_key: "",
            now: NOW,
            random: &random,
            client_ip: "198.51.100.9",
        };
        let out = block_on(handle_request(&ctx, request)).unwrap();
        assert!(out.host_id > 0);
        // The leaderboard lists computers with credit.
        block_on(self.db.run(stmt!(
            "UPDATE host SET total_credit = 100, expavg_credit = 10, expavg_time = ?, rpc_time = ? WHERE id = ?",
            NOW as f64,
            NOW,
            out.host_id
        )))
        .unwrap();
        out.host_id
    }

    fn set(&self, hostid: i64, misc: &str, serialnum: &str) {
        block_on(self.db.run(stmt!(
            "UPDATE host SET misc = ?, serialnum = ? WHERE id = ?",
            misc,
            serialnum,
            hostid
        )))
        .unwrap();
    }

    fn misc(&self, hostid: i64) -> String {
        block_on(
            self.db
                .run(stmt!("SELECT misc FROM host WHERE id = ?", hostid)),
        )
        .unwrap()
        .rows[0]["misc"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn page(&self, path: &str, auth: Option<&str>, q: &[(&str, &str)]) -> String {
        let q: Params = q
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let random = || 4;
        let ctx = SiteCtx {
            db: &self.db,
            project: &self.project,
            base: "https://p.example/",
            auth,
            post: false,
            pepper: None,
            now: NOW + 60,
            random: &random,
            mailer: None,
        };
        let page = block_on(site_page(&ctx, path, &q)).unwrap().unwrap();
        assert_eq!(page.status, 200, "{path}");
        page.body
    }

    /// The detail page as the account sees it, and as a visitor does.
    fn detail(&self, hostid: i64) -> (String, String) {
        let id = hostid.to_string();
        (
            self.page("/show_host_detail.php", Some(AUTH), &[("hostid", &id)]),
            self.page("/show_host_detail.php", None, &[("hostid", &id)]),
        )
    }
}

/// The value of the row of a detail page with this label.
fn row<'a>(page: &'a str, label: &str) -> &'a str {
    let start = page
        .find(&format!("<tr><th>{label}</th><td>"))
        .unwrap_or_else(|| panic!("no {label} row in {page}"));
    let rest = &page[start..];
    let from = rest.find("<td>").unwrap() + 4;
    &rest[from..rest.find("</td>").unwrap()]
}

/// An Apple-silicon Mac with an Apple GPU and Podman, whose client has `<dont_use_docker/>`.
#[test]
fn a_mac_with_an_apple_gpu_and_podman() {
    let request = fixture("8.2.11-arm64-apple-darwin/01-first-contact.request.xml");
    let w = World::new(&request);
    let host = w.rpc(&request);

    // What the scheduler stores is what `host_info_json` in `sched/sched_host.cpp` writes:
    // the same keys, in the same order, with the Metal support value as the driver and the
    // OpenCL level as 100 * major + minor.
    assert_eq!(
        w.misc(host),
        "{\n    \"client_version\": \"8.2.11\",\n    \"gpus\": [\n        {\n            \"type\": \"apple\",\n            \"model\": \"Fixture GPU\",\n            \"count\": 1,\n            \"ram_mb\": 12124,\n            \"driver_version\": \"4\",\n            \"opencl_version\": \"102\"\n        }\n    ],\n    \"docker\": {\n        \"version\": \"6.1.4\",\n        \"type\": 2\n    },\n    \"config\": {\n        \"dont_use_docker\": true,\n        \"dont_use_wsl\": false\n    }\n}"
    );

    let (own, public) = w.detail(host);
    for page in [&own, &public] {
        assert_eq!(
            row(page, "Coprocessors"),
            "1 Fixture GPU; 11.84 GB RAM; Driver: 4; OpenCL: 1.2"
        );
        assert_eq!(
            row(page, "Docker"),
            "Podman version 6.1.4<br>Config: don't use Docker"
        );
        assert_eq!(row(page, "Virtualization"), "---");
        assert_eq!(row(page, "BOINC version"), "8.2.11");
    }
    // The computer's name stays with its account.
    assert!(own.contains("fixture-host"));
    assert!(!public.contains("fixture-host"));

    // The same on the list of the account's computers and the leaderboard.
    let list = w.page("/hosts_user.php", Some(AUTH), &[]);
    assert!(list.contains("1 Fixture GPU; 11.84 GB RAM; Driver: 4; OpenCL: 1.2"));
    assert!(list.contains("<td>8.2.11</td>"));
    let top = w.page("/top_hosts.php", None, &[]);
    assert!(top.contains("1 Fixture GPU; 11.84 GB RAM; Driver: 4; OpenCL: 1.2"));
    assert!(!top.contains("fixture-host"));
}

#[test]
fn a_linux_host_with_no_gpu() {
    let request = fixture("8.0.4/01-first-contact.request.xml");
    let w = World::new(&request);
    let host = w.rpc(&request);
    // Nothing but the client version: no `gpus`, `docker`, `vbox` or `config` key.
    assert_eq!(w.misc(host), "{\n    \"client_version\": \"8.0.4\"\n}");
    let (own, public) = w.detail(host);
    for page in [&own, &public] {
        assert_eq!(row(page, "Coprocessors"), "---");
        assert_eq!(row(page, "Virtualization"), "---");
        assert_eq!(row(page, "Docker"), "---");
        assert_eq!(row(page, "BOINC version"), "8.0.4");
    }
    let list = w.page("/hosts_user.php", Some(AUTH), &[]);
    assert!(list.contains("<td>8.0.4</td>"));
    assert!(w.page("/top_hosts.php", None, &[]).contains("Linux"));
}

/// A Windows client with an empty `<wsl>` has no WSL or Docker to report, as stock.
#[test]
fn every_corpus_host_stores_what_stock_stores() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/client-corpus");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let first = path.join("01-first-contact.request.xml");
        if !first.exists() {
            continue;
        }
        let name = format!(
            "{}/01-first-contact.request.xml",
            path.file_name().unwrap().to_str().unwrap()
        );
        let request = fixture(&name);
        let req = parse(&request).unwrap();
        let w = World::new(&request);
        let host = w.rpc(&request);
        let misc: serde_json::Value = serde_json::from_str(&w.misc(host))
            .unwrap_or_else(|e| panic!("{name}: misc is not JSON: {e}"));
        // The first key is the client's version, `%d.%d.%d`.
        let text = w.misc(host);
        assert!(
            text.starts_with("{\n    \"client_version\": \""),
            "{name}: {text}"
        );
        assert_eq!(
            misc["client_version"],
            format!(
                "{}.{}.{}",
                req.core_client_major_version,
                req.core_client_minor_version,
                req.core_client_release
            ),
            "{name}"
        );
        // Only the keys `host_info_json` writes, in its order (the text is checked: a parsed
        // object sorts its keys).
        let order = ["client_version", "gpus", "wsl", "docker", "vbox", "config"];
        let keys: Vec<&str> = misc
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert!(keys.iter().all(|k| order.contains(k)), "{name}: {keys:?}");
        let at: Vec<usize> = order
            .iter()
            .filter_map(|k| text.find(&format!("\n    \"{k}\":")))
            .collect();
        assert_eq!(at.len(), keys.len(), "{name}: {text}");
        assert!(at.windows(2).all(|p| p[0] < p[1]), "{name}: {text}");
        // An empty `<wsl>` is no WSL.
        if name.contains("windows") {
            assert_eq!(keys, ["client_version"], "{name}");
        }
        // The page reads it back without a `---` where the host has a GPU.
        let (own, _) = w.detail(host);
        let has_gpu = misc.get("gpus").is_some();
        assert_eq!(row(&own, "Coprocessors") != "---", has_gpu, "{name}");
        seen += 1;
    }
    assert!(seen >= 10, "only {seen} first contacts");
}

/// A row from before `misc` was stored, with a `serialnum` in stock's old layout
/// (`html/ops/hostinfo.php` converts it).
#[test]
fn a_row_with_empty_misc_is_described_from_serialnum() {
    let request = fixture("8.0.4/01-first-contact.request.xml");
    let w = World::new(&request);
    let host = w.rpc(&request);
    w.set(
        host,
        "",
        "[BOINC|8.2.4][CUDA|NVIDIA GeForce RTX 3060 Ti|1|8191MB|57260|300][vbox|7.1.6|1|1][docker|27.0.3|1]",
    );
    let (own, public) = w.detail(host);
    for page in [&own, &public] {
        assert_eq!(
            row(page, "Coprocessors"),
            "1 NVIDIA GeForce RTX 3060 Ti; 8.00 GB RAM; Driver: 57260; OpenCL: 3.0"
        );
        assert_eq!(
            row(page, "Virtualization"),
            "Virtualbox 7.1.6<br>HW acceleration: yes<br>Enabled: yes"
        );
        assert_eq!(row(page, "Docker"), "Docker version 27.0.3");
        assert_eq!(row(page, "BOINC version"), "8.2.4");
    }
    let list = w.page("/hosts_user.php", Some(AUTH), &[]);
    assert!(list.contains("1 NVIDIA GeForce RTX 3060 Ti; 8.00 GB RAM; Driver: 57260; OpenCL: 3.0"));
    assert!(list.contains("<td>8.2.4</td>"));
    assert!(w
        .page("/top_hosts.php", None, &[])
        .contains("1 NVIDIA GeForce RTX 3060 Ti"));
}

/// Text that is not JSON, or is not what `host_info_json` writes, is not an error.
#[test]
fn malformed_misc_falls_back_without_failing() {
    let request = fixture("8.0.4/01-first-contact.request.xml");
    let w = World::new(&request);
    let host = w.rpc(&request);
    for bad in [
        "{\n    \"client_version\": \"8.0.4\",\n    \"gpus\": [",
        "not json",
        "null",
        "[1,2]",
        "{}",
        "\u{0}\u{1}",
    ] {
        w.set(host, bad, "[BOINC|8.1.2][CUDA|2|4096MB]");
        let (own, public) = w.detail(host);
        for page in [&own, &public] {
            assert_eq!(row(page, "BOINC version"), "8.1.2", "{bad:?}");
            assert_eq!(row(page, "Coprocessors"), "2 CUDA; 4.00 GB RAM", "{bad:?}");
        }
        w.page("/hosts_user.php", Some(AUTH), &[]);
        w.page("/top_hosts.php", None, &[]);
    }
    // Valid JSON of the wrong shape, and nothing to fall back to.
    w.set(
        host,
        r#"{"gpus": "x", "docker": 5, "vbox": [], "client_version": {}}"#,
        "",
    );
    let (own, _) = w.detail(host);
    assert_eq!(row(&own, "Coprocessors"), "---");
    assert_eq!(row(&own, "Docker"), "---");
    assert_eq!(row(&own, "BOINC version"), "");
    w.set(host, "garbage", "garbage");
    let (own, _) = w.detail(host);
    assert_eq!(row(&own, "BOINC version"), "---");
}

/// A client sets every string in `misc`, so none may reach a page as markup.
#[test]
fn hostile_strings_in_misc_are_escaped() {
    let request = fixture("8.0.4/01-first-contact.request.xml");
    let w = World::new(&request);
    let host = w.rpc(&request);
    let evil = "<script>alert(1)</script><img src=x onerror=alert(2)>\"'&amp;";
    let misc = serde_json::json!({
        "client_version": evil,
        "gpus": [{"type": evil, "model": evil, "count": 1, "ram_mb": 1024, "driver_version": evil, "opencl_version": evil}],
        "docker": {"version": evil, "type": 2, "wsl_distro": evil, "boinc_buda_runner_version": 3},
        "vbox": {"version": evil, "hw_accel": true},
    })
    .to_string();
    w.set(host, &misc, &format!("[BOINC|{evil}]"));
    let (own, public) = w.detail(host);
    let list = w.page("/hosts_user.php", Some(AUTH), &[]);
    let top = w.page("/top_hosts.php", None, &[]);
    for page in [&own, &public, &list, &top] {
        assert!(!page.contains("<script>alert"), "{page}");
        assert!(!page.contains("<img src=x"), "{page}");
        assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    }
    // A distribution that is not BOINC's is named by whoever set the computer up: the
    // account sees the name, a visitor does not.
    assert!(row(&own, "Docker").contains("WSL distro: &lt;script&gt;"));
    assert!(row(&public, "Docker").contains("WSL distro: other ver 3"));
    let distro_line = row(&public, "Docker")
        .split("<br>")
        .nth(1)
        .unwrap()
        .to_string();
    assert!(!distro_line.contains("alert"), "{distro_line}");
}

/// BOINC's own WSL distribution is public, with its runner version, as the rest of the row is.
#[test]
fn the_boinc_wsl_distro_is_shown_to_everyone() {
    let request = fixture("8.0.4/01-first-contact.request.xml");
    let w = World::new(&request);
    let host = w.rpc(&request);
    w.set(
        host,
        r#"{"client_version":"8.2.15","wsl":{"version":"2","use_wslc":false},"docker":{"version":"28.0.4","type":1,"wsl_distro":"boinc-buda-runner","boinc_buda_runner_version":9},"config":{"dont_use_docker":false,"dont_use_wsl":true}}"#,
        "",
    );
    let (own, public) = w.detail(host);
    for page in [&own, &public] {
        assert_eq!(
            row(page, "Docker"),
            "Docker version 28.0.4<br>WSL distro: boinc-buda-runner ver 9<br>Config: don't use WSL"
        );
    }
}

/// A Windows client with WSL: `DOCKER_INFO::get_docker_info` in `sched/sched_host.cpp` takes the
/// BOINC distribution before any other, and `host_info_json` writes the `wsl` and `docker`
/// objects with the distribution's name and runner version.
#[test]
fn a_windows_host_with_wsl_stores_and_shows_the_boinc_distro() {
    let request = fixture("8.2.15-windows_x86_64-wine/01-first-contact.request.xml");
    let distros = "    <wsl_version>2</wsl_version>\n    <wsl>\n        <distro>\n            <distro_name>Ubuntu-22.04</distro_name>\n            <os_name>Ubuntu</os_name>\n            <os_version>22.04</os_version>\n            <wsl_version>2</wsl_version>\n            <docker_version>24.0.7</docker_version>\n            <docker_type>1</docker_type>\n        </distro>\n        <distro>\n            <distro_name>boinc-buda-runner</distro_name>\n            <os_name>Ubuntu</os_name>\n            <os_version>24.04</os_version>\n            <distro_version>9</distro_version>\n            <wsl_version>2</wsl_version>\n            <docker_version>5.0.1</docker_version>\n            <docker_type>2</docker_type>\n        </distro>\n    </wsl>";
    let request = request.replace("    <wsl>\n    </wsl>", distros);
    assert!(request.contains("boinc-buda-runner"));
    let w = World::new(&request);
    let host = w.rpc(&request);
    assert_eq!(
        w.misc(host),
        "{\n    \"client_version\": \"8.2.15\",\n    \"wsl\": {\n        \"version\": \"2\",\n        \"use_wslc\": false\n    },\n    \"docker\": {\n        \"version\": \"5.0.1\",\n        \"type\": 2,\n        \"wsl_distro\": \"boinc-buda-runner\",\n        \"boinc_buda_runner_version\": 9\n    }\n}"
    );
    let (own, public) = w.detail(host);
    for page in [&own, &public] {
        assert_eq!(
            row(page, "Docker"),
            "Podman version 5.0.1<br>WSL distro: boinc-buda-runner ver 9"
        );
    }
}
