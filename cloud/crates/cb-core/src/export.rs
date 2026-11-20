//! Assimilation into a results bucket.
//!
//! The project pulls finished work from the admin API (`ready_to_assimilate`,
//! `/admin/v1/outputs`, the acknowledgement). A deployment can instead let the
//! pipeline assimilate into a second bucket of its own: each ready workunit's
//! canonical outputs are copied there, a ledger object describing the workunit
//! is written after them, and only then is the workunit acknowledged. A
//! consumer then needs nothing but a token for that bucket: no admin token, no
//! call to the server, and the outputs stay there after the server deletes its
//! own copies and purges the rows.
//!
//! Layout in the results bucket:
//!
//! - `files/<workunit name>/<stored file name>`: one object per present
//!   output of the canonical result, byte for byte the uploaded file.
//! - `ready/<workunit name>.json`: the ledger, a [`Ledger`] document. It is
//!   written after every file of the workunit is in place and checked, so a
//!   ledger marks a complete workunit. A consumer lists `ready/`, fetches the
//!   files the ledger names, checks each against the ledger's size and MD5, and
//!   deletes the ledger and the files when it is done with them.
//!
//! A failed workunit (non-zero `error_mask`, no canonical result) gets a
//! ledger and no files, as the stock assimilator writes `<name>_error`.
//!
//! A crash after a copy or after the ledger, before the acknowledgement,
//! leaves the workunit ready; the next pass copies the same bytes to the same
//! keys and writes the ledger again, so a consumer can see a ledger twice and
//! should treat the workunit ID as the key. A workunit re-opened with
//! `reopen_assimilation` is exported again the same way.

use async_trait::async_trait;
use serde::Serialize;

use crate::upload::is_flat_name;
use crate::work::{
    acknowledge_assimilation_report, ready_to_assimilate, AckReport, AssimilateList, ReadyWorkunit,
};
use crate::{CoreResult, FileStat, Files};
use cb_db::Db;

/// Schema of the ledger document; raised on an incompatible change.
pub const LEDGER_SCHEMA: u32 = 1;
/// Workunits one pass takes at most. Each costs a copy and a head per output
/// and one put for its ledger, all against the buckets, not the database.
pub const EXPORT_BATCH: i64 = 25;

/// The results bucket, as far as the export needs it.
#[async_trait(?Send)]
pub trait ResultStore {
    /// Copies the output `name` (the project bucket's `upload/<name>`) to
    /// `key` in the results bucket and returns what is stored there now, or
    /// `None` when the output is not in the project bucket. `md5` is the
    /// lowercase hex MD5 the copy must have; a store that can have the
    /// write checked by it does so.
    async fn copy_output(&self, name: &str, key: &str, md5: &str) -> CoreResult<Option<FileStat>>;
    /// Stores a whole object at `key`, replacing one of the same key.
    async fn put(&self, key: &str, bytes: Vec<u8>) -> CoreResult<()>;
}

/// The key of a workunit's ledger.
pub fn ledger_key(workunit: &str) -> String {
    format!("ready/{workunit}.json")
}

/// The key of one output file.
pub fn file_key(workunit: &str, file: &str) -> String {
    format!("files/{workunit}/{file}")
}

/// The document at `ready/<workunit name>.json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Ledger<'a> {
    pub schema: u32,
    /// Unix time of the pass that wrote it.
    pub exported_at: i64,
    /// The prefix the files are under: `files/<workunit name>/`.
    pub files_prefix: String,
    /// The workunit as `GET /admin/v1/assimilate` lists it. A file with
    /// `present: false` was not copied.
    pub workunit: &'a ReadyWorkunit,
}

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExportReport {
    /// Workunits whose files and ledger were written.
    pub exported: usize,
    /// Output files copied.
    pub files: usize,
    /// Workunits left ready, with the reason; a later pass tries again.
    pub failed: Vec<(i64, String)>,
    /// The acknowledgement of the exported workunits. The caller queues a
    /// transition for each ID in `acknowledged`.
    pub ack: AckReport,
    /// More workunits were ready than this pass took.
    pub has_more: bool,
}

/// Exports up to `limit` ready workunits (1 to 500), oldest first, and
/// acknowledges the ones whose files and ledger are in place. Two database
/// statements whatever the count: the list and the acknowledgement.
pub async fn export_pass(
    db: &dyn Db,
    files: &dyn Files,
    store: &dyn ResultStore,
    limit: i64,
    now: i64,
) -> CoreResult<ExportReport> {
    let page = ready_to_assimilate(db, files, None, AssimilateList::Ready, 0, 0, limit).await?;
    let mut report = ExportReport {
        has_more: page.has_more,
        ..ExportReport::default()
    };
    let mut done = Vec::new();
    for wu in &page.workunits {
        match export_one(store, wu, now).await {
            Ok(n) => {
                report.exported += 1;
                report.files += n;
                done.push(wu.id);
            }
            Err(reason) => report.failed.push((wu.id, reason)),
        }
    }
    if !done.is_empty() {
        report.ack = acknowledge_assimilation_report(db, &done, now).await?;
    }
    Ok(report)
}

/// Copies one workunit's files and writes its ledger. Returns the number of
/// files copied, or why the workunit was left ready.
async fn export_one(
    store: &dyn ResultStore,
    wu: &ReadyWorkunit,
    now: i64,
) -> Result<usize, String> {
    if !is_flat_name(&wu.name) {
        return Err(format!("workunit name {:?} cannot be a key", wu.name));
    }
    let mut copied = 0;
    if let Some(canonical) = &wu.canonical_result {
        let mut seen = std::collections::HashSet::new();
        for f in &canonical.files {
            if !is_flat_name(&f.name) {
                return Err(format!("file name {:?} cannot be a key", f.name));
            }
            if !seen.insert(f.name.as_str()) {
                return Err(format!("file {} listed twice", f.name));
            }
            if !f.present {
                continue;
            }
            let key = file_key(&wu.name, &f.name);
            let stored = store
                .copy_output(&f.name, &key, &f.md5)
                .await
                .map_err(|e| format!("copy {}: {e}", f.name))?;
            match stored {
                None => return Err(format!("{}: not in the project bucket", f.name)),
                Some(s) if s.nbytes != f.nbytes || s.md5 != f.md5 => {
                    return Err(format!(
                        "{}: copy has {} bytes, MD5 {}; listed {} bytes, MD5 {}",
                        f.name, s.nbytes, s.md5, f.nbytes, f.md5
                    ))
                }
                Some(_) => copied += 1,
            }
        }
    }
    let ledger = Ledger {
        schema: LEDGER_SCHEMA,
        exported_at: now,
        files_prefix: format!("files/{}/", wu.name),
        workunit: wu,
    };
    let body = serde_json::to_vec(&ledger).map_err(|e| format!("ledger: {e}"))?;
    store
        .put(&ledger_key(&wu.name), body)
        .await
        .map_err(|e| format!("ledger: {e}"))?;
    Ok(copied)
}
