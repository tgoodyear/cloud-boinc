//! The app the end-to-end tests run on a stock client. It reads the file
//! with the logical name `in`, writes it upper-cased to `out`, and finishes
//! the way `boinc_finish` does, without linking the BOINC library. Build it
//! static (`--target x86_64-unknown-linux-musl`) so it runs on any Linux host.
//!
//! `--seconds <n>` sleeps first, to make a job take that long.

use std::fs;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

/// `boinc_resolve_filename`: a slot file is either the file itself or a
/// one-line pointer into the project directory.
fn resolve(logical: &str) -> String {
    let Ok(text) = fs::read_to_string(logical) else {
        return logical.to_string();
    };
    match text
        .strip_prefix("<soft_link>")
        .and_then(|rest| rest.split("</soft_link>").next())
    {
        Some(target) => target.trim().to_string(),
        None => logical.to_string(),
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--seconds" {
            let n: f64 = args
                .next()
                .and_then(|v| v.parse().ok())
                .ok_or("--seconds needs a number")?;
            sleep(Duration::from_secs_f64(n.clamp(0.0, 3600.0)));
        }
    }
    let input = fs::read(resolve("in")).map_err(|e| format!("in: {e}"))?;
    fs::write(resolve("out"), input.to_ascii_uppercase()).map_err(|e| format!("out: {e}"))?;
    Ok(())
}

fn main() -> ExitCode {
    let status = match run() {
        Ok(()) => 0u8,
        Err(e) => {
            eprintln!("cb-test-app: {e}");
            1
        }
    };
    // What boinc_finish leaves for the client: the exit status, a message
    // line, and a line saying whether the message is a notice.
    let _ = fs::write("boinc_finish_called", format!("{status}\n\n"));
    ExitCode::from(status)
}
