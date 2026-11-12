-- Teams: the `team` table of db/schema.sql with the columns the site pages and
-- web RPCs use (`cloud/crates/cb-core/src/teams.rs`, `site_teams.rs`).
--
-- `user.teamid` already exists; 0 means no team.
CREATE TABLE IF NOT EXISTS team (
    id INTEGER PRIMARY KEY,
    create_time INTEGER NOT NULL,
    -- The founder's account. 0 when that account was deleted.
    userid INTEGER NOT NULL,
    name TEXT NOT NULL,
    -- `name` lower-cased. The name a team is looked up and refused by, so two
    -- names that differ only in case are one name, as on a stock server.
    name_lc TEXT NOT NULL,
    url TEXT NOT NULL DEFAULT '',
    -- A number from `cb_core::teams::TEAM_TYPES`; 0 is none.
    type INTEGER NOT NULL DEFAULT 0,
    -- Plain text. Upstream's HTML description (`team.description`) and HTML
    -- name (`team.name_html`) are not taken.
    description TEXT NOT NULL DEFAULT '',
    -- The number of accounts whose `teamid` is this team. Kept by the triggers
    -- below; upstream recomputes it daily in `sched/update_stats.cpp`.
    nusers INTEGER NOT NULL DEFAULT 0,
    country TEXT NOT NULL DEFAULT '',
    -- Credit the team's members earned while members, grown in the batch that
    -- grants the credit (`host_statements` in `cloud/crates/cb-core/src/pipeline.rs`).
    total_credit REAL NOT NULL DEFAULT 0,
    expavg_credit REAL NOT NULL DEFAULT 0,
    expavg_time REAL NOT NULL DEFAULT 0,
    -- 1 when the team takes new members.
    joinable INTEGER NOT NULL DEFAULT 1,
    -- Sort key of the recent average, as `user.recent_key`
    -- (`recent_key` in `cloud/crates/cb-core/src/avg.rs`).
    recent_key REAL NOT NULL DEFAULT -1e9
);

CREATE UNIQUE INDEX IF NOT EXISTS team_name_lc ON team (name_lc);
-- The two orders of `top_teams.php`, and of the team search.
CREATE INDEX IF NOT EXISTS team_total_credit ON team (total_credit DESC);
CREATE INDEX IF NOT EXISTS team_recent_key ON team (recent_key DESC);
-- Teams an account founded, for its account page.
CREATE INDEX IF NOT EXISTS team_founder ON team (userid);
-- The members of a team. Partial: an account with no team is not in it, and
-- a credit grant does not touch it. A query that uses it repeats the literal
-- `teamid > 0`, which a bound `teamid = ?` does not imply for the planner.
CREATE INDEX IF NOT EXISTS user_team ON user (teamid) WHERE teamid > 0;

-- Team IDs are not given out twice either (see `id_floor` in 0007).
CREATE TRIGGER IF NOT EXISTS team_id_floor AFTER DELETE ON team
BEGIN
    INSERT INTO id_floor (name, last_id) VALUES ('team', old.id)
    ON CONFLICT (name) DO UPDATE SET last_id = max(last_id, excluded.last_id);
END;

-- `team.nusers` follows `user.teamid` whoever changes it: a member joining,
-- quitting or being removed, and an account deletion (`start_deletion` in
-- `cloud/crates/cb-core/src/site_delete.rs` sets it to 0). `UPDATE OF` fires
-- when the column is named, so the `WHEN` skips a write of the same value.
CREATE TRIGGER IF NOT EXISTS user_team_changed AFTER UPDATE OF teamid ON user
WHEN old.teamid != new.teamid
BEGIN
    UPDATE team SET nusers = max(nusers - 1, 0) WHERE id = old.teamid AND old.teamid > 0;
    UPDATE team SET nusers = nusers + 1 WHERE id = new.teamid AND new.teamid > 0;
END;
CREATE TRIGGER IF NOT EXISTS user_team_deleted AFTER DELETE ON user
WHEN old.teamid > 0
BEGIN
    UPDATE team SET nusers = max(nusers - 1, 0) WHERE id = old.teamid;
END;
