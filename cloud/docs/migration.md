# Importing a stock project's database

`cb-import` (crate `cloud/crates/cb-import`) reads a stock BOINC project's MySQL or MariaDB
database and writes the tables of this server's schema (`cloud/migrations`), either into a new
SQLite file or into numbered `.sql` files for D1. It is the tool behind the "Import and cutover"
part of issue #17. [compatibility.md](compatibility.md), "Migration check", says what a move has
to carry and why; this page says what the tool carries and how.

## What it does and does not do

It does:

- read the source with SELECT statements only, in one read-only transaction;
- write `platform`, `app`, `app_version`, `team`, `user`, `host`, `host_app_version`, `workunit`,
  `result`, `assignment`, `batch` and `job_file`, and set the ID floors;
- write a manifest of the files that have to be in the bucket;
- print the `cboinc config-set` lines for a stock `config.xml`;
- compare the source with a target SQLite file (`verify`).

It does not:

- connect to Cloudflare. The `.sql` files are loaded with `wrangler d1 execute --file`
  or the D1 import API, by a person, after a rehearsal;
- copy files. It lists them (`--files-manifest`); the copy is a separate step;
- carry keys. The code-signing and upload public keys go in with `cboinc config-put`, and the upload
  private key is the `UPLOAD_PRIVATE_KEY` secret (`deploy.md`, "Keys" and "Secrets"). The tool only
  checks that a given code-signing public key verifies the signatures stored in `app_version.xml_doc`;
- write the `config` table (see "Config" below);
- carry forums, profiles, badges, private messages, per-application credit or any other table of
  `db/schema.sql` that has no counterpart (the list is under "What is lost").

## Usage

```
cb-import import --source-dsn-file FILE (--target-sqlite FILE | --target-sql-dir DIR)
                 [--project-dir DIR] [--config FILE] [--passwd-pepper-file FILE]
                 [--code-sign-public-key FILE] [--files-manifest FILE]
                 [--finished leave|all|recent] [--finished-days N]
                 [--allow-size-class] [--allow-fileset-id]
                 [--download-dir DIR] [--upload-dir DIR] [--max-file-bytes N]
                 [--skip-valid-counts]
cb-import verify --source-dsn-file FILE --target-sqlite FILE [--sample N] [the scope flags of import]
cb-import config --config FILE
cb-import load-sql --sql-dir DIR --target-sqlite FILE
```

Every argument is a named flag. A target is created new: the tool refuses a SQLite file that exists and
a directory that already holds `.sql` files, and when an import fails it removes what it wrote.

| Flag | Meaning |
|------|---------|
| `--source-dsn-file` | A file with one line, `mysql://user:password@host:port/database`. The DSN is never a command line argument and is never printed. Parameters at the end of the URL: `?tls=required` turns TLS on (the server's certificate is verified), `tls_ca_file=/path/ca.pem` names the CA file for a private CA, and `socket=/path` connects through a Unix socket. TLS is built in (rustls); it was not exercised against a TLS server in the tests. |
| `--target-sqlite` | A new SQLite file with the schema of `cloud/migrations`. |
| `--target-sql-dir` | A directory for `NNNN_<table>.sql` files, one INSERT per line. |
| `--project-dir` | The stock project directory. Needed when workunits are imported: each workunit's `result_template_file` is read from it. Also where the `download/` and `upload/` trees are looked for (`--download-dir` and `--upload-dir` override). |
| `--config` | The stock `config.xml`. Gives `uldl_dir_fanout` (for the manifest), the master URL's host (to tell staged files from other URLs), and the validator daemons. |
| `--passwd-pepper-file` | The deployment's `PASSWD_PEPPER`. See "Password hashes". |
| `--code-sign-public-key` | Checks every signature in `app_version.xml_doc` against this key before anything is written. A mismatch fails the run. |
| `--finished` | What to do with finished workunits. `leave` (the default), `all`, or `recent` with `--finished-days`. |
| `--allow-size-class`, `--allow-fileset-id` | See "Jobs". |
| `--skip-valid-counts` | Do not scan `result` for the count of valid tasks per account and computer. |
| `--max-file-bytes` | Size at which a `.sql` file is closed (default 64 MiB). |

`verify` and `import` take the same scope flags. Run `verify` with the flags `import` was given.

The exit status is 0 on success, 2 for bad flags or unreadable input files, 3 for a source error, 4 for
a target error, 5 when the import refused something, and 6 when `verify` or the key check found a
difference.

## What is read

The session sends only SELECT, SET, START TRANSACTION and SHOW (`is_read_only_statement` in
`source.rs`; every statement is checked before it is sent, and a test asserts the list of statements a
whole run sent). It sets `SESSION TRANSACTION READ ONLY` and starts the transaction with
`START TRANSACTION READ ONLY, WITH CONSISTENT SNAPSHOT`, so every table is read at the same point in time.
It sets the session time zone to UTC so that `UNIX_TIMESTAMP` of a `TIMESTAMP` column is the same
number wherever the server runs. The account needs SELECT on the database and nothing else; the
integration test uses such an account.

Rows are streamed: one result set is open at a time and each row is mapped and written as it arrives. The
only things kept in memory are the account addresses (to find collisions), the IDs of the carried
workunits, the per-account counts of valid tasks, the names of the team and batch IDs seen, and one entry
for each staged file named by a carried document.

## Which rows are carried

Everything of `platform`, `app`, `app_version`, `team`, `user`, `host` and `host_app_version`: the
accounts and computers of a project are small next to its tasks, and clients keep using their host IDs.

For jobs, `--finished leave` carries a workunit, all its results and its assignments when any of these holds:

- a result of it has `server_state` 1 (inactive), 2 (unsent) or 4 (in progress);
- a result of it was received with a success outcome and is not yet validated (`server_state` 5,
  `outcome` 1, `validate_state` 0);
- the workunit has `need_validate` set, or its `assimilate_state` is not 2 (not yet assimilated);
- it is the workunit with the highest ID, or the workunit of the result with the highest ID. This
  server's `workunit` and `result` IDs are `INTEGER PRIMARY KEY` without a floor table, so a new row takes the
  highest ID plus one; the purge keeps the highest rows for the same reason (see "Implementation status"
  in `architecture.md`).

Every other workunit is finished, and with `leave` it and its results stay on the stock server. The
summary says how many workunits and results were left. `--finished all` carries every workunit, result
and assignment. `--finished recent --finished-days N` carries the open ones and the workunits created in
the last N days.

Batches are carried when a carried workunit names them (all of them with `--finished all`).

What is left behind with `leave`: the workunits, results and assignments of finished work, and so the
credit history that the stock `result` table holds. Account and computer credit totals are columns of
`user` and `host` and are carried. `valid_results`, the count behind "jobs completed" on the leaderboards,
is counted from the source's `result` table, so it includes the results left behind.

## Table and column mapping

The target column set is read from the migrations. A column that exists in both schemas is carried
under the same name. The summary lists, for each table, the source columns that are not carried and the
target columns with no source column, so this table can be checked against a real run.

| Table | Carried | Not carried (lost) | Computed or defaulted |
|-------|---------|--------------------|-----------------------|
| `platform` | all | none | |
| `app` | all columns of the target that stock has, including `homogeneous_redundancy` and `homogeneous_app_version` | `min_avg_pfc`, `host_scale_check`, `locality_scheduling`, `n_size_classes` | `validator` from the stock daemon list when `--config` is given, else `trivial`; `validator_url`, `assimilate_url` empty |
| `app_version` | all, `xml_doc` byte for byte | none | |
| `team` | all that the target has | `name_html`, `seti_id`, `ping_user`, `ping_time`, `mod_time` | `nusers` 0 (the triggers count), `name_lc` lower-case `name`, `recent_key` |
| `user` | all that the target has, including `previous_email_addr` and `email_addr_change_time` | `posts`, `seti_id`, `seti_nresults`, `seti_last_result_time`, `seti_total_cpu`, `signature`, `has_profile`, `donated`, `login_token`, `login_token_time` | `email_addr` lower-cased, `passwd_hash` (see below), `blocked` 0, `valid_results`, `recent_key` |
| `host` | all, including `id`, `userid`, `rpc_seqno`, `rpc_time`, `host_cpid`, `misc` | none | `valid_results`, `recent_key` |
| `host_app_version` | all | none | |
| `workunit` | all that the target has, including `hr_class`, `app_version_num`, `n_sent`; `xml_doc` byte for byte | `result_template_file`, `fileset_id`, `size_class` | `result_template` is the text of the file `result_template_file` names; `mod_time` in seconds; `rev` and `commit_token` 0 |
| `result` | all that the target has; `xml_doc_in` and `xml_doc_out` byte for byte | `size_class` | `mod_time` in seconds |
| `assignment` | all | none | |
| `batch` | `id`, `user_id`, `create_time`, `name`, `app_id`, `state`, `description` | `logical_start_time`, `logical_end_time`, `est_completion_time`, `njobs`, `fraction_done`, `nerror_jobs`, `completion_time`, `credit_estimate`, `credit_canonical`, `credit_total`, `project_state`, `expire_time`, `n_success`, `n_in_progress` | |
| `job_file` | built from the documents, see below | the stock `job_file` table (a list of staged files with deletion times) is not read | |

Conversions that apply to every row: a source NULL becomes the target column's default; a stock
`TIMESTAMP` column (`workunit.mod_time`, `result.mod_time`) becomes seconds since 1970; a `double` in an
integer column (`user.email_addr_change_time`) is rounded. A text or document column that is not valid UTF-8 stops
the import with the table, column and row ID, except `result.stderr_out`, where the invalid bytes are
replaced and counted in the summary.

## What is lost

- The columns in the "Not carried" column above. The ones that matter to a project: `user.posts` and
  `user.signature` (forum), `user.has_profile` and `user.donated`, `team.name_html` (this server has a
  plain-text team name, D29), the `batch` progress counters (this server counts from `workunit`), and
  `app.min_avg_pfc`, `host_scale_check` (nothing here reads them).
- `workunit.size_class` and `fileset_id` (locality scheduling): refused unless allowed, see "Jobs".
- Tables with no counterpart: `user_submit`, `user_submit_app`, `msg_from_host`, `msg_to_host`,
  `profile`, `category`, `forum`, `thread`, `post`, `subscriptions`, `forum_preferences`, `forum_logging`,
  `post_ratings`, `sent_email`, `private_messages`, `credited_job`, `donation_items`, `donation_paypal`,
  `team_delta`, `banishment_vote`, `banishment_votes`, `team_admin`, `friend`, `notify`, `badge`,
  `badge_user`, `badge_team`, `credit_user`, `credit_team`, `token`, `user_deleted`, `host_deleted`,
  `consent`, `consent_type`. Of these, `team_admin` and `credit_user`/`credit_team` (per-application credit)
  and the forum and profile tables are features this server does not build; the others are
  per-event logs.
- Finished work, with `--finished leave` (above).
- Stock `token` rows (mailed login and verification links) are not carried; a link mailed before the move
  stops working.

## Rules

### Addresses (D25)

`user.email_addr` is trimmed and lower-cased with the same functions the server uses to look an address
up (`trim` and `to_lowercase` in `cloud/crates/cb-core/src/web.rs`). `previous_email_addr` too. MySQL
compares addresses without regard to case, so the stock unique index normally prevents two accounts whose
addresses differ only in case, but a database with a binary collation, or one that was loaded around the
index, can hold them. Before anything is written the tool reads every address and, if two accounts map to
the same lower-case address, stops and prints the account IDs of each group. There is no flag to import
them anyway: which account keeps the address is a decision for the project, made on the stock server
(merge or change one), after which the import is run again. An account with no authenticator or no address
stops the import the same way.

### Password hashes (D23, D24)

The stock `passwd_hash` is the client's `md5(password + lower-case address)` (32 hex digits, the old
form), or PHP `password_hash()` of that (`$2y$...`, `$argon2id$...`). This server verifies only
`HMAC-SHA-256(PASSWD_PEPPER, md5)` (`verify_peppered_passwd_hash` in `cloud/crates/cb-crypto/src/account.rs`).
The weak account key is `md5(authenticator + stored passwd_hash)` (`weak_auth`), so it stays valid only
while the stored string is the one the stock server had.

The tool carries what it can verify and otherwise leaves the value alone:

| Stored value | Without `--passwd-pepper-file` | With it |
|--------------|-------------------------------|---------|
| 32 hex digits | carried unchanged; the password does not work here, the weak key stays valid | replaced by the HMAC; the password works, the weak key changes (D24) |
| PHP `password_hash()` | carried unchanged | carried unchanged. It cannot be converted: the tool has only the hash of the MD5, not the MD5. The password does not work here (D23); the weak key stays valid |
| empty | carried | carried |
| anything else | carried unchanged | carried unchanged |

The summary counts each kind. Attached clients and account-key sign-ins are not affected. Volunteers
whose hash is the PHP form sign in with the account key or, once mail is set up, the login link; as they
sign in and change their password here the value becomes the HMAC form. Nothing is converted without the pepper, so no weak key changes by accident. Whether to convert the bare-MD5
accounts (working passwords, changed weak keys) is the project's choice, made by giving or not giving the
pepper file. The pepper is read from the file and is never printed.

### Teams and the member count

`team` rows are written before `user` rows, with `nusers` 0. The triggers of
`0009_teams.sql` (`user_team_inserted`) add one for each account inserted with a team, so `nusers` ends
equal to the number of accounts whose `teamid` is the team. This is the bulk-load rule of
`architecture.md`, "Teams". The stock count is not carried: stock marks it "temp" and recounts it daily,
and a stale value would be wrong here. An account whose `teamid` names no team row is imported with that
`teamid` (the team ID allocator knows about such accounts) and counted in the summary.
`verify` checks the count with the same query the documentation gives.

### ID floors

After the last row, `id_floor` gets an entry for `user`, `host` and `team` equal to the larger of the
highest imported ID and the source table's `AUTO_INCREMENT` minus one, so a new account, computer or team
never takes an ID the stock server has used, including those of rows deleted there. `workunit` and
`result` have no floor table; the summary says how many IDs the stock `AUTO_INCREMENT` is above the highest
imported ID (these are IDs of rows the stock server purged, and new IDs here start after the highest
imported one).

### Computers

`host` rows are carried with every column the target has (`id`, `userid`, `rpc_seqno`, `rpc_time`,
`host_cpid`, `misc`, the hardware fields and the totals), and `host_app_version` with its counters, so the
lookup of a client's computer by ID and the reset detection by `rpc_seqno` see what they saw before (D26).

### Jobs

- `xml_doc` of workunits and app versions, and `xml_doc_in` and `xml_doc_out` of results, are copied as
  bytes, not parsed and written back, so the signatures and upload certificates in them stay valid.
  URLs inside are not rewritten: this server serves the stock URL layout (`compatibility.md`,
  "Moving from a stock server: URLs").
- `result_template` is the text of the file `result_template_file` names, read from `--project-dir`. A
  missing file stops the import and names the file.
- `hr_class`, `app.homogeneous_redundancy` and `app.homogeneous_app_version` are carried (issue #3 built
  them; the text of issue #17 predates that).
- A workunit with `size_class` 0 or above (size classes) or a non-zero `fileset_id` (locality scheduling)
  uses a feature this server does not have. The import stops and lists the workunit IDs. With
  `--allow-size-class` or `--allow-fileset-id` the workunit is imported without the value: the work is
  sent to any host, which can be wrong for a project that chose size classes to keep slow hosts off long jobs.
- Stock `result.server_state`, `outcome`, `client_state` and `validate_state` are copied as they are.

### `job_file`

Every `<file_info>` of every carried `app_version.xml_doc` and `workunit.xml_doc` whose `<url>` is on the
project's host (the master URL's or the download URL's, from `--config`; without `--config` every
`<file_info>` counts) gets a row: name, `nbytes` and `md5` from the document (from the file when the
document has none), and `no_delete` 1 for app version files and for files the document marks
`<no_delete/>`. Result output files are not `job_file` rows.

### Config

`cb-import config --config FILE` prints, for each tag of the stock `<config>` element that this server reads
(`Config::option_kinds` in `cloud/crates/cb-core/src/config.rs`), a line

```
cboinc config-set --tag send_result_abort --value '1'
```

(a tag written `<name/>` has the value 1). It adds a `stock_short_name` line taken from the master URL's
last path segment, comment lines for the stock tags that are ignored (names only), and a note for a
validator daemon that is project code. Nothing is applied and tags whose names look like credentials are
never read. `send_result_abort` matters most (D21). The `config` table is
not written by the importer: the lines are run by the operator, after a look.

## Files to copy

`--files-manifest FILE` writes one line per file, four tab-separated fields: the stock path relative to the
project directory, the bucket key, the size and the MD5 of the file as it is on disk.

- `download/<name>`: every app version file and every input of a carried workunit. The stock path is
  `download/<dir>/<name>` with `<dir>` the fan-out directory (`filename_dir` in
  `cloud/crates/cb-core/src/stock.rs`, from `uldl_dir_fanout`).
- `upload/<name>`: the output files of results received and not yet deleted by the stock file deleter
  (`server_state` 5, `outcome` 1, `file_delete_state` not 2), and files a task in progress has already
  uploaded (D22).

The summary warns about files that the documents name and the directory does not have, and about files
whose size or MD5 differs from the document (the importer does not touch the file). A result in progress whose
output is not uploaded yet is not a warning.

## Limits of D1 and how the files stay under them

From the D1 limits page (Cloudflare documentation, read on 2026-11-12):

| Limit | Value | What the tool does |
|-------|-------|--------------------|
| Bound parameters per statement | 100 | None are bound: every value is a literal, one row per statement (`result` and `workunit` have more than 30 columns). |
| SQL statement length | 100,000 bytes | Statements are kept under 96,000. A row that would be longer has its longest text columns written empty in the INSERT and filled by `UPDATE <table> SET <col> = <col> || '<chunk>' WHERE <key>` statements of at most 40,000 bytes of text each (cut at a character boundary). |
| String, BLOB or row size | 2,000,000 bytes | A row over it stops the import with the table and key. |
| File import (`d1 execute`) size | 5 GB | Each `.sql` file is closed at `--max-file-bytes` (64 MiB by default), so a load can be retried file by file. |

Text with a control character (every XML document with a newline) is written as `CAST(X'<hex>' AS TEXT)`, so a
statement never spans lines and nothing that splits a file into statements can mistake text for syntax.
The files hold no `BEGIN` or `COMMIT`. They are numbered in load order: `0001_platform.sql`, `0002_app.sql`,
and so on, with `team` before `user`, and `NNNN_id_floor.sql` last.

## Privacy of output

The tool prints counts, table names, column names and row IDs. It does not print an address, name,
authenticator, password hash, cross-project ID, IP address, host name, account name of the database, or a
DSN: a connection error says only that the connection or the login failed, with the server's error number.
`verify` prints the table and column of a difference and the number of rows that differ, not the values.
Tests check the summary, the verify report and the error messages against sentinel values placed in
every such column of the synthetic database. The `.sql` and SQLite files hold the data. Keep them as private as the source.

## Verify

```
cb-import verify --source-dsn-file dsn --target-sqlite imported.sqlite --sample 1000 [scope flags]
```

For each table it prints the number of rows the import maps from the source and the number in the target,
and compares a sample field by field: the first row and every row whose key is a multiple of
`target rows / N`, found in the target by primary key. It checks that the `id_floor` rows are at or above the
highest imported IDs, that `team.nusers` equals the member count, and, with `--code-sign-public-key`, that a
`code_sign_public_key` row of the target (when it has one) is that key. A difference is reported as a table
and column with the number of sampled rows; the exit status is 6.

For a D1 load, apply the `.sql` files to a scratch SQLite file with
`cb-import load-sql --sql-dir DIR --target-sqlite FILE` and verify that file. After the load into D1,
compare row counts with `wrangler d1 execute --command 'SELECT count(*) ...'` against the verify output.

## Speed

Measured with MariaDB on the same machine as the importer (release build), 500,000 workunits and 500,000
results of about 400 bytes of documents each, `--finished all`:

| Target | `workunit` | `result` | Whole run |
|--------|-----------|----------|-----------|
| SQLite file | 22,900 rows per second | 19,000 rows per second | 48 seconds |
| `.sql` files (27 files) | 38,600 rows per second | 29,900 rows per second | 30 seconds |

Two runs of the same test gave 26,700 and 25,000 rows per second for the SQLite target, so read the figures as
within about 30 percent. The summary prints the same table for each run (`rows/second`, one line per
table). At these rates ten million results take 6 to 9 minutes. The load into D1 is the long step of a
real move and is not measured here. `valid_results` adds one scan of `result` before the first row is
written (`--skip-valid-counts` leaves it out).

The tool reads one table at a time in primary key order and writes with one transaction for every 20,000 rows
on the SQLite side. Memory use is limited to the items listed under "What is read".

## Rehearsal procedure

1. On a copy of the stock database that nothing writes to (a restored backup or a replica), create an
   account with SELECT on the project database and put its DSN in a file readable only by you.
2. Run `cb-import config --config config.xml` and read the lines.
3. Run `cb-import import` with `--target-sqlite` and `--files-manifest`, and the flags the real run will use
   (`--code-sign-public-key`, `--finished`, with or without `--passwd-pepper-file`). Read the summary:
   counts of rows per table, the password kinds, `accounts whose team is not in the team table`, the files
   the manifest found missing. Resolve any refusal on the stock side.
4. Run `cb-import verify` with the same flags. It must print `verify: ok`.
5. Run `import` again with `--target-sql-dir`, then `load-sql` into a scratch SQLite file and `verify`
   that file. This checks the files that will be loaded into D1, including the split rows.
6. Load the `.sql` files into the dev D1 (`wrangler d1 execute <db> --remote --file <file>`, one at a
   time, in name order), copy the files of the manifest to the dev bucket, and compare the row counts of D1
   with the summary. Record the time and the database size.
7. Start a stock client against the dev deployment as in `tests/migration-check/README.md` and
   check that it keeps its host ID and reports a task.

## What was run

Synthetic rows only, on one Linux host with podman. `tests/migration-check/run.sh --importer` ran the
whole migration check with the importer in place of `copy_rows.py`: the stock server image of the check
(Apache, PHP, MariaDB, the stock daemons) as project `proj`; one stock 8.0.4 client attached with the account
key of the second account; three accounts (the first with a PHP `password_hash`, the second with a bare
MD5, the third with capitals in its address and a PHP hash), one computer, one application version signed
with the project's code-signing key, and four workunits of the client. The client's first task was run,
uploaded, reported and validated by the stock daemons; three more were fetched, one finished and
uploaded without being reported, one was running and one not started. Then the switch (`switch.sh
--importer`): the importer read the stock database through its SELECT-only account, wrote the `.sql`
files, a manifest and the config lines; `verify` passed on a SQLite file loaded from the files; the stock
server stopped; cloud-boinc started locally (the Workers under `wrangler dev`) with the stock keys and
`config.xml`; the manifest's files went into the local bucket; the `.sql` files in order went into the local
D1 with `wrangler d1 execute --local`; and the client's network came back.

| Step | Result |
|------|--------|
| `import` (summary) | `platform` 2, `app` 1, `app_version` 1, `user` 3, `host` 1, `host_app_version` 1, `workunit` 3, `result` 3, `job_file` 2 rows. Of four workunits, the finished one (the client's first task) was left on the stock server, and the three open ones were carried. 10 `.sql` files, longest statement 2,233 bytes. Password hashes: one bare MD5 converted with the pepper, two PHP forms carried. One address lower-cased. The application file's signature verified against the code-signing public key (1 of 1) |
| Manifest | three lines: the input file, the application file (which `update_versions` had put at the top of the download directory, with no MD5 in its document, D32), and the upload of the finished task, 406,240 bytes in all |
| `verify`, on the SQLite file loaded from the `.sql` files | `ok` for every table, all rows compared |
| Local D1 after loading | the row counts of `user`, `host`, `workunit` and `result` equal the number of `INSERT` lines for each table (`cloud-load.sh` checks them) |
| The client after the switch | kept host ID 1 and its account key; no detach, no new host; reported its three tasks (the finished one and the other two when they had run), all of which validated here (`server_state` 5, `outcome` 1, `validate_state` 1) |
| New work | application version 1.01 signed with the stock code-signing key and 20 jobs: 40 results, all valid (40 of 40) |
| Detach and attach again | the same host ID, found by the user and host CPID |
| Web RPCs | the account whose address has capitals is found (D25 gave `-136` with the old row copy); the bare-MD5 account's right password hash is accepted and a wrong one refused; the two PHP-hash accounts get `-206` for the right hash (D23), and their site sign-in is refused |

An earlier run of the same steps, before the importer looked for application files at the top of the
download directory (D32), listed no application file in the manifest and warned about it in the summary; the client
did not need the file. That is why D32 is there.

The check ran one client and one version. The multi-client matrix of `compatibility.md` was not rerun.

## Not covered

- A real project's data. Everything above ran on synthetic rows.
- Load into Cloudflare D1 itself. The `.sql` files were checked by loading them into SQLite
  and into the local `wrangler d1 execute --local` database, not into a remote D1, so the time of a D1 load
  and its behaviour on the real statement splitter are not measured.
- TLS to the source. The option is built in and its URL parameters are unit-tested; no test ran against a
  server with TLS.
- Stock tables of non-default projects (a project with its own columns on `user` or `result`): extra
  source columns are listed as not carried.
- Text columns in a character set other than the connection's: the source connection uses `utf8mb4`
  and MySQL converts text columns, but `BLOB` columns (documents, preferences) are read as bytes and must be
  UTF-8.
