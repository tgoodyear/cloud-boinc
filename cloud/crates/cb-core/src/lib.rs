//! Scheduler and job pipeline logic of cloud-boinc.
//!
//! Everything here is written against three small traits: [`cb_db::Db`] for
//! the database, [`Files`] for the object store and [`Webhook`] for outbound
//! HTTP. The Workers supply D1, R2 and `fetch`; the tests supply SQLite and
//! in-memory fakes.

pub mod admin;
pub mod avg;
pub mod config;
pub mod countries;
pub mod mail;
pub mod model;
pub mod pipeline;
pub mod plan_class;
pub mod prefs;
pub mod project;
pub mod purge;
pub mod scheduler;
pub mod site;
pub mod site_account;
pub mod site_email;
pub mod template;
pub mod upload;
pub mod web;
pub mod work;

use async_trait::async_trait;

pub use config::Config;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Db(#[from] cb_db::DbError),
    #[error("crypto: {0}")]
    Crypto(String),
    #[error("xml: {0}")]
    Xml(String),
    #[error("files: {0}")]
    Files(String),
    #[error("{0}")]
    Invalid(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
}

impl From<cb_crypto::Error> for CoreError {
    fn from(e: cb_crypto::Error) -> Self {
        CoreError::Crypto(e.to_string())
    }
}

pub type CoreResult<T> = Result<T, CoreError>;

/// `INT_MAX`: the `transition_time` of a workunit with nothing pending.
pub const NEVER: i64 = 2147483647;

/// Which half of the bucket a file lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    /// App version files and job inputs, served under `/download`.
    Download,
    /// Result outputs, written through `/upload/file_upload_handler`.
    Upload,
    /// Purged workunit and result rows. Nothing serves these.
    Archive,
    /// Pieces of unfinished uploads, `<name>/<start>` (see [`upload`]).
    Partial,
}

impl FileKind {
    pub fn prefix(self) -> &'static str {
        match self {
            FileKind::Download => "download/",
            FileKind::Upload => "upload/",
            FileKind::Archive => "archive/",
            FileKind::Partial => "partial/",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileStat {
    pub nbytes: u64,
    /// Lowercase hex MD5 computed by the server when the file was stored.
    pub md5: String,
}

/// The object store, as far as the core logic needs it.
#[async_trait(?Send)]
pub trait Files {
    async fn stat(&self, kind: FileKind, name: &str) -> CoreResult<Option<FileStat>>;
    /// Deleting a file that does not exist is not an error.
    async fn delete(&self, kind: FileKind, names: &[String]) -> CoreResult<()>;
    /// Stores a whole object, replacing one of the same name.
    async fn put(&self, kind: FileKind, name: &str, bytes: Vec<u8>) -> CoreResult<()>;
}

/// Outbound JSON POST for validator and assimilation webhooks.
#[async_trait(?Send)]
pub trait Webhook {
    async fn post_json(&self, url: &str, body: &serde_json::Value)
        -> CoreResult<serde_json::Value>;
}
