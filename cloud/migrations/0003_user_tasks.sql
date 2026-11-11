-- Tasks of one account, newest first, for results.php?userid=. Results not
-- yet sent (userid 0) stay out of the index.
CREATE INDEX res_user ON result (userid, id) WHERE userid > 0;
