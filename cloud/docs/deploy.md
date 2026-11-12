# Deploying

A deployment is five Workers, one D1 database, one R2 bucket and one queue on a hostname in a
Cloudflare zone. The `wrangler.toml` files under `workers/` describe the reference dev
deployment at `boinc-dev.usnewsmap.com`. For another deployment, change the route patterns,
`zone_name`, `database_id` and the database, bucket and queue names in all five.

You need Rust with the `wasm32-unknown-unknown` target, `worker-build` (`cargo install
worker-build`), Node.js for `wrangler`, and a Cloudflare API token that can edit Workers, D1,
R2, Queues and the zone's routes and DNS.

## Local stack

```
dev/local.sh --state ../state
```

builds the Workers and runs them under `wrangler dev` on `http://127.0.0.1:8787`, behind
`dev/router`, which routes by path as a deployment's routes do. The first run creates keys, an
admin token and a password pepper in the state directory. In a second shell:

```
cargo build --release -p cboinc
cargo build --release -p cb-test-app --target x86_64-unknown-linux-musl
dev/seed.sh --url http://127.0.0.1:8787 --token-file ../state/token --keys ../state/keys
```

sets up a test project with one app and three jobs and prints an account's authenticator. A
BOINC client can then attach to `http://127.0.0.1:8787/` with that authenticator. Use a
separate client instance with its own data directory. `wrangler dev` does not run the
one-minute sweep; `curl http://127.0.0.1:8787/__sweep` runs it once.

## Keys

```
cboinc keygen --private keys/code_sign_private --public keys/code_sign_public
cboinc keygen --private keys/upload_private --public keys/upload_public
```

Keep `code_sign_private` on the machine that signs app versions. It is never uploaded.
Clients pin the code-signing public key when they attach, so losing the private key means
every client has to detach and attach again.

## Resources

```
npx wrangler d1 create cloud-boinc-dev
npx wrangler r2 bucket create cloud-boinc-dev
npx wrangler queues create cloud-boinc-dev-transitions
```

Put the database ID that `d1 create` prints into the five `wrangler.toml` files, then create
the schema:

```
cd workers/scheduler && npx wrangler d1 migrations apply cloud-boinc-dev --remote
```

Unfinished uploads over 8 MiB leave pieces under `partial/` (see "Upload" in
[architecture.md](architecture.md)). A finished upload deletes its own; give the bucket a
lifecycle rule for the ones that are never finished:

```
npx wrangler r2 bucket lifecycle add cloud-boinc-dev --name partial --prefix partial/ --expire-days 14
```

A client whose pieces expired is told it holds fewer bytes and sends the file again from the
start.

The hostname needs a proxied DNS record in the zone. An `AAAA` record pointing at `100::` is
enough, since the routes answer every path.

Add a Configuration Rule so that an upload that is cut off part-way can resume. By default
Cloudflare holds a whole request body before it starts the Worker, so a request whose
connection drops never runs and nothing is kept. The rule makes the body stream to the Worker.
In the dashboard it is under Rules > Configuration Rules. As a ruleset in the zone's
`http_config_settings` phase it is:

```
expression: (http.host eq "<hostname>" and starts_with(http.request.uri.path, "/upload/"))
action:     set_config
action_parameters: { "request_body_buffering": "none" }
```

The Free plan accepts this rule. Cloudflare notes that products that inspect request bodies,
such as the WAF, do not see the bodies of requests that match. The upload bodies here are
checked by the upload certificate signature, the size limit and the MD5. Without the rule,
uploads work, but after a dropped connection they restart from byte 0.

## Workers

```
dev/build.sh
for w in scheduler files pipeline web admin; do (cd workers/$w && npx wrangler deploy); done
```

The configs declare the routes, the queue consumer and the pipeline's two cron triggers (the
sweep every minute, the purge every five) for `wrangler deploy` to create. The dev deployment's resources and Workers were created through
the Cloudflare API, so the `wrangler` commands on this page have not been run as written.

## Secrets

| Secret | Workers | Value |
| --- | --- | --- |
| `UPLOAD_PRIVATE_KEY` | `pipeline`, `admin` | contents of `keys/upload_private` |
| `ADMIN_TOKENS` | `admin` | one or more bearer tokens, separated by commas |
| `PASSWD_PEPPER` | `web`, `admin` | a random string; changing it invalidates every stored password |

Set each with `npx wrangler secret put <NAME>` in the Worker's directory.

## Project setup

`dev/seed.sh` with the deployment's URL, a file holding an admin token and the key directory
sets up the test project. For a real project, do the same steps with `cboinc`: `config-put`
for `config`, `code_sign_public_key` and `upload_public_key`, then `platform-put`, `app-put`,
`version-add`, `file-put` and `submit`. `cboinc assimilate`, `output-get` and `ack` collect
finished work. The pipeline deletes a workunit's files once it is acknowledged and
`delete_delay_hours` have passed.

## Purge

Set `<purge_days>` in the config to have finished workunits and their results removed from
the database that many days after their files are deleted. Each purge pass first writes the
rows to `archive/<date>/` in the bucket. Nothing reads the archive; to keep it from growing
without bound, give the bucket a lifecycle rule for that prefix:

```
npx wrangler r2 bucket lifecycle add cloud-boinc-dev --name archive --prefix archive/ --expire-days 365
```

A database created before `migrations/0002_purge.sql` existed needs
`wrangler d1 migrations apply` again before the purge is switched on.

Likewise, `migrations/0003_user_tasks.sql` adds the index behind an account's task list
(`results.php?userid=`); apply it before the web Worker that serves that page is deployed.

`migrations/0004_email.sql` must be applied before the web or admin Worker of this build is
deployed, whether or not the deployment sends email: those Workers write columns and tables it
adds (see step 4 under Email).

`migrations/0005_upload_epoch.sql` adds the table the files Worker reads to tell the pieces of a
resent task's earlier attempt from its current one; apply it before the scheduler and files
Workers that use it are deployed.

## Dashboards

`migrations/0008_dashboards.sql` must be applied before the pipeline or web Worker of this build
is deployed: the pipeline writes `valid_results` and the rollup tables, and the web pages read
them. It adds a column to `user` and `host` and counts the valid tasks still in the table into
it (one pass over the tasks of accounts and computers that have credit), builds six leaderboard
indexes, replaces `res_host_inprog` with one that also holds the application, builds `res_received`
and `wu_need_validate` unless the per-application server status migration already did, and creates
`stats_bucket` and `stats_snapshot`. Building `res_received` writes one index row for each task
ever received that is still in the table, once. The scheduler, files and admin Workers do not name
the new columns and work before and after.

The pipeline Worker's existing five-minute cron fills in the history: the first pass counts the
open three-hour period and six more, and each later pass adds six, so the week of charts is
complete after about ten passes. `work.php` says the counts are not available until the first
pass has run. To show the project's own figures on `work.php`, set `<dashboard_feed_url>` in the
config (see [dashboard-feed.md](dashboard-feed.md)).

## Email

The site's password recovery, address verification and address-change notices
need a mail transport (see Email in `architecture.md`). The web Worker sends through the
`send_email` binding of Cloudflare Email Service, which the Cloudflare docs call Email Sending
and describe as a beta. A Worker without the binding, or a config without `email_from`, sends
nothing: the pages that need email say the project does not send email, and the rest of the
site is unchanged. To turn email on:

1. **Plan.** The Cloudflare docs (Email Service, "Pricing") say sending to arbitrary recipients
   requires the Workers Paid plan, and that outbound email is not available on Workers Free.
   Sends to verified destination addresses in the account are free on any plan. The same page
   gives the included monthly quota and the price beyond it; read the figures there. The
   "Limits" page says a new account starts with a conservative daily quota and gives no
   number. The zone must use Cloudflare DNS.
2. **Sending domain.** In the dashboard go to Compute > Email Service > Email Sending, choose
   Onboard Domain, and choose the project's own hostname (a subdomain such as the master URL's
   host, not the zone apex). The docs ("Configure subdomains") say Email Sending treats a
   subdomain as a separate sending domain and onboarding adds the records to it. The records
   are, on a `cf-bounce` subdomain of the sending domain: three MX records to
   `route1.mx.cloudflare.net`, `route2.mx.cloudflare.net` and `route3.mx.cloudflare.net` (the
   bounce address); a TXT SPF record `v=spf1 include:_spf.mx.cloudflare.net ~all`; a TXT DKIM
   record at `cf-bounce._domainkey` with a key Cloudflare provides; and a TXT DMARC record at
   `_dmarc` of the sending domain. The docs show `p=reject` in one place and recommend
   `p=none` for new setups in another; look at what onboarding creates and choose the policy
   on purpose. Records usually propagate in 5 to 15 minutes, at most 24 hours, and Settings >
   DNS records on the Email Sending page lists each as Locked or Unlocked (both mean it is
   configured). The sender address must be on this domain.
3. **Binding.** The Worker looks for a binding named `EMAIL`. Restrict it to the project's
   sender address with `allowed_sender_addresses`, which the docs ("Send bindings") describe as
   limiting the binding to sending from the addresses listed. In `workers/web/wrangler.toml`
   (the block is there, commented out):

   ```toml
   [[send_email]]
   name = "EMAIL"
   allowed_sender_addresses = ["boinc@<sending domain>"]
   ```

   For a deployment that uploads the Worker through the API, the binding in the `bindings`
   array of the multipart `metadata` part is (field names from the API reference for the
   Workers script upload, `SendEmail` binding):

   ```json
   { "type": "send_email", "name": "EMAIL", "allowed_sender_addresses": ["boinc@<sending domain>"] }
   ```

   The binding does not restrict the recipients, which is right: the recipients are the
   project's users. The address in `allowed_sender_addresses` has to be the config's
   `email_from`; the binding is not updated when the config is.
4. **Migration.** `migrations/0004_email.sql` adds the `token` and `mail_day` tables, two
   columns of `user` and an index. It is a prerequisite of this build, with or without email,
   not only of the email pages: sign-up and the `create_account.php` RPC, address changes,
   password changes, `am_set_info` and the account-key page of the web Worker, and the admin
   Worker's account creation, name the new columns and tables. Apply it
   (`wrangler d1 migrations apply cloud-boinc-dev --remote`, as above) before deploying the web
   or admin Worker. The scheduler, files and pipeline Workers read `user` rows by `SELECT *`
   and do not name the new columns, so they work before and after.
5. **Config.** Set the sender in the project config (through the admin API, as the other
   settings), the address on the sending domain and optionally a name, and if the default of
   200 messages a day (all kinds, UTC days; 0 sends none) does not suit, the cap:

   ```xml
   <email_from>boinc@<sending domain></email_from>
   <email_from_name>Project name</email_from_name>
   <email_max_per_day>200</email_max_per_day>
   ```

6. **Deploy and check.** Deploy the web Worker (`cd workers/web && npx wrangler deploy`).
   Create an account on the site with an address you can read, and see that the verification
   message arrives (sign-up sends it) and its link marks the address verified on the account
   page. Then ask `get_passwd.php` for a login link for that address; the link should log in.
   A failed send is not shown to the user; the Worker logs `email send failed: <code>`, where
   the code is the Email Service error code (for example `E_SENDER_NOT_VERIFIED`, the domain is
   not onboarded or not yet verified, or `E_SENDER_DOMAIN_NOT_AVAILABLE`). It does not log
   addresses or message text.

The binding is commented out in the repository's `wrangler.toml` because the Cloudflare docs
do not say whether a deploy of a Worker with a `send_email` binding succeeds on the Workers
Free plan or before a sending domain exists. A deployment that is not ready for email keeps it
commented out and deploys as before.

## Homogeneous redundancy and homogeneous app version

An app can send all results of a workunit to hosts of one class (homogeneous redundancy, HR),
or with one app version (homogeneous app version, HAV). The behaviour is described under
"Homogeneous redundancy and homogeneous app version" in `architecture.md`.

`migrations/0006_homogeneous.sql` adds `app.homogeneous_redundancy`,
`app.homogeneous_app_version` and `workunit.hr_class`. It adds columns and nothing else, so the Workers of
an earlier release keep working on the migrated database. Apply it first, then deploy the
`scheduler`, `pipeline` and `admin` Workers (the scheduler reads the new columns, the pipeline
releases a commitment when a workunit's results fail, and the admin Worker sets them). `files`
and `web` need no new deployment. Deploy all three, then turn the settings on.

```
cd workers/scheduler && npx wrangler d1 migrations apply cloud-boinc-dev --remote
cboinc app-put --name myapp --user-friendly-name "My app" --homogeneous-redundancy 2
cboinc app-put --name myapp --user-friendly-name "My app" --homogeneous-app-version true
```

`app-put` sets every other setting of the app from its flags too (validator, URLs and
`--target-nresults` fall back to their defaults when left out), so pass the app's current
values along.

`--homogeneous-redundancy` is 0 for none, 1 for fine classes (OS and CPU family) and 2 for
coarse ones (OS and CPU vendor, AMD counted as Intel). Both flags are left as they are when
a later `app-put` leaves them out, unlike the other settings of an app. Without an app-level
type, the `<homogeneous_redundancy>` option of the config applies to all apps, and
`<hr_class_static/>` keeps a workunit committed after all its results have failed. A host that
HR cannot place gets no work from the app; if every app of the project uses HR, it is told
"Your computer type is not supported by this project".

The settings act on the next result sent. A workunit that already has a result in progress when
HR is switched on is uncommitted and takes its class from the next result that is sent, so
that workunit may end up with results of two classes. `cboinc job --name NAME` shows the class
(`hr_class`) and version (`app_version_id`) a workunit is committed to. A workunit is released
by the pipeline when it has results with errors and none in progress, succeeded or timed out;
to release one that is stuck on a class with no hosts left, set its `hr_class` to 0 in the
database.

A deprecated version of an HAV app, a version superseded by a newer one and one below the
app's minimum version keep being sent for the workunits committed to them, so deprecate an old
version when it should stop taking new workunits, and expect its files to be fetched until
those workunits are done.

## Checks

```
curl https://<host>/get_project_config.php
cboinc status
cboinc load --authenticator <key> --hosts 4 --rounds 3
```

`cboinc load` runs simulated hosts against jobs already submitted. It reports made-up
results for those jobs, so use a deployment that holds test jobs and nothing else.

To check upload resume, submit test jobs whose result template allows outputs above one
piece (`<max_nbytes>` of at least `--output-bytes`), then cut every upload part-way:

```
cboinc submit --app <test app> --input-template <file> --result-template <file with max_nbytes 30000000> --count 4
cboinc load --authenticator <key> --hosts 2 --rounds 2 --output-bytes 20000000 --cut-at 12000000 --cut-mode abort
```

Each upload is first sent with the connection dropped after 12,000,000 bytes of file data
(`--cut-mode short` sends a complete request whose data stops there instead), then the host
asks `<get_file_size>` and sends the rest from the offset it hears, as a client does. The run
fails if the server reports more bytes than were sent or refuses the resumed upload; the final
put checks the MD5 of the whole file, so a success means the stored file is byte-identical.
`resumed_from_bytes` in the output shows the offsets the server gave (multiples of 8 MiB).

In `abort` mode the request announces the length of the whole file, as a client does;
`--cut-pause-ms` waits that long after the last byte before dropping the connection, and
`--resume-wait-ms` waits before the size query. In `abort` mode `resumed_from_bytes` is
8388608 with the request body buffering rule above and 0 without it, because without the rule
the Worker never sees a dropped request and the whole file is sent again. A size query sent
immediately after the drop can be too early: the Worker may not have stored the piece yet, and
it reports 0. Use `--resume-wait-ms` to leave a few seconds. `short` mode reports 8388608 either
way (see "Upload" in [architecture.md](architecture.md)).

## Plan limits

The Workers Free plan stops at 100,000 requests, 100,000 D1 rows written and 10,000 queue
operations a day, and limits a request to 10 ms of CPU. The dev deployment's load test ran
inside those limits. The cost table in [architecture.md](architecture.md) has the details.
