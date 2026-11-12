# Compatibility with the stock client

The aim is that an existing project can move to this server without its volunteers installing a
new client. Two tests check parts of that claim. The parser oracle checks that the stock client's
own C++ parsers accept the replies this server writes and read the values the server meant. The
differential test, further down, checks that the replies equal what the stock server writes to the
same requests, field by field, apart from differences that are listed here with their reasons.

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
recognise, unless the test names it and says why. An element a parser skips is visible in two
places: the client's message log (`[unparsed_xml]`) and standard error, where
`XML_PARSER::skip_unexpected` in `lib/parse.cpp` writes "Unrecognized XML tag" for the parsers
that cannot log (`GLOBAL_PREFS::parse_day` returns success after calling it). The harness reads
both, and `Expect::quiet` applies one allowlist to both
(`an_unknown_element_inside_day_prefs_is_caught` in
`cloud/crates/cb-core/tests/parser_oracle/scheduler.rs`).

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
`oracle` binary. Each test writes its replies into a directory of its own, so tests running in
parallel do not overwrite each other. The directories are removed when a test passes. Set
`CB_PARSER_ORACLE_REPLIES` to a directory (absolute or relative) to keep them there, under
`<group>/<process id>-<number>`. To look at one reply by hand:

```
cloud/tests/parser-oracle/build/oracle --kind scheduler_reply --file reply.xml \
    --master-url https://project.example/ --platform x86_64-pc-linux-gnu
```

Kinds are `scheduler_reply`, `upload_reply`, `master`, `project_config`, `lookup_account` and
`create_account`. Further flags: `--gpu`, `--file-md5 NAME=HEX` and `--code-sign-key FILE` for
signature checks, and `--prior-reply FILE` to give the client the earlier replies a reply refers
to. Paths may be relative to the directory the oracle is started in.

The replies of the web RPCs pass between the client core and the GUI RPC client over socket pairs,
with a writer and a reader on separate threads, so a reply larger than a socket buffer cannot
block them. `--socket-buffer BYTES` shrinks the buffers to make even a short reply need that; the
test `project_config_larger_than_a_socket_buffer` uses it. A reply is also limited by the client
core itself: it builds the `get_project_config_poll` answer with a single 100000 byte buffer
(`MFILE::vprintf` in `lib/mfile.cpp`), so a project configuration larger than that does not reach
the GUI client at all, and the test stays under it. Each program ends itself with a message after
60 seconds (`CB_PARSER_ORACLE_TIMEOUT_SECS` changes this), and the Rust tests stop an oracle that
has not finished after 120 seconds.

## What it does not cover

- One client version. The oracle is the client in this tree (see `version.h`). Older clients
  parse with older code. The scheduler reply writer follows the
  stock server's per-version forms (`core_client_version` in `SchedulerReply`), and they are
  tested in `cb-proto`, but not against old client code. Six real clients from 7.16.6 to 8.2.15
  were run against a deployment; see "Client matrix and request corpus".
- Direction. The oracle reads server replies. The requests a client sends are checked by the
  fixtures in `cb-proto` and, for uploads, by the request `FILE_XFER::init_upload` builds, as above.
- Network behaviour. No HTTP, TLS, redirects, proxies, retries or timeouts; the client's HTTP code
  is linked but not run. Header handling such as `Content-Type` is not checked.
- Client behaviour after parsing: running jobs, downloads, file verification against the downloaded
  bytes (only the signature check against a given MD5 is run), scheduling and backoff.
- The BOINC Manager's use of the project config (the attach wizard), and account managers.

## Moving from a stock server: URLs

A client attached to a stock server holds that server's URLs and keeps using them. The parser
oracle does not cover this; the tests are in `cloud/crates/cb-core/src/stock.rs`,
`cloud/crates/cb-core/src/config.rs` and `cloud/crates/cb-core/tests/stock_layout.rs`, and the
design is under "Stock URL layout" in [architecture.md](architecture.md).

A project moving from a stock server sets, in the `config` setting:

- `stock_short_name` with the short name it was made with, which fills in the `make_project`
  defaults (`/<short>_cgi/cgi`, `/<short>_cgi/file_upload_handler`, `/<short>/download/<dir>/<name>`,
  pages under `/<short>/`), or each of `stock_cgi_prefix`, `stock_upload_prefix`,
  `stock_download_prefix` and `stock_page_prefix` if its `config.xml` put them elsewhere;
- `uldl_dir_fanout` as in the stock `config.xml` (1024 from `make_project`; leave it out if the
  stock server had none);
- `master_url` as before, and the routes of `docs/deploy.md` ("Stock layout routes").

What keeps working: a client's scheduler URL; the download URLs of app versions and input files
it already has, with or without the hash directory; the upload URL of tasks it has not finished,
including `<get_file_size>`, an upload and a resumed upload; the project's pages and web RPCs
under `/<short>/` (including `get_project_config.php`, account pages, the account manager RPCs,
sign-in cookies, which are set for the path the visitor signed in under and cleared for it and the paths above it at logout) and at the root; the
download, upload and scheduler URLs inside imported `app_version.xml_doc`, `workunit.xml_doc` and
`result.xml_doc_in`, unchanged. The file deletion step recognizes the stock download URL as the
project's own. The master page lists the stock scheduler URL by default
(`stock_advertise`), so a client that re-reads it keeps its URL.

What does not: `/<short>_ops/`, `/<short>/stats`, `/<short>/user_profile`, other CGI scripts of a
stock server, and `.md5` files in the download directory. A download URL with a hash directory
other than the one `uldl_dir_fanout` gives is a 404, as it is on the stock server; a project that
changes its fanout in `config.xml` must change it here. The importer for the MySQL database is
not written. "Migration check", at the end of this page, runs stock clients through such a move
with the rows of a small stock project copied by a script, not against a copy of a real project's
rows.

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

## Client matrix and request corpus

Stock clients of six versions were attached to a dev deployment and run through the same steps,
and the requests they sent were kept as fixtures (`cloud/tests/client-corpus/`, with a README
that says how to capture and scrub a new one; the test is
`cloud/crates/cb-core/tests/client_corpus.rs`). Every client ran in a new data directory, on its
own GUI RPC port, as a new computer of one test account, on x86-64 Linux (Debian 13), CPU only.
The same steps were then run for clients of other platforms; see "Other platforms" below.

| Client | Binary | Attached | Work fetched | Jobs validated | Output uploaded | Re-attach in the same data directory |
|--------|--------|----------|--------------|----------------|-----------------|--------------------------------------|
| 7.16.6 | Ubuntu 20.04 package `boinc-client` 7.16.6+dfsg-1, extracted, with libssl1.1 from the same release | yes | yes | 9 of 9 | 9 of 9 | same host ID |
| 7.18.1 | Ubuntu 22.04 package 7.18.1+dfsg-4ubuntu0.1, extracted | yes | yes | 5 of 5 | 5 of 5 | same host ID |
| 7.20.5 | Debian 12 package 7.20.5+dfsg-1.1, extracted | yes | yes | 5 of 5 | 5 of 5 | same host ID |
| 7.24.1 | Ubuntu 24.04 package 7.24.1+dfsg-4build1, extracted | yes | yes | 5 of 5 | 5 of 5 | same host ID |
| 8.0.4 | Debian 13 package 8.0.4+dfsg-1 | yes | yes | 5 of 5 | 5 of 5 | same host ID |
| 8.2.15 | Built from the upstream tag `client_release/8.2/8.2.15` (client and library only) | yes | yes | 5 of 5 | 5 of 5 | same host ID |

The 7.x versions are the ones packaged in Ubuntu 20.04, Ubuntu 22.04, Debian 12 and Ubuntu 24.04;
8.0.4 is the Debian 13 package and 8.2.15 is built from its upstream tag.
The same project's application was used throughout: a small program that upper-cases a file and
takes a second (`cloud/tests/app`), run with an optional `--seconds` delay.

"Validated" is read from the deployment's admin API after the run: for each job the result is
over (server state 5), successful (outcome 1) and valid (validate state 1), it is the workunit's
canonical result, and the workunit waits for assimilation. Each client ran these steps; 7.16.6 also ran four extra jobs while the steps were being worked
out, which is why it has nine jobs and the others five:

- three jobs fetched at attach, with the first request asking for one second of work, then the
  request that fetches the rest;
- the report of those results, with other tasks still held (`<other_results>`);
- requests with nothing to report, from `boinccmd --project URL update`, for work the project did
  not have: the reply says "Project has no tasks available";
- `boinccmd --get_project_config` and `--lookup_account` with the test account's address and a
  wrong password, which the server refuses with `ERR_BAD_PASSWD` (the client prints "bad password"; D2);
- one job with a 40 second delay, with the client quit while it ran and started again, which
  reports it afterwards;
- `boinccmd --project URL detach`, then attach again with the account key, and one more job.

After the re-attach the client sends `<hostid>0</hostid>` and `<rpc_seqno>0</rpc_seqno>` and the
same host CPID (seen in the live runs; the scrubbed fixtures carry a placeholder CPID). The server answers with the same host ID, as the stock server does (it looks the
host up by user and CPID in `handle_request.cpp`), and the client stores it. No second host was
made in any run.

### What the clients logged

With `<sched_op_debug>`, `<unparsed_xml>` and the other flags on, no client logged a parse
warning, an unrecognized element in a reply, a signature complaint, a backoff, a project message
other than "Project has no tasks available", or an error. The replies to the first five requests
of each client (first contact, work, two reports, no work) were equal apart from job and file
names, times, credit, and the `<flops>` of the 8.2.15 computer, which had run its benchmark. The
messages that a stock server would not cause, and what causes them:

- 7.18.1 and 7.20.5 print "This a development version of BOINC and may not function properly" at
  start. It comes from the client, not from the server's reply.
- 7.16.6 prints "Message from task: " with nothing after it when each task finishes. 7.18.1 and
  later print "Found finish file for <task>; exit code 0" and "Task <task>: no shared memory
  segment" for the task that was running when the client was quit. The test application does
  not link the BOINC library and only writes `boinc_finish_called` (an exit status and an empty
  message), which these messages are about.
- "Project has no tasks available" is the stock message (D10 describes when the two servers say
  it).

### Elements the server ignores

`unread_elements_are_the_documented_ones` changes each value in each request and checks whether
the parsed request changes. Every client sends these, except `wsl_available`, which clients up to
8.0.4 send, and they are not read; the stock parser (`SCHEDULER_REQUEST::parse` and the functions
it calls in `sched/sched_types.cpp`) either names them and drops the value, or (the `time_stats`
ones) skips them without logging, so nothing is lost that the stock server keeps:

- `host_info`: `n_usable_coprocs`, `p_calculated`, and `wsl_available` (clients up to 8.0.4);
- `net_stats`: `avg_up`, `avg_time_up`, `avg_down`, `avg_time_down`;
- `result`: `platform`, `version_num`;
- `time_stats`: `now`, `total_start_time`, `total_duration`, `total_active_duration`,
  `total_gpu_active_duration`, `session_active_duration`, `session_gpu_active_duration`
  (the stock parser does not name these and skips them without a log message).

The preferences the server reads from `<working_global_preferences>` are the eight that
`GLOBAL_PREFS::parse` reads: `mod_time`, `disk_max_used_gb`, `disk_max_used_pct`,
`disk_min_free_gb`, `work_buf_min_days`, `ram_max_used_busy_pct`, `ram_max_used_idle_pct` and
`max_ncpus_pct`. After the first contact, clients 7.16 to 7.20 also send their own `<global_preferences>` (with
`<source_scheduler>`), which the server keeps as text. 7.24 and later do not. 8.2.15 is the
first to send `<docker_version>` and `<docker_type>` on this host (Podman is installed on that computer);
the parser reads both.

### Other platforms

The steps above were run again with stock clients of five more kinds, and with the i386 package a
second way, against the same dev deployment and the same test account, each in a new data directory
and on its own GUI RPC port. Only what could be run from one x86-64 Linux machine and one
Apple-silicon Mac was run.

| Client | Platform it names | How it was obtained and run | Jobs validated | Re-attach, same host ID |
|--------|-------------------|-----------------------------|----------------|-------------------------|
| 8.0.4 | `i686-pc-linux-gnu` | Debian 13 package `boinc-client` 8.0.4+dfsg-1 for i386, installed with `apt` in an i386 container (`debian:trixie-slim`) and started under `setarch i686`, which makes `uname -m` say i686 | 5 of 5 | yes |
| 8.0.4 | `aarch64-unknown-linux-gnu` | The same package for arm64, in an arm64 container, run under qemu-user 10.0 | 5 of 5 | yes |
| 8.0.4 | `arm-unknown-linux-gnueabihf` | The same package for armhf, in a linux/arm/v7 container, under qemu-user | 5 of 6 (item 8) | yes |
| 8.2.11 | `arm64-apple-darwin` | The macOS client from the BOINC Mac installer, on an Apple-silicon Mac, as a second client instance in a new data directory: `--allow_multiple_clients`, its own GUI RPC port, `-insecure`, one CPU (`max_ncpus_pct` in `global_prefs_override.xml`) | 5 of 5 | yes |
| 8.2.15 | `windows_x86_64` | The Windows client binary under Wine, not a Windows host: `boinc.exe` of the official 8.2.15 Windows x86-64 installer, taken out of it with 7z without running it, and run by Wine 10.0 (Debian 13) in an amd64 container | 5 of 6 (item 8) | yes |
| 8.0.4 | `x86_64-pc-linux-gnu`, alternate `i686-pc-linux-gnu` | The i386 package run as it is, on a 64-bit kernel. The project had an app version for `i686-pc-linux-gnu` only | 1 of 1 | not run |

In the container the clients ran in, `binfmt_misc` was not mounted. `mount -t binfmt_misc`
worked inside it, and the `qemu-aarch64` and `qemu-arm` handlers were registered there with the
fix-binary flag from the binaries in the `qemu-user` package, extracted with `dpkg-deb -x`
without installing it. The clients ran as root in `podman` containers with the host's network, so
they report the build machine's memory, disk and kernel release (the kernel release is scrubbed in
the fixtures), and the ARM clients report qemu's processor and features.

#### Application

A new app, `upperx`, with one version for each platform, so that the hosts of
the existing apps are not offered anything new. The Linux versions are `cloud/tests/app` built
static for `i686-unknown-linux-musl`, `aarch64-unknown-linux-musl` and
`armv7-unknown-linux-musleabihf`. The macOS and Windows versions are `cloud/tests/app/c/upper.c`,
which does the same, built with the Xcode command line tools (`arm64-apple-darwin`) and with
mingw-w64 (`x86_64-pc-windows-gnu`). Each was signed and registered with `cboinc version-add`. The
Windows file is stored as `upperx_1.00_windows_x86_64`, with no `.exe`, which is how `version-add`
names a main program; Wine ran it. Whether a Windows host runs a main program with no extension
was not checked.

#### Steps and result

The steps listed above for the 64-bit Linux clients, on every client but
the last: attach, fetch work,
download the application and the input file (the client accepted the signature and the MD5 sum),
run, upload, report, validation and credit, a restart in the middle of a task, a detach and attach
again, and `boinccmd`'s `--get_project_config` and (not on macOS) `--lookup_account` with a wrong
password. Every job that was sent and not lost ended valid and canonical. No client logged a parse
warning, an unrecognized element, a signature complaint, a backoff or an error that the server
caused. The replies to the first request of the i686, arm64 and armhf clients were equal but for the
platform in the file names and the `<platform>` elements, and the host ID.

#### What the requests carry

Compared with the 64-bit Linux requests (the set of element paths,
over the files of the corpus), these differ and nothing else does:

- the macOS client sends `<coprocs>` with a `<coproc_apple_gpu>` (the M1's GPU, with its OpenCL
  properties), `<product_name>` in `<host_info>`, `<dont_use_docker/>` at the top (the data
  directory's `cc_config.xml` set `<dont_use_docker>`), and `<final_peak_working_set_size>` in each
  reported result. It has `<docker_version>` and `<docker_type>` 2 (Podman) in `<host_info>`, and no
  `<wsl_available>` (8.2 clients do not send it). `p_vendor` is `Apple`, `os_name` is `Darwin` and
  `os_version` is the kernel's, `27.0.0`;
- the Windows client sends `<alt_platform>` with `windows_intelx86`, an empty `<wsl>` in
  `<host_info>`, `os_name` "Microsoft Windows 10" and an `os_version` of the form "Ultimate N x64
  Edition, (10.00.19043.00)" (Wine's answer). It has no `<docker_version>`, `<docker_type>` or
  `<final_peak_disk_usage>`, and a task's `<stderr_out>` has no `<stderr_txt>` when the
  application wrote nothing;
- the arm64 and armhf clients send the same elements as 8.0.4 on x86-64, with `p_vendor` `ARM` and
  a `p_model` such as "ARMv8 Processor rev 0 (v8l) [Impl 0x00 Arch 8 Variant 0x0 Part 0x051 Rev
  0]", which are qemu's, `p_features` from the `/proc/cpuinfo` that qemu writes (for armhf `swp
  half thumb fast_mult vfp edsp neon ...`), and `<m_cache>` -1;
- the i686 client's reported results have no `<final_peak_disk_usage>`. Run on a 64-bit kernel
  with no `setarch`, it names `x86_64-pc-linux-gnu` first and sends `<alt_platform>` with
  `i686-pc-linux-gnu`.

`unread_elements_are_the_documented_ones` finds no element that the 64-bit Linux clients do not
already send and the parser ignores: every new element is read by `cb-proto` (the `<wsl>`
container, with nothing in it, is listed with the other containers in the test). Each is also
read by the stock parser: `HOST::parse` and `SCHEDULER_REQUEST::parse` (`dont_use_docker`, `dont_use_wsl`) in
`sched/sched_types.cpp`, and `COPROCS::parse` and `COPROC_APPLE::parse` in `lib/coproc.cpp`. No element is read by the stock parser and ignored here.

#### Differences found


1. Not a difference. This is client behaviour that decides which platform is named. A 32-bit client on a
   64-bit kernel names `x86_64-pc-linux-gnu` as its platform and lists `i686-pc-linux-gnu` after
   it: `detect_platforms` in `client/cs_platforms.cpp` runs `uname -m` and finds a 64-bit kernel.
   It reports only `i686-pc-linux-gnu` on a 32-bit kernel, which the i686 run imitates with
   `setarch i686`. A project with versions for both platforms is served by the platform order of
   the request (items 2 to 4).
2. Fixed. Of two versions of an app with the same projected speed, one for the client's primary
   platform and one for an alternate, this server took the one added first. `get_app_version` in
   `sched/sched_version.cpp` scans the platforms in the request's order and replaces its choice
   only by a faster version, so the primary platform's wins. `Version::rank` in
   `crates/cb-core/src/scheduler.rs` orders equal versions by platform; the tests are
   `of_two_equal_versions_the_primary_platform_wins` and
   `a_client_gets_a_version_for_an_alternate_platform_when_its_primary_has_none`. The second shows
   what the live run did: the i386 package on a 64-bit kernel was sent the i686 version, as the
   project had no other, ran it, and was credited.
3. Fixed. `prefer_primary_platform` in the project's config was read and never used. The scan in
   `get_app_version` stops at the first platform that has a feasible version when it is set.
   Test: `prefer_primary_platform_stops_at_the_first_platform_with_a_version`.
4. Fixed. A job with a memory bound above what a 32-bit app can address (3 GiB for a Linux
   client, 2 GiB for Windows, 4 GiB for macOS and Solaris, 2 GiB otherwise) was sent with a version for any platform of the
   host. `get_app_version` skips platforms with no `64` in their name for such a job
   (`job_needs_64b`, `max_32b_address_space`, `is_64b_platform`); this server did that only on the
   anonymous platform. Test: `a_job_too_big_for_a_32_bit_app_goes_only_with_a_64_bit_version`.
5. Fixed. A request with a host CPID that is the account's, and a host ID of 0, was given the host
   with that CPID even when it was another computer. `find_host_by_cpid` is followed in
   `handle_request` by `obviously_different` (processor count, vendor and model, OS name and
   version), and a host that differs is not used. The CPID of a client is the MD5 of its MAC
   address and its data directory path, so the first arm64 attempt, which ran in a container with
   the host's network and the same data directory path as the i686 client, had the same CPID and
   was given the i686 client's computer. Test: `a_host_with_the_clients_cpid_that_is_another_computer_is_not_reused`.
6. Fixed. A request with a new CPID was matched to a host of the same name, address, OS and CPU
   model even when the client said `<allow_multiple_clients>1</allow_multiple_clients>`; the
   second arm64 attempt, which had a new CPID and the name and model of the host the first one
   had left behind, got that host again, and so did a second start of the macOS client in a new
   data directory. `find_host_by_other` in `sched/handle_request.cpp` is not run for such a
   client ("each client instance needs its own host record"), is run only when the name, address,
   OS name and CPU model are all present, and also compares the memory. Test:
   `clients_that_share_a_computer_get_a_host_each_only_when_they_say_so`. Items 5 and 6 are fixed in
   the code; the dev deployment was not updated, so there the i686 and arm64 clients share one host row,
   and the two starts of the macOS client share another.
7. Not a difference. The macOS client looks for `detect_rosetta_cpu` in its data directory and
   adds `x86_64-apple-darwin` to its platforms only when that program runs and exits with status 0
   (`launch_child_process_to_detect_emulated_cpu` in `client/cs_platforms.cpp`). A new data
   directory has none, so the client logged "Emulated CPU detection failed: process exited with
   status 2: No such file or directory" and named `arm64-apple-darwin` alone; no macOS alternate
   platform was served. The alternate-platform replay in the test uses the 32-bit Linux client.
8. Not a difference. The armhf and Windows runs each lost one job: the server sent the job
   created just before the detach, in the same second as the client's "Resetting project"
   message, and the client dropped the reply. The job stayed in progress on the old attach, as the
   stock server would leave it (it marks a host's tasks over after an attach only when the client
   did not say `<allow_multiple_clients>` and lists no other results). Another job after the re-attach completed the steps.
9. Not a difference. The macOS client runs `podman machine init` and `podman machine start` while
   it starts, even with `<dont_use_docker>` set, and says "Docker: version 6.1.4 (podman)" and
   "Config: don't use Docker"; the request carries both `<dont_use_docker/>` and the Podman
   version. A plan class with `<docker/>` is not offered to it ("Client config does not allow
   using Docker", `check_docker`), as in `PLAN_CLASS_SPEC::check`.
10. D17, not fixed. The computer page (`show_host_detail.php`) describes the host from
    the summary string in `host.serialnum`, where stock's `html/inc/host.inc` reads the JSON in
    `host.misc` (`gpu_desc`, `docker_desc`, `vbox_desc`, `boinc_version`). The macOS host shows
    "[1] apple_gpu (12124MB)" for the GPU and "6.1.4" for Docker, where stock shows the
    count, model, memory, driver (for an Apple GPU, the Metal support value) and OpenCL level of the GPU, and "Podman version 6.1.4" with a second line "Config: don't use Docker", as the client sent `<dont_use_docker/>`. The CPU
    type, core count, operating system, memory, cache and speeds are shown as the clients sent
    them (an arm64 host shows "ARM", then qemu's model; a Windows host "Microsoft Windows 10",
    then the version string).

### What this does not cover

- Android clients, 32-bit Windows and macOS clients, 32-bit ARM and arm64 clients on real hardware,
  and a real Windows host: the Windows client ran under Wine, the ARM clients under qemu-user,
  and the macOS client on one Apple-silicon Mac without the `detect_rosetta_cpu` helper. GPU hosts
  other than the Apple GPU of the macOS client, which was sent no GPU work, and plan classes.
- Windows hosts with WSL distributions, and macOS or Linux hosts with Docker: the `<wsl>` and
  `<docker_*>` elements of such requests are read by the parser, from its own tests, and were not
  captured.
- Client behaviour over time: the runs lasted a few minutes each, so backoff after repeated
  failures, daily transfer limits, and client updates were not exercised. The dev project has
  `min_sendwork_interval` 0, so no reply carried a `<request_delay>`.
- A quorum above one, and an account or computer other than the one test account's.
- The replies were read from the clients' logs and the `sched_reply_*.xml` files, not
  compared with a stock server's. The differential test below does that for 8.0.4's requests.
- The test world has no code-signing key: what the server sends when a client reports another
  key is not replayed.

## Differential test against the stock server

The oracle shows that the client can read this server's replies. It does not show that they are the
replies the stock server would give. The differential test gives both servers the same requests
from the same starting rows, and compares what they answer.

### How it works

1. A stock server, built from the upstream tree of this repository
   (`cloud/tests/differential/Containerfile`): Debian 12, `./configure --enable-server
   --disable-client --disable-manager`, MariaDB, Apache with PHP 8.2, the `cgi` scheduler, the
   `file_upload_handler` and the feeder. `tools/make_project` makes the project. Then
   `stock/load_fixture.py` installs the fixture: the key pairs, `xadd` for platforms and apps,
   `update_versions` for the app version (which signs the file with the code-signing key),
   `stage_file_native` and `create_work` for the jobs, and one pass of `transitioner`, which makes
   their results. Users are inserted with SQL, since no stock tool takes a given account key; the
   stored password hash is `password_hash()` of the client's hash, as the stock site stores it.
2. The same project on this server. `World::new` in
   `cloud/crates/cb-core/tests/differential/world.rs` reads the same files
   (`cloud/tests/differential/project/`) and builds the project on SQLite with `put_platform`,
   `put_app`, `add_app_version`, `register_job_file` and `create_work`, and the users with SQL. The
   code-signing and upload key pairs are the same files, made by the stock `crypt_prog -genkey`, so
   signatures of equal data are equal. The URLs are the stock layout of a project moved from a stock server
   (`stock_short_name` and `uldl_dir_fanout` 1024, see "Moving from a stock server: URLs"), so
   download and upload URLs and the scheduler URL on the master page equal the stock ones. The
   other settings are those `make_project` writes to
   `config.xml` (the ones this server reads), plus `config-options.xml`, which turns on upload
   certificates (`make_project` switches them off with `dont_generate_upload_certificates` and
   `ignore_upload_certificates`) and `resend_lost_results`.
3. The request corpus (`cloud/tests/differential/corpus/<scenario>/`). `capture.sh` runs a stock
   BOINC 8.0.4 client against the stock server through a recording proxy
   (`record-proxy.py`), drives it with `boinccmd`, and keeps every exchange.
   `build-corpus.py` turns that into `steps.json` and one request file per step. The scheduler
   requests are the client's, byte for byte, except for what identifies the machine, which gets
   fixed test values (`sanitize_sched` in `build-corpus.py`: host name, IP address, host CPID,
   processor vendor, model and features, OS version, memory and disk sizes, and paths in
   stderr texts). The account key in them is the test user's.
   Some steps are derived from a captured request by changing one thing, or are written by hand
   (the account manager and status requests, which no client sends); each such step says so in its
   `derived` field in `steps.json`. No reply is written by hand.
4. The stock replies. `golden.sh` starts the stock server, puts the project back into its starting
   state for each scenario (`stock.sh --action reset` restores a database dump taken after the
   fixture was loaded), and `replay.py` sends the scenario's requests in order and writes each
   reply to `<step>.reply` and the time of the request to `golden.json`. The stock server's own
   address in a reply becomes `http://project.test`, which is what this server's project is
   configured with. Scheduler requests wait seven seconds for each other, because the stock server
   sends no work to a host that asked less than `min_sendwork_interval` (6 seconds, as
   `make_project` sets it) ago; steps with `"gap": 0` do not wait, to test that.
5. The test (`cargo test -p cb-core --test differential`, part of `cargo test`, and so of CI). For
   each scenario it builds the world, answers every step with the functions the Workers call
   (`handle_request`, `check_header` and `receive` with an in-memory bucket, `file_size`, and the
   web functions), at the time the stock request was sent, and compares the answer with the stock
   reply. CI does not run the stock server; it checks against the stored replies.

### Job names

The stock scheduler picks among the jobs in its feeder's shared memory from a random place, so
which job a request gets differs from run to run, and this server picks in an order of its own.
The corpus therefore names jobs in the order a host was first sent them, `diff_0001`, `diff_0002`,
and so on (the jobs of one reply in the order of their real names). `replay.py` and the test learn
the real names from the replies and write them into the requests that follow (`names.py` and
`names.rs`); the replies are compared with the real names replaced by those. Output file names
(`diff_0001_0_r1013884258_0`) carry a random number. Each complete name becomes a placeholder:
the job's canonical name and `r1` for the first different name that output file has had, `r2`
for the next. The same name gets the same placeholder everywhere, in a reply and in a resend,
and different names get different ones, so a `<file_ref>` that does not match its `<file_info>`
stays visible. Before any renaming the test also checks that every `file_ref/file_name` in a
scheduler reply, from either server, has a `file_info/name` in the same reply, which is what the
client needs to link a result to its files; two tests in `main.rs` show that a reply with a
mismatched reference fails. All jobs use the same input file, so that nothing else differs between
jobs.

### What is compared

Both replies are read into a tree by a tolerant reader (`xmltree.rs`): the stock server writes
text next to elements, `<message priority="low">` attributes, and, in the upload handler, an
unescaped `<` in a message, which a strict parser refuses. The tree becomes a list of leaves, each
with a path such as `scheduler_reply/result[diff_0001_0]/report_deadline`. Records with a name
(`file_info`, `workunit`, `result`, `app`, `app_version`, `host`, `platform`) are keyed by it, so
that replies listing the same records in a different order line up; other repeated elements are
numbered. A difference is a leaf that is only in one reply, or has another value. Numbers are
compared as numbers, since the stock server writes `%f`. Whitespace around text is not compared.

What is left after these rules is a finding and fails the test (`compare.rs`):

- Time fields (`TIMES`) may differ by two seconds, because the stock server reads its clock while
  this server is given the time the request was sent: `host_create_time`, `report_deadline`, and
  the times of a computer in `show_user.php`.
- Random fields (`RANDOM`) must have the same form, not the same value: the account key
  `create_account.php` makes, the weak account key in `am_get_info.php` (it includes the stored
  password hash, which differs between the servers), and the upload certificate of an output file
  (it signs a name with a random part). The test checks every upload certificate in both replies
  against the upload public key, so that two certificates for different names are not taken as
  equal merely because both are long.
- Transforms (`TRANSFORMS`): the number of seconds a host must wait (see D9), the translation
  markers in messages (see D11), and the URL of the application file (see D12).
- Intended differences (`INTENDED`), below. An entry names the step it applies to and the
  element, so it cannot cover a difference elsewhere.

A rule or entry that no step needs makes the test fail, so a list cannot outlive the difference
it describes. The master page (`/`) is a web page; the test compares only the scheduler URLs in
it, which the client reads.

### Coverage

| Scenario | Origin | Steps |
|----------|--------|-------|
| `attach` | client 8.0.4: `boinccmd --get_project_config`, `--lookup_account`, `--create_account` | `get_project_config.php`; `lookup_account.php` with the right password, a wrong one, and an unknown address; `create_account.php` for a new account, for an existing one with its password and with another, with a bad address, and with a short password |
| `work` | client | the master page; the first request (work for one second, hostid 0); a request for more work; four uploads; the request that reports the tasks |
| `report` | client | master page, first request, one upload, a report-only request (`work_req_seconds` 0) |
| `anon` | client with an `app_info.xml`, attached by its account file | as `work`, on the anonymous platform: the request lists the client's `<app_versions>` |
| `lost` | client, plus one derived request | the first request, a request while the first task runs (its `other_results` lists it), then the same with `other_results` empty, so that the server resends the tasks |
| `quick` | derived from the first request of `work` | a request at once after another (the lost task is resent), one that lists its task, so that the server comes to `min_sendwork_interval`, and one after the wait that does not list it, with jobs still unsent |
| `uploads` | uploads of `work`, changed one field at a time | a size query before and after an upload; an offset for a file the server does not hold; the upload; the same upload again; the same name with other contents; a bad certificate; a certificate for another name; more bytes than the certificate allows; an upload cut after ten bytes, a size query, and the rest from offset 10; a request without a name; not a request |
| `web` | written by hand | the first request (so that a computer exists), `get_project_config.php`, `server_status.php?xml=1`, `show_user.php?format=xml` by ID, by key, and for an unknown user, `am_get_info.php` with a key and with a wrong key, `lookup_account.php` without a hash, `am_set_info.php` |

63 steps: 18 scheduler requests, 23 upload requests (14 of them in `uploads`) and 22 web
requests. Scheduler replies are compared with work sent (`<app>`, `<file_info>`,
`<app_version>`, `<workunit>`, `<result>`, upload certificates, file signatures, the code-signing
key), result acknowledgements, resent lost tasks, messages, `<request_delay>`, the preferences
echo, the user, host and credit fields, and the `<gui_urls>` and `<rss_feeds>` lists.

### Intended differences

| ID | Where | Stock server | This server | Why, and what a client sees |
|----|-------|--------------|-------------|-----------------------------|
| D1 | `<gui_urls>` and `<rss_feeds>` in every scheduler reply | The project's buttons in the Manager (message boards, account, tasks) from `gui_urls.xml`, and the URL of the notices feed | Neither | There are no forums or notices. The Manager shows no project buttons and no notices for the project. |
| D2 | `lookup_account.php` with a wrong password | A PHP fatal error, `Call to undefined function hash_equal()`, from `check_passwd_hash` in `html/inc/user_util.inc`. The reply is an HTML error text, not XML. | `<error>` with `ERR_BAD_PASSWD` (-206, "Invalid password"), which is what `html/user/lookup_account.php` means to send after the check | A typo in the upstream tree. Every wrong password and every use of the account-key form of the hash reaches it. The Manager's wizard shows an error either way, a different one. |
| D3 | `am_set_info.php` | `<error>` with -1, "not supported"; the script is switched off | Changes the account as the disabled script's code does | See [architecture.md](architecture.md), "Web RPCs and site". |
| D4 | `server_status.php?xml=1` | Daemon lists (download server, upload server, scheduler, feeder, transitioner and so on, with hosts), `tasks_by_app`, counts of workunits and results waiting for validation and deletion, users and computers with credit and registered in the last day, `current_floating_point_speed` | `daemon_status` with the scheduler and the pipeline, `update_time`, `software_version`, results ready and in progress, workunits waiting for assimilation, `transitioner_backlog_hours` (0.02 when anything is due, else 0), users and computers with recent credit | A Cloudflare-D1 deployment keeps no daemon table and does not count every row on each request. A consumer that reads the other elements finds nothing. |
| D5 | Upload of a file smaller than one piece (8 MiB), cut short | Keeps the bytes received; `<file_size>` answers 10 after ten bytes; the request from offset 10 completes the file. The error for the cut request is "EOF on socket read : asked for 22, got 10". | Keeps nothing; `<file_size>` answers 0; a request from offset 10 gets "length of file ... 0 bytes < offset 10 bytes". The error for the cut request says the upload ended early. | Pieces are stored only for files of at least `PIECE_BYTES` (architecture.md, "Upload"). The client sends such a file again from the start. |
| D6 | Upload under a name that is stored, with other contents | `<status>` 0 | `<status>` -1, "file ... exists with other contents" | A stored file is never replaced, because a result may have been validated against it. A retry of the same upload succeeds in both. |
| D7 | Weak account key (`<weak_auth>` in `am_get_info.php`) | `userid_` and an MD5 that includes the stored password hash | The same form, with the hash this server stores (HMAC with the pepper) | Weak keys made by the stock server stop working after a move. The account key and the password keep working. |
| D8 | Upload certificates of output files, and the random part of their names | Signed with the upload key | Signed with the same key, over another name | Both verify against the upload public key. |
| D9 | "Not sending work. Last request too recent. Please wait N seconds." | N counts from the fraction of a second the last request came in | N counts from the stored time of the last request, in whole seconds, so it can be one more | Stock subtracts that stored time from the current time with its fraction (`handle_request.cpp`). |
| D10 | "Project has no tasks available" and "No tasks sent" | Decided from the feeder's cache in shared memory: a window of `shmem_work_items` unsent results (100 unless set), refilled every few seconds, of any app and platform (`SCHED_SHMEM::no_work` in `sched/sched_shmem.cpp`). The window can be empty for a moment while unsent work exists. Empty: only "Project has no tasks available". Not empty: "No tasks sent" and the reasons. | One probe for an unsent result of any app or platform (`UNSENT_EXISTS` in `cloud/crates/cb-core/src/scheduler.rs`): `SELECT 1 FROM result WHERE server_state = 2 LIMIT 1`, which reads one entry of the partial index `res_unsent`. It runs only when the reply would carry one of the two messages: a work request that was sent nothing, or one that resent lost tasks. It does not run for a host that has candidates, or for a request that was sent work. | There is no cache to lag behind the database, so this server never says "no tasks" while unsent work exists, which the stock server can. A project whose feeder is slow sees "No tasks sent" here where stock says "Project has no tasks available". Log text only. |
| D11 | Messages the stock scheduler marks for translation: 15 of them in `sched/handle_request.cpp`, `sched/sched_send.cpp` and `sched/sched_version.cpp`, among them "Invalid or missing account key", "Your BOINC client software is too old" and "Your app_info.xml file doesn't have a usable version of" | Written as `_("text")`; for the app_info message only the first part is marked: `_("Your app_info.xml file doesn't have a usable version of") App A.` (`#define _(x)` in `lib/sched_msgs.h`) | Plain text, without the marker | The client removes the marker before it shows the message (`strip_translation` in `lib/str_util.cpp`, called from `client/client_msgs.cpp`), so an English client shows the same text. The marker exists so that a message can be found and translated; whether a localized Manager translates these messages by it was not checked, and without the marker it could not. Recorded for the app_info message (scenarios `anon`); the other 14 follow from the macro. Not fixed. |
| D12 | `<url>` of an application file | `download/name`: `update_versions` stages application files flat and writes flat URLs | `download/<hex>/name`: an app version registered through this server gets the hash directory, as input files do | A project that moves keeps the XML of its imported app versions, which has the flat URLs, and this server serves them with or without the directory (see "Moving from a stock server: URLs"). Only app versions added later get a directory. The client follows the URL it is given. Input file URLs are equal. |
| D13 | A plan class with `<gpu_type>apple</gpu_type>` | The host check finds the Apple GPU, but the version's processor type is set only for names that start with `apple_gpu` (`PLAN_CLASS_SPEC::check` in `sched/plan_class_spec.cpp`), so the version runs as a CPU version. `apple_gpu` is found as a generic coprocessor, with no Metal check and no RAM figure | Both spellings select the Apple GPU, check `min_metal_support` and `min_gpu_ram_mb` against the client's `available_ram`, and make an Apple GPU version | Apple GPU hosts get the GPU version counted as a GPU job. A project whose class says `apple` and relied on its running as a CPU version sees it count against the GPU limits. Not covered by the scenarios: no corpus host has an Apple GPU. |
| D14 | `host.misc` | `host_info_json` in `sched/sched_host.cpp`: strings as they come, cut only by the 64 KiB column | The same layout and key order, strings JSON-escaped and cut at 200 bytes, the whole value held to 8 KiB by dropping the GPU list | A quote in a GPU name gives stock an invalid document; here it parses. Tools that read the JSON see the same keys. |
| D15 | Which unsent results a host is offered | The feeder's cache of `shmem_work_items` results, filled without regard to a host; a cache full of work one host cannot take hides the rest from it | The candidate query carries the host's conditions (version number, pinned and committed versions, plan-class windows, replica rule), and a full window is followed by the next, up to four | A host behind a backlog of work it cannot run still gets what it can. The message logic is D10's and is unchanged. |
| D16 | `apps.php` | `html/user/apps.php` lists applications with their creation date and recent average GFLOPS, each linking to `apps.php?app_id=` (versions of one application, with GFLOPS) and `apps.php?avid=` (one version's statistics). `apps.php?xml=1` writes the current versions of every application, with names and plan classes as they are stored, under `encoding="ISO-8859-1"`. A login is required when the project sets `REQUIRE_LOGIN`. | One page lists each application with its current versions (platform, version, plan class, creation date). `app_id`, `avid` and the GFLOPS column are not built. `?xml=1` has the same elements and indentation; text is XML-escaped, and the declaration says UTF-8. No login is asked for. Empty plan classes show as `---` on the page. | Stock writes names into the XML unescaped, so a name with `<` or `&` gives a document that does not parse, and a non-ASCII name is written as UTF-8 bytes under a Latin-1 declaration. A tool that reads the XML sees the same values for names without those characters. The page reads the catalog the Worker loads with the settings, so a new version shows within 30 seconds. See "Applications page and unknown paths" in [architecture.md](architecture.md). |
| D17 | The computer page, `show_host_detail.php`: the rows for coprocessors, virtualization, Docker and BOINC version | `html/inc/host.inc` reads the JSON in `host.misc` (`gpu_desc`, `vbox_desc`, `docker_desc`, `boinc_version`): each GPU as count, model, memory, driver (for an Apple GPU, the Metal support value) and OpenCL level, VirtualBox with its acceleration flags, "Docker" or "Podman" with the version, the WSL distribution and "Config: don't use Docker" or WSL when the client's config says so, the client version and brand | The rows are read from the summary string in `host.serialnum`: each GPU as `[count] type (memory)`, the VirtualBox and Docker versions alone, the client version alone | The scheduler writes both columns on every request (`host.misc` as D14 says). An Apple-silicon Mac with an M1 shows "[1] apple_gpu (12124MB)" and Docker "6.1.4", where stock shows the model, memory, driver and OpenCL level, and "Podman version 6.1.4" with "Config: don't use Docker" below it. The other rows are the same. Not fixed. |

### Differences found, and what was done

Each of these was a difference between the replies that the test found. The first group is fixed
here, in `cloud/crates/cb-core/src/scheduler.rs` and `cloud/crates/cb-proto/src/web_rpc.rs`; the
test covers each fix, because the stored stock replies carry the right values.

1. `<request_delay>` was 1.01 times `min_sendwork_interval` (6.06 for 6). Stock writes the larger
   of that and the interval plus one (`SCHEDULER_REPLY::write` in `sched/sched_types.cpp`): 7. A
   client waited a second less than the project asked.
2. The preferences were sent in every reply to a client that had no separate copy from the web
   site, which is the usual client. Stock sends the account's preferences only when they are newer
   than both of the client's copies, its working copy included (`handle_global_prefs` in
   `sched/handle_request.cpp`). The client's copy replaces the account's only when it comes from
   this account (`global_prefs_source_email_hash`); that check was missing.
3. A new computer's `expavg_time` was 0; stock sets it to the creation time
   (`handle_request` in `sched/handle_request.cpp`). It shows in the computers list of
   `show_user.php?format=xml&auth=`.
4. `get_project_config.php` wrote `<server_version>8.3.0</server_version>`, the BOINC release. Stock
   writes `$server_version` of `html/inc/server_version.inc`, the version of the web interface,
   1.1.0.
5. After resending lost tasks, this server also sent new tasks and said "No tasks sent". Stock
   sends nothing more after a resend (`ok_to_send_work` in `process_request`).
6. The message for a request that came too soon was worded differently and was also written when
   lost tasks were resent; stock writes it only when it would otherwise send work.
7. With no jobs to send, this server wrote "No tasks sent" and then "Project has no tasks
   available". Stock writes only the second when its cache is empty, and "No tasks sent" and the
   reasons only when the cache holds jobs, of any app, that do not suit the host. The first
   fix looked at the host's own candidates, which are only the apps it has a version of; it
   now asks the project (D10). The fixture has a second app, with a version for Windows only, and
   one job of it, so that the Linux host of the corpus always sees unsent work it cannot run:
   the ordinary request and the request that resends lost tasks both cover it, in `work`,
   `anon`, `lost` and `quick`, with replies recorded from the stock server.

Not fixed, and recorded above: D1 (no project web pages), D4, D5 (pieces), D6, D10, D11.

### What the stock tree gets wrong

The test found these in the upstream tree. They are recorded because they decide what "the stock
server's reply" is, and nothing here changes them.

- `check_passwd_hash` calls `hash_equal` instead of `hash_equals` (D2).
- `tools/stage_file` calls `parse_config`, which does not exist, so staging input files needs
  `stage_file_native`.
- `tools/make_project` writes `dont_generate_upload_certificates` and `ignore_upload_certificates`
  as 1: a project made with it checks no upload certificates until its operator changes that.
- The upload handler writes `0 bytes < offset 10 bytes` without escaping the `<`
  (`return_error` in `sched/file_upload_handler.cpp`), the same defect the oracle found in
  another message (finding 1 above). This server escapes it; the test reads both as the same text.

### Running it

On a Linux host with podman or docker, `python3`, `rsync`, and for a capture the stock client
(`boinc` and `boinccmd`; Debian: `boinc-client`):

```
cloud/tests/differential/stock.sh --action build        # the image, about 10 minutes
cloud/tests/differential/capture.sh --out /tmp/capture  # a client against the stock server
cloud/tests/differential/build-corpus.py --capture /tmp/capture \
    --out cloud/tests/differential/corpus               # requests, no replies
cloud/tests/differential/golden.sh                      # the stock replies
cd cloud && cargo test -p cb-core --test differential
```

`capture.sh` and `build-corpus.py` are needed only to record new requests; `golden.sh` alone makes
new stock replies for the stored requests, after a change to the stock fixture or a merge from
upstream. Everything binds to 127.0.0.1: Apache on port 18480 (`--port`), MariaDB on a socket
inside the container. `stock.sh --action stop` removes the container. With
`CB_DIFFERENTIAL_SUMMARY=1` and `--nocapture`, a failing run prints the differences grouped by
element; `CB_DIFFERENTIAL_DUMP=<dir>` writes this server's replies there.

The key pairs under `cloud/tests/differential/project/keys/` are throwaway keys made for this test
and used nowhere else.

### What it does not cover

- One client version. The requests are from the stock 8.0.4 client. A client of this tree
  (8.3.0) was not run. The requests of other versions are in the client corpus (above), which is
  replayed against this server but not compared with the stock server's replies.
- Only the paths a single computer takes: attach, work, upload and report, no work, report only,
  anonymous platform, lost tasks, requests that come too soon. Not covered: several computers or
  accounts, GPU and plan class applications, quorums of two or more (the replies of a workunit
  with several results), assigned jobs, trickles, homogeneous redundancy, and the daemons'
  work (validation, assimilation, credit, file deletion), whose output reaches a client only
  through later replies.
- The preferences of the account in `project_prefs` and the `<project_specific>` part.
- HTTP: headers, `Content-Type`, redirects and downloads (status codes are compared). The files Worker serves downloads
  from R2 and the stock server from Apache.
- The Workers' own glue (`workers/*/src/lib.rs`): the test calls the functions it calls, with
  the arguments it builds from the request. The parser oracle and the dev deployment cover the
  rest.
- The site's HTML pages. Only the master page's scheduler URL is compared.
- Time-dependent behaviour beyond what the steps' timing reaches: daily quotas, deadlines passing,
  back-off.

## Migration check

The goal is that a project can move from a stock server to this one without its volunteers
installing a new client. The check asks what stock clients that are attached to a stock server do
when the same master URL is served from here, with the same code-signing and upload keys and the
same user and host rows. A volunteer should not have to detach, take a new account key or do
anything by hand. The scripts, how to run them and what they need are in
`cloud/tests/migration-check/README.md`; this section is what they found.

### What was run

- The stock server is the one of the differential test (`cloud/tests/differential/stock.sh`, built
  from the upstream sources of this tree: Apache, PHP 8.2, MariaDB, the feeder, the scheduler and
  upload handler CGIs), started as project `proj` with the master URL `http://project.test/proj/`.
  The host name exists only in each client's own hosts file; the clients speak plain HTTP.
  The stock transitioner, file deleter, sample trivial validator and sample assimilator
  run too, so jobs are validated and assimilated.
- The project has new key pairs made for the run, three accounts with random account keys, one
  application, `upper` (the test application of `cloud/tests/app`), with version 1.00 signed by
  the stock `update_versions`, and the `config.xml` of `make_project` plus the options of the
  differential test (upload certificates on, lost results resent), `max_wus_in_progress` and
  `max_wus_to_send` 4, and `min_sendwork_interval` 31 (a reply's `<request_delay>` is 32 seconds,
  which is how long the script has to stop a client's network before its report goes out).
  The first account's password hash is stored as the stock site stores it (PHP `password_hash()` of
  the client's MD5), the second's in the old form, the bare MD5, and the third account's address has
  capitals.
- Stock clients, each in its own data directory with the log flags `task`, `file_xfer`,
  `file_xfer_debug`, `sched_ops`, `sched_op_debug`, `http_debug` and `unparsed_xml` and one CPU:
  7.16.6, 7.18.1, 7.20.5, 7.24.1 (Ubuntu and Debian packages, extracted), 8.0.4 (Debian package) and
  8.2.15 (built from its upstream tag), all x86-64 Linux. Each client was attached with the account
  key and brought to the same state, one at a time: one task fetched, run, uploaded, reported and
  validated by the stock daemons; then three more, with the client not computing; the long two
  suspended, the short one run and uploaded, and the client's network switched off. Then the long
  tasks were let run, which left each client with a task that had finished and uploaded but was not
  reported (`b`), a running one (`c` or `d`) and a not started one.
- The switch: the rows were copied out of the stock database, the stock server was stopped,
  and cloud-boinc was started (the five Workers under `wrangler dev` with a router, local D1, R2
  and queue; `cloud/dev/local.sh`) at the same address with the stock `<config>` element (database
  credentials and host left out, `stock_short_name` `proj` added), the stock code-signing and upload
  public keys and private upload key, the files of the stock download directory, and the rows. From
  stopping the stock server to switching the clients' network on took about 40 seconds, 15 of them
  for the Workers to start, with the Workers already built.
  After the clients had run their long tasks, new work was added: an application version 1.01
  signed with the stock code-signing key (`cboinc version-add`) and 20 jobs of two results each.
  Every client was then detached and attached again with its account key, and the web RPCs
  were asked of both servers.
- Ten clients in the first run. What the switch carried depended on the client (`MODE` in the
  clients file): for six, one of each version, the host row and the job rows of their tasks (`copied`);
  for three (7.16.6, 8.0.4 and 8.2.15) the host row and no job rows (`notcopied`, which is what a
  copy of the users and hosts alone gives); for one (8.0.4), the job rows but not the host row
  (`nohost`). The files of the stock upload directory were carried for all.
- A second run with four clients, 7.16.6 and 8.0.4 each `copied` and `notcopied`, with
  `send_result_abort` set to 0 in the config and the files of the stock upload directory not
  carried. Its results are in D18, D19 and D25.

### What the row copy carried

`cloud/tests/migration-check/copy_rows.py` is not an importer (that is issue #17); it is the least
that let the check run. It copies the columns that the stock table and the migrations
of `cloud/migrations/` both have, and prints what it did for each table. In the run:

| Table | Rows | Carried | Made up (defaults) | Dropped |
|-------|------|---------|--------------------|---------|
| `platform` | 2 | all 5 columns | none | none |
| `app` | 1 | 13 | `validator` (`trivial`), `validator_url`, `assimilate_url` | `min_avg_pfc`, `host_scale_check`, `locality_scheduling`, `n_size_classes` |
| `app_version` | 1 | all 16, including `xml_doc` as the stock `update_versions` wrote it | none | none |
| `user` | 3 | 22, including `authenticator`, `cross_project_id`, `email_addr`, `passwd_hash` | `blocked`, `valid_results`, `recent_key` | `posts`, `seti_*`, `signature`, `has_profile`, `donated`, `login_token`, `login_token_time` |
| `host` | 9 | 49, including `id`, `userid`, `rpc_seqno`, `rpc_time`, `host_cpid` | `valid_results`, `recent_key` | none |
| `host_app_version` | 9 | all 15 | none | none |
| `workunit` | 28 | 33, including `xml_doc`; `result_template` read from the file `result_template_file` names | `rev`, `commit_token` | `result_template_file`, `fileset_id`, `size_class` |
| `result` | 28 | 36, including `xml_doc_in` | none | `size_class` |

Three conversions: `workunit.mod_time` and `result.mod_time` are `TIMESTAMP` columns in the stock
schema and seconds in this one; a `passwd_hash` that is a bare MD5 becomes the HMAC-SHA-256 with
the deployment's `PASSWD_PEPPER`, as this server stores it, and any other value (the PHP
`password_hash()` form) is copied as it is; the workunit's result template is read from the file.

Outside the tables: the `<config>` element of `config.xml` (every tag this server does not read is
ignored), the two public keys, the private upload key (this server signs upload certificates
with it), the private code-signing key only where new application versions are signed with it
(`cboinc version-add`), the files of the stock `download/` directory (each stored with
`cboinc file-put`, the `.md5` files that the stock server leaves beside them left out), and the
files of the stock `upload/` directory (stored under `upload/<name>`, whatever the hash directory
they were in). Not carried, because nothing in the check needs them: forum, team, profile and
badge tables, the credit and statistics history, `project.xml`, templates and `html/` customization.

### What the clients did

In all three modes and for every version the clients behaved alike unless a line says otherwise.
None logged anything about their key, a changed project or a parse problem, and none asked to be
attached again. The first request after the switch is in
`cloud/tests/migration-check/captures/<version>/first-request-after-switch.xml`, scrubbed, and
replayed by `cloud/crates/cb-core/tests/migration.rs`.

| Question | `copied` (all six versions) | `notcopied` (7.16.6, 8.0.4, 8.2.15) | `nohost` (8.0.4) |
|----------|-----------------------------|--------------------------------------|-------------------|
| Stays attached, keeps its account key and project URL | Yes. No detach, no `<request_delay>` other than 32 seconds (the check's `min_sendwork_interval` of 31, plus 1), no message about the key or the project | Same | Same |
| Host ID | Kept. The first request sends the stock host ID and `<rpc_seqno>` 2 (the stock row had 1); the reply has no `<hostid>`; no second host | Same | The row was not carried, so the server makes a host (ID 11) and sends `<hostid>11</hostid>`; the client adopts it and logs "Generated new computer cross-project ID". Its tasks go on under the new ID; the later requests carry the new ID and sequence numbers from 0 |
| Master page and scheduler list | Not fetched again. Every scheduler request of every client went to the stock URL, `/proj_cgi/cgi`. After the detach and attach the clients fetched the master page, found one scheduler URL, the stock one, and used it | Same | Same |
| New work (version 1.01 signed with the stock key, new input file) | Downloaded, signature and MD5 accepted, run, uploaded, reported, validated; the 1.00 file was not fetched again | Same | None offered; the other clients had taken the work |
| The task that finished and uploaded before the switch | Reported with the first request, acknowledged, recorded as a success; validated once the upload files were carried (D19) | Acknowledged; no row, so nothing recorded | As `copied`, on the new host |
| The running task | Went on, finished after the switch, uploaded with the stock server's upload certificate, was reported, acknowledged and validated | Aborted by the first reply (D18) | As `copied` |
| The task that had not started | Ran when the CPU was free, uploaded, reported, validated | Aborted by the first reply (D18) | As `copied` |
| Detach and attach again | Same host ID, found by the user and the host CPID (`<hostid>0</hostid>` in the request, the old ID in the reply); no host row added | Same | Same, with ID 11 |

The 20 new jobs made 40 results, all valid, from the six client versions.

The accounts' own replies, asked of both servers (`web-probe.sh`):

| Request | Stock server | cloud-boinc |
|---------|--------------|-------------|
| `get_project_config.php` | the project's name, URL and platforms | the same text |
| `lookup_account.php`, right password, account 1 (stock password hash) | the account key | `-206` "Invalid password" (D20) |
| the same, account 2 (old form of hash) | PHP fatal error `hash_equal()` (D2) | the account key |
| the same, account 3 (address with capitals) | the account key | `-136` "Not found" (D22) |
| `lookup_account.php`, no password hash | `<success/>` for an address that exists | `<success/>`; `-136` for account 3 |
| `am_get_info.php` with the account key | id, name, country, `<weak_auth>`, cross-project ID, preferences | the same, except the XML declaration (D24) and `<weak_auth>` of account 2 (D21) |
| Site login, `login_action.php`, right password | accounts 1 and 3: redirect with the sign-in cookie; account 2: PHP fatal error (D2) | accounts 1 and 3: 403; account 2: redirect with the cookie |

### Differences

Each is something a project or a volunteer would see on a move, numbered after D17 above. They are
not in the differential test's list of intended differences; that test compares replies to the
same request, and these come from rows and files that a move carries or does not. "Fix" says
what avoids the difference: a step of the move, a setting, or nothing.

| No. | What | Stock server | cloud-boinc | Fix |
|-----|------|--------------|-------------|-----|
| D18 | A client's tasks whose workunit and result rows were not carried, with `send_result_abort` on (the `make_project` default) | `send_result_abort` in `sched/handle_request.cpp` lists a task the client holds that is not an in-progress result of that host in the database as `ABORT_REASON_NOT_FOUND` and answers `<result_abort>` and "Result NAME is no longer usable". On a stock server that is a task whose row is gone. Read from the code, not run | The same decision, from the same list of tasks the client sends (`<other_results>`). After a move without job rows every client's first reply aborts the running and the not started tasks; the volunteer loses what they computed. Client log: "Result NAME is no longer usable" for each task, then "Deferring communication for 00:01:07" (7.16.6), 00:01:10 (8.0.4) or 00:01:03 (8.2.15) with the reason "Unrecoverable error for task NAME", then "garbage_collect(); still have active task for acked result NAME; state 5" (8.0.4 and 8.2.15 again with state 6) and "Output file NAME_r..._0 for task NAME absent". The finished task is acknowledged and its result is not recorded | Carry the workunit and result rows of every task in progress. With `send_result_abort` set to 0 the tasks run to the end, upload and are reported, and are acknowledged and thrown away (D25) |
| D19 | Output files of tasks that were uploaded to the stock server and not yet validated | In `upload/<hash>/` where the stock validator reads them | Read from the bucket as `upload/<name>`. A copied result row whose file is not there is reported by the client, acknowledged and then found invalid: outcome 6 (validate error), validate state 2, and the workunit gets `error_mask` 8 (too many total results). Nothing is visible to the client; the volunteer gets no credit for the task. In the second run both clients' finished task came out so, and the tasks that finished and uploaded after the switch validated | Store the files of the stock `upload/` directory in the bucket before the clients are let in (`cloud-load.sh --carry-uploads`); with them the same results validated |
| D20 | An account whose password hash is PHP `password_hash()` of the client's MD5 (what the stock site stores for new accounts and after a login, `do_passwd_rehash` in `html/inc/user_util.inc`) | `lookup_account.php` and the site's login accept the password | The hash is not verified: this server stores HMAC-SHA-256 of the MD5 with a secret, and a bcrypt value is not that. `lookup_account.php` answers `-206` "Invalid password" and the login form 403, for the right password too. The account key, which attached clients use, is unaffected. A volunteer who attaches a new computer by address and password, or signs in to the site, cannot | Nothing converts a bcrypt hash. The volunteer uses the account key (`login_action.php` takes it), or a project that has email set up sends the login link (`get_passwd.php`, not run here). A hash in the old form, the bare MD5, is converted by the row copy and then works |
| D21 | The weak account key (`<weak_auth>` in `am_get_info.php`, `<userid>_<md5(authenticator + passwd_hash)>`) of an account whose hash was converted | From the stored value | `weak_auth` in `cloud/crates/cb-crypto/src/account.rs` uses the value this server stores (D7 gives the form), so the HMAC replaces the MD5 and the key changes (account 2). A bcrypt value copied as it is keeps its weak key (accounts 1 and 3) | An account manager or a link that holds a weak key of a converted account must fetch it again. Copying the old hash unchanged (and so not allowing the password) keeps the key |
| D22 | An address stored with capitals | MySQL compares addresses without regard to case: `lookup_account.php` and the site's login find `Migrate.Three@Example.ORG` from the lower-case address a client sends | `user.email_addr` is compared as stored. `lookup_account.php` answers `-136` "Not found" and the login form 403 | Lower-case the addresses when copying them. The password hash a client sends is made from the lower-case address (`passwd_hash` in `cloud/crates/cb-crypto/src/account.rs`) |
| D23 | A host row that was not carried | The host ID the client sends is looked up; if there is no such row, the host CPID is looked up among the user's computers (`find_host_by_cpid` in `sched/handle_request.cpp`; a host with that CPID whose processor count, vendor and model, or OS name and version differ is not used, `obviously_different`); if there is none, a computer of the user with the same name, address, OS, processor model and memory is looked for (`find_host_by_other`, only when the name, address, OS name and processor model are all present, and skipped for a client run with `allow_multiple_clients`); if there is none, a new row is made | The same for the host ID and CPID steps. The clients of the check all ran with `allow_multiple_clients`, so the name and address step was skipped on both sides and was not exercised. It differs in the code: this server does not write off the matched host's tasks in progress when the client lists none (upstream does), and has no `dont_search_host_for_userid` setting. The client gets `<hostid>` with the new ID and adopts it. 8.0.4 logs "Generated new computer cross-project ID: ..." (`handle_scheduler_reply` in `client/cs_scheduler.cpp`, which generates a new CPID when the project sends a host ID after it has already sent one, taking that as a sign that the state was copied from another computer); in the captured later requests of that client the `<host_cpid>` was the same as before. Credit totals, `rpc_seqno` and the host's statistics start again, and `host_app_version` is empty. Tasks that were sent to the old ID are accepted from the new host, as they are for any host of the same user. Found from the code, not run: the stock server aborts such tasks, because it only looks at results of the host that asks that are in progress or timed out, where this server looks them up by name | Carry the host rows (and `host_app_version`) with their IDs |
| D24 | `am_get_info.php` | The reply starts `<?xml version="1.0" encoding="ISO-8859-1" ?>` | `encoding="UTF-8"` | An account manager reads the same values; see D16 for the same declaration |
| D25 | A task the project has no row for, with `send_result_abort` set to 0 | `handle_results` in `sched/sched_result.cpp` acknowledges a reported result that is not in the database ("reported result not in DB") and records nothing (read from the code, not run) | The same: the clients' tasks (7.16.6, 8.0.4) ran to the end, uploaded (the stock upload certificate is accepted although no result row exists), were reported and acknowledged, with no message on the client. Nothing is recorded, so the volunteer's computation earns no credit and the project never sees the result | Carry the rows (D18) |

### What a move has to carry

From what the check needed, for a project that keeps its master URL and its volunteers' clients:

1. The master URL and its layout: `master_url`, `stock_short_name` (or the four prefixes),
   `uldl_dir_fanout`, and the routes of `docs/deploy.md`, as under "Moving from a stock server:
   URLs". The code-signing public key as the same text (leading and trailing white space does
   not matter), the upload public key, and the upload private key.
2. The stock `config.xml` `<config>` element. This server reads the tags it knows and ignores the
   rest. `min_sendwork_interval` (replies carried a delay of 32 seconds, the carried 31 plus one) and
   `send_result_abort` (D18; this server's own default is off, so it has to come from the carried config) were seen to take effect, and the upload certificates the stock
   server signed were accepted under `ignore_upload_certificates` 0. Database credentials and the
   host name are not needed.
3. `platform`, `app` and `app_version` rows with their IDs, `app_version.xml_doc` as the stock
   `update_versions` wrote it (it holds the file's signature, made with the code-signing key), and
   the application files from the download directory.
4. `user` rows with `authenticator` (the account key), `cross_project_id`, `email_addr` in lower
   case and `passwd_hash` (D20 to D22); `host` rows with `id`, `userid`, `rpc_seqno`, `rpc_time`
   and `host_cpid`, and `host_app_version` (D23).
5. For every task a client holds: its `workunit` and `result` rows, with `xml_doc` and
   `xml_doc_in`, and the input files in the download directory (D18). For every result that has
   been uploaded and not yet validated: the output files from the `upload/` tree (D19).
6. The stock server must not take any more reports while the rows are copied, or reports made after the copy are missing from it. The check copied rows while every client had its network off
   and stopped the stock server afterwards.

### What the check did not cover

- Cloudflare's platform. cloud-boinc ran as the five Workers under `wrangler dev` (local workerd, with
  a local SQLite file for D1, a local R2 bucket and a local queue), so Cloudflare's network, the
  platform limits (CPU time, memory, D1 statements per request, queue delivery), HTTPS and
  certificates, and the real routes were not in the loop. The scheduled sweep does not run under
  `wrangler dev` and was not run; nothing in the check needed it, but task timeouts did not happen.
- HTTPS. Stock clients speak plain HTTP to a project at the made-up host name; their handling of a
  TLS certificate and of a redirect to another host was not tried.
- A client that contacts the project while the switch is happening. Each client's network
  was off from before the dump until about 40 seconds after the stock server stopped, so no
  request met a closed port.
- Windows, macOS and Android clients, 32-bit and aarch64 Linux, GPU hosts and plan classes, anonymous
  platform, account managers (only the replies of their RPCs were compared, no manager ran),
  several projects in one client, and a client that has been attached for months.
- A project's real rows: the database was small (three accounts, ten computers, 40 tasks) and made
  for the check, with no credit history, forum, teams or profiles. The copy script is not an
  importer, and what it does not carry (the table above) was not needed for a client to carry on.
- Mail. Site sign-up, `get_passwd.php` and the address change send mail; none was sent.
- Quorums larger than one for work that was in flight, tasks past their deadline, and daily
  quotas.
- Only one stock server version: the upstream tree of this repository, with the PHP error of D2.

### The automated test

`cloud/crates/cb-core/tests/migration.rs` replays the first request each client version sent after the switch
through `handle_request` in a database that has the stock rows, or part of them. It checks the host
and sequence number, the code-signing key comparison, the acknowledgement and recording of the
finished task, that the tasks held are left alone with the rows copied and aborted without them
(and left alone with `send_result_abort` off), and the host row cases of D23. It needs no stock
server, client or Worker.
