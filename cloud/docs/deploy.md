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

The hostname needs a proxied DNS record in the zone. An `AAAA` record pointing at `100::` is
enough, since the routes answer every path.

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

## Checks

```
curl https://<host>/get_project_config.php
cboinc status
cboinc load --authenticator <key> --hosts 4 --rounds 3
```

`cboinc load` runs simulated hosts against jobs already submitted. It reports made-up
results for those jobs, so use a deployment that holds test jobs and nothing else.

## Plan limits

The Workers Free plan stops at 100,000 requests, 100,000 D1 rows written and 10,000 queue
operations a day, and limits a request to 10 ms of CPU. The dev deployment's load test ran
inside those limits. The cost table in [architecture.md](architecture.md) has the details.
