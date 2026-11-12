# Migration check

The check runs stock BOINC clients against a stock BOINC server, then stops that server and
serves the same master URL from cloud-boinc with the stock project's keys and rows, and records what
the clients do. The result and what it means for a project moving over are in
[compatibility.md](../../docs/compatibility.md), "Migration check". This page says how to run it.

Everything runs on one Linux host as root. Nothing touches a deployment: the project's host
name, `project.test`, exists only in each client's private hosts file, and both servers listen on
`127.0.0.1` port 80. The project's URLs name no port, so `run.sh` has no `--port`.

## What it needs

- Linux, root, `podman`, `python3`, `rsync`, `unshare`, `curl`.
- Node.js and `wrangler` (the local Workers run under `wrangler dev`, see `dev/local.sh`), and
  `worker-build` with the `wasm32-unknown-unknown` Rust target.
- Stock client binaries of the versions to test, each with its own `boinccmd`: the packaged
  7.16.6, 7.18.1, 7.20.5, 7.24.1 and 8.0.4, or a client built from an upstream tag (8.2.15 was).
  They are listed in a clients file (below).
- The stock server image, built from the upstream tree of this repository:
  `cloud/tests/differential/stock.sh --action build --image NAME`.
- Built: `cargo build --release -p cboinc`, the test application
  (`cargo build --release --target x86_64-unknown-linux-musl -p cb-test-app`) and the Workers
  (`dev/build.sh`), all in `cloud/`.

## The clients file

One line per client, fields separated by spaces, `#` starts a comment:

```
TAG VERSION USERNO BOINC BOINCCMD LIB PORT MODE
c804 8.0.4 2 /usr/bin/boinc /usr/bin/boinccmd - 31605 copied
```

`TAG` names the client's data directory and its jobs (`<TAG>_a_0` and so on). `USERNO` is which of
the three accounts of the fixture it attaches to (1, 2 or 3). `BOINC` and `BOINCCMD` are the
binaries, `LIB` is a `LD_LIBRARY_PATH` for an extracted package (`-` for none), `PORT` is the GUI RPC
port, one for each client. `MODE` is what the switch does with the client's rows:

| Mode | Host row | Job rows (workunit, result) of the client's tasks |
|------|----------|----------------------------------------------------|
| `copied` | carried | carried |
| `notcopied` | carried | not carried |
| `nohost` | not carried | carried |

The user rows and the platform, application and application version rows are always carried, and
so is every file of the stock upload directory, unless `run.sh` gets `--no-uploads`.

## Running it

```
cloud/tests/migration-check/run.sh --work /some/empty/dir --clients FILE \
    --image NAME --cboinc cloud/target/release/cboinc \
    --app-binary cloud/target/x86_64-unknown-linux-musl/release/cb-test-app
```

`--steps` runs some of the steps, in this order: `up drive release probe switch observe down`.
`--long-seconds` is how long the two long tasks of each client take (default 240). `--cloud-config
TAG=VALUE` changes one tag of the config that cloud-boinc gets (for example
`send_result_abort=0`). A step that fails stops the run with its output kept; `--steps down`
cleans up. The scripts it calls can be used alone; each prints its flags when called without any.

1. `up` (`stock-up.sh`): makes new code-signing and upload key pairs and three accounts with random
   account keys (`prepare.py`), starts the stock server as project `proj` with the master URL
   `http://project.test/proj/`, and starts the stock transitioner, file deleter, trivial
   validator and assimilator. The second account's password hash is stored in the old form, the
   bare MD5; the other two are bcrypt, as the stock site stores them. The third account's address
   has capitals.
2. `drive` (`drive-client.sh`, one client at a time): the client is attached with its account key. Its
   first job runs to the end and is validated by the stock daemons. Then three more jobs are
   fetched with the client not computing, the two long ones are suspended, the short one runs and
   uploads, and the client's network is switched off before it can report it. The check sets
   `min_sendwork_interval` to 31 seconds (`make_project` sets 0), which makes a client wait 32
   seconds between requests, and that is the time the script has.
3. `release`: the long tasks are resumed, so one runs and the other waits for the one CPU
   each client is allowed (`<ncpus>1` in `cc_config.xml`). A client now holds a task that finished
   and uploaded but is not reported, a running one, and a not started one.
4. `probe` (`web-probe.sh`): `get_project_config.php`, `lookup_account.php` with the right password hash, a wrong one and none, `am_get_info.php` and the site's login form, as they answer on the
   stock server.
5. `switch` (`switch.sh`): the rows are copied out of the stock database (`dump-rows.sh`,
   `copy_rows.py`), the stock server is stopped, cloud-boinc starts (`cloud-up.sh`) and gets the
   stock `<config>` element, the two public keys, the files of the stock download directory and
   the upload directory (`cloud-load.sh`), and the rows. The clients' network is switched on.
6. `observe` (`observe.sh`): waits for the long tasks to end, adds new work (an application version
   1.01 signed with the stock code-signing key, and jobs), asks for it and waits until the database shows every result of the new work received (an
   empty task list on a client does not count, and a wait that runs out is reported and fails the
   step), detaches and attaches every client again, and repeats the web probe on cloud-boinc.
7. `down`: stops the clients, cloud-boinc and the stock server.

## What it leaves in the work directory

`results/` has the logs of each client since the switch, the web probe of both servers, and the
tables of cloud-boinc's database. `captures/<TAG>/` has every scheduler request and reply, as the
clients wrote them. `clients/<TAG>/client.log` has the whole log with the HTTP debug flags on.
These hold the accounts' keys and the hosts' CPIDs; `secrets.json` in `fixtures/` holds the
passwords. All of it is for the person who ran the check and is not to be published. The key
pairs are made for the run and used nowhere else.

## The captured requests

`captures/<version>/first-request-after-switch.xml` is the first request each client version sent
after the switch (`captures/<TAG>/002_request.xml` of a run), scrubbed with
`cloud/dev/scrub-sched-request.py` as `cloud/tests/client-corpus/README.md` describes:

```
cloud/dev/scrub-sched-request.py --input RUN/captures/c804/002_request.xml \
    --output cloud/tests/migration-check/captures/8.0.4/first-request-after-switch.xml \
    --forbid cb-mig-c804 --forbid project.test
```

`cargo test -p cb-core --test migration` replays them through `handle_request` in databases that have
the stock rows or part of them. It needs no stock server, no client and no Worker.

## What the scripts do not do

They are not an importer (issue #17). `copy_rows.py` is the smallest row copy that lets the check
run; it prints, for each table, how many columns it carried and which ones cloud-boinc fills with
defaults and which it drops. It copies from a running stock database through its socket, one project,
with the conversions described in its header.
