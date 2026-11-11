//! Project settings: the rows of the `config` table and the catalog, as one
//! value a Worker loads and keeps for a few seconds.

use cb_crypto::PublicKey;
use cb_db::{stmt, Db, Stmt};
use serde::Deserialize;

use crate::plan_class::PlanClassSpecs;
use crate::scheduler::Catalog;
use crate::{Config, CoreError, CoreResult};

/// The `<config>` element of upstream `config.xml`.
pub const SETTING_CONFIG: &str = "config";
/// The upstream `plan_class_spec.xml` document.
pub const SETTING_PLAN_CLASSES: &str = "plan_class_spec";
/// The code-signing public key, in the BOINC key file format.
pub const SETTING_CODE_SIGN_KEY: &str = "code_sign_public_key";
/// The upload public key, in the BOINC key file format.
pub const SETTING_UPLOAD_KEY: &str = "upload_public_key";

pub const SETTINGS: [&str; 4] = [
    SETTING_CONFIG,
    SETTING_PLAN_CLASSES,
    SETTING_CODE_SIGN_KEY,
    SETTING_UPLOAD_KEY,
];

#[derive(Debug, Clone, Default)]
pub struct Project {
    pub config: Config,
    pub plan_classes: PlanClassSpecs,
    pub catalog: Catalog,
    /// Text of the code-signing public key; empty when not set.
    pub code_sign_key: String,
    pub upload_public_key: Option<PublicKey>,
}

#[derive(Deserialize)]
struct SettingRow {
    name: String,
    value: String,
}

impl Project {
    /// Two round trips: the settings, then the catalog.
    pub async fn load(db: &dyn Db) -> CoreResult<Project> {
        let rows = db.run(Stmt::new("SELECT name, value FROM config")).await?;
        let mut p = Project::default();
        for row in rows.decode::<SettingRow>()? {
            match row.name.as_str() {
                SETTING_CONFIG => p.config = Config::parse(&row.value)?,
                SETTING_PLAN_CLASSES => p.plan_classes = PlanClassSpecs::parse(&row.value)?,
                SETTING_CODE_SIGN_KEY => p.code_sign_key = row.value,
                SETTING_UPLOAD_KEY => p.upload_public_key = Some(PublicKey::parse(&row.value)?),
                _ => {}
            }
        }
        p.catalog = Catalog::load(db).await?;
        Ok(p)
    }

    /// Where clients send scheduler requests.
    pub fn scheduler_url(&self) -> String {
        scheduler_url(&self.config)
    }
}

pub fn scheduler_url(config: &Config) -> String {
    format!("{}sched", config.master_url)
}

/// Stores a setting after checking that it parses, so that a bad document
/// is refused here rather than failing every request.
pub async fn put_setting(db: &dyn Db, name: &str, value: &str, now: i64) -> CoreResult<()> {
    match name {
        SETTING_CONFIG => {
            let c = Config::parse(value)?;
            if !c.master_url.starts_with("https://") && !c.master_url.starts_with("http://") {
                return Err(CoreError::Invalid(
                    "config: <master_url> must be an http or https URL".into(),
                ));
            }
            if !c.email_from.is_empty() && !crate::web::is_strict_email(&c.email_from) {
                return Err(CoreError::Invalid(
                    "config: <email_from> must be one email address".into(),
                ));
            }
        }
        SETTING_PLAN_CLASSES => {
            PlanClassSpecs::parse(value)?;
        }
        SETTING_CODE_SIGN_KEY | SETTING_UPLOAD_KEY => {
            PublicKey::parse(value)?;
        }
        _ => {
            return Err(CoreError::NotFound(format!("setting {name}")));
        }
    }
    db.run(stmt!(
        "INSERT INTO config (name, value, mod_time) VALUES (?, ?, ?) \
         ON CONFLICT (name) DO UPDATE SET value = excluded.value, mod_time = excluded.mod_time",
        name,
        value,
        now
    ))
    .await?;
    Ok(())
}

pub async fn get_setting(db: &dyn Db, name: &str) -> CoreResult<Option<String>> {
    if !SETTINGS.contains(&name) {
        return Err(CoreError::NotFound(format!("setting {name}")));
    }
    let rows = db
        .run(stmt!("SELECT name, value FROM config WHERE name = ?", name))
        .await?;
    Ok(rows.first::<SettingRow>()?.map(|r| r.value))
}
