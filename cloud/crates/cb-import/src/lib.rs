//! Imports a stock BOINC project's MySQL database into cloud-boinc's schema.
//!
//! The tool reads the stock database with SELECT statements only, in one
//! read-only transaction, and writes either a SQLite file created from
//! `cloud/migrations` or numbered `.sql` files of single-row INSERT statements
//! sized for D1. Both targets get the same rows from the same mapping. The
//! command line is in `main.rs`; `cloud/docs/migration.md` describes the
//! rules.
//!
//! Nothing here prints a value read from the source. Summaries and errors
//! carry counts, table names, column names and row IDs.

pub mod config_xml;
pub mod error;
pub mod files;
pub mod fsutil;
pub mod import;
pub mod schema;
pub mod sink;
pub mod source;
pub mod summary;
pub mod value;
pub mod verify;

pub use error::{ImportError, Kind};
pub use import::{Finished, Options, Preflight};
