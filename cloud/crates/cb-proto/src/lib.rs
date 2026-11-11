//! BOINC wire protocols.
//!
//! This crate holds the parsers and writers for everything a stock BOINC
//! client exchanges with a project server:
//!
//! - [`xml`]: a tolerant tokenizer modelled on `XML_PARSER` in `lib/parse.cpp`,
//!   the escaping rules of `xml_escape` / `xml_unescape`, and a small writer.
//! - [`sched_request`]: the `<scheduler_request>` document, following
//!   `SCHEDULER_REQUEST::parse` in `sched/sched_types.cpp`.
//! - [`coproc`]: the `<coprocs>` element, following `lib/coproc.cpp`.
//! - [`sched_reply`]: the `<scheduler_reply>` document, following
//!   `SCHEDULER_REPLY::write`, checked against the client's
//!   `SCHEDULER_REPLY::parse` in `client/scheduler_op.cpp`.
//! - [`upload`]: the `file_upload_handler` protocol.
//! - [`web_rpc`]: replies of `get_project_config.php`, `lookup_account.php`
//!   and `create_account.php`, and the error numbers of `lib/error_numbers.h`.
//! - [`consts`]: state constants from `db/boinc_db_types.h` and
//!   `lib/common_defs.h`.
//!
//! The crate does no I/O and has no dependencies, so it builds for native
//! targets and for `wasm32-unknown-unknown` alike.

/// The upstream idiom `if (xp.parse_x("name", dst)) continue;`.
macro_rules! take {
    ($xp:ident . $method:ident ( $name:literal ) => $dst:expr) => {
        if let Some(v) = $xp.$method($name) {
            $dst = v;
            continue;
        }
    };
}

pub mod consts;
pub mod coproc;
mod error;
pub mod sched_reply;
pub mod sched_request;
pub mod upload;
pub mod web_rpc;
pub mod xml;

pub use error::ProtoError;
pub use sched_reply::SchedulerReply;
pub use sched_request::SchedulerRequest;

/// Format a float the way C's `printf("%f")` does.
///
/// Upstream writes almost every floating point value with `%f`; the client
/// reads them back with `strtod`, which accepts `inf` and `nan`.
pub fn fmt_f(x: f64) -> String {
    if x.is_nan() {
        "nan".to_string()
    } else if x.is_infinite() {
        if x < 0.0 { "-inf" } else { "inf" }.to_string()
    } else {
        format!("{x:.6}")
    }
}

#[cfg(test)]
mod tests {
    use super::fmt_f;

    #[test]
    fn fmt_f_matches_printf() {
        assert_eq!(fmt_f(1.0), "1.000000");
        assert_eq!(fmt_f(0.1234567), "0.123457");
        assert_eq!(fmt_f(-2.5), "-2.500000");
        assert_eq!(fmt_f(f64::INFINITY), "inf");
        assert_eq!(fmt_f(f64::NEG_INFINITY), "-inf");
        assert_eq!(fmt_f(f64::NAN), "nan");
    }
}
