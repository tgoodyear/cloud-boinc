-- The attempt number of an upload file: 0 for a file whose task has been sent
-- once, one more for each time the scheduler sent it again after the client
-- lost it (a resend runs the task again under the same file names). A row is
-- written when a lost result is resent, one for each of its upload names, and
-- deleted with the result. Pieces of an unfinished upload are stored under
-- their epoch, and only the current one counts.
CREATE TABLE upload_epoch (
    name TEXT NOT NULL PRIMARY KEY,
    result_id INTEGER NOT NULL,
    epoch INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX upload_epoch_result ON upload_epoch (result_id);
