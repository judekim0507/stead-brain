CREATE TABLE IF NOT EXISTS auth_exchanges (
  exchange_request_id TEXT PRIMARY KEY,
  client_kind TEXT NOT NULL,
  client_version TEXT NOT NULL,
  loopback_redirect_uri TEXT NOT NULL,
  code_challenge TEXT NOT NULL,
  state_hash TEXT NOT NULL,
  created_at TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  code_hash TEXT,
  code_issued_at TEXT,
  user_id TEXT,
  used_at TEXT,
  FOREIGN KEY (user_id) REFERENCES users(user_id)
);

CREATE INDEX IF NOT EXISTS idx_auth_exchanges_expires ON auth_exchanges(expires_at, used_at);
