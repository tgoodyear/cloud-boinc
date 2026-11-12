//! Files and directories the tool creates are private to the user running it: the
//! SQLite files and `.sql` files hold accounts' keys and password hashes.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
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
