//! What the Workers share: the bindings by name, R2 behind the core's
//! [`Files`] trait, `fetch` behind [`Webhook`], and the per-isolate cache of
//! project settings, and the email binding behind the core's `Mailer` trait.

mod email;

use std::cell::RefCell;
use std::rc::Rc;

use async_trait::async_trait;
use cb_core::project::Project;
use cb_core::{CoreError, CoreResult, FileKind, FileStat, Files, Webhook};
use cb_crypto::account::constant_time_eq;
use cb_crypto::{PrivateKey, Signer};
pub use cb_d1::D1Db;
pub use email::{mailer, EmailBinding, EMAIL};
use futures_util::StreamExt;
use worker::wasm_bindgen::JsValue;
use worker::{Bucket, Date, Env, Fetch, Headers, Method, Request, RequestInit, Response, Result};

/// Binding names, the same in every Worker's `wrangler.toml`.
pub const DB: &str = "DB";
pub const BUCKET: &str = "FILES";
pub const QUEUE: &str = "TRANSITIONS";
/// Secret: the upload private key, in the BOINC key file format.
pub const SECRET_UPLOAD_KEY: &str = "UPLOAD_PRIVATE_KEY";
/// Secret: admin bearer tokens, separated by commas.
pub const SECRET_ADMIN_TOKENS: &str = "ADMIN_TOKENS";
/// Secret: the pepper of stored password hashes.
pub const SECRET_PASSWD_PEPPER: &str = "PASSWD_PEPPER";
/// Secret, optional: sent as a bearer token with validator and assimilation
/// webhooks.
pub const SECRET_WEBHOOK_TOKEN: &str = "WEBHOOK_TOKEN";

/// How long an isolate keeps the project settings. A changed setting
/// reaches every isolate within this time.
const PROJECT_TTL_MS: u64 = 30_000;

pub fn now() -> i64 {
    (Date::now().as_millis() / 1000) as i64
}

pub fn now_ms() -> u64 {
    Date::now().as_millis()
}

/// 32 bits from the runtime's cryptographic generator.
pub fn random_u32() -> u32 {
    let mut b = [0u8; 4];
    // `crypto.getRandomValues` does not fail in a Worker.
    let _ = getrandom::getrandom(&mut b);
    u32::from_le_bytes(b)
}

pub fn db(env: &Env) -> Result<D1Db> {
    Ok(D1Db::new(env.d1(DB)?))
}

pub fn core_err(e: CoreError) -> worker::Error {
    worker::Error::RustError(e.to_string())
}

thread_local! {
    static PROJECT: RefCell<Option<(u64, Rc<Project>)>> = const { RefCell::new(None) };
    static SIGNER: RefCell<Option<Rc<Signer>>> = const { RefCell::new(None) };
}

/// The project settings and catalog, at most `PROJECT_TTL_MS` old.
pub async fn project(db: &D1Db) -> CoreResult<Rc<Project>> {
    let now = now_ms();
    let cached = PROJECT.with(|c| {
        c.borrow()
            .as_ref()
            .filter(|(at, _)| now.saturating_sub(*at) < PROJECT_TTL_MS)
            .map(|(_, p)| p.clone())
    });
    if let Some(p) = cached {
        return Ok(p);
    }
    let p = Rc::new(Project::load(db).await?);
    PROJECT.with(|c| *c.borrow_mut() = Some((now, p.clone())));
    Ok(p)
}

/// Drops the cached settings, after this isolate changed them.
pub fn forget_project() {
    PROJECT.with(|c| *c.borrow_mut() = None);
}

/// The upload signer, when this Worker holds the private key.
pub fn signer(env: &Env) -> Result<Option<Rc<Signer>>> {
    if let Some(s) = SIGNER.with(|c| c.borrow().clone()) {
        return Ok(Some(s));
    }
    let Ok(secret) = env.secret(SECRET_UPLOAD_KEY) else {
        return Ok(None);
    };
    let key = PrivateKey::parse(&secret.to_string())
        .map_err(|e| worker::Error::RustError(format!("{SECRET_UPLOAD_KEY}: {e}")))?;
    let signer = Rc::new(
        Signer::new(&key).map_err(|e| worker::Error::RustError(format!("upload key: {e}")))?,
    );
    SIGNER.with(|c| *c.borrow_mut() = Some(signer.clone()));
    Ok(Some(signer))
}

pub fn secret_bytes(env: &Env, name: &str) -> Result<Vec<u8>> {
    let v = env.secret(name)?.to_string();
    if v.is_empty() {
        return Err(worker::Error::RustError(format!("secret {name} is empty")));
    }
    Ok(v.into_bytes())
}

/// Whether the request carries one of the admin tokens.
pub fn is_admin(req: &Request, env: &Env) -> bool {
    let Ok(Some(header)) = req.headers().get("Authorization") else {
        return false;
    };
    let Some(presented) = header.strip_prefix("Bearer ") else {
        return false;
    };
    let Ok(tokens) = env.secret(SECRET_ADMIN_TOKENS) else {
        return false;
    };
    let mut ok = false;
    for token in tokens.to_string().split(',') {
        let token = token.trim();
        // Short tokens are refused so that an empty secret admits nobody.
        if token.len() >= 24 && constant_time_eq(token.as_bytes(), presented.trim().as_bytes()) {
            ok = true;
        }
    }
    ok
}

/// Puts workunit IDs on the transition queue. The database already has
/// `transition_time` set, so a failure here costs latency only.
pub async fn enqueue(env: &Env, ids: &[i64]) {
    if ids.is_empty() {
        return;
    }
    let Ok(queue) = env.queue(QUEUE) else {
        return;
    };
    for chunk in ids.chunks(100) {
        if let Err(e) = queue.send_batch(chunk.to_vec()).await {
            worker::console_warn!("queue send failed: {e}");
        }
    }
}

/// The request body, or `None` when it is longer than `limit`. A declared
/// length over the limit is refused before anything is read, and a body
/// without one is dropped as soon as it passes the limit.
pub async fn read_body(req: &mut Request, limit: usize) -> Result<Option<Vec<u8>>> {
    let declared = req
        .headers()
        .get("Content-Length")?
        .and_then(|v| v.trim().parse::<u64>().ok());
    if declared.is_some_and(|n| n > limit as u64) {
        return Ok(None);
    }
    let mut stream = req.stream()?;
    let mut out = Vec::with_capacity(declared.unwrap_or(0) as usize);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if out.len() + chunk.len() > limit {
            return Ok(None);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(Some(out))
}

pub fn xml(body: String) -> Result<Response> {
    Ok(Response::builder()
        .with_header("Content-Type", "text/xml")?
        .with_header("Cache-Control", "no-store")?
        .fixed(body.into_bytes()))
}

/// Like [`xml`], for a reply that is UTF-8 and declares it.
pub fn xml_utf8(body: String) -> Result<Response> {
    Ok(Response::builder()
        .with_header("Content-Type", "text/xml; charset=utf-8")?
        .with_header("Cache-Control", "no-store")?
        .fixed(body.into_bytes()))
}

pub fn text(status: u16, body: &str) -> Result<Response> {
    Ok(Response::builder()
        .with_status(status)
        .with_header("Content-Type", "text/plain; charset=utf-8")?
        .fixed(body.as_bytes().to_vec()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn object_key(kind: FileKind, name: &str) -> String {
    format!("{}{}", kind.prefix(), name)
}

/// The bucket behind the core's [`Files`] trait.
pub struct R2Files(pub Bucket);

fn files_err(e: worker::Error) -> CoreError {
    CoreError::Files(e.to_string())
}

#[async_trait(?Send)]
impl Files for R2Files {
    async fn stat(&self, kind: FileKind, name: &str) -> CoreResult<Option<FileStat>> {
        let Some(object) = self
            .0
            .head(object_key(kind, name))
            .await
            .map_err(files_err)?
        else {
            return Ok(None);
        };
        // Every object is written with a single put, so R2 has its MD5.
        let md5 = object
            .checksum()
            .md5
            .map(|m| hex(&m))
            .ok_or_else(|| CoreError::Files(format!("{name}: the object has no MD5")))?;
        Ok(Some(FileStat {
            nbytes: object.size(),
            md5,
        }))
    }

    async fn delete(&self, kind: FileKind, names: &[String]) -> CoreResult<()> {
        for chunk in names.chunks(1000) {
            let keys: Vec<String> = chunk.iter().map(|n| object_key(kind, n)).collect();
            self.0.delete_multiple(keys).await.map_err(files_err)?;
        }
        Ok(())
    }

    async fn put(&self, kind: FileKind, name: &str, bytes: Vec<u8>) -> CoreResult<()> {
        self.0
            .put(object_key(kind, name), bytes)
            .execute()
            .await
            .map_err(files_err)?;
        Ok(())
    }
}

/// `fetch` behind the core's [`Webhook`] trait.
pub struct FetchWebhook {
    pub token: Option<String>,
}

impl FetchWebhook {
    pub fn from_env(env: &Env) -> Self {
        FetchWebhook {
            token: env.secret(SECRET_WEBHOOK_TOKEN).ok().map(|s| s.to_string()),
        }
    }
}

#[async_trait(?Send)]
impl Webhook for FetchWebhook {
    async fn post_json(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> CoreResult<serde_json::Value> {
        let fail = |e: worker::Error| CoreError::Invalid(format!("webhook {url}: {e}"));
        let headers = Headers::new();
        headers
            .set("Content-Type", "application/json")
            .map_err(fail)?;
        if let Some(token) = &self.token {
            headers
                .set("Authorization", &format!("Bearer {token}"))
                .map_err(fail)?;
        }
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_headers(headers)
            .with_body(Some(JsValue::from_str(&body.to_string())));
        let request = Request::new_with_init(url, &init).map_err(fail)?;
        let mut response = Fetch::Request(request).send().await.map_err(fail)?;
        let status = response.status_code();
        if !(200..300).contains(&status) {
            return Err(CoreError::Invalid(format!(
                "webhook {url}: status {status}"
            )));
        }
        response.json().await.map_err(fail)
    }
}
