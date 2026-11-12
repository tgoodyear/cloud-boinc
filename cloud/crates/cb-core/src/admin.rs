//! Project administration: what upstream does with `xadd`, `update_versions`
//! and the ops pages. Every function is one admin API operation.

use std::collections::HashMap;

use cb_crypto::{verify_md5, PublicKey};
use cb_db::{query, query_one, stmt, Db, Stmt, Value};
use cb_proto::upload::is_valid_filename;
use cb_proto::xml::xml_escape;
use serde::{Deserialize, Serialize};

use crate::hr::HR_NTYPES;
use crate::model::{App, AppVersion, JobResult, Platform, Workunit};
use crate::{Config, CoreError, CoreResult};

/// Most names in one `IN (...)` list; D1 allows 100 bound parameters.
const MAX_IN_LIST: usize = 90;
/// Most files in one app version. Each statement takes 90 names, and a
/// request may run about 50 statements on D1's free plan, so this leaves the
/// batches of `add_app_version` well within that.
pub const MAX_VERSION_FILES: usize = 1000;
/// Longest description of an app version. D1 limits a row to 2 MB; the
/// scheduler keeps every version's description in memory and sends it in
/// replies.
pub const MAX_VERSION_XML: usize = 1024 * 1024;

fn invalid<T>(msg: impl Into<String>) -> CoreResult<T> {
    Err(CoreError::Invalid(msg.into()))
}

fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 254
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct NewPlatform {
    pub name: String,
    /// Empty: the platform's name for a new platform, and the name it has
    /// for an existing one.
    pub user_friendly_name: String,
    /// Absent: not deprecated for a new platform, and as it is for an
    /// existing one.
    pub deprecated: Option<bool>,
}

/// What `put_platform` did, as `tools/xadd` reports it ("added platform",
/// "updated user friendly name", "already in DB").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PutAction {
    Added,
    Updated,
    Unchanged,
}

/// The platform `put_platform` created or found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PutPlatform {
    pub id: i64,
    pub action: PutAction,
}

/// Creates the platform or updates the one with this name, as `tools/xadd`
/// does for each `<platform>` of `project.xml`, with these differences (see
/// "Admin API and CLI" in `docs/architecture.md`):
///
/// - An existing platform changes only in the fields the request states.
///   `xadd` never touches `deprecated` and updates only the user-friendly
///   name; a request here without `deprecated` (the CLI's `platform-put`
///   does not send it unless asked to) must not bring a deprecated platform
///   back, and one without a user-friendly name must not replace it with the
///   platform's name.
/// - The name is looked up exactly; MySQL's collation makes `xadd`'s lookup
///   ignore case.
/// - The name must be at most 254 characters of letters, digits, `.`, `_`
///   and `-`; `xadd` puts the text of `project.xml` into SQL as it is.
pub async fn put_platform(db: &dyn Db, now: i64, p: &NewPlatform) -> CoreResult<PutPlatform> {
    if !is_name(&p.name) {
        return invalid("platform: bad name");
    }
    let friendly = if p.user_friendly_name.is_empty() {
        &p.name
    } else {
        &p.user_friendly_name
    };
    // The read and the write are one batch, which runs as one transaction
    // on both back ends, so the row the action is judged by is the one the
    // write found, whatever else is writing. The upsert also holds when the
    // platform did not exist at the read: the fields the request leaves out
    // keep what the row has.
    let mut out = db
        .batch(vec![
            stmt!("SELECT * FROM platform WHERE name = ?", p.name.as_str()),
            stmt!(
                "INSERT INTO platform (create_time, name, user_friendly_name, deprecated) \
                 VALUES (?, ?, ?, ?) \
                 ON CONFLICT (name) DO UPDATE SET \
                 user_friendly_name = CASE WHEN ? THEN excluded.user_friendly_name \
                     ELSE platform.user_friendly_name END, \
                 deprecated = CASE WHEN ? THEN excluded.deprecated ELSE platform.deprecated END \
                 RETURNING id",
                now,
                p.name.as_str(),
                friendly.as_str(),
                p.deprecated.unwrap_or(false),
                !p.user_friendly_name.is_empty(),
                p.deprecated.is_some()
            ),
        ])
        .await?
        .into_iter();
    let existing: Option<Platform> = out.next().unwrap_or_default().first()?;
    let rows = out.next().unwrap_or_default();
    let action = match &existing {
        None => PutAction::Added,
        Some(e)
            if (p.user_friendly_name.is_empty()
                || e.user_friendly_name == p.user_friendly_name)
                && p.deprecated.is_none_or(|d| (e.deprecated != 0) == d) =>
        {
            PutAction::Unchanged
        }
        Some(_) => PutAction::Updated,
    };
    Ok(PutPlatform {
        id: rows.scalar_i64().unwrap_or(0),
        action,
    })
}

/// An app to create or change. A setting left out (`None`) keeps the stored
/// value of an existing app, and takes the default for a new one.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct NewApp {
    pub name: String,
    /// New apps: the name.
    pub user_friendly_name: Option<String>,
    /// New apps: 1.
    pub weight: Option<f64>,
    pub beta: Option<bool>,
    /// Above 1 turns on adaptive replication for the app.
    pub target_nresults: Option<i32>,
    pub non_cpu_intensive: Option<bool>,
    pub fraction_done_exact: Option<bool>,
    pub min_version: Option<i32>,
    pub deprecated: Option<bool>,
    /// `trivial`, `bitwise` or `webhook`. New apps: `trivial`.
    pub validator: Option<String>,
    pub validator_url: Option<String>,
    pub assimilate_url: Option<String>,
    /// Homogeneous redundancy type: 0 for none, 1 fine, 2 coarse.
    pub homogeneous_redundancy: Option<i32>,
    /// Send all results of a workunit with one app version.
    pub homogeneous_app_version: Option<bool>,
}

/// Creates the app or updates the one with this name. The settings the call
/// states are written; the others stay as they are. (`xadd` has no such
/// call: it rewrites the app from `project.xml`.)
///
/// An update writes only the columns the call states, so a call that states
/// one setting cannot undo another operator's change to another. The
/// validator is the one setting that is checked against the rest of the row
/// (a webhook needs a URL): the check runs on the row as it was read, and
/// the update carries the values it read of the validator and URL it did not
/// itself state, so it changes nothing if either has changed since. It reads
/// again and checks again, up to five times, as `change_config_tag` does.
pub async fn put_app(db: &dyn Db, now: i64, a: &NewApp) -> CoreResult<i64> {
    if !is_name(&a.name) {
        return invalid("app: bad name");
    }
    if a.assimilate_url
        .as_ref()
        .is_some_and(|u| !u.is_empty() && !u.starts_with("https://"))
    {
        return invalid("app: assimilate_url must be https");
    }
    if let Some(t) = a.homogeneous_redundancy {
        if !(0..HR_NTYPES).contains(&t) {
            return invalid("app: homogeneous_redundancy must be 0 (none), 1 (fine) or 2 (coarse)");
        }
    }
    let friendly = a
        .user_friendly_name
        .as_deref()
        .filter(|f| !f.is_empty())
        .map(str::to_string);
    let flag = |x: Option<bool>| x.map(i32::from);
    // Column, value, for each setting the call states.
    let mut stated: Vec<(&str, Value)> = Vec::new();
    {
        let mut add = |column: &'static str, v: Option<Value>| {
            if let Some(v) = v {
                stated.push((column, v));
            }
        };
        add("user_friendly_name", friendly.clone().map(Value::from));
        add("weight", a.weight.map(Value::from));
        add("beta", flag(a.beta).map(Value::from));
        add("target_nresults", a.target_nresults.map(Value::from));
        add(
            "non_cpu_intensive",
            flag(a.non_cpu_intensive).map(Value::from),
        );
        add(
            "fraction_done_exact",
            flag(a.fraction_done_exact).map(Value::from),
        );
        add("min_version", a.min_version.map(Value::from));
        add("deprecated", flag(a.deprecated).map(Value::from));
        add("validator", a.validator.clone().map(Value::from));
        add("validator_url", a.validator_url.clone().map(Value::from));
        add("assimilate_url", a.assimilate_url.clone().map(Value::from));
        add(
            "homogeneous_redundancy",
            a.homogeneous_redundancy.map(Value::from),
        );
        add(
            "homogeneous_app_version",
            flag(a.homogeneous_app_version).map(Value::from),
        );
    }
    let check = |validator: &str, url: &str| -> CoreResult<()> {
        if !["trivial", "bitwise", "webhook"].contains(&validator) {
            return invalid("app: validator must be trivial, bitwise or webhook");
        }
        if validator == "webhook" && !url.starts_with("https://") {
            return invalid("app: the webhook validator needs an https validator_url");
        }
        Ok(())
    };
    for _ in 0..5 {
        let existing: Option<App> = query_one(
            db,
            stmt!("SELECT * FROM app WHERE name = ?", a.name.as_str()),
        )
        .await?;
        let Some(e) = existing else {
            // A new app: what is left out takes its default.
            let validator = a.validator.clone().unwrap_or_else(|| "trivial".into());
            let url = a.validator_url.clone().unwrap_or_default();
            check(&validator, &url)?;
            let rows = db
                .run(stmt!(
                    "INSERT INTO app (create_time, name, user_friendly_name, weight, beta, \
                     target_nresults, non_cpu_intensive, fraction_done_exact, min_version, \
                     deprecated, validator, validator_url, assimilate_url, \
                     homogeneous_redundancy, homogeneous_app_version) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                     ON CONFLICT (name) DO NOTHING RETURNING id",
                    now,
                    a.name.as_str(),
                    friendly.clone().unwrap_or_else(|| a.name.clone()),
                    a.weight.unwrap_or(1.0),
                    flag(a.beta).unwrap_or(0),
                    a.target_nresults.unwrap_or(0),
                    flag(a.non_cpu_intensive).unwrap_or(0),
                    flag(a.fraction_done_exact).unwrap_or(0),
                    a.min_version.unwrap_or(0),
                    flag(a.deprecated).unwrap_or(0),
                    validator,
                    url,
                    a.assimilate_url.clone().unwrap_or_default(),
                    a.homogeneous_redundancy.unwrap_or(0),
                    flag(a.homogeneous_app_version).unwrap_or(0)
                ))
                .await?;
            match rows.scalar_i64() {
                Some(id) => return Ok(id),
                // Created by another call meanwhile: read it and update.
                None => continue,
            }
        };
        if stated.is_empty() {
            return Ok(e.id);
        }
        let mut sql = String::new();
        let mut set: Vec<String> = Vec::new();
        let mut params: Vec<Value> = Vec::new();
        for (column, v) in &stated {
            set.push(format!("{column} = ?"));
            params.push(v.clone());
        }
        sql.push_str(&format!("UPDATE app SET {} WHERE id = ?", set.join(", ")));
        params.push(e.id.into());
        // The validator and URL are checked together, so the update holds
        // the ones it did not state to what the check read.
        if a.validator.is_some() || a.validator_url.is_some() {
            let validator = a.validator.clone().unwrap_or_else(|| e.validator.clone());
            let url = a
                .validator_url
                .clone()
                .unwrap_or_else(|| e.validator_url.clone());
            check(&validator, &url)?;
            if a.validator.is_none() {
                sql.push_str(" AND validator = ?");
                params.push(e.validator.clone().into());
            }
            if a.validator_url.is_none() {
                sql.push_str(" AND validator_url = ?");
                params.push(e.validator_url.clone().into());
            }
        }
        sql.push_str(" RETURNING id");
        let rows = db.run(Stmt { sql, params }).await?;
        if let Some(id) = rows.scalar_i64() {
            return Ok(id);
        }
    }
    Err(CoreError::Conflict(
        "the app kept changing under the update; try again".into(),
    ))
}

/// A file of an app version. The file itself is staged under `download/`
/// first; its size and MD5 come from there.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct VersionFile {
    /// Name in the bucket and on the client's disk. Names are global to the
    /// project, so they carry the version, e.g. `worker_1.02_x86_64-pc-linux-gnu`.
    pub name: String,
    /// The signature block `cboinc sign` printed for the file.
    pub signature: String,
    pub main_program: bool,
    pub executable: bool,
    /// Name the app opens the file by, if not `name`.
    pub open_name: String,
    pub copy_file: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct NewAppVersion {
    pub app: String,
    pub platform: String,
    pub version_num: i32,
    pub plan_class: String,
    pub api_version: String,
    pub files: Vec<VersionFile>,
    pub beta: bool,
    pub min_core_version: i32,
    pub max_core_version: i32,
    pub dont_throttle: bool,
    pub is_wrapper: bool,
    pub needs_network: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct StagedRow {
    name: String,
    nbytes: i64,
    md5: String,
}

/// Registers an app version (`update_versions`). Each file must be staged,
/// and when the project has a code-signing public key each signature must
/// verify against the stored MD5.
pub async fn add_app_version(
    db: &dyn Db,
    config: &Config,
    code_sign_key: Option<&PublicKey>,
    now: i64,
    v: &NewAppVersion,
) -> CoreResult<i64> {
    if v.version_num <= 0 {
        return invalid("app version: version_num must be positive");
    }
    if !v.plan_class.is_empty() && !is_name(&v.plan_class) {
        return invalid("app version: bad plan class name");
    }
    if v.files.is_empty() || v.files.len() > MAX_VERSION_FILES {
        return invalid(format!(
            "app version: between 1 and {MAX_VERSION_FILES} files"
        ));
    }
    if v.files.iter().filter(|f| f.main_program).count() != 1 {
        return invalid("app version: exactly one file must be the main program");
    }
    let mut lookups = vec![
        stmt!("SELECT * FROM app WHERE name = ?", v.app.as_str()),
        stmt!("SELECT * FROM platform WHERE name = ?", v.platform.as_str()),
    ];
    for f in &v.files {
        if !is_valid_filename(&f.name) || f.name.contains('/') || f.name.is_empty() {
            return invalid(format!("app version: bad file name {:?}", f.name));
        }
    }
    // D1 binds at most 100 values to a statement.
    for chunk in v.files.chunks(MAX_IN_LIST) {
        let mut files = Stmt::new(format!(
            "SELECT name, nbytes, md5 FROM job_file WHERE name IN ({})",
            vec!["?"; chunk.len()].join(", ")
        ));
        for f in chunk {
            files = files.bind(f.name.as_str());
        }
        lookups.push(files);
    }
    let mut out = db.batch(lookups).await?.into_iter();
    let app: App = out
        .next()
        .unwrap_or_default()
        .first()?
        .ok_or_else(|| CoreError::NotFound(format!("app {}", v.app)))?;
    let platform: Platform = out
        .next()
        .unwrap_or_default()
        .first()?
        .ok_or_else(|| CoreError::NotFound(format!("platform {}", v.platform)))?;
    let mut staged: Vec<StagedRow> = Vec::new();
    for rows in out {
        staged.extend(rows.decode::<StagedRow>()?);
    }
    let staged: HashMap<&str, &StagedRow> = staged.iter().map(|r| (r.name.as_str(), r)).collect();

    let mut xml = String::new();
    for f in &v.files {
        let Some(s) = staged.get(f.name.as_str()) else {
            return Err(CoreError::NotFound(format!(
                "file {} is not staged",
                f.name
            )));
        };
        let signature = f.signature.trim();
        if signature.is_empty() {
            return invalid(format!("app version: file {} has no signature", f.name));
        }
        if signature.contains('<') {
            return invalid(format!("app version: bad signature for {}", f.name));
        }
        if let Some(key) = code_sign_key {
            if !verify_md5(&s.md5, signature, key) {
                return invalid(format!(
                    "app version: the signature of {} does not match the code-signing key",
                    f.name
                ));
            }
        }
        xml.push_str(&format!(
            "<file_info>\n    <name>{}</name>\n    <url>{}</url>\n",
            xml_escape(&f.name),
            xml_escape(&config.download_urls().download(&f.name))
        ));
        if f.executable || f.main_program {
            xml.push_str("    <executable/>\n");
        }
        xml.push_str(&format!(
            "    <file_signature>\n{signature}\n</file_signature>\n    <nbytes>{}</nbytes>\n</file_info>\n",
            s.nbytes
        ));
    }
    xml.push_str(&format!(
        "<app_version>\n    <app_name>{}</app_name>\n    <version_num>{}</version_num>\n",
        app.name, v.version_num
    ));
    if !v.api_version.is_empty() {
        xml.push_str(&format!(
            "    <api_version>{}</api_version>\n",
            xml_escape(&v.api_version)
        ));
    }
    for f in &v.files {
        xml.push_str(&format!(
            "    <file_ref>\n        <file_name>{}</file_name>\n",
            xml_escape(&f.name)
        ));
        if !f.open_name.is_empty() {
            xml.push_str(&format!(
                "        <open_name>{}</open_name>\n",
                xml_escape(&f.open_name)
            ));
        }
        if f.main_program {
            xml.push_str("        <main_program/>\n");
        }
        if f.copy_file {
            xml.push_str("        <copy_file/>\n");
        }
        xml.push_str("    </file_ref>\n");
    }
    for (flag, tag) in [
        (v.dont_throttle, "dont_throttle"),
        (v.is_wrapper, "is_wrapper"),
        (v.needs_network, "needs_network"),
    ] {
        if flag {
            xml.push_str(&format!("    <{tag}/>\n"));
        }
    }
    xml.push_str("</app_version>\n");
    if xml.len() > MAX_VERSION_XML {
        return invalid(format!(
            "app version: its description is {} bytes, over the {MAX_VERSION_XML} a row holds",
            xml.len()
        ));
    }

    let mut writes = vec![stmt!(
        "INSERT INTO app_version (create_time, appid, version_num, platformid, xml_doc, \
         min_core_version, max_core_version, plan_class, beta) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (appid, platformid, version_num, plan_class) DO NOTHING RETURNING id",
        now,
        app.id,
        v.version_num,
        platform.id,
        xml.as_str(),
        v.min_core_version,
        v.max_core_version,
        v.plan_class.as_str(),
        v.beta
    )];
    // App files outlive the jobs that use them.
    for chunk in v.files.chunks(MAX_IN_LIST) {
        let mut keep = Stmt::new(format!(
            "UPDATE job_file SET no_delete = 1 WHERE name IN ({})",
            vec!["?"; chunk.len()].join(", ")
        ));
        for f in chunk {
            keep = keep.bind(f.name.as_str());
        }
        writes.push(keep);
    }
    // The signatures were checked against the MD5s read above. A file staged
    // again with other contents (`replace=1`) since would leave a version that
    // describes the old one, so the batch checks every file still has the MD5
    // that was read, and fails, writing nothing, if one does not. The check
    // raises an error when the count is off: `abs` of the smallest integer
    // overflows in SQLite.
    let keys: Vec<String> = v
        .files
        .iter()
        .filter_map(|f| {
            staged
                .get(f.name.as_str())
                .map(|s| format!("{}:{}", f.name, s.md5))
        })
        .collect();
    let mut distinct = keys.clone();
    distinct.sort();
    distinct.dedup();
    for chunk in distinct.chunks(MAX_IN_LIST) {
        let mut guard = Stmt::new(format!(
            "SELECT CASE WHEN (SELECT count(*) FROM job_file WHERE name || ':' || md5 IN ({})) = ? \
             THEN 1 ELSE abs(-9223372036854775808) END AS ok",
            vec!["?"; chunk.len()].join(", ")
        ));
        for k in chunk {
            guard = guard.bind(k.as_str());
        }
        writes.push(guard.bind(chunk.len() as i64));
    }
    let done = match db.batch(writes).await {
        Ok(d) => d,
        Err(e) => {
            // Was it the check? Look at the files again.
            let mut changed = false;
            for chunk in distinct.chunks(MAX_IN_LIST) {
                let mut q = Stmt::new(format!(
                    "SELECT count(*) AS n FROM job_file WHERE name || ':' || md5 IN ({})",
                    vec!["?"; chunk.len()].join(", ")
                ));
                for k in chunk {
                    q = q.bind(k.as_str());
                }
                let n = db.run(q).await?.scalar_i64().unwrap_or(0);
                changed |= n != chunk.len() as i64;
            }
            return Err(if changed {
                CoreError::Conflict(
                    "a staged file changed while the version was registered; try again".into(),
                )
            } else {
                e.into()
            });
        }
    };
    match done.first().and_then(|r| r.scalar_i64()) {
        Some(id) => Ok(id),
        None => Err(CoreError::Conflict(format!(
            "app version {} {} {} {:?} exists",
            v.app, v.platform, v.version_num, v.plan_class
        ))),
    }
}

/// Deprecates or restores an app version; a deprecated version is no longer
/// sent, and its jobs in progress still report.
pub async fn set_app_version_deprecated(
    db: &dyn Db,
    id: i64,
    deprecated: bool,
) -> CoreResult<bool> {
    let rows = db
        .run(stmt!(
            "UPDATE app_version SET deprecated = ? WHERE id = ? RETURNING id",
            deprecated,
            id
        ))
        .await?;
    Ok(!rows.rows.is_empty())
}

/// The catalog with deprecated rows, for display.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CatalogListing {
    pub platforms: Vec<Platform>,
    pub apps: Vec<App>,
    pub app_versions: Vec<AppVersion>,
}

pub async fn list_catalog(db: &dyn Db) -> CoreResult<CatalogListing> {
    let mut out = db
        .batch(vec![
            Stmt::new("SELECT * FROM platform ORDER BY id"),
            Stmt::new("SELECT * FROM app ORDER BY id"),
            Stmt::new("SELECT * FROM app_version ORDER BY id"),
        ])
        .await?
        .into_iter();
    Ok(CatalogListing {
        platforms: out.next().unwrap_or_default().decode()?,
        apps: out.next().unwrap_or_default().decode()?,
        app_versions: out.next().unwrap_or_default().decode()?,
    })
}

/// A workunit with its results, without the XML documents.
#[derive(Debug, Clone, Serialize)]
pub struct JobStatus {
    pub id: i64,
    pub name: String,
    pub batch: i64,
    pub error_mask: i32,
    pub assimilate_state: i32,
    pub file_delete_state: i32,
    pub canonical_resultid: i64,
    pub canonical_credit: f64,
    pub transition_time: i64,
    /// The homogeneous redundancy class the workunit is committed to; 0 if none.
    pub hr_class: i32,
    /// The app version it is committed to (homogeneous app version); 0 if none.
    pub app_version_id: i64,
    /// The version number the workunit is pinned to; 0 if none.
    pub app_version_num: i32,
    pub results: Vec<ResultStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResultStatus {
    pub id: i64,
    pub name: String,
    pub server_state: i32,
    pub outcome: i32,
    pub client_state: i32,
    pub validate_state: i32,
    pub hostid: i64,
    pub userid: i64,
    pub sent_time: i64,
    pub received_time: i64,
    pub report_deadline: i64,
    pub exit_status: i32,
    pub elapsed_time: f64,
    pub granted_credit: f64,
    pub app_version_id: i64,
}

/// Status of one workunit by name.
pub async fn job_status(db: &dyn Db, name: &str) -> CoreResult<Option<JobStatus>> {
    let Some(wu): Option<Workunit> =
        query_one(db, stmt!("SELECT * FROM workunit WHERE name = ?", name)).await?
    else {
        return Ok(None);
    };
    let results: Vec<JobResult> = query(
        db,
        stmt!(
            "SELECT id, name, server_state, outcome, client_state, validate_state, hostid, userid, \
             sent_time, received_time, report_deadline, exit_status, elapsed_time, granted_credit, \
             app_version_id FROM result WHERE workunitid = ? ORDER BY id",
            wu.id
        ),
    )
    .await?;
    Ok(Some(JobStatus {
        id: wu.id,
        name: wu.name,
        batch: wu.batch,
        error_mask: wu.error_mask,
        assimilate_state: wu.assimilate_state,
        file_delete_state: wu.file_delete_state,
        canonical_resultid: wu.canonical_resultid,
        canonical_credit: wu.canonical_credit,
        transition_time: wu.transition_time,
        hr_class: wu.hr_class,
        app_version_id: wu.app_version_id,
        app_version_num: wu.app_version_num,
        results: results
            .into_iter()
            .map(|r| ResultStatus {
                id: r.id,
                name: r.name,
                server_state: r.server_state,
                outcome: r.outcome,
                client_state: r.client_state,
                validate_state: r.validate_state,
                hostid: r.hostid,
                userid: r.userid,
                sent_time: r.sent_time,
                received_time: r.received_time,
                report_deadline: r.report_deadline,
                exit_status: r.exit_status,
                elapsed_time: r.elapsed_time,
                granted_credit: r.granted_credit,
                app_version_id: r.app_version_id,
            })
            .collect(),
    }))
}

/// Which workunits `reset_homogeneous` releases.
#[derive(Debug, Clone)]
pub enum ResetTarget {
    /// Every workunit committed to this homogeneous redundancy class.
    Class(i32),
    /// These workunits, by name.
    Names(Vec<String>),
}

/// What to release.
#[derive(Debug, Clone, Copy)]
pub struct ResetWhat {
    /// The homogeneous redundancy class the workunit is committed to.
    pub hr_class: bool,
    /// The app version it is committed to (homogeneous app version).
    pub app_version: bool,
}

/// Releases workunits from the class or app version their first sent result
/// committed them to, so the next result may go to any host. For a class this
/// is `html/ops/reset_hrclass.php`, which sets `hr_class` to 0 for every
/// workunit in it; that page cannot release a single workunit or an app
/// version, which this can. A result already sent stays where it is; only
/// the choice for the results still to be sent changes. Returns the number of
/// workunits changed.
pub async fn reset_homogeneous(
    db: &dyn Db,
    target: &ResetTarget,
    what: ResetWhat,
    now: i64,
) -> CoreResult<usize> {
    if !what.hr_class && !what.app_version {
        return invalid("reset: nothing to reset");
    }
    let mut set = String::from("mod_time = ?, rev = rev + 1");
    let mut cond = Vec::new();
    if what.hr_class {
        set.push_str(", hr_class = 0");
        cond.push("hr_class != 0");
    }
    if what.app_version {
        set.push_str(", app_version_id = 0");
        cond.push("app_version_id != 0");
    }
    // Only workunits that have something to release.
    let committed = format!("({})", cond.join(" OR "));
    let mut stmts = Vec::new();
    match target {
        ResetTarget::Class(class) => {
            if *class <= 0 {
                return invalid("reset: hr_class must be positive");
            }
            stmts.push(stmt!(
                format!(
                    "UPDATE workunit SET {set} WHERE hr_class = ? AND {committed} RETURNING id"
                ),
                now,
                *class
            ));
        }
        ResetTarget::Names(names) => {
            for chunk in names.chunks(MAX_IN_LIST) {
                let mut s = Stmt::new(format!(
                    "UPDATE workunit SET {set} WHERE name IN ({}) AND {committed} RETURNING id",
                    vec!["?"; chunk.len()].join(", ")
                ))
                .bind(now);
                for n in chunk {
                    s = s.bind(n.as_str());
                }
                stmts.push(s);
            }
        }
    }
    if stmts.is_empty() {
        return Ok(0);
    }
    Ok(db.batch(stmts).await?.iter().map(|r| r.rows.len()).sum())
}

/// Blocks or unblocks an account; a blocked account's scheduler requests
/// are refused.
pub async fn set_user_blocked(db: &dyn Db, user_id: i64, blocked: bool) -> CoreResult<bool> {
    let rows = db
        .run(stmt!(
            "UPDATE user SET blocked = ? WHERE id = ? RETURNING id",
            blocked,
            user_id
        ))
        .await?;
    Ok(!rows.rows.is_empty())
}

/// Sets a host's `max_results_day`; -1 stops the host getting work.
pub async fn set_host_max_results_day(db: &dyn Db, host_id: i64, max: i32) -> CoreResult<bool> {
    let rows = db
        .run(stmt!(
            "UPDATE host SET max_results_day = ? WHERE id = ? RETURNING id",
            max,
            host_id
        ))
        .await?;
    Ok(!rows.rows.is_empty())
}
