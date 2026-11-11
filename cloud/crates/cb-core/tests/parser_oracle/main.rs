//! The BOINC client's own parsers read the replies this server writes.
//!
//! Each test builds replies in-process against SQLite, writes them to a
//! directory, runs the parser oracle on them and compares what the stock
//! client's C++ classes understood with what the server meant to say. The
//! expected values come from this side: fixture constants and the
//! structures the server builds its replies from, never from the oracle.
//!
//! The oracle is built from the upstream sources in this tree with
//! `make -C cloud/tests/parser-oracle`. Without it the tests still write
//! the replies and then skip, saying so. Set `CB_PARSER_ORACLE` to the path
//! of the `oracle` binary to use another build, `CB_PARSER_ORACLE_REQUIRED`
//! to make a missing oracle an error (CI does), and `CB_PARSER_ORACLE_REPLIES`
//! to choose where the replies are written. See `cloud/docs/compatibility.md`.

mod harness;
mod scheduler;
mod web;
