//! `cboinc`: the admin API of a cloud-boinc deployment from the command
//! line, plus the key tools that run offline.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cb_crypto::{generate_key_pair, sign_md5, PrivateKey};
use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};

mod load;

type Res<T> = Result<T, String>;

#[derive(Parser)]
#[command(name = "cboinc", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Where the deployment is and how to authenticate.
#[derive(Args, Clone)]
struct Server {
    /// Base URL of the deployment, e.g. https://boinc.example.org
    #[arg(long, env = "CBOINC_URL")]
    url: String,
    /// File holding an admin token. Without it, CBOINC_TOKEN is used.
    #[arg(long, env = "CBOINC_TOKEN_FILE")]
    token_file: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a key pair in the BOINC key file format.
    Keygen {
        #[arg(long)]
        private: PathBuf,
        #[arg(long)]
        public: PathBuf,
    },
    /// Print the signature block of a file, as `sign_executable` does.
    Sign {
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        file: PathBuf,
    },
    /// Project counts.
    Status {
        #[command(flatten)]
        server: Server,
    },
    /// Platforms, apps and app versions.
    Catalog {
        #[command(flatten)]
        server: Server,
    },
    /// Print a setting: config, plan_class_spec, code_sign_public_key or upload_public_key.
    ConfigGet {
        #[command(flatten)]
        server: Server,
        #[arg(long)]
        name: String,
    },
    /// Replace a setting with the contents of a file.
    ConfigPut {
        #[command(flatten)]
        server: Server,
        #[arg(long)]
        name: String,
        #[arg(long)]
        file: PathBuf,
    },
    /// Create or update a platform.
    PlatformPut {
        #[command(flatten)]
        server: Server,
        #[arg(long)]
        name: String,
        #[arg(long)]
        user_friendly_name: String,
    },
    /// Create or update an app.
    AppPut {
        #[command(flatten)]
        server: Server,
        #[arg(long)]
        name: String,
        #[arg(long)]
        user_friendly_name: String,
        /// trivial, bitwise or webhook
        #[arg(long, default_value = "trivial")]
        validator: String,
        #[arg(long, default_value = "")]
        validator_url: String,
        #[arg(long, default_value = "")]
        assimilate_url: String,
        #[arg(long, default_value_t = 0)]
        target_nresults: i32,
    },
    /// Store a file under download/ and record it for use by jobs.
    FilePut {
        #[command(flatten)]
        server: Server,
        #[arg(long)]
        file: PathBuf,
        /// Name in the project; the file's own name if not given.
        #[arg(long)]
        name: Option<String>,
        /// Keep the file when the jobs using it are done.
        #[arg(long)]
        no_delete: bool,
        /// Overwrite a stored file of the same name that has other contents.
        #[arg(long)]
        replace: bool,
    },
    /// Upload, sign and register the files of an app version.
    VersionAdd {
        #[command(flatten)]
        server: Server,
        #[arg(long)]
        app: String,
        #[arg(long)]
        platform: String,
        #[arg(long)]
        version_num: i32,
        #[arg(long, default_value = "")]
        plan_class: String,
        #[arg(long, default_value = "")]
        api_version: String,
        /// The code-signing private key. It stays on this machine.
        #[arg(long)]
        key: PathBuf,
        /// The main program. It is stored as <app>_<version>_<platform>[__<plan class>].
        #[arg(long)]
        main_program: PathBuf,
        /// Another file of the version, as <logical name>=<path>; may repeat.
        #[arg(long = "extra-file")]
        extra_files: Vec<String>,
        #[arg(long)]
        is_wrapper: bool,
    },
    /// Stop sending an app version, or restore it with --restore.
    VersionDeprecate {
        #[command(flatten)]
        server: Server,
        #[arg(long)]
        id: i64,
        #[arg(long)]
        restore: bool,
    },
    /// Submit jobs of one app.
    Submit {
        #[command(flatten)]
        server: Server,
        #[arg(long)]
        app: String,
        #[arg(long)]
        input_template: PathBuf,
        #[arg(long)]
        result_template: PathBuf,
        /// JSON array of job objects. Without it, --count jobs are made.
        #[arg(long)]
        jobs: Option<PathBuf>,
        #[arg(long, default_value_t = 1)]
        count: u32,
        /// Jobs are named <prefix>_<number>.
        #[arg(long, default_value = "job")]
        name_prefix: String,
        /// A staged input file of every job; may repeat.
        #[arg(long = "input-file")]
        input_files: Vec<String>,
        #[arg(long)]
        command_line: Option<String>,
        #[arg(long)]
        batch: Option<i64>,
        /// Sets min_quorum and target_nresults.
        #[arg(long)]
        quorum: Option<i32>,
        #[arg(long)]
        delay_bound: Option<i64>,
    },
    /// The state of a job and its results.
    Job {
        #[command(flatten)]
        server: Server,
        #[arg(long)]
        name: String,
    },
    /// Cancel a batch, or workunits by ID.
    Cancel {
        #[command(flatten)]
        server: Server,
        #[arg(long)]
        batch: Option<i64>,
        #[arg(long = "workunit-id")]
        workunit_ids: Vec<i64>,
    },
    /// Workunits ready to assimilate.
    Assimilate {
        #[command(flatten)]
        server: Server,
        #[arg(long)]
        app: String,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: i64,
    },
    /// Mark workunits assimilated, which lets their files be deleted.
    Ack {
        #[command(flatten)]
        server: Server,
        #[arg(long = "workunit-id", required = true)]
        workunit_ids: Vec<i64>,
    },
    /// Fetch an uploaded output file.
    OutputGet {
        #[command(flatten)]
        server: Server,
        #[arg(long)]
        name: String,
        #[arg(long)]
        out: PathBuf,
    },
    /// Create an account and print its authenticator.
    UserAdd {
        #[command(flatten)]
        server: Server,
        #[arg(long)]
        email: String,
        #[arg(long, default_value = "")]
        user_name: String,
        /// Without a password the account works by authenticator only.
        #[arg(long)]
        password: Option<String>,
    },
    /// Send scheduler requests from simulated hosts and report latency.
    Load(load::LoadArgs),
}

fn read(path: &Path) -> Res<String> {
    fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn file_md5(path: &Path) -> Res<(String, u64)> {
    let mut f = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut md5 = cb_crypto::Md5Stream::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        md5.update(&buf[..n]);
    }
    let nbytes = md5.nbytes();
    Ok((md5.finalize_hex(), nbytes))
}

fn private_key(path: &Path) -> Res<PrivateKey> {
    PrivateKey::parse(&read(path)?).map_err(|e| format!("{}: {e}", path.display()))
}

struct Client {
    base: String,
    token: String,
    agent: ureq::Agent,
}

impl Client {
    fn new(server: &Server) -> Res<Client> {
        let token = match &server.token_file {
            Some(p) => read(p)?,
            None => std::env::var("CBOINC_TOKEN")
                .map_err(|_| "give --token-file or set CBOINC_TOKEN".to_string())?,
        };
        Ok(Client {
            base: server.url.trim_end_matches('/').to_string(),
            token: token.trim().to_string(),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(std::time::Duration::from_secs(15))
                .build(),
        })
    }

    fn request(&self, method: &str, path: &str) -> ureq::Request {
        self.agent
            .request(method, &format!("{}/admin/v1/{path}", self.base))
            .set("Authorization", &format!("Bearer {}", self.token))
    }

    fn finish(r: Result<ureq::Response, ureq::Error>) -> Res<ureq::Response> {
        match r {
            Ok(r) => Ok(r),
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                let message = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v["error"].as_str().map(str::to_string))
                    .unwrap_or(body);
                Err(format!("HTTP {code}: {message}"))
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn json(r: ureq::Response) -> Res<Value> {
        r.into_json().map_err(|e| e.to_string())
    }

    fn get(&self, path: &str) -> Res<Value> {
        Self::json(Self::finish(self.request("GET", path).call())?)
    }

    fn send(&self, method: &str, path: &str, body: &Value) -> Res<Value> {
        Self::json(Self::finish(
            self.request(method, path).send_json(body.clone()),
        )?)
    }

    /// Stores a local file under `download/<name>`; returns what the server
    /// recorded.
    fn put_file(&self, path: &Path, name: &str, no_delete: bool, replace: bool) -> Res<Value> {
        let (md5, _) = file_md5(path)?;
        let f = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let len = f
            .metadata()
            .map_err(|e| format!("{}: {e}", path.display()))?
            .len();
        let mut target = format!("files/{name}?md5={md5}");
        if no_delete {
            target.push_str("&no_delete=1");
        }
        if replace {
            target.push_str("&replace=1");
        }
        Self::json(Self::finish(
            self.request("PUT", &target)
                .set("Content-Type", "application/octet-stream")
                .set("Content-Length", &len.to_string())
                .send(f),
        )?)
    }
}

fn show(v: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
    );
}

fn file_name(path: &Path) -> Res<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .ok_or_else(|| format!("{}: no file name", path.display()))
}

/// Uploads a version file, checks the server stored what is on disk, and
/// signs its MD5.
fn stage_signed(client: &Client, key: &PrivateKey, path: &Path, name: &str) -> Res<String> {
    let (md5, nbytes) = file_md5(path)?;
    let stored = client.put_file(path, name, true, false)?;
    if stored["md5"].as_str() != Some(md5.as_str()) || stored["nbytes"].as_u64() != Some(nbytes) {
        return Err(format!(
            "{name}: the server stored something else than {}",
            path.display()
        ));
    }
    sign_md5(&md5, key).map_err(|e| e.to_string())
}

fn run(cli: Cli) -> Res<()> {
    match cli.command {
        Command::Keygen { private, public } => {
            if private.exists() || public.exists() {
                return Err("refusing to overwrite an existing key file".into());
            }
            let (sk, pk) = generate_key_pair().map_err(|e| e.to_string())?;
            write_private(&private, &sk.to_key_string())?;
            fs::write(&public, pk.to_key_string())
                .map_err(|e| format!("{}: {e}", public.display()))?;
        }
        Command::Sign { key, file } => {
            let (md5, _) = file_md5(&file)?;
            print!(
                "{}",
                sign_md5(&md5, &private_key(&key)?).map_err(|e| e.to_string())?
            );
        }
        Command::Status { server } => show(&Client::new(&server)?.get("status")?),
        Command::Catalog { server } => show(&Client::new(&server)?.get("catalog")?),
        Command::ConfigGet { server, name } => {
            let c = Client::new(&server)?;
            let r = Client::finish(c.request("GET", &format!("config/{name}")).call())?;
            print!("{}", r.into_string().map_err(|e| e.to_string())?);
        }
        Command::ConfigPut { server, name, file } => {
            let c = Client::new(&server)?;
            let body = read(&file)?;
            Client::finish(
                c.request("PUT", &format!("config/{name}"))
                    .set("Content-Type", "text/plain")
                    .send_string(&body),
            )?;
        }
        Command::PlatformPut {
            server,
            name,
            user_friendly_name,
        } => show(&Client::new(&server)?.send(
            "PUT",
            "platforms",
            &json!({ "name": name, "user_friendly_name": user_friendly_name }),
        )?),
        Command::AppPut {
            server,
            name,
            user_friendly_name,
            validator,
            validator_url,
            assimilate_url,
            target_nresults,
        } => show(&Client::new(&server)?.send(
            "PUT",
            "apps",
            &json!({
                "name": name,
                "user_friendly_name": user_friendly_name,
                "validator": validator,
                "validator_url": validator_url,
                "assimilate_url": assimilate_url,
                "target_nresults": target_nresults,
            }),
        )?),
        Command::FilePut {
            server,
            file,
            name,
            no_delete,
            replace,
        } => {
            let name = match name {
                Some(n) => n,
                None => file_name(&file)?,
            };
            show(&Client::new(&server)?.put_file(&file, &name, no_delete, replace)?)
        }
        Command::VersionAdd {
            server,
            app,
            platform,
            version_num,
            plan_class,
            api_version,
            key,
            main_program,
            extra_files,
            is_wrapper,
        } => {
            let client = Client::new(&server)?;
            let key = private_key(&key)?;
            let suffix = if plan_class.is_empty() {
                String::new()
            } else {
                format!("__{plan_class}")
            };
            let stem = format!(
                "{app}_{}.{:02}_{platform}{suffix}",
                version_num / 100,
                version_num % 100
            );
            let mut files = vec![json!({
                "name": stem,
                "signature": stage_signed(&client, &key, &main_program, &stem)?,
                "main_program": true,
                "executable": true,
            })];
            for spec in &extra_files {
                let (logical, path) = spec
                    .split_once('=')
                    .ok_or_else(|| format!("--extra-file {spec}: want <logical name>=<path>"))?;
                let name = format!("{stem}_{logical}");
                files.push(json!({
                    "name": name,
                    "signature": stage_signed(&client, &key, Path::new(path), &name)?,
                    "open_name": logical,
                    "copy_file": true,
                }));
            }
            show(&client.send(
                "POST",
                "app-versions",
                &json!({
                    "app": app,
                    "platform": platform,
                    "version_num": version_num,
                    "plan_class": plan_class,
                    "api_version": api_version,
                    "is_wrapper": is_wrapper,
                    "files": files,
                }),
            )?)
        }
        Command::VersionDeprecate {
            server,
            id,
            restore,
        } => show(&Client::new(&server)?.send(
            "POST",
            &format!("app-versions/{id}/deprecate"),
            &json!({ "deprecated": !restore }),
        )?),
        Command::Submit {
            server,
            app,
            input_template,
            result_template,
            jobs,
            count,
            name_prefix,
            input_files,
            command_line,
            batch,
            quorum,
            delay_bound,
        } => {
            let jobs = match jobs {
                Some(p) => serde_json::from_str::<Value>(&read(&p)?)
                    .map_err(|e| format!("{}: {e}", p.display()))?,
                None => {
                    let inputs: Vec<Value> =
                        input_files.iter().map(|n| json!({ "name": n })).collect();
                    Value::Array(
                        (0..count)
                            .map(|i| {
                                json!({
                                    "name": format!("{name_prefix}_{i}"),
                                    "input_files": inputs,
                                    "command_line": command_line,
                                    "batch": batch,
                                    "min_quorum": quorum,
                                    "target_nresults": quorum,
                                    "delay_bound": delay_bound,
                                })
                            })
                            .collect(),
                    )
                }
            };
            show(&Client::new(&server)?.send(
                "POST",
                "jobs",
                &json!({
                    "app": app,
                    "input_template": read(&input_template)?,
                    "result_template": read(&result_template)?,
                    "jobs": jobs,
                }),
            )?)
        }
        Command::Job { server, name } => show(&Client::new(&server)?.get(&format!("jobs/{name}"))?),
        Command::Cancel {
            server,
            batch,
            workunit_ids,
        } => {
            if batch.is_none() && workunit_ids.is_empty() {
                return Err("give --batch or --workunit-id".into());
            }
            show(&Client::new(&server)?.send(
                "POST",
                "jobs/cancel",
                &json!({ "batch": batch, "workunit_ids": workunit_ids }),
            )?)
        }
        Command::Assimilate {
            server,
            app,
            after,
            limit,
        } => show(
            &Client::new(&server)?
                .get(&format!("assimilate?app={app}&after={after}&limit={limit}"))?,
        ),
        Command::Ack {
            server,
            workunit_ids,
        } => show(&Client::new(&server)?.send(
            "POST",
            "assimilate/ack",
            &json!({ "workunit_ids": workunit_ids }),
        )?),
        Command::OutputGet { server, name, out } => {
            let c = Client::new(&server)?;
            let r = Client::finish(c.request("GET", &format!("outputs/{name}")).call())?;
            let mut f = fs::File::create(&out).map_err(|e| format!("{}: {e}", out.display()))?;
            std::io::copy(&mut r.into_reader(), &mut f)
                .map_err(|e| format!("{}: {e}", out.display()))?;
        }
        Command::UserAdd {
            server,
            email,
            user_name,
            password,
        } => {
            let email = email.trim().to_lowercase();
            let hash = password
                .map(|p| cb_crypto::passwd_hash(&p, &email))
                .unwrap_or_default();
            show(&Client::new(&server)?.send(
                "POST",
                "users",
                &json!({ "email_addr": email, "passwd_hash": hash, "user_name": user_name }),
            )?)
        }
        Command::Load(args) => load::run(&args)?,
    }
    Ok(())
}

#[cfg(unix)]
fn write_private(path: &Path, text: &str) -> Res<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(text.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(not(unix))]
fn write_private(path: &Path, text: &str) -> Res<()> {
    fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cboinc: {e}");
            ExitCode::FAILURE
        }
    }
}
