//! Command line behaviour that needs no database.

use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cb-import"))
}

#[test]
fn load_sql_never_deletes_a_file_it_did_not_create() {
    let dir = tempfile::tempdir().unwrap();
    let sql = dir.path().join("sql");
    std::fs::create_dir(&sql).unwrap();
    let existing = dir.path().join("existing.sqlite");
    std::fs::write(&existing, "precious").unwrap();
    let out = bin()
        .args(["load-sql", "--sql-dir"])
        .arg(&sql)
        .arg("--target-sqlite")
        .arg(&existing)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(std::fs::read_to_string(&existing).unwrap(), "precious");

    // A load that fails removes the file it made.
    std::fs::write(
        sql.join("0001_x.sql"),
        "INSERT INTO no_such_table VALUES (1);\n",
    )
    .unwrap();
    let fresh = dir.path().join("fresh.sqlite");
    let out = bin()
        .args(["load-sql", "--sql-dir"])
        .arg(&sql)
        .arg("--target-sqlite")
        .arg(&fresh)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4));
    assert!(!fresh.exists());
}

#[test]
fn a_misspelt_tls_setting_is_a_usage_error_that_does_not_echo_the_dsn() {
    let dir = tempfile::tempdir().unwrap();
    let dsn = dir.path().join("dsn");
    std::fs::write(
        &dsn,
        "mysql://sentinel_user:sentinel-password@127.0.0.1:1/db?tls=requred\n",
    )
    .unwrap();
    let out = bin()
        .args(["import", "--source-dsn-file"])
        .arg(&dsn)
        .arg("--rows-only")
        .arg("--target-sqlite")
        .arg(dir.path().join("o.sqlite"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("tls="), "{err}");
    assert!(!err.contains("sentinel") && !err.contains("requred"));
    assert!(!dir.path().join("o.sqlite").exists());
}

const SENTINELS: &[&str] = &[
    "alice.sentinel@example.org",
    "c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1",
    "$2y$10$sentinelbcrypthashvalue",
];

fn load(sql: &str) -> (Option<i32>, String, std::path::PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().join("sql");
    std::fs::create_dir(&d).unwrap();
    std::fs::write(d.join("0001_user.sql"), sql).unwrap();
    let target = dir.path().join("t.sqlite");
    let out = bin()
        .args(["load-sql", "--sql-dir"])
        .arg(&d)
        .arg("--target-sqlite")
        .arg(&target)
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        target,
        dir,
    )
}

#[test]
fn a_damaged_sql_file_does_not_print_its_text() {
    let user = "INSERT INTO user (id, create_time, email_addr, authenticator, passwd_hash) VALUES (1, 5, 'alice.sentinel@example.org', 'c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1', '$2y$10$sentinelbcrypthashvalue');\n";
    // Truncated in the middle of a value.
    let (code, err, target, _d) = load(&user[..user.len() - 20]);
    assert_eq!(code, Some(4));
    assert!(err.contains("failed at statement 1"), "{err}");
    for s in SENTINELS {
        assert!(!err.contains(s), "{err}");
    }
    assert!(!target.exists());
    // A syntax error with the values after it, and a wrong column count.
    for bad in [
        user.replace("INSERT INTO", "INSERT INTOO"),
        user.replace("(id, create_time,", "(id,"),
        format!("{user}{}", user.replace("VALUES (1,", "VALUES (2,")),
    ] {
        let (code, err, _t, _d) = load(&bad);
        assert_eq!(code, Some(4), "{err}");
        for s in SENTINELS {
            assert!(!err.contains(s), "{err}");
        }
    }
    // A constraint failure names the column, not the value.
    let (_, err, _t, _d) = load(&format!(
        "{user}{}",
        user.replace("VALUES (1,", "VALUES (2,")
    ));
    assert!(
        err.contains("statement 2") && err.contains("UNIQUE constraint failed: user."),
        "{err}"
    );
}

#[cfg(unix)]
#[test]
fn a_loaded_sqlite_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let (code, _err, target, _d) = load("");
    assert_eq!(code, Some(0));
    assert_eq!(
        std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn output_paths_that_collide_are_refused_before_anything_is_created() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    let dsn = p.join("dsn");
    std::fs::write(&dsn, "mysql://u:p@127.0.0.1:1/db\n").unwrap();
    std::fs::create_dir(p.join("sub")).unwrap();
    let run = |args: &[&std::ffi::OsStr]| {
        bin()
            .args(["import", "--source-dsn-file"])
            .arg(&dsn)
            .arg("--project-dir")
            .arg(p)
            .args(args)
            .output()
            .unwrap()
    };
    let db = p.join("x.sqlite");
    let spelled = p.join("sub/../x.sqlite");
    let sql = p.join("out");
    let cases: Vec<Vec<std::path::PathBuf>> = vec![
        // manifest = target, two spellings
        vec![
            "--target-sqlite".into(),
            db.clone(),
            "--files-manifest".into(),
            spelled,
        ],
        // manifest inside the .sql directory, named like a numbered file
        vec![
            "--target-sql-dir".into(),
            sql.clone(),
            "--files-manifest".into(),
            sql.join("0001_platform.sql"),
        ],
        // manifest = the DSN file
        vec![
            "--target-sqlite".into(),
            db.clone(),
            "--files-manifest".into(),
            dsn.clone(),
        ],
    ];
    for c in cases {
        let a: Vec<&std::ffi::OsStr> = c.iter().map(|x| x.as_os_str()).collect();
        let out = run(&a);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stderr).contains("same path"));
    }
    assert!(!db.exists() && !sql.exists());
    assert_eq!(
        std::fs::read_to_string(&dsn).unwrap(),
        "mysql://u:p@127.0.0.1:1/db\n"
    );
}
