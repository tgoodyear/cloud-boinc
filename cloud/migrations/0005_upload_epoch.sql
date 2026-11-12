-- Where an upload file's current attempt starts, for the files a lost result
-- was sent again with: the size query ignores pieces stored up to then. A
-- row is written when a lost result is resent, one for each of its upload
-- names, and deleted with the result. Names no row names have had one
-- attempt.
CREATE TABLE upload_epoch (
    name TEXT NOT NULL PRIMARY KEY,
    result_id INTEGER NOT NULL,
    since INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX upload_epoch_result ON upload_epoch (result_id);
