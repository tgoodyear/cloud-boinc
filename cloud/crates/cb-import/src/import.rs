//! The import: what is read from the stock database, how each row becomes a
//! row of the target, and in what order it is written.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use cb_core::avg::recent_key;
use cb_crypto::{is_md5_hex, pepper_passwd_hash, verify_md5, PublicKey};

use crate::config_xml::StockConfig;
use crate::error::{show_ids, ImportError, Result};
use crate::files::{doc_files, find_in_dir, size_and_md5, FileCollector};
use crate::schema::{TableInfo, TABLES};
use crate::sink::Sink;
use crate::source::{Source, SrcCol, SrcRow};
use crate::summary::{Summary, TableStat};
use crate::value::{coerce, CoerceError, Sv, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finished {
    /// Workunits with nothing left to do stay on the stock server.
    Leave,
    /// Every workunit and result.
    All,
    /// Open workunits and those created in the last `finished_days` days.
    Recent,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub finished: Finished,
    pub finished_days: u32,
    pub allow_size_class: bool,
    pub allow_fileset_id: bool,
    /// The server's `PASSWD_PEPPER`. With it a bare-MD5 password hash is
    /// stored as this server stores it; without it every hash is carried unchanged.
    pub pepper: Option<Vec<u8>>,
    pub project_dir: Option<PathBuf>,
    pub download_dir: Option<PathBuf>,
    pub upload_dir: Option<PathBuf>,
    pub config: Option<StockConfig>,
    /// Text of the code-signing public key file.
    pub code_sign_public_key: Option<String>,
    pub skip_valid_counts: bool,
    pub files_manifest: Option<PathBuf>,
    /// Rows only: no project directory is needed. A result template that cannot be
    /// read is counted and stored empty, and no manifest is written. The output is
    /// not loadable for a real move.
    pub rows_only: bool,
    /// How many file names a warning lists (0: counts only).
    pub list_missing: usize,
    /// Unix time, for `Finished::Recent`.
    pub now: i64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            finished: Finished::Leave,
            finished_days: 0,
            allow_size_class: false,
            allow_fileset_id: false,
            pepper: None,
            project_dir: None,
            download_dir: None,
            upload_dir: None,
            config: None,
            code_sign_public_key: None,
            skip_valid_counts: false,
            files_manifest: None,
            rows_only: false,
            list_missing: 0,
            now: 0,
        }
    }
}

// ------------------------------------------------------------ password hashes

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswdKind {
    Empty,
    /// 32 hex digits: the client's `md5(password + email)`, the old form.
    Md5,
    /// PHP `password_hash()` output (`$2y$...`, `$argon2id$...`).
    Php,
    Other,
}

pub fn classify_passwd(h: &str) -> PasswdKind {
    if h.is_empty() {
        PasswdKind::Empty
    } else if is_md5_hex(h) || (h.len() == 32 && h.chars().all(|c| c.is_ascii_hexdigit())) {
        PasswdKind::Md5
    } else if h.starts_with('$') {
        PasswdKind::Php
    } else {
        PasswdKind::Other
    }
}

// ------------------------------------------------------------ preflight

#[derive(Debug, Default)]
pub struct Preflight {
    pub src_cols: HashMap<String, Vec<SrcCol>>,
    /// Workunits that are open, sorted. Empty for `Finished::All`.
    pub wu_ids: Vec<u64>,
    /// Workunits the pipeline still has work on: unsent, inactive or in-progress results,
    /// results awaiting validation, `need_validate` set, or not yet assimilated.
    pub open_ids: HashSet<u64>,
    pub open_unsent_or_in_progress: u64,
    pub open_awaiting_validation: u64,
    pub open_awaiting_assimilation: u64,
    pub kept_for_highest_id: u64,
    pub auto_inc: HashMap<String, i64>,
    pub valid_user: HashMap<u64, i64>,
    pub valid_host: HashMap<u64, i64>,
    pub total_workunits: i64,
    pub total_results: i64,
}

fn group_ids(rows: &BTreeMap<String, Vec<u64>>) -> Vec<Vec<u64>> {
    rows.values().filter(|v| v.len() > 1).cloned().collect()
}

fn id_list(src: &mut Source, sql: &str, col: &str) -> Result<Vec<u64>> {
    let mut v = Vec::new();
    src.query(sql, |r| {
        if let Some(i) = r.int(col) {
            if i > 0 {
                v.push(i as u64);
            }
        }
        Ok(())
    })?;
    Ok(v)
}

pub fn preflight(src: &mut Source, opts: &Options, summary: &mut Summary) -> Result<Preflight> {
    let mut pre = Preflight::default();
    for t in TABLES.iter().filter(|t| **t != "job_file") {
        let cols = src.columns(t)?;
        pre.src_cols.insert(t.to_string(), cols);
    }
    for required in [
        "platform",
        "app",
        "app_version",
        "user",
        "host",
        "workunit",
        "result",
    ] {
        if pre.src_cols[required].is_empty() {
            return Err(ImportError::source(format!(
                "the source database has no table {required}; is it a BOINC project database?"
            )));
        }
    }
    for t in ["team", "host_app_version", "assignment", "batch"] {
        if pre.src_cols[t].is_empty() {
            summary.note(format!("the source has no table {t}; none is imported"));
        }
    }

    // Accounts: addresses are compared by this server as stored, so two that
    // differ only in case or surrounding spaces would collide.
    let mut emails: HashMap<String, u64> = HashMap::new();
    let mut collisions: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut empty_auth: Vec<u64> = Vec::new();
    let mut empty_email: Vec<u64> = Vec::new();
    src.query("SELECT id, email_addr, authenticator FROM `user`", |r| {
        let id = r.int("id").unwrap_or(0) as u64;
        let e = r
            .text("email_addr")
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        if e.is_empty() {
            empty_email.push(id);
        }
        if r.text("authenticator").unwrap_or_default().is_empty() {
            empty_auth.push(id);
        }
        if let Some(first) = emails.get(&e) {
            let ids = collisions.entry(e).or_insert_with(|| vec![*first]);
            ids.push(id);
        } else {
            emails.insert(e, id);
        }
        Ok(())
    })?;
    drop(emails);
    if !empty_auth.is_empty() || !empty_email.is_empty() {
        return Err(ImportError::refused(format!(
            "accounts without an account key (ids {}) or without an address (ids {}) cannot be imported; fix them on the stock server first",
            show_ids(&empty_auth),
            show_ids(&empty_email)
        )));
    }
    let groups = group_ids(&collisions);
    if !groups.is_empty() {
        let mut msg = format!(
            "{} groups of accounts have addresses that differ only in letter case or surrounding spaces; this server compares addresses as stored, so they cannot all be imported (D25). Merge or change them on the stock server, then run again. Account ids per group:",
            groups.len()
        );
        for g in groups.iter().take(20) {
            msg.push_str(&format!(
                " [{}]",
                g.iter()
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        return Err(ImportError::refused(msg));
    }
    if !pre.src_cols["team"].is_empty() {
        let mut names: HashMap<String, u64> = HashMap::new();
        let mut team_coll: Vec<Vec<u64>> = Vec::new();
        src.query("SELECT id, name FROM `team`", |r| {
            let id = r.int("id").unwrap_or(0) as u64;
            let n = r.text("name").unwrap_or_default().to_lowercase();
            if let Some(first) = names.get(&n) {
                team_coll.push(vec![*first, id]);
            } else {
                names.insert(n, id);
            }
            Ok(())
        })?;
        if !team_coll.is_empty() {
            return Err(ImportError::refused(format!(
                "{} pairs of teams have names that differ only in letter case; team names are unique without regard to case here. Team id pairs: {:?}",
                team_coll.len(),
                team_coll.iter().take(20).collect::<Vec<_>>()
            )));
        }
    }

    // The code-signing key against the signatures in the app versions.
    if let Some(text) = &opts.code_sign_public_key {
        let key = PublicKey::parse(text).map_err(|_| {
            ImportError::usage("the --code-sign-public-key file is not a BOINC public key file")
        })?;
        let (mut ok, mut bad, mut none) = (0u64, 0u64, 0u64);
        let mut bad_versions: Vec<u64> = Vec::new();
        src.query("SELECT id, xml_doc FROM `app_version`", |r| {
            let id = r.int("id").unwrap_or(0) as u64;
            let doc = r.text("xml_doc").unwrap_or_default();
            let mut this_bad = false;
            for f in doc_files(&doc) {
                // `update_versions` writes no `<md5_cksum>` for an application file; it is
                // the MD5 of the file in the download directory.
                let md5 = f.md5.clone().or_else(|| {
                    if !cb_core::upload::is_flat_name(&f.name) {
                        return None;
                    }
                    let dir = opts
                        .download_dir
                        .clone()
                        .or_else(|| opts.project_dir.as_ref().map(|p| p.join("download")))?;
                    let fanout = opts.config.as_ref().map(|c| c.fanout).unwrap_or(0);
                    size_and_md5(&find_in_dir(&dir, &f.name, fanout)?).map(|(_, m)| m)
                });
                match (&md5, &f.signature) {
                    (Some(m), Some(s)) => {
                        if verify_md5(m, s, &key) {
                            ok += 1;
                        } else {
                            bad += 1;
                            this_bad = true;
                        }
                    }
                    _ => none += 1,
                }
            }
            if this_bad {
                bad_versions.push(id);
            }
            Ok(())
        })?;
        summary.count(
            "app version files signed with the given code-signing key",
            ok,
        );
        summary.count(
            "app version files that could not be checked (no signature, or no MD5 in the document or file)",
            none,
        );
        if bad > 0 {
            return Err(ImportError::new(
                crate::error::Kind::Mismatch,
                format!(
                    "the code-signing public key does not verify {bad} file signatures in app versions {}; clients pin the key, so a different one detaches every client",
                    show_ids(&bad_versions)
                ),
            ));
        }
    }

    // Counts and AUTO_INCREMENT values.
    pre.total_workunits = src.scalar("SELECT count(*) FROM `workunit`")?.unwrap_or(0);
    pre.total_results = src.scalar("SELECT count(*) FROM `result`")?.unwrap_or(0);
    for t in ["user", "host", "team", "workunit", "result"] {
        if let Some(a) = src.auto_increment(t)? {
            pre.auto_inc.insert(t.to_string(), a);
        }
    }

    // Which workunits are carried.
    {
        let mut open: BTreeSet<u64> = BTreeSet::new();
        // Results not yet sent, or in progress, or inactive.
        let a = id_list(
            src,
            "SELECT DISTINCT workunitid FROM `result` WHERE server_state IN (1, 2, 4)",
            "workunitid",
        )?;
        pre.open_unsent_or_in_progress = a.len() as u64;
        open.extend(a);
        // Results uploaded and not yet validated.
        let b = id_list(
            src,
            "SELECT DISTINCT workunitid FROM `result` WHERE server_state = 5 AND outcome = 1 AND validate_state = 0",
            "workunitid",
        )?;
        let before = open.len();
        open.extend(b);
        pre.open_awaiting_validation = (open.len() - before) as u64;
        // Workunits the validator or assimilator has not finished.
        let c = id_list(
            src,
            "SELECT id FROM `workunit` WHERE need_validate <> 0 OR assimilate_state <> 2",
            "id",
        )?;
        let before = open.len();
        open.extend(c);
        pre.open_awaiting_assimilation = (open.len() - before) as u64;
        // These are swept soon after the load (see `Ctx::workunit`).
        pre.open_ids = open.iter().copied().collect();
    }
    if opts.finished != Finished::All {
        let mut open: BTreeSet<u64> = pre.open_ids.iter().copied().collect();
        if opts.finished == Finished::Recent {
            let since = opts.now - opts.finished_days as i64 * 86400;
            let d = id_list(
                src,
                &format!("SELECT id FROM `workunit` WHERE create_time >= {since}"),
                "id",
            )?;
            open.extend(d);
        }
        // The highest workunit and result IDs: new rows take `max(id) + 1`, and
        // the purge keeps these two for that reason.
        let before = open.len();
        if let Some(m) = src.scalar("SELECT max(id) FROM `workunit`")? {
            open.insert(m as u64);
        }
        if let Some(m) = src.scalar("SELECT max(id) FROM `result`")? {
            let w = id_list(
                src,
                &format!("SELECT workunitid FROM `result` WHERE id = {m}"),
                "workunitid",
            )?;
            open.extend(w);
        }
        pre.kept_for_highest_id = (open.len() - before) as u64;
        pre.wu_ids = open.into_iter().collect();
    }

    // Valid results per account and per computer, for the leaderboards.
    if !opts.skip_valid_counts {
        src.query(
            "SELECT userid, count(*) AS n FROM `result` WHERE validate_state = 1 AND userid > 0 GROUP BY userid",
            |r| {
                if let (Some(u), Some(n)) = (r.int("userid"), r.int("n")) {
                    pre.valid_user.insert(u as u64, n);
                }
                Ok(())
            },
        )?;
        src.query(
            "SELECT hostid, count(*) AS n FROM `result` WHERE validate_state = 1 AND hostid > 0 GROUP BY hostid",
            |r| {
                if let (Some(h), Some(n)) = (r.int("hostid"), r.int("n")) {
                    pre.valid_host.insert(h as u64, n);
                }
                Ok(())
            },
        )?;
    }
    Ok(pre)
}

// ------------------------------------------------------------ mapping

/// Source columns read although the target has no column for them.
fn needed_extra(table: &str) -> &'static [&'static str] {
    match table {
        "workunit" => &["result_template_file", "size_class", "fileset_id"],
        _ => &[],
    }
}

pub struct TableMapper {
    pub info: TableInfo,
    carried: Vec<(usize, String)>,
    select: String,
    pub made_up: Vec<String>,
    pub dropped: Vec<String>,
}

impl TableMapper {
    pub fn new(info: &TableInfo, src_cols: &[SrcCol]) -> TableMapper {
        let mut carried = Vec::new();
        let mut select = Vec::new();
        let mut dropped = Vec::new();
        let extra = needed_extra(&info.name);
        for c in src_cols {
            if let Some(i) = info.index(&c.name) {
                carried.push((i, c.name.clone()));
                if c.is_time() {
                    select.push(format!("UNIX_TIMESTAMP(`{0}`) AS `{0}`", c.name));
                } else {
                    select.push(format!("`{}`", c.name));
                }
            } else if extra.contains(&c.name.as_str()) {
                select.push(format!("`{}`", c.name));
            } else {
                dropped.push(c.name.clone());
            }
        }
        let have: HashSet<&str> = src_cols.iter().map(|c| c.name.as_str()).collect();
        let made_up = info
            .cols
            .iter()
            .filter(|c| !have.contains(c.name.as_str()))
            .filter(|c| !(info.name == "workunit" && c.name == "result_template"))
            .map(|c| c.name.clone())
            .collect();
        TableMapper {
            info: info.clone(),
            carried,
            select: select.join(", "),
            made_up,
            dropped,
        }
    }

    pub fn select(&self, where_clause: &str) -> String {
        format!(
            "SELECT {} FROM `{}` {}",
            self.select, self.info.name, where_clause
        )
    }
}

#[derive(Default)]
pub struct Ctx {
    pub pepper: Option<Vec<u8>>,
    pub project_dir: Option<PathBuf>,
    pub rows_only: bool,
    pub open_ids: HashSet<u64>,
    pub now: i64,
    pub allow_size_class: bool,
    pub allow_fileset_id: bool,
    pub validators: BTreeMap<String, String>,
    pub valid_user: HashMap<u64, i64>,
    pub valid_host: HashMap<u64, i64>,
    templates: HashMap<String, String>,
    pub team_ids: HashSet<u64>,
    pub batch_ids: BTreeSet<u64>,
    pub size_class_wus: Vec<u64>,
    pub fileset_wus: Vec<u64>,
    pub max_id: HashMap<String, i64>,
    pub summary: Summary,
    pub files: Option<FileCollector>,
    pub unused_pepper_warned: bool,
}

fn get<'a>(info: &TableInfo, vals: &'a [Value], col: &str) -> &'a Value {
    &vals[info.index(col).expect("column of the target")]
}

fn set(info: &TableInfo, vals: &mut [Value], col: &str, v: Value) {
    if let Some(i) = info.index(col) {
        vals[i] = v;
    }
}

impl Ctx {
    pub fn from_options(opts: &Options, pre: &Preflight) -> Ctx {
        Ctx {
            pepper: opts.pepper.clone(),
            project_dir: opts.project_dir.clone(),
            rows_only: opts.rows_only,
            open_ids: pre.open_ids.clone(),
            now: opts.now,
            allow_size_class: opts.allow_size_class,
            allow_fileset_id: opts.allow_fileset_id,
            validators: opts
                .config
                .as_ref()
                .map(|c| c.validators.clone())
                .unwrap_or_default(),
            valid_user: pre.valid_user.clone(),
            valid_host: pre.valid_host.clone(),
            ..Default::default()
        }
    }

    fn template(&mut self, file: &str, wu: i64) -> Result<String> {
        if file.is_empty() {
            return Ok(String::new());
        }
        if let Some(t) = self.templates.get(file) {
            return Ok(t.clone());
        }
        let text = match &self.project_dir {
            Some(dir) => std::fs::read_to_string(dir.join(file)).ok(),
            None => None,
        };
        let Some(text) = text else {
            if self.rows_only {
                self.summary.count(
                    "workunits whose result template could not be read (stored empty)",
                    1,
                );
                return Ok(String::new());
            }
            return Err(if self.project_dir.is_none() {
                ImportError::usage(
                    "workunits are imported and --project-dir is not given; the result template files are read from it (--rows-only skips them for a rehearsal)",
                )
            } else {
                // The file name is a value of the source; the workunit ID finds it.
                ImportError::refused(format!(
                    "cannot read the result template file (under --project-dir) that workunit {wu} names in result_template_file"
                ))
            });
        };
        self.templates.insert(file.to_string(), text.clone());
        Ok(text)
    }

    /// Makes the target row for one source row.
    pub fn map(&mut self, m: &TableMapper, row: &SrcRow) -> Result<Vec<Value>> {
        let info = &m.info;
        let mut vals: Vec<Value> = (0..info.cols.len()).map(|i| info.default_of(i)).collect();
        let id = row.int("id").unwrap_or(0);
        for (i, name) in &m.carried {
            let col = &info.cols[*i];
            let sv = row.get(name).unwrap_or(&Sv::Null);
            match coerce(sv, col.kind) {
                Ok(Value::Null) => {}
                Ok(v) => vals[*i] = v,
                Err(CoerceError::NotUtf8) if info.name == "result" && col.name == "stderr_out" => {
                    // The client's standard error is diagnostic text and may hold any
                    // bytes; the documents and preferences are not changed.
                    if let Sv::Bytes(b) = sv {
                        vals[*i] = Value::Text(String::from_utf8_lossy(b).into_owned());
                    }
                    self.summary.count(
                        "result.stderr_out values with invalid UTF-8 (bytes replaced)",
                        1,
                    );
                }
                Err(CoerceError::NotUtf8) => {
                    return Err(ImportError::refused(format!(
                        "{}.{} of row {id} is not valid UTF-8 text",
                        info.name, col.name
                    )))
                }
                Err(CoerceError::NotNumber) => {
                    return Err(ImportError::refused(format!(
                        "{}.{} of row {id} is not a number",
                        info.name, col.name
                    )))
                }
            }
        }
        match info.name.as_str() {
            "user" => self.user(info, &mut vals, id),
            "host" => {
                let key = recent_key(
                    get(info, &vals, "expavg_credit").as_f64().unwrap_or(0.0),
                    get(info, &vals, "expavg_time").as_f64().unwrap_or(0.0),
                );
                set(info, &mut vals, "recent_key", Value::Real(key));
                let n = self.valid_host.get(&(id as u64)).copied().unwrap_or(0);
                set(info, &mut vals, "valid_results", Value::Int(n));
            }
            "team" => {
                // Loaded with 0 members: the triggers of 0009_teams.sql count them as the
                // accounts are inserted.
                set(info, &mut vals, "nusers", Value::Int(0));
                let name = get(info, &vals, "name")
                    .as_str()
                    .unwrap_or("")
                    .to_lowercase();
                set(info, &mut vals, "name_lc", Value::Text(name));
                let key = recent_key(
                    get(info, &vals, "expavg_credit").as_f64().unwrap_or(0.0),
                    get(info, &vals, "expavg_time").as_f64().unwrap_or(0.0),
                );
                set(info, &mut vals, "recent_key", Value::Real(key));
            }
            "app" => {
                let name = get(info, &vals, "name").as_str().unwrap_or("").to_string();
                let kind = match self.validators.get(&name).map(|s| s.as_str()) {
                    Some("bitwise") => "bitwise",
                    Some("other") => {
                        self.summary
                            .count("apps whose stock validator is project code", 1);
                        "trivial"
                    }
                    Some(_) => "trivial",
                    None => {
                        self.summary
                            .count("apps with no validator daemon known (left trivial)", 1);
                        "trivial"
                    }
                };
                set(info, &mut vals, "validator", Value::Text(kind.into()));
            }
            "workunit" => self.workunit(info, &mut vals, row, id)?,
            _ => {}
        }
        Ok(vals)
    }

    fn user(&mut self, info: &TableInfo, vals: &mut [Value], id: i64) {
        let email = get(info, vals, "email_addr")
            .as_str()
            .unwrap_or("")
            .trim()
            .to_lowercase();
        if get(info, vals, "email_addr").as_str() != Some(email.as_str()) {
            self.summary
                .count("account addresses changed (lower-cased or trimmed)", 1);
        }
        set(info, vals, "email_addr", Value::Text(email));
        let prev = get(info, vals, "previous_email_addr")
            .as_str()
            .unwrap_or("")
            .trim()
            .to_lowercase();
        set(info, vals, "previous_email_addr", Value::Text(prev));

        let hash = get(info, vals, "passwd_hash")
            .as_str()
            .unwrap_or("")
            .to_string();
        match classify_passwd(&hash) {
            PasswdKind::Empty => self.summary.count("password hashes: empty", 1),
            PasswdKind::Php => self.summary.count(
                "password hashes: PHP password_hash form, carried unchanged",
                1,
            ),
            PasswdKind::Other => self
                .summary
                .count("password hashes: other form, carried unchanged", 1),
            PasswdKind::Md5 => match &self.pepper {
                Some(p) => {
                    let h = pepper_passwd_hash(p, &hash.to_ascii_lowercase());
                    set(info, vals, "passwd_hash", Value::Text(h));
                    self.summary
                        .count("password hashes: bare MD5, converted with the pepper", 1);
                }
                None => self
                    .summary
                    .count("password hashes: bare MD5, carried unchanged", 1),
            },
        }
        let n = self.valid_user.get(&(id as u64)).copied().unwrap_or(0);
        set(info, vals, "valid_results", Value::Int(n));
        let key = recent_key(
            get(info, vals, "expavg_credit").as_f64().unwrap_or(0.0),
            get(info, vals, "expavg_time").as_f64().unwrap_or(0.0),
        );
        set(info, vals, "recent_key", Value::Real(key));
        let team = get(info, vals, "teamid").as_i64().unwrap_or(0);
        if team > 0 && !self.team_ids.contains(&(team as u64)) {
            self.summary
                .count("accounts whose team is not in the team table", 1);
        }
    }

    fn workunit(
        &mut self,
        info: &TableInfo,
        vals: &mut [Value],
        row: &SrcRow,
        id: i64,
    ) -> Result<()> {
        if row.int("size_class").unwrap_or(-1) >= 0 {
            self.size_class_wus.push(id as u64);
        }
        if row.int("fileset_id").unwrap_or(0) != 0 {
            self.fileset_wus.push(id as u64);
        }
        // The stock transitioner's schedule does not carry over, and loading SQL
        // enqueues nothing: the sweep takes workunits with `transition_time`
        // before now. A workunit the pipeline still has work on is made due now, and
        // its first transition recomputes the real time (`due_workunits` and
        // `transitioner_pass` in cb-core). Finished workunits keep theirs.
        if self.open_ids.contains(&(id as u64)) {
            let t = get(info, vals, "transition_time").as_i64().unwrap_or(0);
            if t > self.now {
                set(info, vals, "transition_time", Value::Int(self.now));
                self.summary
                    .count("open workunits made due for the sweep (transition_time set to the import time)", 1);
            }
        }
        let file = row.text("result_template_file").unwrap_or_default();
        let text = self.template(&file, id)?;
        set(info, vals, "result_template", Value::Text(text));
        Ok(())
    }

    /// Called with each row after it is written.
    pub fn observe(&mut self, info: &TableInfo, vals: &[Value]) {
        let id = |c: &str| get(info, vals, c).as_i64().unwrap_or(0);
        match info.name.as_str() {
            "user" | "host" | "team" => {
                let m = self.max_id.entry(info.name.clone()).or_insert(0);
                *m = (*m).max(id("id"));
                if info.name == "team" {
                    self.team_ids.insert(id("id") as u64);
                }
            }
            "workunit" => {
                let m = self.max_id.entry("workunit".into()).or_insert(0);
                *m = (*m).max(id("id"));
                let b = id("batch");
                if b > 0 {
                    self.batch_ids.insert(b as u64);
                }
                if let Some(f) = &mut self.files {
                    f.workunit(
                        id("create_time"),
                        get(info, vals, "xml_doc").as_str().unwrap_or(""),
                    );
                }
            }
            "app_version" => {
                if let Some(f) = &mut self.files {
                    f.app_version(
                        id("create_time"),
                        get(info, vals, "xml_doc").as_str().unwrap_or(""),
                    );
                }
            }
            "result" => {
                let m = self.max_id.entry("result".into()).or_insert(0);
                *m = (*m).max(id("id"));
                if let Some(f) = &mut self.files {
                    f.result(
                        id("server_state"),
                        id("outcome"),
                        id("file_delete_state"),
                        get(info, vals, "xml_doc_in").as_str().unwrap_or(""),
                        get(info, vals, "xml_doc_out").as_str().unwrap_or(""),
                    );
                }
            }
            _ => {}
        }
    }
}

// ------------------------------------------------------------ the run

fn copy_table(
    src: &mut Source,
    sink: &mut dyn Sink,
    m: &TableMapper,
    ctx: &mut Ctx,
    queries: &[String],
) -> Result<TableStat> {
    let t0 = Instant::now();
    sink.begin_table(&m.info)?;
    let mut read = 0u64;
    for q in queries {
        src.query(q, |row| {
            read += 1;
            let vals = ctx.map(m, &row)?;
            sink.row(&m.info, &vals)?;
            ctx.observe(&m.info, &vals);
            Ok(())
        })?;
    }
    let written = sink.end_table(&m.info)?;
    Ok(TableStat {
        name: m.info.name.clone(),
        read,
        written,
        secs: t0.elapsed().as_secs_f64(),
        made_up: m.made_up.clone(),
        dropped: m.dropped.clone(),
    })
}

fn id_chunks(ids: &[u64]) -> Vec<String> {
    ids.chunks(500)
        .map(|c| {
            c.iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect()
}

/// Queries (SELECT statements) that read the rows of `table` that are carried.
fn queries_for(
    table: &str,
    m: &TableMapper,
    opts: &Options,
    pre: &Preflight,
    ctx: &Ctx,
) -> Vec<String> {
    let all = opts.finished == Finished::All;
    match table {
        "workunit" if !all => id_chunks(&pre.wu_ids)
            .iter()
            .map(|c| m.select(&format!("WHERE id IN ({c}) ORDER BY id")))
            .collect(),
        "result" if !all => id_chunks(&pre.wu_ids)
            .iter()
            .map(|c| m.select(&format!("WHERE workunitid IN ({c}) ORDER BY id")))
            .collect(),
        "assignment" if !all => id_chunks(&pre.wu_ids)
            .iter()
            .map(|c| m.select(&format!("WHERE workunitid IN ({c}) ORDER BY id")))
            .collect(),
        "batch" if !all => {
            let ids: Vec<u64> = ctx.batch_ids.iter().copied().collect();
            id_chunks(&ids)
                .iter()
                .map(|c| m.select(&format!("WHERE id IN ({c}) ORDER BY id")))
                .collect()
        }
        "host_app_version" => vec![m.select("ORDER BY host_id, app_version_id")],
        _ => vec![m.select("ORDER BY id")],
    }
}

/// Reads the stock database and writes every carried table to `sink`.
pub fn run(
    src: &mut Source,
    sink: &mut dyn Sink,
    opts: &Options,
    pre: &Preflight,
    target: &BTreeMap<String, TableInfo>,
    summary: &mut Summary,
) -> Result<Ctx> {
    let t0 = Instant::now();
    let mut ctx = Ctx::from_options(opts, pre);
    let (fanout, origins) = match &opts.config {
        Some(c) => {
            let mut o = Vec::new();
            for u in [&c.master_url, &c.download_url] {
                if let Some(origin) = cb_core::stock::url_origin(u) {
                    o.push(origin.to_string());
                }
            }
            (c.fanout, o)
        }
        None => (0, Vec::new()),
    };
    ctx.files = Some(FileCollector::new(
        fanout,
        opts.project_dir.clone(),
        opts.download_dir.clone(),
        opts.upload_dir.clone(),
        origins,
    ));
    ctx.summary = std::mem::take(summary);

    for table in TABLES.iter().filter(|t| **t != "job_file") {
        let cols = &pre.src_cols[*table];
        if cols.is_empty() {
            continue;
        }
        let m = TableMapper::new(&target[*table], cols);
        let queries = queries_for(table, &m, opts, pre, &ctx);
        let stat = copy_table(src, sink, &m, &mut ctx, &queries)?;
        ctx.summary.tables.push(stat);
        if *table == "workunit" {
            check_unsupported(&ctx, opts)?;
        }
    }

    // job_file, from the documents of everything carried.
    let jf = &target["job_file"];
    let t1 = Instant::now();
    let rows = ctx
        .files
        .as_ref()
        .map(|f| f.job_file_rows(jf, &mut ctx.summary))
        .unwrap_or_default();
    sink.begin_table(jf)?;
    for r in &rows {
        sink.row(jf, r)?;
    }
    let written = sink.end_table(jf)?;
    ctx.summary.tables.push(TableStat {
        name: "job_file".into(),
        read: rows.len() as u64,
        written,
        secs: t1.elapsed().as_secs_f64(),
        made_up: vec![],
        dropped: vec![],
    });

    if let Some(f) = &ctx.files {
        let n = f.refused_names;
        if n > 0 {
            ctx.summary.count(
                "file names refused (not a flat file name; no file was looked up for them)",
                n,
            );
        }
    }
    if let (Some(path), false) = (&opts.files_manifest, opts.rows_only) {
        if let Some(f) = ctx.files.take() {
            f.write_manifest(path, opts.list_missing, &mut ctx.summary)?;
            ctx.files = Some(f);
        }
    }

    // IDs of accounts, computers and teams are never given out twice: the
    // floor is the highest imported ID or the stock AUTO_INCREMENT, whichever is higher.
    let mut floors = Vec::new();
    for t in ["user", "host", "team"] {
        let imported = ctx.max_id.get(t).copied().unwrap_or(0);
        let stock = pre.auto_inc.get(t).map(|a| a - 1).unwrap_or(0);
        let f = imported.max(stock);
        if f > 0 {
            floors.push((t.to_string(), f));
        }
    }
    if opts.now > 0 {
        sink.set_import_time(opts.now)?;
    }
    sink.finish(&floors)?;

    notes(&mut ctx, opts, pre);
    ctx.summary.total_secs = t0.elapsed().as_secs_f64();
    *summary = std::mem::take(&mut ctx.summary);
    Ok(ctx)
}

fn check_unsupported(ctx: &Ctx, _opts: &Options) -> Result<()> {
    if !ctx.size_class_wus.is_empty() && !ctx.allow_size_class {
        return Err(ImportError::refused(format!(
            "{} workunits use size classes, which this server does not have (workunit ids {}). They would be sent to any host. Run again with --allow-size-class to import them without the class",
            ctx.size_class_wus.len(),
            show_ids(&ctx.size_class_wus)
        )));
    }
    if !ctx.fileset_wus.is_empty() && !ctx.allow_fileset_id {
        return Err(ImportError::refused(format!(
            "{} workunits use locality scheduling (fileset_id), which this server does not have (workunit ids {}). Run again with --allow-fileset-id to import them without it",
            ctx.fileset_wus.len(),
            show_ids(&ctx.fileset_wus)
        )));
    }
    Ok(())
}

fn notes(ctx: &mut Ctx, opts: &Options, pre: &Preflight) {
    if opts.rows_only {
        ctx.summary.warn("ROWS ONLY: no files manifest was written, and result templates that could not be read are stored empty. This output is for a rehearsal of the row mapping and is NOT loadable for a real move");
    }
    let wu = ctx
        .summary
        .tables
        .iter()
        .find(|t| t.name == "workunit")
        .map(|t| t.written as i64)
        .unwrap_or(0);
    let res = ctx
        .summary
        .tables
        .iter()
        .find(|t| t.name == "result")
        .map(|t| t.written as i64)
        .unwrap_or(0);
    let s = &mut ctx.summary;
    s.count("workunits in the source", pre.total_workunits as u64);
    s.count("results in the source", pre.total_results as u64);
    match opts.finished {
        Finished::Leave => {
            s.count(
                "workunits not carried (finished, left on the stock server)",
                (pre.total_workunits - wu).max(0) as u64,
            );
            s.count(
                "results not carried (finished, left on the stock server)",
                (pre.total_results - res).max(0) as u64,
            );
            s.note("finished workunits and their results stay on the stock server; with --finished all they are carried");
        }
        Finished::Recent => {
            s.count(
                "workunits not carried (finished, older than the window)",
                (pre.total_workunits - wu).max(0) as u64,
            );
            s.count(
                "results not carried (finished, older than the window)",
                (pre.total_results - res).max(0) as u64,
            );
        }
        Finished::All => {}
    }
    s.count(
        "open workunits: with unsent, inactive or in-progress results",
        pre.open_unsent_or_in_progress,
    );
    s.count(
        "open workunits: results uploaded and not yet validated",
        pre.open_awaiting_validation,
    );
    s.count(
        "open workunits: waiting for validation or assimilation",
        pre.open_awaiting_assimilation,
    );
    s.count(
        "workunits kept only for the highest workunit and result IDs",
        pre.kept_for_highest_id,
    );
    for t in ["workunit", "result"] {
        if let Some(a) = pre.auto_inc.get(t) {
            let top = ctx.max_id.get(t).copied().unwrap_or(0);
            let gap = (*a - 1 - top).max(0) as u64;
            if gap > 0 {
                ctx.summary.count(
                    &format!(
                        "{t} ids above the highest imported {t} id in the stock AUTO_INCREMENT"
                    ),
                    gap,
                );
            }
        }
    }
    if opts.pepper.is_none()
        && ctx
            .summary
            .counters
            .contains_key("password hashes: bare MD5, carried unchanged")
    {
        ctx.summary.note("bare-MD5 password hashes were carried as they are because no --passwd-pepper-file was given; this server does not accept them as passwords (accounts sign in with the account key or a mailed link; weak account keys stay valid)");
    }
    ctx.summary.note("every account, computer and team is imported; the id floors are set so new ones never reuse an ID");
}
