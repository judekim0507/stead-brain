CREATE TABLE IF NOT EXISTS endpoints (
  endpoint_id TEXT PRIMARY KEY,
  owner_agent_id TEXT NOT NULL,
  token_hash TEXT NOT NULL UNIQUE,
  label TEXT NOT NULL,
  mode TEXT NOT NULL,
  created_at TEXT NOT NULL,
  revoked_at TEXT,
  last_used_at TEXT,
  rl_window_start TEXT,
  rl_count INTEGER NOT NULL DEFAULT 0,
  FOREIGN KEY (owner_agent_id) REFERENCES agents(agent_id)
);

CREATE INDEX IF NOT EXISTS idx_endpoints_owner ON endpoints(owner_agent_id, revoked_at);
