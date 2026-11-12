//! The target schema: `cloud/migrations`, embedded, and what each table holds.

use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::error::Result;
use crate::value::{ColKind, Value};

/// Every file of `cloud/migrations`, in order. `tests::migrations_are_complete`
/// fails when a file is added to the directory and not to this list.
pub const MIGRATIONS: &[(&str, &str)] = &[
    (
        "0001_init.sql",
        include_str!("../../../migrations/0001_init.sql"),
    ),
    (
        "0002_purge.sql",
        include_str!("../../../migrations/0002_purge.sql"),
    ),
    (
        "0003_user_tasks.sql",
        include_str!("../../../migrations/0003_user_tasks.sql"),
    ),
    (
        "0004_email.sql",
        include_str!("../../../migrations/0004_email.sql"),
    ),
    (
        "0005_upload_epoch.sql",
        include_str!("../../../migrations/0005_upload_epoch.sql"),
    ),
    (
        "0006_homogeneous.sql",
        include_str!("../../../migrations/0006_homogeneous.sql"),
    ),
    (
        "0007_site_ops.sql",
        include_str!("../../../migrations/0007_site_ops.sql"),
    ),
    (
        "0008_dashboards.sql",
        include_str!("../../../migrations/0008_dashboards.sql"),
    ),
    (
        "0009_teams.sql",
        include_str!("../../../migrations/0009_teams.sql"),
    ),
];

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub kind: ColKind,
    pub notnull: bool,
    pub default: Option<Value>,
    /// 1-based position in the primary key, 0 when not part of it.
    pub pk: usize,
}

#[derive(Debug, Clone)]
pub struct TableInfo {
    pub name: String,
    pub cols: Vec<Column>,
}

impl TableInfo {
    pub fn index(&self, col: &str) -> Option<usize> {
        self.cols.iter().position(|c| c.name == col)
    }
    pub fn col_names(&self) -> Vec<&str> {
        self.cols.iter().map(|c| c.name.as_str()).collect()
    }
    /// Primary key columns in key order.
    pub fn pk_cols(&self) -> Vec<usize> {
        let mut v: Vec<(usize, usize)> = self
            .cols
            .iter()
            .enumerate()
            .filter(|(_, c)| c.pk > 0)
            .map(|(i, c)| (c.pk, i))
            .collect();
        v.sort();
        v.into_iter().map(|(_, i)| i).collect()
    }
    /// The value of a column that the source does not supply.
    pub fn default_of(&self, i: usize) -> Value {
        let c = &self.cols[i];
        c.default.clone().unwrap_or_else(|| c.kind.zero())
    }
    pub fn insert_sql(&self) -> String {
        let names = self.col_names();
        let marks: Vec<String> = (1..=names.len()).map(|i| format!("?{i}")).collect();
        format!(
            "INSERT INTO {} ({}) VALUES ({})",
            self.name,
            names.join(", "),
            marks.join(", ")
        )
    }
}

/// Registers the functions D1's SQLite has and this build lacks, which the
/// migrations and the server's statements use.
pub fn register_functions(conn: &Connection) -> Result<()> {
    use rusqlite::functions::FunctionFlags;
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    conn.create_scalar_function("ln", 1, flags, |ctx| Ok(ctx.get::<f64>(0)?.ln()))?;
    conn.create_scalar_function("exp", 1, flags, |ctx| Ok(ctx.get::<f64>(0)?.exp()))?;
    Ok(())
}

/// Applies every migration to an empty connection.
pub fn apply_migrations(conn: &Connection) -> Result<()> {
    register_functions(conn)?;
    for (_, sql) in MIGRATIONS {
        conn.execute_batch(sql)?;
    }
    Ok(())
}

fn default_value(text: &str, kind: ColKind) -> Option<Value> {
    let t = text.trim();
    if t.eq_ignore_ascii_case("null") {
        return None;
    }
    if let Some(inner) = t.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return Some(Value::Text(inner.replace("''", "'")));
    }
    match kind {
        ColKind::Int => t.parse::<i64>().ok().map(Value::Int),
        ColKind::Real => t.parse::<f64>().ok().map(Value::Real),
        ColKind::Text => Some(Value::Text(t.to_string())),
    }
}

/// The columns of one table of the target.
pub fn table_info(conn: &Connection, table: &str) -> Result<TableInfo> {
    let mut st = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, i64>(5)?,
        ))
    })?;
    let mut cols = Vec::new();
    for r in rows {
        let (name, decl, notnull, dflt, pk) = r?;
        let kind = ColKind::from_decl(&decl);
        cols.push(Column {
            name,
            kind,
            notnull: notnull != 0,
            default: dflt.and_then(|d| default_value(&d, kind)),
            pk: pk as usize,
        });
    }
    Ok(TableInfo {
        name: table.to_string(),
        cols,
    })
}

/// The tables the importer fills, in the order it fills them. `team` comes
/// before `user` so that the triggers of `0009_teams.sql` count the members.
pub const TABLES: &[&str] = &[
    "platform",
    "app",
    "app_version",
    "team",
    "user",
    "host",
    "host_app_version",
    "workunit",
    "result",
    "assignment",
    "batch",
    "job_file",
];

/// Target tables the importer does not fill, with the reason.
pub const NOT_IMPORTED: &[(&str, &str)] = &[
    (
        "config",
        "settings come from config.xml and the key files; see the config subcommand",
    ),
    ("token", "mailed links of the stock site are not carried"),
    ("mail_day", "a counter of this server"),
    (
        "upload_epoch",
        "written by this server when it resends a lost result",
    ),
    ("user_deletion", "deletions in progress on this server"),
    ("stats_bucket", "rebuilt by this server from new results"),
    ("stats_snapshot", "rebuilt by this server"),
    (
        "id_floor",
        "set at the end of the import from the highest IDs",
    ),
];

pub fn all_tables(conn: &Connection) -> Result<BTreeMap<String, TableInfo>> {
    let mut m = BTreeMap::new();
    for t in TABLES {
        m.insert(t.to_string(), table_info(conn, t)?);
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_complete() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations");
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".sql"))
            .collect();
        names.sort();
        let listed: Vec<String> = MIGRATIONS.iter().map(|(n, _)| n.to_string()).collect();
        assert_eq!(names, listed);
    }

    #[test]
    fn tables_and_defaults() {
        let conn = Connection::open_in_memory().unwrap();
        apply_migrations(&conn).unwrap();
        let t = all_tables(&conn).unwrap();
        let user = &t["user"];
        let i = user.index("recent_key").unwrap();
        assert_eq!(user.default_of(i), Value::Real(-1e9));
        let app = &t["app"];
        let i = app.index("validator").unwrap();
        assert_eq!(app.default_of(i), Value::Text("trivial".into()));
        assert_eq!(t["host_app_version"].pk_cols().len(), 2);
        assert_eq!(t["job_file"].cols[0].name, "name");
    }
}
