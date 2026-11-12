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
use cb_import::verify::verify;

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
        now,
    })
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
            let r = load_sql_dir(&sql_dir, &target_sqlite);
            match r {
                Ok(files) => {
                    println!("applied {} files", files.len());
                    Ok(0)
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&target_sqlite);
                    Err(e)
                }
            }
        }
        Command::Import {
            common,
            target_sqlite,
            target_sql_dir,
            max_file_bytes,
            files_manifest,
        } => {
            if target_sqlite.is_some() == target_sql_dir.is_some() {
                return Err(ImportError::usage(
                    "give exactly one of --target-sqlite and --target-sql-dir",
                ));
            }
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
                    for f in &sink.files {
                        let _ = std::fs::remove_file(f);
                    }
                }
                r
            };
            result?;
            print!("{}", summary.render());
            Ok(0)
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
