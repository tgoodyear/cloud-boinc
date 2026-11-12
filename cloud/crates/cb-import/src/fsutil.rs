//! Files and directories the tool creates are private to the user running it: the
//! SQLite files and `.sql` files hold accounts' keys and password hashes.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// A new file, mode 0600 (it must not exist).
pub fn create_private_file(path: &Path) -> io::Result<File> {
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

/// Creates `dir` and any missing parents with mode 0700. A directory that exists is left as it is.
pub fn create_private_dir_all(dir: &Path) -> io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

/// An absolute, symlink-resolved spelling of a path that may not exist yet: the longest
/// existing ancestor is canonicalised and the rest is appended with `.` and `..` resolved.
pub fn resolve(p: &Path) -> PathBuf {
    use std::path::Component;
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    let mut done: Vec<std::ffi::OsString> = Vec::new();
    let mut base = PathBuf::new();
    // Split into the part that exists (canonicalised) and the rest.
    let comps: Vec<Component> = abs.components().collect();
    let mut i = comps.len();
    while i > 0 {
        let prefix: PathBuf = comps[..i].iter().collect();
        if let Ok(c) = prefix.canonicalize() {
            base = c;
            break;
        }
        i -= 1;
    }
    for c in &comps[i..] {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                done.pop();
            }
            Component::Normal(n) => done.push(n.to_os_string()),
            _ => {}
        }
    }
    for n in done {
        base.push(n);
    }
    base
}

fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(x), Ok(y)) = (std::fs::metadata(a), std::fs::metadata(b)) {
            return x.dev() == y.dev() && x.ino() == y.ino();
        }
    }
    resolve(a) == resolve(b)
}

/// Refuses outputs that collide with each other or with an input, however the paths are
/// spelled: the same path, a path inside the `.sql` directory (it could be a numbered file or
/// `import_time.txt`), or a hard link or symlink to the same file. Checked before anything is created.
pub fn check_distinct_paths(
    outputs: &[(&str, PathBuf)],
    inputs: &[(&str, PathBuf)],
) -> crate::error::Result<()> {
    let bad = |a: &str, b: &str| {
        crate::error::ImportError::usage(format!(
            "{a} and {b} name the same path or one is inside the other"
        ))
    };
    for (i, (na, pa)) in outputs.iter().enumerate() {
        let ra = resolve(pa);
        for (nb, pb) in outputs.iter().skip(i + 1) {
            let rb = resolve(pb);
            if ra == rb || same_file(pa, pb) || ra.starts_with(&rb) || rb.starts_with(&ra) {
                return Err(bad(na, nb));
            }
        }
        for (ni, pi) in inputs {
            if ra == resolve(pi) || same_file(pa, pi) {
                return Err(bad(na, ni));
            }
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn colliding_outputs_are_refused_under_any_spelling() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        let db = p.join("x.sqlite");
        let spelled = p.join("sub/../x.sqlite");
        std::fs::create_dir(p.join("sub")).unwrap();
        let sql = p.join("out");
        let dsn = p.join("dsn");
        std::fs::write(&dsn, "x").unwrap();
        let ok = |o: &[(&str, PathBuf)], i: &[(&str, PathBuf)]| check_distinct_paths(o, i);
        assert!(ok(&[("a", db.clone()), ("b", spelled)], &[]).is_err());
        assert!(ok(
            &[("a", db.clone()), ("b", p.join("y.tsv"))],
            &[("dsn", dsn.clone())]
        )
        .is_ok());
        // A manifest inside the directory of `.sql` files, named like a numbered file or the marker.
        for n in ["0001_platform.sql", "import_time.txt", "anything"] {
            assert!(
                ok(&[("dir", sql.clone()), ("m", sql.join(n))], &[]).is_err(),
                "{n}"
            );
        }
        // An output that is an input.
        assert!(ok(&[("m", dsn.clone())], &[("dsn", dsn.clone())]).is_err());
        // The same file by a symlink.
        std::os::unix::fs::symlink(&dsn, p.join("link")).unwrap();
        assert!(ok(&[("m", p.join("link"))], &[("dsn", dsn)]).is_err());
    }

    #[test]
    fn private_modes_whatever_the_umask() {
        let d = tempfile::tempdir().unwrap();
        let sub = d.path().join("a/b");
        create_private_dir_all(&sub).unwrap();
        assert_eq!(mode(&sub), 0o700);
        assert_eq!(mode(&d.path().join("a")), 0o700);
        let f = sub.join("x");
        create_private_file(&f).unwrap();
        assert_eq!(mode(&f), 0o600);
        assert!(create_private_file(&f).is_err());
    }
}
