-- Workunits whose files are deleted (file_delete_state 2), oldest change first:
-- what the purge pass reads.
CREATE INDEX wu_purge ON workunit (mod_time) WHERE file_delete_state = 2;
