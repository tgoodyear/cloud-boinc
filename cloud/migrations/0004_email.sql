-- Email sent by the site: password recovery, address verification, and notices
-- of an address change.

-- Upstream's `previous_email_addr` and `email_addr_change_time`: the address
-- before the last change (so that it can undo the change) and when the change
-- was made (the next change must wait a week).
ALTER TABLE user ADD COLUMN previous_email_addr TEXT NOT NULL DEFAULT '';
ALTER TABLE user ADD COLUMN email_addr_change_time INTEGER NOT NULL DEFAULT 0;

-- Links sent by email. Upstream's `token` table, except that `token_hash` is
-- the SHA-256 of the token, not the token, and a used token stays as a row
-- (`used_time`) so that it still counts toward the limit on mail per account.
-- `type`: 'L' login link, 'V' address verification, 'E' undo an address change.
-- `data`: the address a 'V' token was issued for, or the previous address for
-- an 'E' token; the link works only while the account still has that address.
CREATE TABLE token (
    id INTEGER PRIMARY KEY,
    userid INTEGER NOT NULL,
    type TEXT NOT NULL,
    token_hash TEXT NOT NULL,
    data TEXT NOT NULL DEFAULT '',
    create_time INTEGER NOT NULL,
    expire_time INTEGER NOT NULL,
    used_time INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX token_hash ON token (token_hash);
CREATE INDEX token_user ON token (userid, type, create_time);
CREATE INDEX token_created ON token (create_time);

-- An address an account has changed away from stays reserved for it while its
-- undo link works (a week): sign-up and address changes refuse it. The query
-- repeats `previous_email_addr != ''` so that it can use this index.
CREATE INDEX user_previous_email ON user (previous_email_addr) WHERE previous_email_addr != '';

-- Messages sent per UTC day (`day` is the Unix time divided by 86400), for the
-- project-wide cap `email_max_per_day`. `signup` counts the part of `n` that is
-- verification mail sent at sign-up, which may use half of the cap.
CREATE TABLE mail_day (
    day INTEGER PRIMARY KEY,
    n INTEGER NOT NULL,
    signup INTEGER NOT NULL DEFAULT 0
);
