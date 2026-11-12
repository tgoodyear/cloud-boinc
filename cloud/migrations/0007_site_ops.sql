-- Account deletion in progress, and the indexes behind the per-application
-- server status and the operator summary.

-- One row for each account whose deletion has started: its login was ended and
-- its personal fields cleared by one batch, and the rows that refer to it (the
-- computers and tasks) are dealt with in bounded steps until none are left
-- (`cloud/crates/cb-core/src/site_delete.rs`). `mode` is 1 (obfuscate) or 2
-- (wipe), the value of `enable_delete_account` when it started. `gen` is the
-- value the account's key and address were set to (`deleted_<time>_<random>`),
-- which no other account or run of the deletion has: every statement that
-- deletes or changes something for the deletion checks it, with the account's
-- `blocked` state, so a pass that read the row before it was finished does
-- nothing afterwards.
CREATE TABLE IF NOT EXISTS user_deletion (
    userid INTEGER PRIMARY KEY,
    mode INTEGER NOT NULL,
    create_time INTEGER NOT NULL,
    gen TEXT NOT NULL DEFAULT '',
    -- The ID of the last computer an obfuscation has handled: it walks the
    -- account's computers in ID order, 100 at a time, so a pass reads only the
    -- ones after it.
    host_cursor INTEGER NOT NULL DEFAULT 0
);

-- IDs of users and computers are never given out twice, as on a stock server
-- (MySQL `AUTO_INCREMENT`). `id INTEGER PRIMARY KEY` without `AUTOINCREMENT`
-- gives a new row the largest ID plus one, which is a deleted row's ID when it
-- was the largest. `last_id` is the largest ID a table has had deleted; the
-- statements that insert a user or a computer take
-- `max(max(id), last_id) + 1` (`next_id_sql` in `cloud/crates/cb-core/src/web.rs`),
-- and these triggers record every deletion, whoever makes it.
CREATE TABLE IF NOT EXISTS id_floor (
    name TEXT PRIMARY KEY,
    last_id INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS user_id_floor AFTER DELETE ON user
BEGIN
    INSERT INTO id_floor (name, last_id) VALUES ('user', old.id)
    ON CONFLICT (name) DO UPDATE SET last_id = max(last_id, excluded.last_id);
END;
CREATE TRIGGER IF NOT EXISTS host_id_floor AFTER DELETE ON host
BEGIN
    INSERT INTO id_floor (name, last_id) VALUES ('host', old.id)
    ON CONFLICT (name) DO UPDATE SET last_id = max(last_id, excluded.last_id);
END;

-- Tasks the server has received, by time received: the tasks returned in the
-- last 24 hours, for `server_status.php` and the operator summary. Tasks not
-- yet returned have `received_time` 0 and are not in it.
CREATE INDEX IF NOT EXISTS res_received ON result (received_time) WHERE received_time > 0;

-- Workunits waiting for the validator. The state is normally set and cleared
-- within one transition, so this holds almost nothing.
CREATE INDEX IF NOT EXISTS wu_need_validate ON workunit (id) WHERE need_validate = 1;

-- Batches an account submitted, for giving them no owner when it is deleted.
CREATE INDEX IF NOT EXISTS batch_user ON batch (user_id) WHERE user_id > 0;
