# Compatibility with the stock client

The aim is that an existing project can move to this server without its volunteers installing a
new client. The parser oracle checks one part of that claim: that the stock client's own C++
parsers accept the replies this server writes and read the values the server meant.

## What the oracle is

`cloud/tests/parser-oracle/` builds two small programs, `oracle` and `oracle-gui`, from the
upstream sources in this tree. They contain no parsing code of their own. They call the client's
classes and functions and print what those filled in, one `key=value` line per field:

| Reply | Client code that reads it |
|-------|---------------------------|
| Scheduler reply | `SCHEDULER_REPLY::parse` in `client/scheduler_op.cpp`, which calls `APP::parse`, `FILE_INFO::parse`, `APP_VERSION::parse`, `WORKUNIT::parse`, `RESULT::parse_server` and `FILE_REF::parse` in `client/client_types.cpp` and `client/result.cpp`; then `CLIENT_STATE::handle_scheduler_reply` in `client/cs_scheduler.cpp`, which links the objects to each other and applies the preferences |
| Master page | `SCHEDULER_OP::poll` and `SCHEDULER_OP::parse_master_file` in `client/scheduler_op.cpp` |
| Upload handler reply | `FILE_XFER::parse_upload_response` in `client/file_xfer.cpp` |
| `get_project_config.php` | The core's `handle_get_project_config_poll` in `client/gui_rpc_server_ops.cpp`, then `RPC_CLIENT::get_project_config_poll` and `PROJECT_CONFIG::parse` in `lib/gui_rpc_client_ops.cpp` |
| `lookup_account.php`, `create_account.php` | The core's `handle_lookup_account_poll` and `handle_create_account_poll`, then `RPC_CLIENT::lookup_account_poll`, `create_account_poll` and `ACCOUNT_OUT::parse` |

The web RPC replies follow the same path as in a running client. The core reads the reply file
(`LOOKUP_ACCOUNT_OP::handle_reply` in `client/acct_setup.cpp`) and answers a `*_poll` GUI RPC over
a socket pair, using the real `GUI_RPC_CONN`; the GUI side then reads that answer with the real
`RPC_CLIENT`. The two halves are separate programs because `lib/gui_rpc_client_ops.cpp` defines its
own `APP`, `PROJECT`, `RESULT` and other classes that clash with the client's at link time; upstream
never links them into one binary either. `oracle` runs `oracle-gui` itself for those reply kinds.

Beyond parsing, the oracle runs three checks with the client's own code:

- Signatures. `check_file_signature` and `check_string_signature` from `lib/crypt.cpp` verify the
  `<file_signature>` of application files and the signature of a new code-signing key, against the
  key in the reply.
- References. `handle_scheduler_reply` links apps, app versions, workunits, results and their files
  and reports what it cannot link ("No app version found", "Missing app", ...). The test fails on
  any such message.
- Upload requests. `FILE_XFER::init_upload` builds the request the client would send for each
  output file, from the `<file_info>` it parsed. The test feeds it to the upload handler's parser
  and checks the certificate.

Nothing is stubbed. `oracle` links every file of `boinc_client_SOURCES` in `client/Makefile.am`
except `main.cpp`, and the library files of `lib/`; the compiler flags and the include path are
upstream's. Four deviations:

- `config.h` is `cloud/tests/parser-oracle/config.h`, a trimmed copy of what `./configure` writes
  on x86-64 Linux, because the oracle does not run the autotools build.
- `oracle.cpp` defines `private` as `public` while including the client headers, so that it can
  put `SCHEDULER_OP` in the state a finished HTTP fetch leaves and call its real `poll()`.
- The oracle sets up the client's global state by hand: the platform list (`--platform`) and the
  GPUs of the host (`--gpu`, which also marks the CUDA, CAL and OpenCL libraries present, as the
  client's plan class check wants). A real client detects these.
- The client's messages go to standard error, and each run uses a fresh temporary directory as the
  client's working directory.

## What the tests compare

The Rust tests in `cloud/crates/cb-core/tests/parser_oracle/` build the replies in-process against
SQLite, using the same functions the Workers call: `handle_request`, `create_work`,
`add_app_version` and `register_job_file` for the scheduler, `project_config`, `lookup_account`,
`create_account` and `master_page` for the web RPCs, and the `cb_proto::upload` reply writers.
They write every reply to a directory, run the oracle on it and compare what the client read with
what the server meant. The expected values come from fixture constants and from the structures the
server builds its replies from, not from the oracle or from a recorded file. Any mismatch fails
the test, and so does any message the client logs as an error or any element it does not
recognise, unless the test names it and says why.

Replies and fields covered:

- Scheduler reply with work: project, user, host and team fields, credit, global and project
  preferences (and that the client applies the preferences), code-signing key, application, two
  signed application files, three input files, two workunits with command line and resource
  bounds, two results with deadlines and output files, output file upload URLs, size limits and
  upload certificates, and the references between them.
- Scheduler reply with no work, with an unknown account key, with an unsupported platform, and
  from a project that is down.
- Scheduler reply with result acknowledgements, aborts, conditional aborts, file deletions, GUI
  URLs, project files, request and next-RPC delays, messages of each priority, `<send_full_workload/>`
  and the other flags, a key signature, and a GPU application version.
- `get_project_config.php` as the server writes it from project settings (accounts open, closed,
  scheduler stopped) and each variant of `ProjectConfig`.
- `lookup_account.php` and `create_account.php` for success, repeated creation, and every refusal
  `AccountError` defines.
- Upload handler replies: success, file size queries, and every refusal message the handler and
  `cb_proto::upload` produce, transient and permanent.
- The master page, with the project's scheduler URL found through `<link rel="boinc_scheduler">`
  and the commented `<scheduler>` form of older clients.

## Running the oracle

On Linux with a C++ compiler, `make`, and the development packages of OpenSSL, libcurl and zlib
(on Debian or Ubuntu: `g++ make libssl-dev libcurl4-openssl-dev zlib1g-dev`):

```
make -C cloud/tests/parser-oracle -j4
cd cloud && cargo test -p cb-core --test parser_oracle
```

The build took about 25 seconds on four cores, and the tests run in about three seconds. Without the built oracle the tests write the replies
and skip, with a message. `CB_PARSER_ORACLE_REQUIRED=1` turns a missing oracle into a failure, as
the `parser-oracle` job in `.github/workflows/cloud-ci.yml` does. `CB_PARSER_ORACLE` names another
`oracle` binary and `CB_PARSER_ORACLE_REPLIES` names the directory the replies go to (by default
`target/tmp/parser-oracle-replies` under `cloud/`). To look at one reply by hand:

```
cloud/tests/parser-oracle/build/oracle --kind scheduler_reply --file reply.xml \
    --master-url https://project.example/ --platform x86_64-pc-linux-gnu
```

Kinds are `scheduler_reply`, `upload_reply`, `master`, `project_config`, `lookup_account` and
`create_account`. Further flags: `--gpu`, `--file-md5 NAME=HEX` and `--code-sign-key FILE` for
signature checks, and `--prior-reply FILE` to give the client the earlier replies a reply refers
to.

## What it does not cover

- One client version. The oracle is the client in this tree (see `version.h`). Older clients
  parse with older code. The scheduler reply writer follows the
  stock server's per-version forms (`core_client_version` in `SchedulerReply`), and they are
  tested in `cb-proto`, but not against old client code.
- Direction. The oracle reads server replies. The requests a client sends are checked by the
  fixtures in `cb-proto` and, for uploads, by the request `FILE_XFER::init_upload` builds, as above.
- Network behaviour. No HTTP, TLS, redirects, proxies, retries or timeouts; the client's HTTP code
  is linked but not run. Header handling such as `Content-Type` is not checked.
- Client behaviour after parsing: running jobs, downloads, file verification against the downloaded
  bytes (only the signature check against a given MD5 is run), scheduling and backoff.
- The BOINC Manager's use of the project config (the attach wizard), and account managers.

## Findings

Each is a difference between what the server writes and what the client reads, with the client
code responsible.

1. Fixed. An upload handler error message that contains `<` reached the user cut short. The
   handler wrote `<message>Missing <data> tag</message>`, as upstream's `return_error` does, and
   `FILE_XFER::parse_upload_response` in `client/file_xfer.cpp` reads it with `parse_str` from
   `lib/parse.cpp`, which ends the value at the first `<`. The user saw "Missing". `reply_error` in
   `cloud/crates/cb-proto/src/upload.rs` now escapes the message; `parse_str` unescapes it. The
   status was never affected, so the client already treated the error as permanent.
2. Not fixed, same as the stock server. `<gpu_ram>` in an `<app_version>` is ignored. The stock
   scheduler writes `<gpu_ram>` (`APP_VERSION::write` in `sched/sched_types.cpp`), but
   `APP_VERSION::parse` in `client/client_types.cpp` reads `resource_usage.gpu_ram`, so the value
   never reaches the client. The test `gpu_app_version` records that the client reads 0.
3. Not fixed, same as the stock server. `<verify_files_on_app_start/>` is not read by
   `SCHEDULER_REPLY::parse`; the client logs it as unrecognised when `unparsed_xml` logging is on.
4. Not a defect. `get_project_config.php` lists one `<platform>` per platform and plan class, as
   upstream's script does, so a platform with two plan classes appears twice, and
   `PROJECT_CONFIG::parse` in `lib/gui_rpc_client_ops.cpp` adds one entry to `platforms` for each.
5. Not a defect. `GLOBAL_PREFS::parse` in `lib/prefs.cpp` limits `<mod_time>` to the
   client's clock. Preferences stored with a modification time in the future are applied with the
   time the client reads them, and are then not newer than a later copy.

The run found nothing else: every field listed above matched, the client linked every app version,
workunit, result and file of the work replies, verified the application file signatures against the
key in the reply, accepted the stored preferences, and built upload requests the upload handler
accepts.
