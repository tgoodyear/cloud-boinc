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
  tested in `cb-proto`, but not against old client code.
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
not written, so none of this has been run against a copy of a real project's rows.

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
   signatures of equal data are equal. The settings are those `make_project` writes to
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
- Transforms (`TRANSFORMS`): the layout of `download/` (see D1) and the number of seconds a host
  must wait (see D11).
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
| D1 | `<file_info><url>` of input and application files | `download/<hex>/name`: files are spread over directories named by a hash of the file name (`uldl_dir_fanout`, `dir_hier_url` in `sched/sched_util.cpp`) | `download/name` | Names are flat in an object store ([architecture.md](architecture.md), "Files"). The client follows the URL it is given. |
| D2 | `<gui_urls>` and `<rss_feeds>` in every scheduler reply | The project's buttons in the Manager (message boards, account, tasks) from `gui_urls.xml`, and the URL of the notices feed | Neither | There are no forums or notices. The Manager shows no project buttons and no notices for the project. |
| D3 | `lookup_account.php` with a wrong password | A PHP fatal error, `Call to undefined function hash_equal()`, from `check_passwd_hash` in `html/inc/user_util.inc`. The reply is an HTML error text, not XML. | `<error>` with `ERR_BAD_PASSWD` (-206, "Invalid password"), which is what `html/user/lookup_account.php` means to send after the check | A typo in the upstream tree. Every wrong password and every use of the account-key form of the hash reaches it. The Manager's wizard shows an error either way, a different one. |
| D4 | `am_set_info.php` | `<error>` with -1, "not supported"; the script is switched off | Changes the account as the disabled script's code does | See [architecture.md](architecture.md), "Web RPCs and site". |
| D5 | `server_status.php?xml=1` | Daemon lists (download server, upload server, scheduler, feeder, transitioner and so on, with hosts), `tasks_by_app`, counts of workunits and results waiting for validation and deletion, users and computers with credit and registered in the last day, `current_floating_point_speed` | `daemon_status` with the scheduler and the pipeline, `update_time`, `software_version`, results ready and in progress, workunits waiting for assimilation, `transitioner_backlog_hours` (0.02 when anything is due, else 0), users and computers with recent credit | A D1 deployment keeps no daemon table and does not count every row on each request. A consumer that reads the other elements finds nothing. |
| D6 | Upload of a file smaller than one piece (8 MiB), cut short | Keeps the bytes received; `<file_size>` answers 10 after ten bytes; the request from offset 10 completes the file. The error for the cut request is "EOF on socket read : asked for 22, got 10". | Keeps nothing; `<file_size>` answers 0; a request from offset 10 gets "length of file ... 0 bytes < offset 10 bytes". The error for the cut request says the upload ended early. | Pieces are stored only for files of at least `PIECE_BYTES` (architecture.md, "Upload"). The client sends such a file again from the start. |
| D7 | Upload under a name that is stored, with other contents | `<status>` 0 | `<status>` -1, "file ... exists with other contents" | A stored file is never replaced, because a result may have been validated against it. A retry of the same upload succeeds in both. |
| D8 | Scheduler URL on the master page | The CGI path, `/diff_cgi/cgi` | The master URL followed by `sched`, a route of the scheduler Worker | The client reads the URL from the page. Projects that move keep their master URL. |
| D9 | Weak account key (`<weak_auth>` in `am_get_info.php`) | `userid_` and an MD5 that includes the stored password hash | The same form, with the hash this server stores (HMAC with the pepper) | Weak keys made by the stock server stop working after a move. The account key and the password keep working. |
| D10 | Upload certificates of output files, and the random part of their names | Signed with the upload key | Signed with the same key, over another name | Both verify against the upload public key. |
| D11 | "Not sending work. Last request too recent. Please wait N seconds." | N counts from the fraction of a second the last request came in | N counts from the stored time of the last request, in whole seconds, so it can be one more | Stock subtracts that stored time from the current time with its fraction (`handle_request.cpp`). |

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
   reasons only when the cache holds jobs that do not suit the host.

Not fixed, and recorded above: D2 (no project web pages), D5, D6 (pieces), D7.

### What the stock tree gets wrong

The test found these in the upstream tree. They are recorded because they decide what "the stock
server's reply" is, and nothing here changes them.

- `check_passwd_hash` calls `hash_equal` instead of `hash_equals` (D3).
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
  (8.3.0) was not run, and older clients send other requests.
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
