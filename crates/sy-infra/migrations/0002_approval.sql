-- Migration 2 (spec 0003): approvals. Additive: one new table, no rebuild.
CREATE TABLE approval (
  id             TEXT    PRIMARY KEY,
  run_id         TEXT    NOT NULL REFERENCES run (id) ON DELETE CASCADE,
  kind           TEXT    NOT NULL,
  summary        TEXT    NOT NULL,
  detail         TEXT    NOT NULL CHECK (json_valid(detail)),
  status         TEXT    NOT NULL CHECK (status IN ('pending', 'approved', 'denied', 'expired', 'cancelled')),
  requested_at   INTEGER NOT NULL,
  decided_at     INTEGER,
  decided_by     TEXT    CHECK (decided_by IN ('user', 'policy', 'agent')),
  decided_by_ref TEXT,
  response       TEXT    CHECK (json_valid(response))
) STRICT;
CREATE INDEX ix_approval_pending ON approval (requested_at) WHERE status = 'pending';
CREATE INDEX ix_approval_run ON approval (run_id, requested_at, id);
