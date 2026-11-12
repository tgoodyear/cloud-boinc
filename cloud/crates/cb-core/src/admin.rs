//! Project administration: what upstream does with `xadd`, `update_versions`
//! and the ops pages. Every function is one admin API operation.

use cb_crypto::{verify_md5, PublicKey};
use cb_db::{query, query_one, stmt, Db, Stmt, Value};
use cb_proto::upload::is_valid_filename;
use cb_proto::xml::xml_escape;
use serde::{Deserialize, Serialize};

use crate::hr::HR_NTYPES;
use crate::model::{App, AppVersion, JobResult, Platform, Workunit};
use crate::{Config, CoreError, CoreResult};

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

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NewApp {
    pub name: String,
    pub user_friendly_name: String,
    pub weight: f64,
    pub beta: bool,
    /// Above 1 turns on adaptive replication for the app.
    pub target_nresults: i32,
    pub non_cpu_intensive: bool,
    pub fraction_done_exact: bool,
    pub min_version: i32,
    pub deprecated: bool,
    /// `trivial`, `bitwise` or `webhook`.
    pub validator: String,
    pub validator_url: String,
    pub assimilate_url: String,
    /// Homogeneous redundancy type: 0 for none, 1 fine, 2 coarse. Left as it
    /// is when absent, unlike the other settings, which a put replaces: a put
    /// that only changes a URL must not end a project's HR.
    pub homogeneous_redundancy: Option<i32>,
    /// Send all results of a workunit with one app version. Left as it is
    /// when absent.
    pub homogeneous_app_version: Option<bool>,
}

impl Default for NewApp {
    fn default() -> Self {
        NewApp {
            name: String::new(),
            user_friendly_name: String::new(),
            weight: 1.0,
            beta: false,
            target_nresults: 0,
            non_cpu_intensive: false,
            fraction_done_exact: false,
            min_version: 0,
            deprecated: false,
            validator: "trivial".into(),
            validator_url: String::new(),
            assimilate_url: String::new(),
            homogeneous_redundancy: None,
            homogeneous_app_version: None,
        }
    }
}

/// Creates the app or updates the one with this name.
pub async fn put_app(db: &dyn Db, now: i64, a: &NewApp) -> CoreResult<i64> {
    if !is_name(&a.name) {
        return invalid("app: bad name");
    }
    if !["trivial", "bitwise", "webhook"].contains(&a.validator.as_str()) {
        return invalid("app: validator must be trivial, bitwise or webhook");
    }
    if a.validator == "webhook" && !a.validator_url.starts_with("https://") {
        return invalid("app: the webhook validator needs an https validator_url");
    }
    if !a.assimilate_url.is_empty() && !a.assimilate_url.starts_with("https://") {
        return invalid("app: assimilate_url must be https");
    }
    if let Some(t) = a.homogeneous_redundancy {
        if !(0..HR_NTYPES).contains(&t) {
            return invalid("app: homogeneous_redundancy must be 0 (none), 1 (fine) or 2 (coarse)");
        }
    }
    let friendly = if a.user_friendly_name.is_empty() {
        &a.name
    } else {
        &a.user_friendly_name
    };
    // NULL keeps the stored value of an existing app, and is 0 for a new one.
    let hr_type = a.homogeneous_redundancy.map_or(Value::Null, Value::from);
    let hav = a
        .homogeneous_app_version
        .map_or(Value::Null, |b| Value::from(i32::from(b)));
    let rows = db
        .run(stmt!(
            "INSERT INTO app (create_time, name, user_friendly_name, weight, beta, target_nresults, \
             non_cpu_intensive, fraction_done_exact, min_version, deprecated, validator, validator_url, \
             assimilate_url, homogeneous_redundancy, homogeneous_app_version) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, coalesce(?, 0), coalesce(?, 0)) \
             ON CONFLICT (name) DO UPDATE SET user_friendly_name = excluded.user_friendly_name, \
             weight = excluded.weight, beta = excluded.beta, target_nresults = excluded.target_nresults, \
             non_cpu_intensive = excluded.non_cpu_intensive, \
             fraction_done_exact = excluded.fraction_done_exact, min_version = excluded.min_version, \
             deprecated = excluded.deprecated, validator = excluded.validator, \
             validator_url = excluded.validator_url, assimilate_url = excluded.assimilate_url, \
             homogeneous_redundancy = coalesce(?, app.homogeneous_redundancy), \
             homogeneous_app_version = coalesce(?, app.homogeneous_app_version) \
             RETURNING id",
            now,
            a.name.as_str(),
            friendly.as_str(),
            a.weight,
            a.beta,
            a.target_nresults,
            a.non_cpu_intensive,
            a.fraction_done_exact,
            a.min_version,
            a.deprecated,
            a.validator.as_str(),
            a.validator_url.as_str(),
            a.assimilate_url.as_str(),
            hr_type.clone(),
            hav.clone(),
            hr_type,
            hav
        ))
        .await?;
    Ok(rows.scalar_i64().unwrap_or(0))
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
    if v.files.is_empty() || v.files.len() > 40 {
        return invalid("app version: between 1 and 40 files");
    }
    if v.files.iter().filter(|f| f.main_program).count() != 1 {
        return invalid("app version: exactly one file must be the main program");
    }
    let mut lookups = vec![
        stmt!("SELECT * FROM app WHERE name = ?", v.app.as_str()),
        stmt!("SELECT * FROM platform WHERE name = ?", v.platform.as_str()),
    ];
    let mut files = Stmt::new(format!(
        "SELECT name, nbytes, md5 FROM job_file WHERE name IN ({})",
        vec!["?"; v.files.len()].join(", ")
    ));
    for f in &v.files {
        if !is_valid_filename(&f.name) || f.name.contains('/') || f.name.is_empty() {
            return invalid(format!("app version: bad file name {:?}", f.name));
        }
        files = files.bind(f.name.as_str());
    }
    lookups.push(files);
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
    let staged: Vec<StagedRow> = out.next().unwrap_or_default().decode()?;

    let mut xml = String::new();
    for f in &v.files {
        let Some(s) = staged.iter().find(|s| s.name == f.name) else {
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
            "<file_info>\n    <name>{0}</name>\n    <url>{1}/{0}</url>\n",
            xml_escape(&f.name),
            config.download_url
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
    let mut keep = Stmt::new(format!(
        "UPDATE job_file SET no_delete = 1 WHERE name IN ({})",
        vec!["?"; v.files.len()].join(", ")
    ));
    for f in &v.files {
        keep = keep.bind(f.name.as_str());
    }
    writes.push(keep);
    let done = db.batch(writes).await?;
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
