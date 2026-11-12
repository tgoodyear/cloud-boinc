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

impl From<rusqlite::Error> for ImportError {
    fn from(e: rusqlite::Error) -> Self {
        // SQLite messages name tables and columns, not values.
        ImportError::target(format!("SQLite: {e}"))
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
