//! This server in the starting state of `cloud/tests/differential/project`,
//! and the functions that answer a corpus step.
//!
//! The Workers are thin: they cut a request into the arguments of the
//! functions used here and send the result back. This file does the same
//! cutting, so what is compared is what a Worker would send, minus the HTTP
//! headers.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use cb_core::admin::{
    add_app_version, put_app, put_platform, NewApp, NewAppVersion, NewPlatform, VersionFile,
};
use cb_core::project::Project;
use cb_core::scheduler::{handle_request, SchedCtx};
use cb_core::site::{site_page, SiteCtx};
use cb_core::template::InputFile;
use cb_core::upload::{
    check_header, file_size, is_flat_name, md5_to_hex, receive, CompleteWrite, PieceRun,
    PieceWrite, StoredFile, StoredPiece, UploadStore, PIECE_BYTES,
};
use cb_core::web::{
    am_get_info, am_set_info, create_account, lookup_account, master_page, project_config,
    server_status, server_status_xml, NewAccount,
};
use cb_core::work::{create_work, register_job_file, JobSpec, Submission};
use cb_core::{Config, CoreResult};
use cb_crypto::{md5_hex, pepper_passwd_hash, PrivateKey, PublicKey, Signer};
use cb_db::{stmt, Db};
use cb_proto::upload::{
    check_get_file_size_name, incomplete_error, parse_request, reply_error, reply_file_size,
    reply_success, UploadError, UploadRequest,
};
use cb_proto::ProtoError;
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;
use serde_json::Value;

/// What the golden replies call the project's address: `replay.py` replaces
/// the address of the stock server with it.
pub const BASE: &str = "http://project.test";
/// The password pepper of the test world.
const PEPPER: &[u8] = b"differential test pepper, 32+ bytes";

pub fn differential_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/differential")
}

/// An answer: what the Worker would send.
#[derive(Debug, Clone)]
pub struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
}

pub struct World {
    db: SqliteDb,
    project: Project,
    code_public: PublicKey,
    upload_public: PublicKey,
    seq: Cell<u32>,
    store: MemStore,
}

fn read(dir: &Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

impl World {
    /// The project of `project.json`, with the jobs, users, keys, app files
    /// and settings of that directory.
    pub fn new() -> World {
        let fx = differential_dir().join("project");
        let spec: Value = serde_json::from_str(&read(&fx, "project.json")).unwrap();
        let code_private = PrivateKey::parse(&read(&fx, "keys/code_sign_private")).unwrap();
        let code_public = PublicKey::parse(&read(&fx, "keys/code_sign_public")).unwrap();
        let upload_private = PrivateKey::parse(&read(&fx, "keys/upload_private")).unwrap();
        let upload_public = PublicKey::parse(&read(&fx, "keys/upload_public")).unwrap();
        let upload_signer = Signer::new(&upload_private).unwrap();
        let short = spec["short_name"].as_str().unwrap();

        // The URLs stock `make_project` gives a project made at BASE, and the
        // other options it writes to config.xml that this server reads
        // (`Project.config` in py/Boinc/setup_project.py). Then the options
        // the stock project is given besides (config-options.xml).
        let config_xml = format!(
            "<config>\n<long_name>{}</long_name>\n<master_url>{BASE}/{short}/</master_url>\n\
             <download_url>{BASE}/{short}/download</download_url>\n\
             <upload_url>{BASE}/{short}_cgi/file_upload_handler</upload_url>\n\
             <min_sendwork_interval>6</min_sendwork_interval>\n\
             <max_wus_to_send>50</max_wus_to_send>\n\
             <daily_result_quota>500</daily_result_quota>\n\
             <one_result_per_user_per_wu>0</one_result_per_user_per_wu>\n\
             <send_result_abort>1</send_result_abort>\n\
             {}<account_creation>1</account_creation>\n</config>\n",
            spec["long_name"].as_str().unwrap(),
            read(&fx, "config-options.xml"),
        );
        let config = Config::parse(&config_xml).unwrap();
        let db = SqliteDb::with_schema().unwrap();
        let now = 1_700_000_000;
        block_on(async {
            for p in spec["platforms"].as_array().unwrap() {
                put_platform(
                    &db,
                    now,
                    &NewPlatform {
                        name: p["name"].as_str().unwrap().into(),
                        user_friendly_name: p["user_friendly_name"].as_str().unwrap().into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            }
            for a in spec["apps"].as_array().unwrap() {
                put_app(
                    &db,
                    now,
                    &NewApp {
                        name: a["name"].as_str().unwrap().into(),
                        user_friendly_name: a["user_friendly_name"].as_str().unwrap().into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            }
            for v in spec["app_versions"].as_array().unwrap() {
                let mut files = Vec::new();
                for f in v["files"].as_array().unwrap() {
                    let name = f["name"].as_str().unwrap();
                    let bytes = std::fs::read(fx.join("app").join(name)).unwrap();
                    let md5 = md5_hex(&bytes);
                    register_job_file(&db, name, &md5, bytes.len() as u64, true, now)
                        .await
                        .unwrap();
                    let main = f["main_program"].as_bool().unwrap_or(false);
                    files.push(VersionFile {
                        name: name.into(),
                        signature: cb_crypto::sign_md5(&md5, &code_private).unwrap(),
                        main_program: main,
                        executable: main,
                        open_name: String::new(),
                        copy_file: false,
                    });
                }
                add_app_version(
                    &db,
                    &config,
                    Some(&code_public),
                    now,
                    &NewAppVersion {
                        app: v["app"].as_str().unwrap().into(),
                        platform: v["platform"].as_str().unwrap().into(),
                        version_num: v["version_num"].as_i64().unwrap() as i32,
                        files,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            }
            for u in spec["users"].as_array().unwrap() {
                let s = |k: &str| u[k].as_str().unwrap().to_string();
                db.run(stmt!(
                    "INSERT INTO user (id, create_time, email_addr, name, authenticator, country, \
                     postal_code, total_credit, expavg_credit, expavg_time, global_prefs, \
                     project_prefs, teamid, venue, url, send_email, show_hosts, \
                     cross_project_id, passwd_hash) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, 0, 0, ?, ?, ?, 0, '', '', 1, 1, ?, ?)",
                    u["id"].as_i64().unwrap(),
                    u["create_time"].as_i64().unwrap(),
                    s("email_addr"),
                    s("name"),
                    s("authenticator"),
                    s("country"),
                    s("postal_code"),
                    u["create_time"].as_i64().unwrap(),
                    s("global_prefs"),
                    s("project_prefs"),
                    s("cross_project_id"),
                    pepper_passwd_hash(PEPPER, &s("passwd_md5"))
                ))
                .await
                .unwrap();
            }
            // The jobs, in the order the stock fixture loader creates them.
            let in_template = |app: &str| read(&fx, &format!("templates/{app}_in"));
            let out_template = |app: &str| read(&fx, &format!("templates/{app}_out"));
            for j in spec["jobs"].as_array().unwrap() {
                let input = j["input"].as_str().unwrap();
                let bytes = std::fs::read(fx.join("inputs").join(input)).unwrap();
                register_job_file(&db, input, &md5_hex(&bytes), bytes.len() as u64, false, now)
                    .await
                    .unwrap();
                let app = j["app"].as_str().unwrap();
                let counter = Cell::new(0u32);
                let random = || {
                    counter.set(counter.get().wrapping_add(1));
                    counter.get()
                };
                let out = create_work(
                    &db,
                    &config,
                    Some(&upload_signer),
                    now,
                    &random,
                    &Submission {
                        app: app.into(),
                        input_template: in_template(app),
                        result_template: out_template(app),
                        jobs: vec![JobSpec {
                            name: j["name"].as_str().unwrap().into(),
                            // The `create_work` program, which the stock
                            // fixture loader runs, always passes a command
                            // line, and writes an empty one into the job.
                            command_line: Some(String::new()),
                            input_files: vec![InputFile {
                                name: input.into(),
                                ..Default::default()
                            }],
                            ..Default::default()
                        }],
                    },
                )
                .await
                .unwrap();
                assert_eq!(out[0].error, None);
            }
        });
        let project = block_on(async {
            let mut p = Project::load(&db).await.unwrap();
            p.config = config;
            p.code_sign_key = code_public.to_key_string();
            p.upload_public_key = Some(upload_public.clone());
            p
        });
        World {
            db,
            project,
            code_public,
            upload_public,
            seq: Cell::new(0x9e37_79b9),
            store: MemStore::default(),
        }
    }

    fn rand(&self) -> u32 {
        self.seq.set(
            self.seq
                .get()
                .wrapping_mul(1_664_525)
                .wrapping_add(1_013_904_223),
        );
        self.seq.get()
    }

    /// A scheduler request at time `now`.
    pub fn scheduler(&self, body: &[u8], now: i64) -> Answer {
        let body = String::from_utf8_lossy(body).into_owned();
        let random = || self.rand();
        let key = self.code_public.to_key_string();
        let catalog = block_on(cb_core::scheduler::Catalog::load(&self.db)).unwrap();
        let ctx = SchedCtx {
            db: &self.db,
            config: &self.project.config,
            plan_classes: &self.project.plan_classes,
            catalog: &catalog,
            code_sign_key: &key,
            now,
            random: &random,
            client_ip: "127.0.0.1",
        };
        let out = block_on(handle_request(&ctx, &body)).unwrap();
        Answer {
            status: 200,
            body: out.reply.write().unwrap().into_bytes(),
        }
    }

    /// An upload handler request: the header and then the file.
    pub fn upload(&self, body: &[u8]) -> Answer {
        let reply = block_on(self.upload_reply(body));
        Answer {
            status: 200,
            body: reply.into_bytes(),
        }
    }

    async fn upload_reply(&self, body: &[u8]) -> String {
        let refuse = |e: UploadError| e.reply();
        // As the files Worker reads the header, with the whole body in hand.
        let request = match parse_request(body) {
            Ok(r) => r,
            Err(ProtoError::Incomplete) => return refuse(incomplete_error(body)),
            Err(_) => return refuse(UploadError::permanent("the request header is too long")),
        };
        match request {
            UploadRequest::GetFileSize { file_name, .. } => {
                if let Err(e) = check_get_file_size_name(&file_name) {
                    return refuse(e);
                }
                if !is_flat_name(&file_name) {
                    return refuse(UploadError::permanent("invalid filename"));
                }
                match file_size(&self.store, &self.db, &file_name, PIECE_BYTES).await {
                    Ok(n) => reply_file_size(n),
                    Err(_) => reply_error(true, "cannot stat file"),
                }
            }
            UploadRequest::FileUpload(header) => {
                if let Err(e) = check_header(
                    &header,
                    self.project.config.ignore_upload_certificates,
                    self.project.upload_public_key.as_ref(),
                    self.project.config.max_upload_nbytes,
                ) {
                    return refuse(e);
                }
                let data = MemBody::new(body[header.data_offset.min(body.len())..].to_vec());
                match receive(&self.store, &self.db, &header, &data, PIECE_BYTES).await {
                    Ok(Ok(())) => reply_success(),
                    Ok(Err(e)) => refuse(e),
                    Err(_) => reply_error(true, "the file could not be stored"),
                }
            }
        }
    }

    /// The project's upload public key, which both servers' certificates
    /// must verify against.
    pub fn upload_key(&self) -> &PublicKey {
        &self.upload_public
    }

    /// A request for a page of the project's web directory, as the web Worker
    /// routes it. `url` is relative to the project's directory.
    pub fn web(&self, method: &str, url: &str, now: i64) -> Answer {
        let (path, query) = url.split_once('?').unwrap_or((url, ""));
        let path = format!("/{path}");
        let q: HashMap<String, String> = query
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (percent_decode(k), percent_decode(v))
            })
            .collect();
        let get = |k: &str| q.get(k).map(String::as_str).unwrap_or_default();
        let random = || self.rand();
        let ok = |body: String| Answer {
            status: 200,
            body: body.into_bytes(),
        };
        let _ = method;
        match path.as_str() {
            "/get_project_config.php" => ok(project_config(&self.project)),
            "/" | "/index.php" => ok(master_page(&self.project)),
            "/lookup_account.php" => ok(block_on(lookup_account(
                &self.db,
                PEPPER,
                get("email_addr"),
                get("passwd_hash"),
            ))
            .unwrap()),
            "/create_account.php" => ok(block_on(create_account(
                &self.db,
                &self.project.config,
                PEPPER,
                now,
                &random,
                &NewAccount {
                    email_addr: get("email_addr"),
                    passwd_hash: get("passwd_hash"),
                    user_name: get("user_name"),
                    ..Default::default()
                },
            ))
            .unwrap()),
            "/am_get_info.php" => ok(block_on(am_get_info(&self.db, get("account_key"))).unwrap()),
            "/am_set_info.php" => {
                ok(block_on(am_set_info(&self.db, &self.project.config, PEPPER, now, &q)).unwrap())
            }
            "/server_status.php" => {
                let status = block_on(server_status(&self.db, now)).unwrap();
                ok(server_status_xml(&self.project, &status, now))
            }
            p => {
                let ctx = SiteCtx {
                    db: &self.db,
                    project: &self.project,
                    auth: None,
                    post: method == "POST",
                    pepper: Some(PEPPER),
                    now,
                    random: &random,
                    // The test project has no `email_from`: no mail is sent.
                    mailer: None,
                };
                match block_on(site_page(&ctx, p, &q)).unwrap() {
                    Some(page) => Answer {
                        status: page.status,
                        body: page.body.into_bytes(),
                    },
                    None => Answer {
                        status: 404,
                        body: b"not found\n".to_vec(),
                    },
                }
            }
        }
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let h = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz");
                match u8::from_str_radix(h, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The bytes after the header of an upload request.
pub struct MemBody {
    data: Vec<u8>,
    pos: Cell<usize>,
}

impl MemBody {
    fn new(data: Vec<u8>) -> MemBody {
        MemBody {
            data,
            pos: Cell::new(0),
        }
    }

    fn take(&self, n: u64) -> Option<Vec<u8>> {
        let pos = self.pos.get();
        let end = pos + n as usize;
        if end > self.data.len() {
            self.pos.set(self.data.len());
            return None;
        }
        self.pos.set(end);
        Some(self.data[pos..end].to_vec())
    }
}

/// Finished uploads, by name. Files here are far smaller than a piece.
#[derive(Default)]
struct MemStore {
    objects: RefCell<BTreeMap<String, Vec<u8>>>,
}

#[async_trait(?Send)]
impl UploadStore for MemStore {
    type Body = MemBody;

    async fn complete(&self, name: &str) -> CoreResult<Option<StoredFile>> {
        Ok(self.objects.borrow().get(name).map(|d| StoredFile {
            size: d.len() as u64,
            md5: cb_core::upload::parse_md5(&md5_hex(d)),
        }))
    }

    async fn list_pieces(&self, _name: &str) -> CoreResult<Vec<StoredPiece>> {
        Ok(Vec::new())
    }

    async fn skip(&self, body: &MemBody, n: u64) -> CoreResult<bool> {
        Ok(body.take(n).is_some())
    }

    async fn put_piece(
        &self,
        _name: &str,
        _epoch: u64,
        _md5: &[u8; 16],
        _start: u64,
        _body: &MemBody,
        _len: u64,
    ) -> CoreResult<PieceWrite> {
        unreachable!("the files of the corpus are smaller than a piece")
    }

    async fn put_complete(
        &self,
        pieces: &PieceRun<'_>,
        body: &MemBody,
        tail: u64,
        md5: Option<[u8; 16]>,
    ) -> CoreResult<CompleteWrite> {
        let name = pieces.name;
        let Some(data) = body.take(tail) else {
            return Ok(CompleteWrite::BodyEnded);
        };
        if let Some(m) = md5 {
            if md5_to_hex(&m) != md5_hex(&data) {
                return Ok(CompleteWrite::Rejected);
            }
        }
        let mut objects = self.objects.borrow_mut();
        if objects.contains_key(name) {
            return Ok(CompleteWrite::Exists);
        }
        objects.insert(name.to_string(), data);
        Ok(CompleteWrite::Stored)
    }

    async fn delete_pieces(&self, _name: &str, _pieces: &[StoredPiece]) -> CoreResult<()> {
        Ok(())
    }
}
