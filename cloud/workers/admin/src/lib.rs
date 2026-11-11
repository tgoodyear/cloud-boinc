//! The admin API under `/admin/v1`: JSON in and out, a bearer token on
//! every request. It replaces the project's command-line tools.

use cb_core::admin::{
    add_app_version, job_status, list_catalog, put_app, put_platform, set_app_version_deprecated,
    set_host_max_results_day, set_user_blocked, NewApp, NewAppVersion, NewPlatform,
};
use cb_core::project::{get_setting, put_setting};
use cb_core::web::{make_user, server_status, NewAccount};
use cb_core::work::{
    acknowledge_assimilation, cancel_workunits, create_work, ready_to_assimilate,
    register_job_file, CancelTarget, Submission,
};
use cb_core::{CoreError, FileKind};
use cb_crypto::PublicKey;
use cb_proto::upload::is_valid_filename;
use cb_worker::{
    db, enqueue, forget_project, is_admin, now, object_key, project, random_u32, read_body,
    secret_bytes, signer, text, D1Db, R2Files, BUCKET, SECRET_PASSWD_PEPPER,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use worker::{
    console_error, event, Context, Env, FixedLengthStream, Method, Request, Response, Result,
};

const PREFIX: &str = "/admin/v1/";
/// JSON request bodies; a submission of a few thousand jobs fits.
const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;
/// One setting; a D1 row holds no more than this.
const MAX_SETTING_BYTES: usize = 1024 * 1024;

fn json_status<T: Serialize>(status: u16, value: &T) -> Result<Response> {
    Ok(Response::from_json(value)?.with_status(status))
}

fn ok<T: Serialize>(value: &T) -> Result<Response> {
    json_status(200, value)
}

fn error(status: u16, message: &str) -> Result<Response> {
    json_status(status, &json!({ "error": message }))
}

fn core_error(e: CoreError) -> Result<Response> {
    match &e {
        CoreError::Invalid(_) | CoreError::Xml(_) | CoreError::Crypto(_) => {
            error(400, &e.to_string())
        }
        CoreError::NotFound(_) => error(404, &e.to_string()),
        CoreError::Conflict(_) => error(409, &e.to_string()),
        CoreError::Db(_) | CoreError::Files(_) => {
            console_error!("admin: {e}");
            error(500, &e.to_string())
        }
    }
}

/// Answers with the value, or with the status that fits the error.
fn reply<T: Serialize>(r: std::result::Result<T, CoreError>) -> Result<Response> {
    match r {
        Ok(v) => ok(&v),
        Err(e) => core_error(e),
    }
}

fn found(changed: bool, what: &str) -> Result<Response> {
    if changed {
        ok(&json!({ "ok": true }))
    } else {
        error(404, &format!("no such {what}"))
    }
}

async fn body<T: for<'de> Deserialize<'de>>(
    req: &mut Request,
) -> Result<std::result::Result<T, String>> {
    let Some(bytes) = read_body(req, MAX_JSON_BYTES).await? else {
        return Ok(Err("the request body is too large".into()));
    };
    Ok(serde_json::from_slice(&bytes).map_err(|e| format!("bad JSON: {e}")))
}

macro_rules! parse {
    ($req:expr) => {
        match body(&mut $req).await? {
            Ok(v) => v,
            Err(m) => return error(400, &m),
        }
    };
}

fn query(req: &Request, name: &str) -> Result<Option<String>> {
    Ok(req
        .url()?
        .query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned()))
}

fn is_flat_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && is_valid_filename(name) && !name.contains('/')
}

#[derive(Deserialize)]
struct RegisterFile {
    #[serde(default)]
    no_delete: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Cancel {
    workunit_ids: Vec<i64>,
    batch: Option<i64>,
}

#[derive(Deserialize)]
struct Ack {
    workunit_ids: Vec<i64>,
}

#[derive(Deserialize)]
struct Deprecate {
    #[serde(default = "yes")]
    deprecated: bool,
}

#[derive(Deserialize)]
struct Block {
    #[serde(default = "yes")]
    blocked: bool,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct MaxResultsDay {
    max_results_day: i32,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct NewUser {
    email_addr: String,
    passwd_hash: String,
    user_name: String,
}

/// Records the object under `download/<name>` as a job file.
async fn register_file(env: &Env, db: &D1Db, name: &str, no_delete: bool) -> Result<Response> {
    let files = R2Files(env.bucket(BUCKET)?);
    let stat = match cb_core::Files::stat(&files, FileKind::Download, name).await {
        Ok(Some(s)) => s,
        Ok(None) => return error(404, "no such file in the bucket"),
        Err(e) => return core_error(e),
    };
    if let Err(e) = register_job_file(db, name, &stat.md5, stat.nbytes, no_delete, now()).await {
        return core_error(e);
    }
    ok(&json!({ "name": name, "nbytes": stat.nbytes, "md5": stat.md5 }))
}

async fn put_file(mut req: Request, env: &Env, db: &D1Db, name: &str) -> Result<Response> {
    let Some(length) = req
        .headers()
        .get("Content-Length")?
        .and_then(|v| v.parse::<u64>().ok())
    else {
        return error(411, "Content-Length is required");
    };
    let no_delete = query(&req, "no_delete")?.is_some_and(|v| v != "0");
    let replace = query(&req, "replace")?.is_some_and(|v| v != "0");
    let md5 = match query(&req, "md5")? {
        None => None,
        Some(v) => match hex::decode(&v) {
            Ok(bytes) if bytes.len() == 16 => Some((v.to_ascii_lowercase(), bytes)),
            _ => return error(400, "md5 is not 32 hex digits"),
        },
    };
    // App versions and jobs refer to a stored file by name, with its MD5 and
    // signature recorded elsewhere, so a name keeps its contents unless the
    // caller asks to replace them.
    if !replace {
        let files = R2Files(env.bucket(BUCKET)?);
        match cb_core::Files::stat(&files, FileKind::Download, name).await {
            Ok(None) => {}
            Ok(Some(stat)) => {
                if md5.as_ref().is_some_and(|(sent, _)| *sent == stat.md5) {
                    return register_file(env, db, name, no_delete).await;
                }
                return error(
                    409,
                    "a file of that name is stored with other contents; pass replace=1 to overwrite it",
                );
            }
            Err(e) => return core_error(e),
        }
    }
    let bucket = env.bucket(BUCKET)?;
    let key = object_key(FileKind::Download, name);
    let mut put = if length == 0 {
        bucket.put(&key, Vec::new())
    } else {
        bucket.put(&key, FixedLengthStream::wrap(req.stream()?, length))
    };
    if let Some((_, bytes)) = md5 {
        put = put.md5(bytes);
    }
    if let Err(e) = put.execute().await {
        console_error!("put {name}: {e}");
        return error(500, "the file could not be stored");
    }
    register_file(env, db, name, no_delete).await
}

async fn get_output(env: &Env, name: &str) -> Result<Response> {
    let bucket = env.bucket(BUCKET)?;
    let Some(object) = bucket
        .get(object_key(FileKind::Upload, name))
        .execute()
        .await?
    else {
        return error(404, "no such output file");
    };
    let size = object.size();
    let Some(body) = object.body() else {
        return error(404, "no such output file");
    };
    Ok(Response::builder()
        .with_header("Content-Type", "application/octet-stream")?
        .with_header("X-Content-Bytes", &size.to_string())?
        .body(body.response_body()?))
}

async fn route(mut req: Request, env: &Env, rest: &str) -> Result<Response> {
    let db = db(env)?;
    let method = req.method();
    let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
    match (&method, parts.as_slice()) {
        (Method::Get, ["status"]) => reply(server_status(&db, now()).await),

        (Method::Get, ["config", name]) => match get_setting(&db, name).await {
            Ok(Some(v)) => text(200, &v),
            Ok(None) => error(404, "no such setting"),
            Err(e) => core_error(e),
        },
        (Method::Put, ["config", name]) => {
            let Some(value) = read_body(&mut req, MAX_SETTING_BYTES).await? else {
                return error(413, "the setting is too large");
            };
            let Ok(value) = String::from_utf8(value) else {
                return error(400, "the setting is not UTF-8 text");
            };
            match put_setting(&db, name, &value, now()).await {
                Ok(()) => {
                    forget_project();
                    ok(&json!({ "ok": true }))
                }
                Err(e) => core_error(e),
            }
        }

        (Method::Get, ["catalog"]) => reply(list_catalog(&db).await),
        (Method::Put, ["platforms"]) => {
            let p: NewPlatform = parse!(req);
            reply(
                put_platform(&db, now(), &p)
                    .await
                    .map(|id| json!({ "id": id })),
            )
        }
        (Method::Put, ["apps"]) => {
            let a: NewApp = parse!(req);
            reply(put_app(&db, now(), &a).await.map(|id| json!({ "id": id })))
        }
        (Method::Post, ["app-versions"]) => {
            let v: NewAppVersion = parse!(req);
            let project = match project(&db).await {
                Ok(p) => p,
                Err(e) => return core_error(e),
            };
            let key = if project.code_sign_key.trim().is_empty() {
                None
            } else {
                match PublicKey::parse(&project.code_sign_key) {
                    Ok(k) => Some(k),
                    Err(e) => return error(500, &format!("code-signing key: {e}")),
                }
            };
            let r = add_app_version(&db, &project.config, key.as_ref(), now(), &v).await;
            forget_project();
            reply(r.map(|id| json!({ "id": id })))
        }
        (Method::Post, ["app-versions", id, "deprecate"]) => {
            let Ok(id) = id.parse::<i64>() else {
                return error(400, "bad id");
            };
            let d: Deprecate = parse!(req);
            match set_app_version_deprecated(&db, id, d.deprecated).await {
                Ok(changed) => {
                    forget_project();
                    found(changed, "app version")
                }
                Err(e) => core_error(e),
            }
        }

        (Method::Put, ["files", name]) => {
            if !is_flat_name(name) {
                return error(400, "bad file name");
            }
            let name = name.to_string();
            put_file(req, env, &db, &name).await
        }
        (Method::Post, ["files", name, "register"]) => {
            if !is_flat_name(name) {
                return error(400, "bad file name");
            }
            let r: RegisterFile = parse!(req);
            register_file(env, &db, name, r.no_delete).await
        }

        (Method::Post, ["jobs"]) => {
            let sub: Submission = parse!(req);
            let project = match project(&db).await {
                Ok(p) => p,
                Err(e) => return core_error(e),
            };
            let signer = signer(env)?;
            let random = random_u32;
            let jobs = create_work(
                &db,
                &project.config,
                signer.as_deref(),
                now(),
                &random,
                &sub,
            )
            .await;
            reply(jobs.map(|jobs| json!({ "jobs": jobs })))
        }
        (Method::Post, ["jobs", "cancel"]) => {
            let c: Cancel = parse!(req);
            let target = match c.batch {
                Some(b) => CancelTarget::Batch(b),
                None => CancelTarget::Ids(c.workunit_ids),
            };
            match cancel_workunits(&db, &target, now()).await {
                Ok(ids) => {
                    enqueue(env, &ids).await;
                    ok(&json!({ "cancelled": ids.len() }))
                }
                Err(e) => core_error(e),
            }
        }
        (Method::Get, ["jobs", name]) => match job_status(&db, name).await {
            Ok(Some(s)) => ok(&s),
            Ok(None) => error(404, "no such job"),
            Err(e) => core_error(e),
        },

        (Method::Get, ["assimilate"]) => {
            let Some(app) = query(&req, "app")? else {
                return error(400, "app is required");
            };
            let after = query(&req, "after")?
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0);
            let limit = query(&req, "limit")?
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(100)
                .clamp(1, 500);
            let files = R2Files(env.bucket(BUCKET)?);
            reply(
                ready_to_assimilate(&db, &files, &app, after, limit)
                    .await
                    .map(|w| json!({ "workunits": w })),
            )
        }
        (Method::Post, ["assimilate", "ack"]) => {
            let a: Ack = parse!(req);
            match acknowledge_assimilation(&db, &a.workunit_ids, now()).await {
                Ok(ids) => {
                    enqueue(env, &ids).await;
                    ok(&json!({ "acknowledged": ids }))
                }
                Err(e) => core_error(e),
            }
        }
        (Method::Get, ["outputs", name]) => {
            if !is_flat_name(name) {
                return error(400, "bad file name");
            }
            get_output(env, name).await
        }

        (Method::Post, ["users"]) => {
            let u: NewUser = parse!(req);
            let pepper = secret_bytes(env, SECRET_PASSWD_PEPPER)?;
            let random = random_u32;
            let account = NewAccount {
                email_addr: &u.email_addr,
                passwd_hash: &u.passwd_hash,
                user_name: &u.user_name,
            };
            match make_user(&db, &pepper, now(), &random, &account).await {
                Ok(Ok(authenticator)) => ok(&json!({ "authenticator": authenticator })),
                Ok(Err(e)) => error(400, &format!("{e:?}")),
                Err(e) => core_error(e),
            }
        }
        (Method::Post, ["users", id, "block"]) => {
            let Ok(id) = id.parse::<i64>() else {
                return error(400, "bad id");
            };
            let b: Block = parse!(req);
            match set_user_blocked(&db, id, b.blocked).await {
                Ok(changed) => found(changed, "user"),
                Err(e) => core_error(e),
            }
        }
        (Method::Post, ["hosts", id, "max-results-day"]) => {
            let Ok(id) = id.parse::<i64>() else {
                return error(400, "bad id");
            };
            let m: MaxResultsDay = parse!(req);
            match set_host_max_results_day(&db, id, m.max_results_day).await {
                Ok(changed) => found(changed, "host"),
                Err(e) => core_error(e),
            }
        }

        _ => error(404, "no such route"),
    }
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let path = req.path();
    let Some(rest) = path.strip_prefix(PREFIX) else {
        return error(404, "no such route");
    };
    if !is_admin(&req, &env) {
        return error(401, "a valid admin token is required");
    }
    route(req, &env, rest).await
}
