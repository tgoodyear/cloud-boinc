-- Leaderboards, the jobs overview and the activity page.

-- Valid tasks of each account and each computer: "jobs completed" on the
-- leaderboards. The pipeline adds to it in the statement that grants the credit.
-- Tasks purged before this migration are not counted by the backfill below.
ALTER TABLE user ADD COLUMN valid_results INTEGER NOT NULL DEFAULT 0;
ALTER TABLE host ADD COLUMN valid_results INTEGER NOT NULL DEFAULT 0;

-- A sort key for "recent average credit" that compares across rows without
-- decaying each one: `ln(expavg_credit) + expavg_time * ln 2 / half_life`, with
-- the half life of a week (`CREDIT_HALF_LIFE`, `recent_key` in `avg.rs`). An average
-- that halves every week halves its value at every time by the same factor
-- whatever time the comparison is made, so this orders rows as their averages
-- stand now. The pipeline writes it wherever it writes `expavg_credit`.
ALTER TABLE user ADD COLUMN recent_key REAL NOT NULL DEFAULT -1e9;
ALTER TABLE host ADD COLUMN recent_key REAL NOT NULL DEFAULT -1e9;
UPDATE user SET recent_key = ln(expavg_credit) + expavg_time * 0.6931471805599453 / 604800.0
    WHERE total_credit > 0 AND expavg_credit > 0 AND expavg_time > 0;
UPDATE host SET recent_key = ln(expavg_credit) + expavg_time * 0.6931471805599453 / 604800.0
    WHERE total_credit > 0 AND expavg_credit > 0 AND expavg_time > 0;

-- One scan of the tasks of accounts and computers that have credit, once. The
-- extra `> 0` terms let the partial indexes `res_user` and `res_host` serve the
-- subqueries.
UPDATE user SET valid_results = (
    SELECT count(*) FROM result WHERE userid > 0 AND userid = user.id AND validate_state = 1
) WHERE total_credit > 0;
UPDATE host SET valid_results = (
    SELECT count(*) FROM result WHERE hostid > 0 AND hostid = host.id AND validate_state = 1
) WHERE total_credit > 0;

-- The three leaderboard orders (total credit, recent average by `recent_key`, valid
-- tasks), for accounts and for computers. Partial, so
-- that an account or computer enters them with its first credit; an account
-- leaves the account ones when it is blocked, since the page leaves it out and
-- a run of blocked accounts would otherwise have to be read past. Each page
-- reads the first rows of one index.
CREATE INDEX user_total_credit ON user (total_credit DESC) WHERE total_credit > 0 AND blocked = 0;
CREATE INDEX user_recent_key ON user (recent_key DESC) WHERE total_credit > 0 AND blocked = 0;
CREATE INDEX user_valid_results ON user (valid_results DESC) WHERE total_credit > 0 AND blocked = 0;
CREATE INDEX host_total_credit ON host (total_credit DESC) WHERE total_credit > 0;
CREATE INDEX host_recent_key ON host (recent_key DESC) WHERE total_credit > 0;
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
-- `appid` 0 for the whole project and -1 for the applications that are not
-- tracked one by one (the first 18 by id are, deprecated or not, so that the set does not change). `returned_ok` counts tasks received with a
-- success outcome, `valid` and `invalid` those of them the validator has judged,
-- `failed` tasks received with any other outcome, `timed_out` tasks written off
-- at their deadline (counted by the transition), `queued` the tasks ready to send
-- at the last count in the period (-1 when no count was made in it), and `users` and `hosts` (project row only) the
-- accounts and computers that returned a task. `computed_at` is when the
-- recount of the period last ran, 0 for a row a timeout made before its period
-- was counted (the pages treat such a period as not counted). See `cloud/crates/cb-core/src/stats.rs`.
CREATE TABLE stats_bucket (
    appid INTEGER NOT NULL,
    bucket INTEGER NOT NULL,
    returned_ok INTEGER NOT NULL DEFAULT 0,
    valid INTEGER NOT NULL DEFAULT 0,
    invalid INTEGER NOT NULL DEFAULT 0,
    failed INTEGER NOT NULL DEFAULT 0,
    timed_out INTEGER NOT NULL DEFAULT 0,
    queued INTEGER NOT NULL DEFAULT -1,
    -- 1 when `queued` is a lower bound, because the count stopped at its cap
    queued_capped INTEGER NOT NULL DEFAULT 0,
    users INTEGER NOT NULL DEFAULT 0,
    hosts INTEGER NOT NULL DEFAULT 0,
    -- project row only: tasks returned with a success outcome and not yet
    -- judged by the validator when the period was last counted
    pending INTEGER NOT NULL DEFAULT 0,
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
