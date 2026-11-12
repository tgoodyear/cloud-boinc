use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};

use cb_import::config_xml;
use cb_import::error::{ImportError, Kind};
use cb_import::import::{preflight, run, Finished, Options};
use cb_import::schema::{all_tables, apply_migrations};
use cb_import::sink::{load_sql_dir, SqlDirSink, SqliteSink, DEFAULT_MAX_FILE_BYTES};
use cb_import::source::Source;
use cb_import::summary::Summary;
use cb_import::verify::{verify, verify_with};

#[derive(Parser)]
#[command(
    name = "cb-import",
    about = "Imports a stock BOINC project's database into cloud-boinc's schema"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum FinishedArg {
    Leave,
    All,
    Recent,
}

#[derive(Args, Clone)]
struct Common {
    /// File holding the source DSN, one line `mysql://user:password@host:port/database`.
    #[arg(long)]
    source_dsn_file: PathBuf,
    /// The stock project directory: result templates, and the download and upload trees.
    #[arg(long)]
    project_dir: Option<PathBuf>,
    /// The stock download directory, when it is not DIR/download of --project-dir.
    #[arg(long)]
    download_dir: Option<PathBuf>,
    /// The stock upload directory, when it is not DIR/upload of --project-dir.
    #[arg(long)]
    upload_dir: Option<PathBuf>,
    /// The stock config.xml: fan-out, validators, URLs.
    #[arg(long)]
    config: Option<PathBuf>,
    /// What to do with finished workunits: leave them behind, carry all, or
    /// carry those created in the last --finished-days days.
    #[arg(long, value_enum, default_value = "leave")]
    finished: FinishedArg,
    #[arg(long, default_value_t = 30)]
    finished_days: u32,
    /// Import workunits that have a size class, dropping the class.
    #[arg(long)]
    allow_size_class: bool,
    /// Import workunits that have a fileset_id, dropping it.
    #[arg(long)]
    allow_fileset_id: bool,
    /// The server's PASSWD_PEPPER file: bare-MD5 password hashes are stored as
    /// this server stores them. Without it every hash is carried unchanged.
    #[arg(long)]
    passwd_pepper_file: Option<PathBuf>,
    /// Check that the code-signing public key verifies the app version signatures.
    #[arg(long)]
    code_sign_public_key: Option<PathBuf>,
    /// A rehearsal from the database alone: result templates that cannot be read
    /// are stored empty and counted, no manifest is written, and the summary says
    /// the output is not loadable for a real move.
    #[arg(long)]
    rows_only: bool,
    /// List up to N file names in the warnings about missing or changed files
    /// (default 0: counts only).
    #[arg(long, default_value_t = 0)]
    list_missing: usize,
    /// Do not count valid results per account and computer (saves a scan of `result`).
    #[arg(long)]
    skip_valid_counts: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Reads the stock database and writes a SQLite file or D1 SQL files.
    Import {
        #[command(flatten)]
        common: Common,
        /// A new SQLite file with the target schema.
        #[arg(long)]
        target_sqlite: Option<PathBuf>,
        /// A directory for numbered .sql files of single-row INSERT statements.
        #[arg(long)]
        target_sql_dir: Option<PathBuf>,
        /// Size at which a .sql file is closed and the next one started.
        #[arg(long, default_value_t = DEFAULT_MAX_FILE_BYTES)]
        max_file_bytes: u64,
        /// Write the list of files to copy to the bucket.
        #[arg(long)]
        files_manifest: Option<PathBuf>,
        /// After the import, still inside the same read transaction (the same snapshot),
        /// compare this many rows per table with what was written. A separate `verify`
        /// run later only agrees on a source that is not changing.
        #[arg(long)]
        verify_sample: Option<u64>,
    },
    /// Compares the source with a target SQLite file.
    Verify {
        #[command(flatten)]
        common: Common,
        #[arg(long)]
        target_sqlite: PathBuf,
        /// Rows compared field by field in each table (chosen by ID).
        #[arg(long, default_value_t = 1000)]
        sample: u64,
    },
    /// Prints `cboinc config-set` lines for a stock config.xml. Applies nothing.
    Config {
        #[arg(long)]
        config: PathBuf,
    },
    /// Applies the .sql files of a directory to a new SQLite file (rehearsal without D1).
    LoadSql {
        #[arg(long)]
        sql_dir: PathBuf,
        #[arg(long)]
        target_sqlite: PathBuf,
    },
}

fn read_trim(p: &PathBuf, what: &str) -> Result<String, ImportError> {
    std::fs::read_to_string(p)
        .map(|s| s.trim().to_string())
        .map_err(|_| ImportError::usage(format!("cannot read the {what} file")))
}

fn options(c: &Common, manifest: Option<PathBuf>) -> Result<Options, ImportError> {
    let config = match &c.config {
        Some(p) => Some(config_xml::parse(&read_trim(p, "--config")?)?),
        None => None,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok(Options {
        finished: match c.finished {
            FinishedArg::Leave => Finished::Leave,
            FinishedArg::All => Finished::All,
            FinishedArg::Recent => Finished::Recent,
        },
        finished_days: c.finished_days,
        allow_size_class: c.allow_size_class,
        allow_fileset_id: c.allow_fileset_id,
        pepper: match &c.passwd_pepper_file {
            Some(p) => Some(read_trim(p, "--passwd-pepper-file")?.into_bytes()),
            None => None,
        },
        project_dir: c.project_dir.clone(),
        download_dir: c.download_dir.clone(),
        upload_dir: c.upload_dir.clone(),
        config,
        code_sign_public_key: match &c.code_sign_public_key {
            Some(p) => Some(read_trim(p, "--code-sign-public-key")?),
            None => None,
        },
        skip_valid_counts: c.skip_valid_counts,
        files_manifest: manifest,
        rows_only: c.rows_only,
        list_missing: c.list_missing,
        now,
    })
}

fn check_output_paths(
    c: &Common,
    sqlite: Option<&std::path::Path>,
    sql_dir: Option<&std::path::Path>,
    manifest: Option<&std::path::Path>,
) -> Result<(), ImportError> {
    let mut outputs: Vec<(&str, PathBuf)> = Vec::new();
    let mut inputs: Vec<(&str, PathBuf)> = Vec::new();
    if let Some(p) = sqlite {
        outputs.push(("--target-sqlite", p.to_path_buf()));
    }
    if let Some(p) = sql_dir {
        outputs.push(("--target-sql-dir", p.to_path_buf()));
    }
    if let Some(p) = manifest {
        outputs.push(("--files-manifest", p.to_path_buf()));
    }
    inputs.push(("--source-dsn-file", c.source_dsn_file.clone()));
    for (n, p) in [
        ("--config", &c.config),
        ("--passwd-pepper-file", &c.passwd_pepper_file),
        ("--code-sign-public-key", &c.code_sign_public_key),
    ] {
        if let Some(p) = p {
            inputs.push((n, p.clone()));
        }
    }
    cb_import::fsutil::check_distinct_paths(&outputs, &inputs)
}

fn opts_rows_only_manifest(c: &Common, m: &Option<PathBuf>) -> bool {
    c.rows_only && m.is_some()
}

fn go() -> Result<i32, ImportError> {
    let cli = Cli::parse();
    match cli.command {
        Command::Config { config } => {
            let cfg = config_xml::parse(&read_trim(&config, "--config")?)?;
            for l in config_xml::config_set_lines(&cfg) {
                println!("{l}");
            }
            Ok(0)
        }
        Command::LoadSql {
            sql_dir,
            target_sqlite,
        } => {
            // `load_sql_dir` removes the file only when it created it.
            let files = load_sql_dir(&sql_dir, &target_sqlite)?;
            println!("applied {} files", files.len());
            Ok(0)
        }
        Command::Import {
            common,
            target_sqlite,
            target_sql_dir,
            max_file_bytes,
            files_manifest,
            verify_sample,
        } => {
            if opts_rows_only_manifest(&common, &files_manifest) {
                return Err(ImportError::usage(
                    "--rows-only writes no manifest; drop --files-manifest",
                ));
            }
            if target_sqlite.is_some() == target_sql_dir.is_some() {
                return Err(ImportError::usage(
                    "give exactly one of --target-sqlite and --target-sql-dir",
                ));
            }
            // Before anything is created or connected: no output may be another output, or
            // an input, under any spelling of its path.
            check_output_paths(
                &common,
                target_sqlite.as_deref(),
                target_sql_dir.as_deref(),
                files_manifest.as_deref(),
            )?;
            let opts = options(&common, files_manifest)?;
            let mut src = Source::connect(&common.source_dsn_file)?;
            let mut summary = Summary::default();
            let pre = preflight(&mut src, &opts, &mut summary)?;
            let mem = rusqlite::Connection::open_in_memory()?;
            apply_migrations(&mem)?;
            let tables = all_tables(&mem)?;
            let result = if let Some(path) = &target_sqlite {
                let mut sink = SqliteSink::create(path)?;
                let r = run(&mut src, &mut sink, &opts, &pre, &tables, &mut summary);
                drop(sink);
                if r.is_err() {
                    let _ = std::fs::remove_file(path);
                }
                r
            } else {
                let dir = target_sql_dir.as_ref().unwrap();
                let mut sink = SqlDirSink::create(dir, max_file_bytes)?;
                let r = run(&mut src, &mut sink, &opts, &pre, &tables, &mut summary);
                if r.is_ok() {
                    summary.count("sql files written", sink.files.len() as u64);
                    summary.count("rows split into more than one statement", sink.split_rows);
                    summary.count("longest statement in bytes", sink.max_statement_seen as u64);
                } else {
                    // Only what this run created: the marker is in the list only once
                    // this run has written it.
                    for f in sink.files.iter().chain(sink.marker.iter()) {
                        let _ = std::fs::remove_file(f);
                    }
                }
                r
            };
            result?;
            print!("{}", summary.render());
            let Some(n) = verify_sample else {
                return Ok(0);
            };
            // Same session, same snapshot: the source cannot have drifted, and the facts
            // the import took at its start are reused.
            let _tmp; // owner-only directory, removed when this scope ends
            let sqlite = match &target_sqlite {
                Some(p) => p.clone(),
                None => {
                    let dir = target_sql_dir.as_ref().unwrap();
                    let parent = dir
                        .parent()
                        .filter(|p| !p.as_os_str().is_empty())
                        .unwrap_or(std::path::Path::new("."));
                    // Next to the output, where the data already is; 0700 by construction.
                    let t = tempfile::Builder::new()
                        .prefix(".cb-import-verify-")
                        .tempdir_in(parent)
                        .or_else(|_| tempfile::tempdir())
                        .map_err(|_| ImportError::usage("cannot make a temporary directory"))?;
                    let p = t.path().join("verify.sqlite");
                    _tmp = t;
                    load_sql_dir(dir, &p)?;
                    p
                }
            };
            let mut vsum = Summary::default();
            let report = verify_with(&mut src, &sqlite, &opts, n, &mut vsum, Some(&pre))?;
            println!("verify inside the import's snapshot:");
            print!("{}", report.render());
            Ok(if report.ok() {
                0
            } else {
                ImportError::new(Kind::Mismatch, "").exit_code()
            })
        }
        Command::Verify {
            common,
            target_sqlite,
            sample,
        } => {
            let opts = options(&common, None)?;
            let mut src = Source::connect(&common.source_dsn_file)?;
            let mut summary = Summary::default();
            let report = verify(&mut src, &target_sqlite, &opts, sample, &mut summary)?;
            print!("{}", report.render());
            Ok(if report.ok() {
                0
            } else {
                ImportError::new(Kind::Mismatch, "").exit_code()
            })
        }
    }
}

fn main() -> ExitCode {
    match go() {
        Ok(0) => ExitCode::SUCCESS,
        Ok(n) => ExitCode::from(n as u8),
        Err(e) => {
            eprintln!("cb-import: {}", e.message);
            ExitCode::from(e.exit_code() as u8)
        }
    }
}
