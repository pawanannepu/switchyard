-- Migration 3 (spec 0004): the processes each run started, so a restarted
-- engine can stop what a crashed one left behind. Additive: one new table.
-- Rows exist only while their process might still be alive. `name` is the
-- short executable name, never arguments or environment.
CREATE TABLE run_process (
  run_id        TEXT    NOT NULL REFERENCES run (id) ON DELETE CASCADE,
  pid           INTEGER NOT NULL,
  started_at    INTEGER NOT NULL,
  name          TEXT    NOT NULL,
  first_seen_at INTEGER NOT NULL,
  PRIMARY KEY (run_id, pid, started_at)
) STRICT;
