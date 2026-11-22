-- Migration 1, the spine (spec 0002, "Data model sketch").
-- Ids are TEXT `<prefix>_<uuidv7 hex>`; every *_at is INTEGER Unix ms UTC.
-- Unique indexes carry the names the store maps constraint errors by.
-- Additive only from here: later migrations never rename, drop or repurpose a column.

CREATE TABLE project (
  id                TEXT    PRIMARY KEY,
  name              TEXT    NOT NULL,
  repo_path         TEXT    NOT NULL,
  base_branch       TEXT    NOT NULL,
  settings_revision INTEGER NOT NULL DEFAULT 0,
  created_at        INTEGER NOT NULL,
  archived_at       INTEGER
) STRICT;
CREATE UNIQUE INDEX uq_project_repo_path ON project (repo_path);

CREATE TABLE project_setting (
  project_id TEXT    NOT NULL REFERENCES project (id) ON DELETE CASCADE,
  key        TEXT    NOT NULL,
  value      TEXT    NOT NULL CHECK (json_valid(value)),
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (project_id, key)
) STRICT;

CREATE TABLE setting (
  key        TEXT    PRIMARY KEY,
  value      TEXT    NOT NULL CHECK (json_valid(value)),
  updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE task (
  id             TEXT    PRIMARY KEY,
  project_id     TEXT    NOT NULL REFERENCES project (id) ON DELETE CASCADE,
  title          TEXT    NOT NULL,
  prompt         TEXT    NOT NULL,
  base_ref       TEXT    NOT NULL,
  parent_task_id TEXT    REFERENCES task (id) ON DELETE SET NULL,
  created_at     INTEGER NOT NULL,
  archived_at    INTEGER,
  UNIQUE (id, project_id)
) STRICT;
CREATE INDEX ix_task_project ON task (project_id, archived_at, created_at);
CREATE INDEX ix_task_parent ON task (parent_task_id);

CREATE TABLE workspace (
  id            TEXT    PRIMARY KEY,
  task_id       TEXT    NOT NULL,
  project_id    TEXT    NOT NULL,
  agent_id      TEXT    NOT NULL,
  agent_options TEXT    NOT NULL CHECK (json_valid(agent_options)),
  branch        TEXT    NOT NULL,
  worktree_path TEXT    NOT NULL,
  base_commit   TEXT    NOT NULL,
  state         TEXT    NOT NULL CHECK (state IN ('preparing', 'ready', 'merged', 'discarded')),
  outcome       TEXT    CHECK (outcome IN ('winner', 'not_chosen')),
  merged_commit TEXT,
  created_at    INTEGER NOT NULL,
  closed_at     INTEGER,
  FOREIGN KEY (task_id, project_id) REFERENCES task (id, project_id) ON DELETE CASCADE
) STRICT;
CREATE UNIQUE INDEX uq_workspace_worktree_path ON workspace (worktree_path);
CREATE UNIQUE INDEX uq_workspace_branch ON workspace (project_id, branch) WHERE state <> 'discarded';
CREATE UNIQUE INDEX uq_workspace_winner ON workspace (task_id) WHERE outcome = 'winner';
CREATE INDEX ix_workspace_task ON workspace (task_id, project_id);

CREATE TABLE run (
  id                TEXT    PRIMARY KEY,
  workspace_id      TEXT    NOT NULL REFERENCES workspace (id) ON DELETE CASCADE,
  kind              TEXT    NOT NULL CHECK (kind IN ('agent', 'setup', 'run_script')),
  input             TEXT,
  resumes_run_id    TEXT    REFERENCES run (id) ON DELETE SET NULL,
  agent_session_ref TEXT,
  status            TEXT    NOT NULL CHECK (status IN ('queued', 'starting', 'running', 'waiting_input', 'finished', 'failed', 'stopped', 'lost')),
  exit_code         INTEGER,
  end_reason        TEXT,
  pid               INTEGER,
  pgid              INTEGER,
  pid_started_at    INTEGER,
  queued_at         INTEGER NOT NULL,
  started_at        INTEGER,
  ended_at          INTEGER,
  log_bytes         INTEGER NOT NULL DEFAULT 0 CHECK (log_bytes >= 0),
  log_state         TEXT    NOT NULL DEFAULT 'present' CHECK (log_state IN ('present', 'incomplete', 'trimmed', 'deleted'))
) STRICT;
CREATE UNIQUE INDEX uq_run_live_agent ON run (workspace_id)
  WHERE kind = 'agent' AND status IN ('queued', 'starting', 'running', 'waiting_input');
CREATE INDEX ix_run_workspace ON run (workspace_id, queued_at);
CREATE INDEX ix_run_status ON run (status);
CREATE INDEX ix_run_resumes ON run (resumes_run_id);

CREATE TABLE event (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id   TEXT    REFERENCES project (id) ON DELETE CASCADE,
  task_id      TEXT    REFERENCES task (id) ON DELETE CASCADE,
  workspace_id TEXT    REFERENCES workspace (id) ON DELETE CASCADE,
  run_id       TEXT    REFERENCES run (id) ON DELETE CASCADE,
  at           INTEGER NOT NULL,
  actor        TEXT    NOT NULL CHECK (actor IN ('user', 'engine', 'policy', 'agent')),
  actor_ref    TEXT,
  kind         TEXT    NOT NULL,
  payload      TEXT    NOT NULL CHECK (json_valid(payload))
) STRICT;
CREATE INDEX ix_event_project ON event (project_id, id);
CREATE INDEX ix_event_task ON event (task_id, id);
CREATE INDEX ix_event_workspace ON event (workspace_id, id);
CREATE INDEX ix_event_run ON event (run_id, id);
