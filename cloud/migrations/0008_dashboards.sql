-- Leaderboards, the jobs overview and the activity page.

-- Valid tasks of each account and each computer: "jobs completed" on the
-- leaderboards. The pipeline adds to it in the statement that grants the credit.
-- Tasks purged before this migration are not counted by the backfill below.
ALTER TABLE user ADD COLUMN valid_results INTEGER NOT NULL DEFAULT 0;
ALTER TABLE host ADD COLUMN valid_results INTEGER NOT NULL DEFAULT 0;

-- One scan of the tasks of accounts and computers that have credit, once. The
-- extra `> 0` terms let the partial indexes `res_user` and `res_host` serve the
-- subqueries.
UPDATE user SET valid_results = (
    SELECT count(*) FROM result WHERE userid > 0 AND userid = user.id AND validate_state = 1
) WHERE total_credit > 0;
UPDATE host SET valid_results = (
    SELECT count(*) FROM result WHERE hostid > 0 AND hostid = host.id AND validate_state = 1
) WHERE total_credit > 0;

-- The three leaderboard orders, for accounts and for computers. Partial, so
-- that an account or computer enters them with its first credit. Each page
-- reads the first rows of one index.
CREATE INDEX user_total_credit ON user (total_credit DESC) WHERE total_credit > 0;
CREATE INDEX user_expavg_credit ON user (expavg_credit DESC) WHERE total_credit > 0;
CREATE INDEX user_valid_results ON user (valid_results DESC) WHERE total_credit > 0;
CREATE INDEX host_total_credit ON host (total_credit DESC) WHERE total_credit > 0;
CREATE INDEX host_expavg_credit ON host (expavg_credit DESC) WHERE total_credit > 0;
CREATE INDEX host_valid_results ON host (valid_results DESC) WHERE total_credit > 0;

-- Tasks in progress, by computer and application. With the application in the
-- index, "in progress per application" and "per computer and application" are
-- answered from the index alone. The lookups by `hostid` that used the old
-- index use this one.
DROP INDEX res_host_inprog;
CREATE INDEX res_host_inprog ON result (hostid, appid) WHERE server_state = 4;

-- Tasks by time received, for the rollup's recount of the open periods. Also
-- created by the per-application server status; either migration may come
-- first. Tasks not yet returned have `received_time` 0 and are not in it.
CREATE INDEX IF NOT EXISTS res_received ON result (received_time) WHERE received_time > 0;

-- Workunits waiting for the validator. The state is normally set and cleared
-- within one transition, so this holds almost nothing. Also created by the
-- per-application server status.
CREATE INDEX IF NOT EXISTS wu_need_validate ON workunit (id) WHERE need_validate = 1;

-- The rollup behind the history charts and the jobs overview: one row per
-- three-hour period (`bucket` is its start, Unix seconds) and application, and
-- `appid` 0 for the whole project. `returned_ok` counts tasks received with a
-- success outcome, `valid` and `invalid` those of them the validator has judged,
-- `failed` tasks received with any other outcome, `timed_out` tasks written off
-- at their deadline (counted by the transition), `queued` the tasks ready to send
-- at the last count in the period (-1 when no count was made in it), and `users` and `hosts` (project row only) the
-- accounts and computers that returned a task. `computed_at` is when the
-- recount of the period last ran. See `cloud/crates/cb-core/src/stats.rs`.
CREATE TABLE stats_bucket (
    appid INTEGER NOT NULL,
    bucket INTEGER NOT NULL,
    returned_ok INTEGER NOT NULL DEFAULT 0,
    valid INTEGER NOT NULL DEFAULT 0,
    invalid INTEGER NOT NULL DEFAULT 0,
    failed INTEGER NOT NULL DEFAULT 0,
    timed_out INTEGER NOT NULL DEFAULT 0,
    queued INTEGER NOT NULL DEFAULT -1,
    users INTEGER NOT NULL DEFAULT 0,
    hosts INTEGER NOT NULL DEFAULT 0,
    computed_at INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (appid, bucket)
) WITHOUT ROWID;

-- Documents the rollup writes for the pages to read in one row: the per
-- application figures of the jobs overview (`overview`), the tasks in progress
-- by computer (`inflight`), and when the rollup last ran (`rollup_run`, `at`).
CREATE TABLE stats_snapshot (
    name TEXT PRIMARY KEY,
    at INTEGER NOT NULL,
    body TEXT NOT NULL
) WITHOUT ROWID;
