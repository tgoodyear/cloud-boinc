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
//! Every export of a workunit is a publication of its own, named
//! `<workunit id>-<run>`, where the workunit ID is never reused (a name can be,
//! once the row is purged) and `run` is random per cron invocation. Layout in
//! the results bucket:
//!
//! - `files/<publication>/<stored file name>`: one object per present output
//!   of the canonical result, byte for byte the uploaded file.
//! - `ready/<publication>.json`: the ledger, a [`Ledger`] document. It is
//!   written after every file of the publication is in place and checked, so
//!   a ledger marks a complete workunit. A consumer lists `ready/`, fetches the
//!   files the ledger names under its `files_prefix`, checks each against the
//!   ledger's size and MD5, and deletes the ledger and that prefix when it is
//!   done. Deleting one publication never touches another's files.
//! - `export/cursor.json`: where the next cron invocation starts reading the
//!   ready list ([`Cursor`]). Not for consumers.
//!
//! A failed workunit (non-zero `error_mask`, no canonical result) gets a
//! ledger and no files, as the stock assimilator writes `<name>_error`.
//!
//! A workunit is left ready, and tried again by a later pass, when a copy
//! fails or does not match, or when a required output (uploadable and not
//! `<optional/>`) is missing from the project bucket. Passes walk the ready
//! list with a cursor that survives invocations, so workunits that keep failing
//! never stop newer ones from being exported.
//!
//! A crash after the ledger, before the acknowledgement, leaves the workunit
//! ready, and so does an overlapping invocation; the workunit is then
//! published again under another run. A consumer can see a workunit twice and
//! should treat the workunit ID as the key. A workunit re-opened with
//! `reopen_assimilation` is published again the same way. Files of an attempt
//! that failed before its ledger are never named by a ledger; the bucket's
//! lifecycle rule removes them.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::upload::is_flat_name;
use crate::work::{
    acknowledge_assimilation_report, ready_to_assimilate, AckReport, AssimilateList, ReadyWorkunit,
};
use crate::{CoreResult, FileStat, Files};
use cb_db::Db;

/// Schema of the ledger document; raised on an incompatible change.
pub const LEDGER_SCHEMA: u32 = 2;
/// Workunits one pass takes at most. Each costs a copy and a head per output
/// and one put for its ledger, all against the buckets, not the database.
pub const EXPORT_BATCH: i64 = 25;
/// The key of the [`Cursor`].
pub const CURSOR_KEY: &str = "export/cursor.json";

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
    /// The object at `key`, or `None` when there is none.
    async fn get(&self, key: &str) -> CoreResult<Option<Vec<u8>>>;
}

/// The name of one export of a workunit: `<workunit id>-<run>`.
pub fn publication(workunit_id: i64, run: &str) -> String {
    format!("{workunit_id}-{run}")
}

/// The key of a publication's ledger.
pub fn ledger_key(workunit_id: i64, run: &str) -> String {
    format!("ready/{}.json", publication(workunit_id, run))
}

/// The prefix of a publication's files.
pub fn files_prefix(workunit_id: i64, run: &str) -> String {
    format!("files/{}/", publication(workunit_id, run))
}

/// The key of one output file of a publication.
pub fn file_key(workunit_id: i64, run: &str, file: &str) -> String {
    format!("{}{file}", files_prefix(workunit_id, run))
}

/// The document at `ready/<publication>.json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Ledger<'a> {
    pub schema: u32,
    /// Unix time of the pass that wrote it.
    pub exported_at: i64,
    /// The run of the invocation that wrote it.
    pub run: &'a str,
    /// The prefix the files are under: `files/<publication>/`.
    pub files_prefix: String,
    /// The workunit as `GET /admin/v1/assimilate` lists it. A file with
    /// `present: false` was not copied.
    pub workunit: &'a ReadyWorkunit,
}

/// The document at [`CURSOR_KEY`]: the next invocation lists ready workunits
/// with IDs above `after`. 0 starts from the oldest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Cursor {
    pub after: i64,
}

/// The stored cursor; a missing or unreadable one starts from the oldest.
pub async fn read_cursor(store: &dyn ResultStore) -> CoreResult<Cursor> {
    Ok(store
        .get(CURSOR_KEY)
        .await?
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default())
}

/// Stores the cursor for the next invocation.
pub async fn write_cursor(store: &dyn ResultStore, cursor: Cursor) -> CoreResult<()> {
    let body = serde_json::to_vec(&cursor).expect("a cursor serializes");
    store.put(CURSOR_KEY, body).await
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
    /// More workunits were ready after this pass's page.
    pub has_more: bool,
    /// Where the next pass starts: the last listed ID while more follow, 0
    /// once the end of the list is reached (the next pass then goes back to
    /// the oldest, failed workunits included).
    pub next_after: i64,
}

/// Exports up to `limit` ready workunits (1 to 500) with IDs above `after`,
/// oldest first, as publications of `run`, and acknowledges the ones whose
/// files and ledger are in place. Two database statements whatever the
/// count: the list and the acknowledgement.
pub async fn export_pass(
    db: &dyn Db,
    files: &dyn Files,
    store: &dyn ResultStore,
    after: i64,
    limit: i64,
    now: i64,
    run: &str,
) -> CoreResult<ExportReport> {
    let page = ready_to_assimilate(
        db,
        files,
        None,
        AssimilateList::Ready,
        after.max(0),
        0,
        limit,
    )
    .await?;
    let next_after = match page.workunits.last() {
        Some(last) if page.has_more => last.id,
        _ => 0,
    };
    let mut report = ExportReport {
        has_more: page.has_more,
        next_after,
        ..ExportReport::default()
    };
    let mut done = Vec::new();
    for wu in &page.workunits {
        match export_one(store, wu, now, run).await {
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
    run: &str,
) -> Result<usize, String> {
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
                if f.required {
                    return Err(format!(
                        "{}: a required output is not in the project bucket",
                        f.name
                    ));
                }
                continue;
            }
            let key = file_key(wu.id, run, &f.name);
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
        run,
        files_prefix: files_prefix(wu.id, run),
        workunit: wu,
    };
    let body = serde_json::to_vec(&ledger).map_err(|e| format!("ledger: {e}"))?;
    store
        .put(&ledger_key(wu.id, run), body)
        .await
        .map_err(|e| format!("ledger: {e}"))?;
    Ok(copied)
}
