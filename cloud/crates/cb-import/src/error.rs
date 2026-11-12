//! Errors. The message of every error is built from counts, table and column
//! names and row IDs, never from a value read from the source database.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Bad flags or unreadable input files.
    Usage,
    /// The source database could not be read.
    Source,
    /// The target could not be written.
    Target,
    /// A row or a setting that the import refuses.
    Refused,
    /// Source and target differ (verify), or a check failed.
    Mismatch,
}

#[derive(Debug, Clone)]
pub struct ImportError {
    pub kind: Kind,
    pub message: String,
}

impl ImportError {
    pub fn new(kind: Kind, message: impl Into<String>) -> Self {
        ImportError {
            kind,
            message: message.into(),
        }
    }
    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(Kind::Usage, message)
    }
    pub fn source(message: impl Into<String>) -> Self {
        Self::new(Kind::Source, message)
    }
    pub fn target(message: impl Into<String>) -> Self {
        Self::new(Kind::Target, message)
    }
    pub fn refused(message: impl Into<String>) -> Self {
        Self::new(Kind::Refused, message)
    }
    /// The process exit status for this error.
    pub fn exit_code(&self) -> i32 {
        match self.kind {
            Kind::Usage => 2,
            Kind::Source => 3,
            Kind::Target => 4,
            Kind::Refused => 5,
            Kind::Mismatch => 6,
        }
    }
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ImportError {}

/// The one place a SQLite error is turned into text. A full rusqlite error can hold the
/// statement that failed (`SqlInputError`) or text from it, and so addresses, keys and
/// hashes from a damaged dump. Only the result code and class are shown, and the
/// column list of a constraint failure, which comes from the schema.
pub fn sqlite_message(e: &rusqlite::Error) -> String {
    use rusqlite::Error as E;
    let (code, msg): (Option<(i32, String)>, Option<&str>) = match e {
        E::SqliteFailure(f, m) => (
            Some((f.extended_code, format!("{:?}", f.code))),
            m.as_deref(),
        ),
        E::SqlInputError { error, .. } => (
            Some((error.extended_code, format!("{:?}", error.code))),
            None,
        ),
        _ => (None, None),
    };
    let Some((c, class)) = code else {
        return "SQLite error (not a database failure)".to_string();
    };
    let mut s = format!("SQLite error {c} ({class})");
    if let Some(m) = msg {
        for p in ["UNIQUE constraint failed: ", "NOT NULL constraint failed: "] {
            if let Some(cols) = m.strip_prefix(p) {
                if cols
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | ',' | ' '))
                {
                    s.push_str(&format!(": {p}{cols}"));
                }
            }
        }
    }
    s
}

impl From<rusqlite::Error> for ImportError {
    fn from(e: rusqlite::Error) -> Self {
        ImportError::target(sqlite_message(&e))
    }
}

pub type Result<T> = std::result::Result<T, ImportError>;

/// At most this many IDs are listed in a message.
pub const MAX_IDS_SHOWN: usize = 20;

/// `1, 2, 3 and 7 more` for a list of row IDs.
pub fn show_ids(ids: &[u64]) -> String {
    let shown: Vec<String> = ids
        .iter()
        .take(MAX_IDS_SHOWN)
        .map(|i| i.to_string())
        .collect();
    if ids.len() > MAX_IDS_SHOWN {
        format!(
            "{} and {} more",
            shown.join(", "),
            ids.len() - MAX_IDS_SHOWN
        )
    } else {
        shown.join(", ")
    }
}
