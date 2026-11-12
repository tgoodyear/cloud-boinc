//! Admin operations against SQLite: app settings that a put leaves alone,
//! an app version with many files, releasing workunits from their
//! homogeneous redundancy commitments, and changing one option of the config.

use std::cell::RefCell;

use async_trait::async_trait;
use cb_core::admin::{
    add_app_version, put_app, reset_homogeneous, NewApp, NewAppVersion, ResetTarget, ResetWhat,
    VersionFile, MAX_VERSION_FILES,
};
use cb_core::config::OptionKind;
use cb_core::model::{App, Workunit};
use cb_core::project::{
    change_config_tag, edit_config_tag, get_setting, put_setting, SETTING_CONFIG,
};
use cb_core::work::register_job_file;
use cb_core::{Config, CoreError};
use cb_crypto::md5_hex;
use cb_db::{query_one, stmt, Db, DbResult, Rows, Stmt};
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;

const CONFIG: &str = "<boinc>\n<config>\n    <long_name>Test</long_name>\n    <master_url>https://p.example/</master_url>\n    <email_from>boinc@p.example</email_from>\n    <email_from_name>Test Project</email_from_name>\n    <email_max_per_day>150</email_max_per_day>\n</config>\n<daemons>\n</daemons>\n</boinc>\n";

fn world() -> SqliteDb {
    let db = SqliteDb::with_schema().unwrap();
    block_on(put_setting(&db, SETTING_CONFIG, CONFIG, 10)).unwrap();
    db
}

fn app_named(db: &SqliteDb, name: &str) -> App {
    block_on(query_one(
        db,
        stmt!("SELECT * FROM app WHERE name = ?", name),
    ))
    .unwrap()
    .unwrap()
}

// ---- app-put ---------------------------------------------------------------

#[test]
fn a_put_changes_what_it_states_and_keeps_the_rest() {
    let db = world();
    let put = |a: NewApp| block_on(put_app(&db, 100, &a));
    put(NewApp {
        name: "w".into(),
        user_friendly_name: Some("Worker".into()),
        weight: Some(3.5),
        beta: Some(true),
        target_nresults: Some(3),
        non_cpu_intensive: Some(true),
        fraction_done_exact: Some(true),
        min_version: Some(102),
        validator: Some("webhook".into()),
        validator_url: Some("https://v.example/check".into()),
        assimilate_url: Some("https://a.example/done".into()),
        homogeneous_redundancy: Some(2),
        homogeneous_app_version: Some(true),
        ..Default::default()
    })
    .unwrap();
    let a = app_named(&db, "w");
    assert_eq!(
        (
            a.user_friendly_name.as_str(),
            a.weight,
            a.beta,
            a.target_nresults
        ),
        ("Worker", 3.5, 1, 3)
    );
    // One setting changes; nothing else does.
    put(NewApp {
        name: "w".into(),
        weight: Some(1.0),
        ..Default::default()
    })
    .unwrap();
    let b = app_named(&db, "w");
    assert_eq!(b.weight, 1.0);
    assert_eq!(b.id, a.id);
    assert_eq!(
        (
            b.user_friendly_name.as_str(),
            b.beta,
            b.target_nresults,
            b.non_cpu_intensive,
            b.fraction_done_exact,
            b.min_version,
            b.validator.as_str(),
            b.validator_url.as_str(),
            b.assimilate_url.as_str(),
            b.homogeneous_redundancy,
            b.homogeneous_app_version,
            b.deprecated
        ),
        (
            "Worker",
            1,
            3,
            1,
            1,
            102,
            "webhook",
            "https://v.example/check",
            "https://a.example/done",
            2,
            1,
            0
        )
    );
    // A stated value can be switched off, an empty URL cleared.
    put(NewApp {
        name: "w".into(),
        beta: Some(false),
        assimilate_url: Some(String::new()),
        deprecated: Some(true),
        ..Default::default()
    })
    .unwrap();
    let c = app_named(&db, "w");
    assert_eq!(
        (c.beta, c.assimilate_url.as_str(), c.deprecated),
        (0, "", 1)
    );
    assert_eq!((c.weight, c.target_nresults, c.min_version), (1.0, 3, 102));
}

#[test]
fn a_new_app_takes_the_defaults_for_what_is_left_out() {
    let db = world();
    block_on(put_app(
        &db,
        100,
        &NewApp {
            name: "n".into(),
            ..Default::default()
        },
    ))
    .unwrap();
    let a = app_named(&db, "n");
    assert_eq!(
        (
            a.user_friendly_name.as_str(),
            a.weight,
            a.beta,
            a.validator.as_str(),
            a.target_nresults,
            a.min_version,
            a.deprecated,
            a.homogeneous_redundancy
        ),
        ("n", 1.0, 0, "trivial", 0, 0, 0, 0)
    );
}

#[test]
fn the_validator_is_checked_against_what_the_app_will_have() {
    let db = world();
    let put = |a: NewApp| block_on(put_app(&db, 100, &a));
    put(NewApp {
        name: "w".into(),
        validator: Some("webhook".into()),
        validator_url: Some("https://v.example/check".into()),
        ..Default::default()
    })
    .unwrap();
    // A put that states neither keeps a working webhook.
    put(NewApp {
        name: "w".into(),
        weight: Some(2.0),
        ..Default::default()
    })
    .unwrap();
    // One that clears the URL of a webhook validator is refused, and so is
    // switching a bitwise app to a webhook with no URL.
    assert!(put(NewApp {
        name: "w".into(),
        validator_url: Some(String::new()),
        ..Default::default()
    })
    .is_err());
    put(NewApp {
        name: "b".into(),
        validator: Some("bitwise".into()),
        ..Default::default()
    })
    .unwrap();
    assert!(put(NewApp {
        name: "b".into(),
        validator: Some("webhook".into()),
        ..Default::default()
    })
    .is_err());
    assert_eq!(app_named(&db, "b").validator, "bitwise");
    assert_eq!(app_named(&db, "w").validator_url, "https://v.example/check");
}

// ---- An app version with many files ------------------------------------------

fn files(db: &SqliteDb, n: usize) -> Vec<VersionFile> {
    (0..n)
        .map(|i| {
            let name = format!("app_1.02_file{i}");
            let body = format!("contents {i}");
            block_on(register_job_file(
                db,
                &name,
                &md5_hex(body.as_bytes()),
                body.len() as u64,
                false,
                5,
            ))
            .unwrap();
            VersionFile {
                name,
                signature: "0123456789abcdef".into(),
                main_program: i == 0,
                executable: i == 0,
                open_name: if i == 0 {
                    String::new()
                } else {
                    format!("f{i}")
                },
                copy_file: i != 0,
            }
        })
        .collect()
}

fn add(db: &SqliteDb, v: &NewAppVersion) -> Result<i64, CoreError> {
    let config = Config::parse(CONFIG).unwrap();
    block_on(add_app_version(db, &config, None, 20, v))
}

#[test]
fn a_version_with_a_couple_of_hundred_files_is_registered() {
    let db = world();
    block_on(db.batch(vec![
        stmt!("INSERT INTO platform (id, create_time, name, user_friendly_name) VALUES (1, 0, 'p', 'p')"),
        stmt!("INSERT INTO app (id, create_time, name, user_friendly_name, validator) VALUES (1, 0, 'app', 'app', 'trivial')"),
    ]))
    .unwrap();
    let version = NewAppVersion {
        app: "app".into(),
        platform: "p".into(),
        version_num: 102,
        files: files(&db, 250),
        ..Default::default()
    };
    let id = add(&db, &version).unwrap();
    let doc: String = block_on(query_one::<DocRow>(
        &db,
        stmt!("SELECT xml_doc FROM app_version WHERE id = ?", id),
    ))
    .unwrap()
    .unwrap()
    .xml_doc;
    assert_eq!(doc.matches("<file_ref>").count(), 250);
    assert_eq!(doc.matches("<file_info>").count(), 250);
    assert!(doc.contains("<file_name>app_1.02_file249</file_name>"));
    // Every file is kept after the jobs using it are done.
    let kept: i64 = block_on(query_one::<N>(
        &db,
        Stmt::new("SELECT count(*) AS n FROM job_file WHERE no_delete = 1"),
    ))
    .unwrap()
    .unwrap()
    .n;
    assert_eq!(kept, 250);
    // The same version again is refused as before.
    assert!(matches!(add(&db, &version), Err(CoreError::Conflict(_))));
    // One file not staged, in the last chunk of the lookup, is found.
    let mut missing = files(&db, 200);
    missing[199].name = "not_staged".into();
    let v2 = NewAppVersion {
        version_num: 103,
        files: missing,
        ..version.clone()
    };
    assert!(matches!(add(&db, &v2), Err(CoreError::NotFound(_))));
}

#[derive(serde::Deserialize)]
struct DocRow {
    xml_doc: String,
}

#[derive(serde::Deserialize)]
struct N {
    n: i64,
}

#[test]
fn a_version_over_the_limit_is_refused() {
    let db = world();
    let too_many: Vec<VersionFile> = (0..=MAX_VERSION_FILES)
        .map(|i| VersionFile {
            name: format!("f{i}"),
            signature: "s".into(),
            main_program: i == 0,
            ..Default::default()
        })
        .collect();
    let v = NewAppVersion {
        app: "app".into(),
        platform: "p".into(),
        version_num: 1,
        files: too_many,
        ..Default::default()
    };
    let e = add(&db, &v).unwrap_err();
    assert!(e.to_string().contains("between 1 and"), "{e}");
}

// ---- Releasing workunits from HR commitments ---------------------------------

fn workunit(db: &SqliteDb, name: &str, hr_class: i32, app_version_id: i64) {
    block_on(db.run(stmt!(
        "INSERT INTO workunit (create_time, appid, name, xml_doc, rsc_fpops_est, rsc_fpops_bound, rsc_memory_bound, rsc_disk_bound, transition_time, delay_bound, min_quorum, target_nresults, max_error_results, max_total_results, max_success_results, result_template, hr_class, app_version_id) VALUES (1, 1, ?, '', 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, '', ?, ?)",
        name,
        hr_class,
        app_version_id
    )))
    .unwrap();
}

fn wu(db: &SqliteDb, name: &str) -> Workunit {
    block_on(query_one(
        db,
        stmt!("SELECT * FROM workunit WHERE name = ?", name),
    ))
    .unwrap()
    .unwrap()
}

#[test]
fn workunits_are_released_by_class_or_by_name() {
    let db = world();
    workunit(&db, "a", 258, 7);
    workunit(&db, "b", 258, 0);
    workunit(&db, "c", 386, 8);
    workunit(&db, "d", 0, 9);
    let both = ResetWhat {
        hr_class: true,
        app_version: false,
    };
    // A class, as `reset_hrclass.php`: the class only.
    let n = block_on(reset_homogeneous(&db, &ResetTarget::Class(258), both, 50)).unwrap();
    assert_eq!(n, 2);
    assert_eq!((wu(&db, "a").hr_class, wu(&db, "a").app_version_id), (0, 7));
    assert_eq!(wu(&db, "b").hr_class, 0);
    assert_eq!(wu(&db, "c").hr_class, 386);
    assert_eq!(wu(&db, "a").mod_time, 50);
    // Again: nothing left to release.
    assert_eq!(
        block_on(reset_homogeneous(&db, &ResetTarget::Class(258), both, 51)).unwrap(),
        0
    );
    // One workunit, class and app version.
    let all = ResetWhat {
        hr_class: true,
        app_version: true,
    };
    let n = block_on(reset_homogeneous(
        &db,
        &ResetTarget::Names(vec!["c".into(), "nope".into()]),
        all,
        52,
    ))
    .unwrap();
    assert_eq!(n, 1);
    assert_eq!((wu(&db, "c").hr_class, wu(&db, "c").app_version_id), (0, 0));
    // Only the app version.
    let version = ResetWhat {
        hr_class: false,
        app_version: true,
    };
    workunit(&db, "e", 258, 7);
    block_on(reset_homogeneous(
        &db,
        &ResetTarget::Names(vec!["e".into(), "a".into()]),
        version,
        53,
    ))
    .unwrap();
    assert_eq!(
        (wu(&db, "e").hr_class, wu(&db, "e").app_version_id),
        (258, 0)
    );
    assert_eq!(wu(&db, "a").app_version_id, 0);
    // The rest is untouched, and nonsense is refused.
    assert_eq!(wu(&db, "d").app_version_id, 9);
    assert!(block_on(reset_homogeneous(&db, &ResetTarget::Class(0), both, 1)).is_err());
    let nothing = ResetWhat {
        hr_class: false,
        app_version: false,
    };
    assert!(block_on(reset_homogeneous(&db, &ResetTarget::Class(5), nothing, 1)).is_err());
    // More names than one statement binds.
    let names: Vec<String> = (0..250).map(|i| format!("m{i}")).collect();
    for n in &names {
        workunit(&db, n, 258, 0);
    }
    assert_eq!(
        block_on(reset_homogeneous(&db, &ResetTarget::Names(names), both, 60)).unwrap(),
        250
    );
}

// ---- One option of the config -------------------------------------------------

#[test]
fn a_tag_is_replaced_added_and_removed_in_place() {
    // Replaced: only that element changes.
    let (new, prev) = edit_config_tag(CONFIG, "email_max_per_day", Some("20")).unwrap();
    assert_eq!(prev.as_deref(), Some("150"));
    assert_eq!(
        new,
        CONFIG.replace(
            "<email_max_per_day>150</email_max_per_day>",
            "<email_max_per_day>20</email_max_per_day>"
        )
    );
    // Added: before the end of the config, indented like the others.
    let (new, prev) = edit_config_tag(CONFIG, "maintenance_delay", Some("7200")).unwrap();
    assert_eq!(prev, None);
    assert_eq!(
        new,
        CONFIG.replace(
            "</config>",
            "    <maintenance_delay>7200</maintenance_delay>\n</config>"
        )
    );
    // Removed with its line.
    let (new, prev) = edit_config_tag(CONFIG, "email_from_name", None).unwrap();
    assert_eq!(prev.as_deref(), Some("Test Project"));
    assert_eq!(
        new,
        CONFIG.replace("    <email_from_name>Test Project</email_from_name>\n", "")
    );
    // Removing what is not there changes nothing.
    let (new, prev) = edit_config_tag(CONFIG, "dont_send_jobs", None).unwrap();
    assert_eq!((new.as_str(), prev), (CONFIG, None));
    // The daemons list is not the config: a tag of that name is not found.
    let (new, prev) = edit_config_tag(
        &CONFIG.replace("<daemons>", "<daemons><ended>9</ended>"),
        "ended",
        Some("1"),
    )
    .unwrap();
    assert_eq!(prev, None);
    assert!(new.contains("<ended>9</ended>"));
    assert!(new.contains("    <ended>1</ended>\n</config>"));
}

#[test]
fn flags_text_and_odd_layouts() {
    // A flag written `<tag/>` reads as an empty value and is replaced whole.
    let xml = "<config><master_url>https://p.example/</master_url><dont_send_jobs/><!-- <ended>1</ended> --></config>";
    let (new, prev) = edit_config_tag(xml, "dont_send_jobs", Some("0")).unwrap();
    assert_eq!(prev.as_deref(), Some(""));
    assert_eq!(
        new,
        "<config><master_url>https://p.example/</master_url><dont_send_jobs>0</dont_send_jobs><!-- <ended>1</ended> --></config>"
    );
    // Added in a config on one line, and not inside the comment.
    let (new, _) = edit_config_tag(xml, "ended", Some("1")).unwrap();
    assert_eq!(
        new,
        "<config><master_url>https://p.example/</master_url><dont_send_jobs/><!-- <ended>1</ended> --><ended>1</ended></config>"
    );
    // A tag stated twice: the loader takes the last, so that one is changed
    // (and reported as the previous value); removal takes all of them out.
    let twice = "<config>\n  <a>1</a>\n  <ban_os>x</ban_os>\n  <ban_os>y</ban_os>\n</config>";
    let (new, prev) = edit_config_tag(twice, "ban_os", Some("z")).unwrap();
    assert_eq!(prev.as_deref(), Some("y"));
    assert_eq!(
        new,
        "<config>\n  <a>1</a>\n  <ban_os>x</ban_os>\n  <ban_os>z</ban_os>\n</config>"
    );
    let (new, _) = edit_config_tag(twice, "ban_os", None).unwrap();
    assert_eq!(new, "<config>\n  <a>1</a>\n</config>");
    // Text is escaped and read back as it was.
    let (new, _) = edit_config_tag(CONFIG, "long_name", Some("A & B <c>")).unwrap();
    assert!(new.contains("<long_name>A &amp; B &lt;c></long_name>"));
    let (_, prev) = edit_config_tag(&new, "long_name", Some("x")).unwrap();
    assert_eq!(prev.as_deref(), Some("A & B <c>"));
    // What is not a tag name, a config, or a sensible value is refused.
    for bad in ["", "A", "a b", "a/b", "1a", "config", &"a".repeat(65)] {
        assert!(edit_config_tag(CONFIG, bad, Some("1")).is_err(), "{bad}");
    }
    assert!(edit_config_tag("<boinc></boinc>", "a", Some("1")).is_err());
    assert!(edit_config_tag(CONFIG, "a", Some("x\0y")).is_err());
    assert!(edit_config_tag(CONFIG, "a", Some(&"x".repeat(5000))).is_err());
}

#[test]
fn changing_a_tag_goes_through_the_checks_and_leaves_the_rest() {
    let db = world();
    let c = block_on(change_config_tag(
        &db,
        "maintenance_delay",
        Some("1800"),
        20,
    ))
    .unwrap();
    assert!(c.changed);
    assert_eq!(c.previous, None);
    let stored = block_on(get_setting(&db, SETTING_CONFIG)).unwrap().unwrap();
    assert!(stored.contains("<maintenance_delay>1800</maintenance_delay>"));
    assert!(stored.contains("<email_from>boinc@p.example</email_from>"));
    assert!(stored.contains("<email_max_per_day>150</email_max_per_day>"));
    assert!(stored.contains("<daemons>"));
    assert_eq!(Config::parse(&stored).unwrap().maintenance_delay, 1800.0);
    // The same value again: nothing written.
    let c = block_on(change_config_tag(
        &db,
        "maintenance_delay",
        Some("1800"),
        30,
    ))
    .unwrap();
    assert!(!c.changed);
    assert_eq!(c.previous.as_deref(), Some("1800"));
    // Removed.
    let c = block_on(change_config_tag(&db, "maintenance_delay", None, 40)).unwrap();
    assert!(c.changed);
    let stored = block_on(get_setting(&db, SETTING_CONFIG)).unwrap().unwrap();
    assert_eq!(stored, CONFIG);
    // A change that would break the config is refused and nothing is stored.
    for (tag, value) in [
        ("master_url", Some("ftp://x/")),
        ("master_url", None),
        ("email_from", Some("not an address")),
        ("homogeneous_redundancy", Some("9")),
    ] {
        assert!(
            block_on(change_config_tag(&db, tag, value, 50)).is_err(),
            "{tag}"
        );
    }
    assert_eq!(
        block_on(get_setting(&db, SETTING_CONFIG)).unwrap().unwrap(),
        CONFIG
    );
    // No config yet.
    let empty = SqliteDb::with_schema().unwrap();
    assert!(matches!(
        block_on(change_config_tag(&empty, "ended", Some("1"), 1)),
        Err(CoreError::NotFound(_))
    ));
}

/// A database that lets another operator write the config between this
/// operator's read and its write.
struct Raced<'a> {
    inner: &'a SqliteDb,
    other: RefCell<Option<(&'static str, &'static str)>>,
}

#[async_trait(?Send)]
impl<'a> Db for Raced<'a> {
    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<Rows>> {
        if stmts
            .iter()
            .any(|s| s.sql.starts_with("UPDATE config SET value"))
        {
            if let Some((tag, value)) = self.other.borrow_mut().take() {
                block_on(change_config_tag(self.inner, tag, Some(value), 25)).unwrap();
            }
        }
        self.inner.batch(stmts).await
    }
}

#[test]
fn two_operators_changing_different_tags_both_land() {
    let db = world();
    let raced = Raced {
        inner: &db,
        other: RefCell::new(Some(("maintenance_delay", "900"))),
    };
    let c = block_on(change_config_tag(
        &raced,
        "email_max_per_day",
        Some("20"),
        30,
    ))
    .unwrap();
    assert!(c.changed);
    assert_eq!(c.previous.as_deref(), Some("150"));
    let stored =
        Config::parse(&block_on(get_setting(&db, SETTING_CONFIG)).unwrap().unwrap()).unwrap();
    // Neither wiped the other.
    assert_eq!(stored.email_max_per_day, 20);
    assert_eq!(stored.maintenance_delay, 900.0);
    assert_eq!(stored.email_from, "boinc@p.example");
    assert_eq!(stored.email_from_name, "Test Project");
}

#[test]
fn the_same_tag_changed_twice_at_once_leaves_the_later_write() {
    let db = world();
    let raced = Raced {
        inner: &db,
        other: RefCell::new(Some(("email_max_per_day", "99"))),
    };
    let c = block_on(change_config_tag(
        &raced,
        "email_max_per_day",
        Some("20"),
        30,
    ))
    .unwrap();
    // It read again and saw the other's value as the previous one.
    assert_eq!(c.previous.as_deref(), Some("99"));
    let stored =
        Config::parse(&block_on(get_setting(&db, SETTING_CONFIG)).unwrap().unwrap()).unwrap();
    assert_eq!(stored.email_max_per_day, 20);
}

#[test]
fn the_maintenance_options_are_read() {
    let c = Config::parse(
        "<config><master_url>https://p.example/</master_url><maintenance_delay>120</maintenance_delay><min_core_client_version_announced>803</min_core_client_version_announced><min_core_client_upgrade_deadline>1800000000</min_core_client_upgrade_deadline></config>",
    )
    .unwrap();
    assert_eq!(c.maintenance_delay, 120.0);
    // The short form is multiplied by 100, as `min_core_client_version` is.
    assert_eq!(c.min_core_client_version_announced, 80300);
    assert_eq!(c.min_core_client_upgrade_deadline, 1_800_000_000);
    let d = Config::parse("<config><master_url>https://p.example/</master_url></config>").unwrap();
    assert_eq!(d.maintenance_delay, 3600.0);
    assert_eq!(d.min_core_client_version_announced, 0);
}

// ---- The config is found as the loader finds it ------------------------------

/// The value `Config::parse` ends up with for `maintenance_delay`.
fn delay(xml: &str) -> f64 {
    Config::parse(xml).unwrap().maintenance_delay
}

#[test]
fn the_active_config_is_edited_not_a_commented_one() {
    let xml = "<!-- <config><maintenance_delay>5</maintenance_delay></config> -->\n<boinc>\n<config>\n    <master_url>https://p.example/</master_url>\n    <!-- <maintenance_delay>7</maintenance_delay> -->\n    <maintenance_delay>9</maintenance_delay>\n</config>\n</boinc>\n";
    assert_eq!(delay(xml), 9.0);
    let (new, prev) = edit_config_tag(xml, "maintenance_delay", Some("1800")).unwrap();
    assert_eq!(prev.as_deref(), Some("9"));
    assert_eq!(delay(&new), 1800.0);
    // Both comments are as they were.
    assert!(new.starts_with("<!-- <config><maintenance_delay>5</maintenance_delay></config> -->"));
    assert!(new.contains("<!-- <maintenance_delay>7</maintenance_delay> -->"));
    // A tag only a comment has is added, not "changed" in the comment.
    let (new, prev) = edit_config_tag(xml, "ended", Some("1")).unwrap();
    assert_eq!(prev, None);
    assert!(Config::parse(&new).unwrap().ended);
    let (new, prev) = edit_config_tag(xml, "maintenance_delay", None).unwrap();
    assert_eq!(prev.as_deref(), Some("9"));
    assert_eq!(delay(&new), 3600.0);
    assert!(new.contains("<!-- <maintenance_delay>7</maintenance_delay> -->"));
}

#[test]
fn the_loaders_spellings_of_the_config_element_are_accepted() {
    for open in [
        "<config >",
        "<config\n>",
        "<config attr=\"1\">",
        "<config  \t>",
    ] {
        let xml = format!(
            "<boinc>{open}<master_url>https://p.example/</master_url><maintenance_delay>9</maintenance_delay></config></boinc>"
        );
        assert_eq!(delay(&xml), 9.0, "{open}");
        let (new, _) = edit_config_tag(&xml, "maintenance_delay", Some("77")).unwrap();
        assert_eq!(delay(&new), 77.0, "{open}");
        let (new, _) = edit_config_tag(&xml, "ended", Some("1")).unwrap();
        assert!(Config::parse(&new).unwrap().ended, "{open}");
    }
}

#[test]
fn cdata_whitespace_attributes_and_nesting() {
    // CDATA text is read as it is, not unescaped.
    let xml = "<config><master_url>https://p.example/</master_url><long_name><![CDATA[A &amp; B]]></long_name><email_from_name>  spaced  </email_from_name><maintenance_delay a=\"x\">9</maintenance_delay><dont_send_jobs/><nested><x>1</x><x>2</x></nested></config>";
    assert_eq!(Config::parse(xml).unwrap().long_name, "A &amp; B");
    let (new, prev) = edit_config_tag(xml, "long_name", Some("C & D")).unwrap();
    assert_eq!(prev.as_deref(), Some("A &amp; B"));
    assert_eq!(Config::parse(&new).unwrap().long_name, "C & D");
    // Attributes and surrounding space do not matter.
    let (new, prev) = edit_config_tag(xml, "maintenance_delay", Some("5")).unwrap();
    assert_eq!(prev.as_deref(), Some("9"));
    assert_eq!(delay(&new), 5.0);
    assert_eq!(Config::parse(&new).unwrap().email_from_name, "spaced");
    // An element holding elements is replaced or removed whole.
    let (new, prev) = edit_config_tag(xml, "nested", None).unwrap();
    assert_eq!(prev, None);
    assert!(!new.contains("nested"));
    assert_eq!(Config::parse(&new).unwrap().long_name, "A &amp; B");
    // A value the loader would not give back as it was is refused.
    for bad in [" lead", "trail ", "a\u{1}b"] {
        assert!(
            edit_config_tag(xml, "long_name", Some(bad)).is_err(),
            "{bad:?}"
        );
    }
    // A document that is not closed is refused.
    assert!(edit_config_tag("<config><a>1</a>", "a", Some("2")).is_err());
    assert!(edit_config_tag("<config><a>1</config>", "b", Some("2")).is_err());
}

/// For documents built from the loader's awkward spellings, an edit that is
/// accepted gives the value asked for, and no other option of the parsed
/// config changes.
#[test]
fn set_then_parse_returns_the_value_and_changes_nothing_else() {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move |n: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % n as u64) as usize
    };
    let tags = [
        (
            "maintenance_delay",
            "<maintenance_delay>%</maintenance_delay>",
        ),
        (
            "email_max_per_day",
            "<email_max_per_day>%</email_max_per_day>",
        ),
        ("long_name", "<long_name>%</long_name>"),
        ("dont_send_jobs", "<dont_send_jobs>%</dont_send_jobs>"),
        ("max_wus_to_send", "<max_wus_to_send>%</max_wus_to_send>"),
    ];
    let values = [
        "0",
        "1",
        "20",
        "7200",
        "12.5",
        "x y",
        "<![CDATA[a&b]]>",
        "a &amp; b",
        "",
    ];
    let noise = [
        "",
        "\n    ",
        "<!-- <maintenance_delay>1</maintenance_delay> -->",
        "<!-- </config> -->",
        "<unknown><maintenance_delay>3</maintenance_delay></unknown>",
        "<db_passwd>secret</db_passwd>",
        "text between",
        "<ended/>",
    ];
    let opens = ["<config>", "<config >", "<config a=\"1\">", "<config\n>"];
    let mut accepted = 0;
    for round in 0..600 {
        let mut doc = String::new();
        if next(3) == 0 {
            doc.push_str("<!-- <config><long_name>old</long_name></config> -->");
        }
        if next(2) == 0 {
            doc.push_str("<boinc>");
        }
        doc.push_str(opens[next(opens.len())]);
        doc.push_str("<master_url>https://p.example/</master_url>");
        for _ in 0..next(7) {
            doc.push_str(noise[next(noise.len())]);
            let (_, pattern) = tags[next(tags.len())];
            doc.push_str(&pattern.replace('%', values[next(values.len())]));
        }
        doc.push_str("</config>");
        let Ok(before) = Config::parse(&doc) else {
            continue;
        };
        let (tag, _) = tags[next(tags.len())];
        let asked = ["0", "1", "42", "3600", "new name", "9.5"][next(6)];
        let unset = next(4) == 0;
        let result = edit_config_tag(&doc, tag, (!unset).then_some(asked));
        let Ok((new, _)) = result else {
            continue;
        };
        accepted += 1;
        let after = Config::parse(&new).unwrap_or_else(|e| panic!("{round}: {e}\n{doc}\n{new}"));
        // Everything but the option is as it was.
        let mut expect = before.clone();
        let mut got = after.clone();
        match tag {
            "maintenance_delay" => {
                expect.maintenance_delay = 0.0;
                got.maintenance_delay = 0.0
            }
            "email_max_per_day" => {
                expect.email_max_per_day = 0;
                got.email_max_per_day = 0
            }
            "long_name" => {
                expect.long_name.clear();
                got.long_name.clear()
            }
            "dont_send_jobs" => {
                expect.dont_send_jobs = false;
                got.dont_send_jobs = false
            }
            _ => {
                expect.max_wus_to_send = 0;
                got.max_wus_to_send = 0
            }
        }
        assert_eq!(expect, got, "{round}: {tag}\n{doc}\n{new}");
        // And the option itself.
        if unset {
            let d = Config::default();
            match tag {
                "maintenance_delay" => assert_eq!(after.maintenance_delay, d.maintenance_delay),
                "email_max_per_day" => assert_eq!(after.email_max_per_day, d.email_max_per_day),
                "long_name" => assert_eq!(after.long_name, d.long_name),
                "dont_send_jobs" => assert!(!after.dont_send_jobs),
                _ => assert_eq!(after.max_wus_to_send, d.max_wus_to_send),
            }
        } else {
            // A value the option's type cannot hold (words for a number) is
            // not read back by the loader: the edit then must have been
            // refused, or the loader must see what was asked.
            let typed = match tag {
                "maintenance_delay" => asked
                    .parse::<f64>()
                    .ok()
                    .map(|v| after.maintenance_delay == v),
                "email_max_per_day" => asked
                    .parse::<i32>()
                    .ok()
                    .map(|v| after.email_max_per_day == v),
                "long_name" => Some(after.long_name == asked),
                "dont_send_jobs" => asked
                    .parse::<i32>()
                    .ok()
                    .map(|v| after.dont_send_jobs == (v != 0)),
                _ => asked
                    .parse::<i32>()
                    .ok()
                    .map(|v| after.max_wus_to_send == v),
            };
            if let Some(ok) = typed {
                assert!(ok, "{round}: {tag}={asked}\n{doc}\n{new}");
            }
        }
    }
    assert!(accepted > 300, "only {accepted} edits accepted");
}

// ---- app-put races -----------------------------------------------------------

/// A database that runs another call to `put_app` before the first write
/// of this one that is an update of the app table.
struct AppRace<'a> {
    inner: &'a SqliteDb,
    other: RefCell<Option<NewApp>>,
}

#[async_trait(?Send)]
impl<'a> Db for AppRace<'a> {
    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<Rows>> {
        if stmts.iter().any(|s| s.sql.starts_with("UPDATE app SET")) {
            if let Some(a) = self.other.borrow_mut().take() {
                block_on(put_app(self.inner, 150, &a)).unwrap();
            }
        }
        self.inner.batch(stmts).await
    }
}

fn webhook_app(db: &SqliteDb) {
    block_on(put_app(
        db,
        100,
        &NewApp {
            name: "w".into(),
            validator: Some("webhook".into()),
            validator_url: Some("https://v.example/old".into()),
            ..Default::default()
        },
    ))
    .unwrap();
}

#[test]
fn a_weight_update_does_not_undo_a_validator_change_made_meanwhile() {
    let db = world();
    webhook_app(&db);
    let raced = AppRace {
        inner: &db,
        other: RefCell::new(Some(NewApp {
            name: "w".into(),
            validator: Some("bitwise".into()),
            validator_url: Some(String::new()),
            ..Default::default()
        })),
    };
    block_on(put_app(
        &raced,
        200,
        &NewApp {
            name: "w".into(),
            weight: Some(4.0),
            ..Default::default()
        },
    ))
    .unwrap();
    let a = app_named(&db, "w");
    // Both survive.
    assert_eq!(a.weight, 4.0);
    assert_eq!(
        (a.validator.as_str(), a.validator_url.as_str()),
        ("bitwise", "")
    );
}

#[test]
fn a_url_update_checked_against_a_validator_that_then_changes_is_checked_again() {
    let db = world();
    webhook_app(&db);
    // The call states a new URL for what it read as a webhook; another call
    // makes the app bitwise first. The URL is written all the same (a
    // bitwise app may carry one), the validator stays as the other call set it.
    let raced = AppRace {
        inner: &db,
        other: RefCell::new(Some(NewApp {
            name: "w".into(),
            validator: Some("bitwise".into()),
            ..Default::default()
        })),
    };
    block_on(put_app(
        &raced,
        200,
        &NewApp {
            name: "w".into(),
            validator_url: Some("https://v.example/new".into()),
            ..Default::default()
        },
    ))
    .unwrap();
    let a = app_named(&db, "w");
    assert_eq!(
        (a.validator.as_str(), a.validator_url.as_str()),
        ("bitwise", "https://v.example/new")
    );
    // The reverse: the call clears the URL of what it read as bitwise, and
    // another call makes the app a webhook first. Checked again against the
    // new row, the clearing is refused and nothing of it is written.
    let db = world();
    block_on(put_app(
        &db,
        100,
        &NewApp {
            name: "w".into(),
            validator: Some("bitwise".into()),
            validator_url: Some("https://v.example/old".into()),
            ..Default::default()
        },
    ))
    .unwrap();
    let raced = AppRace {
        inner: &db,
        other: RefCell::new(Some(NewApp {
            name: "w".into(),
            validator: Some("webhook".into()),
            ..Default::default()
        })),
    };
    let e = block_on(put_app(
        &raced,
        200,
        &NewApp {
            name: "w".into(),
            validator_url: Some(String::new()),
            weight: Some(9.0),
            ..Default::default()
        },
    ))
    .unwrap_err();
    assert!(e.to_string().contains("https validator_url"), "{e}");
    let a = app_named(&db, "w");
    assert_eq!((a.validator.as_str(), a.weight), ("webhook", 1.0));
    assert_eq!(a.validator_url, "https://v.example/old");
}

#[test]
fn creating_an_app_that_another_call_created_first_updates_it() {
    let db = world();
    let raced = RaceOnInsert {
        inner: &db,
        done: RefCell::new(false),
    };
    block_on(put_app(
        &raced,
        100,
        &NewApp {
            name: "n".into(),
            weight: Some(5.0),
            ..Default::default()
        },
    ))
    .unwrap();
    let a = app_named(&db, "n");
    // The other call's setting and this call's both hold.
    assert_eq!(a.weight, 5.0);
    assert_eq!(a.min_version, 7);
}

struct RaceOnInsert<'a> {
    inner: &'a SqliteDb,
    done: RefCell<bool>,
}

#[async_trait(?Send)]
impl<'a> Db for RaceOnInsert<'a> {
    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<Rows>> {
        if stmts.iter().any(|s| s.sql.starts_with("INSERT INTO app")) && !*self.done.borrow() {
            *self.done.borrow_mut() = true;
            block_on(put_app(
                self.inner,
                90,
                &NewApp {
                    name: "n".into(),
                    min_version: Some(7),
                    ..Default::default()
                },
            ))
            .unwrap();
        }
        self.inner.batch(stmts).await
    }
}

/// A database that changes a staged file's MD5 between the reads and the
/// write of `add_app_version`.
struct FileSwap<'a> {
    inner: &'a SqliteDb,
}

#[async_trait(?Send)]
impl<'a> Db for FileSwap<'a> {
    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<Rows>> {
        if stmts
            .iter()
            .any(|s| s.sql.starts_with("INSERT INTO app_version"))
        {
            self.inner
                .apply("UPDATE job_file SET md5 = 'ffffffffffffffffffffffffffffffff' WHERE name = 'app_1.02_file7'")
                .unwrap();
        }
        self.inner.batch(stmts).await
    }
}

#[test]
fn a_file_replaced_while_a_version_is_registered_stops_it() {
    let db = world();
    block_on(db.batch(vec![
        stmt!("INSERT INTO platform (id, create_time, name, user_friendly_name) VALUES (1, 0, 'p', 'p')"),
        stmt!("INSERT INTO app (id, create_time, name, user_friendly_name, validator) VALUES (1, 0, 'app', 'app', 'trivial')"),
    ]))
    .unwrap();
    let version = NewAppVersion {
        app: "app".into(),
        platform: "p".into(),
        version_num: 102,
        files: files(&db, 100),
        ..Default::default()
    };
    let config = Config::parse(CONFIG).unwrap();
    let e = block_on(add_app_version(
        &FileSwap { inner: &db },
        &config,
        None,
        20,
        &version,
    ))
    .unwrap_err();
    assert!(matches!(e, CoreError::Conflict(_)), "{e}");
    // Nothing was written: no version, and the files are not marked kept.
    let n: i64 = block_on(query_one::<N>(
        &db,
        Stmt::new("SELECT count(*) AS n FROM app_version"),
    ))
    .unwrap()
    .unwrap()
    .n;
    assert_eq!(n, 0);
    let kept: i64 = block_on(query_one::<N>(
        &db,
        Stmt::new("SELECT count(*) AS n FROM job_file WHERE no_delete = 1"),
    ))
    .unwrap()
    .unwrap()
    .n;
    assert_eq!(kept, 0);
    // Registered again after the file is staged properly, it goes through.
    let version = NewAppVersion {
        files: files(&db, 100),
        ..version
    };
    db.apply("UPDATE job_file SET no_delete = 0").ok();
    assert!(add(&db, &version).is_ok());
}

/// Counts the statements sent through it.
struct Counting<'a> {
    inner: &'a SqliteDb,
    statements: std::cell::Cell<usize>,
    sql: RefCell<Vec<String>>,
}

#[async_trait(?Send)]
impl<'a> Db for Counting<'a> {
    async fn batch(&self, stmts: Vec<Stmt>) -> DbResult<Vec<Rows>> {
        self.statements.set(self.statements.get() + stmts.len());
        self.sql
            .borrow_mut()
            .extend(stmts.iter().map(|s| s.sql.clone()));
        self.inner.batch(stmts).await
    }
}

#[test]
fn a_thousand_file_version_fits_the_free_plans_statement_budget_without_scans() {
    let db = world();
    block_on(db.batch(vec![
        stmt!("INSERT INTO platform (id, create_time, name, user_friendly_name) VALUES (1, 0, 'p', 'p')"),
        stmt!("INSERT INTO app (id, create_time, name, user_friendly_name, validator) VALUES (1, 0, 'app', 'app', 'trivial')"),
    ]))
    .unwrap();
    let version = NewAppVersion {
        app: "app".into(),
        platform: "p".into(),
        version_num: 102,
        files: files(&db, MAX_VERSION_FILES),
        ..Default::default()
    };
    let counting = Counting {
        inner: &db,
        statements: Default::default(),
        sql: Default::default(),
    };
    let config = Config::parse(CONFIG).unwrap();
    block_on(add_app_version(&counting, &config, None, 20, &version)).unwrap();
    assert_eq!(counting.statements.get(), 18);
    // Every statement that touches `job_file` finds its rows by primary key.
    let mut seen = std::collections::HashSet::new();
    for sql in counting.sql.borrow().iter() {
        if !sql.contains("job_file") || !seen.insert(sql.clone()) {
            continue;
        }
        let plan = block_on(db.run(Stmt::new(format!(
            "EXPLAIN QUERY PLAN {}",
            sql.replace('?', "'[]'")
        ))))
        .unwrap();
        let text: Vec<String> = plan
            .rows
            .iter()
            .map(|r| {
                r.get("detail")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string()
            })
            .collect();
        assert!(
            !text
                .iter()
                .any(|d| d.contains("SCAN job_file") || d.contains("SCAN f")),
            "{sql}\n{text:?}"
        );
    }
}

// ---- A value the loader would skip is refused --------------------------------

/// A value that reads for the kind, and a second that reads differently.
fn samples(tag: &str, kind: OptionKind) -> (&'static str, &'static str) {
    match (tag, kind) {
        ("homogeneous_redundancy", _) => ("1", "2"),
        ("enable_delete_account", _) => ("1", "2"),
        (_, OptionKind::Str) => ("value one", "value two"),
        (_, OptionKind::Bool) => ("1", "0"),
        (_, OptionKind::Double) => ("2.5", "3.5"),
        _ => ("7", "8"),
    }
}

/// None of these is a number or a flag in any reading, so the loader skips
/// them for every option that is not text.
#[test]
fn every_option_the_loader_reads_refuses_text_it_would_skip() {
    let base = "<config>\n    <master_url>https://p.example/</master_url>\n</config>\n";
    let mut n = 0;
    for (tag, kind) in Config::option_kinds() {
        n += 1;
        if kind == OptionKind::Str {
            continue;
        }
        for bad in ["1800s", "abc", "1 2", "0x", "1.5.2", "--3"] {
            // The loader skips it: the config is as if the tag were absent.
            let xml = format!("<config><{tag}>{bad}</{tag}></config>");
            assert_eq!(
                Config::parse(&xml).unwrap(),
                Config::parse("<config></config>").unwrap(),
                "the loader reads {tag}={bad:?}"
            );
            // So `config-set` must not take it.
            assert!(
                matches!(
                    edit_config_tag(base, tag, Some(bad)),
                    Err(CoreError::Invalid(_))
                ),
                "{tag}={bad:?} was accepted"
            );
        }
    }
    assert!(n > 50, "{n} options");
}

#[test]
fn text_the_loader_skips_is_refused_by_config_set() {
    let db = world();
    for (tag, bad) in [
        ("maintenance_delay", "1800s"),
        ("email_max_per_day", "many"),
        ("max_wus_to_send", "10x"),
        ("min_core_client_upgrade_deadline", "tomorrow"),
        ("dont_send_jobs", "yes please"),
        ("homogeneous_redundancy", "9"),
    ] {
        let e = block_on(change_config_tag(&db, tag, Some(bad), 20)).unwrap_err();
        assert!(matches!(e, CoreError::Invalid(_)), "{tag}: {e}");
    }
    assert_eq!(
        block_on(get_setting(&db, SETTING_CONFIG)).unwrap().unwrap(),
        CONFIG
    );
    // The same options take what the loader reads.
    block_on(change_config_tag(
        &db,
        "maintenance_delay",
        Some("1800"),
        20,
    ))
    .unwrap();
    assert_eq!(
        Config::parse(&block_on(get_setting(&db, SETTING_CONFIG)).unwrap().unwrap())
            .unwrap()
            .maintenance_delay,
        1800.0
    );
    // A tag the loader does not read is stored as it is.
    block_on(change_config_tag(
        &db,
        "some_stock_option",
        Some("anything"),
        21,
    ))
    .unwrap();
}

/// For every option, what a set stores shows in the parsed config: set one
/// sample, then the other, and the two configs differ.
#[test]
fn a_set_is_visible_in_the_parsed_config_for_every_option() {
    let base = "<config>\n    <master_url>https://p.example/</master_url>\n</config>\n";
    let mut checked = 0;
    for (tag, kind) in Config::option_kinds() {
        // The stock layout options only make sense together (the loader
        // refuses a prefix without a short name); `tests/stock_layout.rs`
        // covers them.
        if tag.starts_with("stock_") {
            continue;
        }
        let (a, b) = samples(tag, kind);
        let (xa, _) = edit_config_tag(base, tag, Some(a)).unwrap();
        let (xb, _) = edit_config_tag(base, tag, Some(b)).unwrap();
        Config::check_option(tag, a).unwrap();
        Config::check_option(tag, b).unwrap();
        let (ca, cb) = (Config::parse(&xa).unwrap(), Config::parse(&xb).unwrap());
        // Aliases that write the same field in the same direction would
        // not differ; none of the options do.
        assert_ne!(ca, cb, "{tag}: {a} and {b} read the same");
        // And a set after a set on one document: the last value shows.
        let (xc, prev) = edit_config_tag(&xa, tag, Some(b)).unwrap();
        assert_eq!(prev.as_deref(), Some(a), "{tag}");
        assert_eq!(Config::parse(&xc).unwrap(), cb, "{tag}");
        checked += 1;
    }
    assert!(checked > 45);
}
