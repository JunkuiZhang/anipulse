CREATE TABLE IF NOT EXISTS web_admin (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    role TEXT NOT NULL DEFAULT 'owner' CHECK(role = 'owner'),
    disabled INTEGER NOT NULL DEFAULT 0 CHECK(disabled IN (0, 1)),
    created_at TEXT NOT NULL,
    password_changed_at TEXT NOT NULL,
    last_login_at TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_web_admin_single_owner
ON web_admin((1));

CREATE TABLE IF NOT EXISTS web_session (
    token_hmac BLOB PRIMARY KEY,
    admin_id INTEGER NOT NULL REFERENCES web_admin(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL,
    renewed_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    idle_expires_at TEXT NOT NULL,
    absolute_expires_at TEXT NOT NULL,
    user_agent_hash BLOB,
    source_ip_hash BLOB,
    revoked_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_web_session_admin
ON web_session(admin_id, revoked_at);

CREATE INDEX IF NOT EXISTS idx_web_session_expiry
ON web_session(idle_expires_at, absolute_expires_at, revoked_at);

CREATE TABLE IF NOT EXISTS auth_throttle (
    key_hmac BLOB PRIMARY KEY,
    window_started_at TEXT NOT NULL,
    failure_count INTEGER NOT NULL DEFAULT 0 CHECK(failure_count >= 0),
    blocked_until TEXT,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_auth_throttle_updated
ON auth_throttle(updated_at);

CREATE TABLE IF NOT EXISTS audit_event (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    actor_type TEXT NOT NULL CHECK(actor_type IN ('admin', 'cli', 'scheduler', 'system')),
    actor_admin_id INTEGER REFERENCES web_admin(id) ON DELETE SET NULL,
    action TEXT NOT NULL,
    entity_type TEXT,
    entity_id TEXT,
    outcome TEXT NOT NULL CHECK(outcome IN ('success', 'failure', 'denied')),
    request_id TEXT,
    source_ip_hash BLOB,
    metadata_json TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_audit_event_created
ON audit_event(created_at DESC, id DESC);

CREATE TABLE IF NOT EXISTS management_job (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL CHECK(kind IN (
        'check_anime', 'sync_schedule', 'notification_test', 'accept_bilibili_url',
        'resolve_anime_draft'
    )),
    target_type TEXT,
    target_id TEXT,
    payload_json TEXT NOT NULL DEFAULT '{}',
    state TEXT NOT NULL DEFAULT 'queued' CHECK(state IN (
        'queued', 'running', 'completed', 'failed'
    )),
    requested_by INTEGER REFERENCES web_admin(id) ON DELETE SET NULL,
    dedupe_key TEXT,
    attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts >= 0),
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    error TEXT
);

CREATE INDEX IF NOT EXISTS idx_management_job_queue
ON management_job(state, created_at, id);

CREATE UNIQUE INDEX IF NOT EXISTS idx_management_job_active_dedupe
ON management_job(dedupe_key)
WHERE dedupe_key IS NOT NULL AND state IN ('queued', 'running');

CREATE TABLE IF NOT EXISTS web_action_nonce (
    nonce_hmac BLOB PRIMARY KEY,
    admin_id INTEGER NOT NULL REFERENCES web_admin(id) ON DELETE CASCADE,
    action TEXT NOT NULL,
    entity_id TEXT,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    consumed_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_web_action_nonce_expiry
ON web_action_nonce(expires_at, consumed_at);

CREATE TABLE IF NOT EXISTS anime_draft (
    id TEXT PRIMARY KEY,
    admin_id INTEGER NOT NULL REFERENCES web_admin(id) ON DELETE CASCADE,
    session_token_hmac BLOB NOT NULL,
    payload_json TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'queued' CHECK(state IN ('queued', 'ready', 'failed')),
    resolved_json TEXT,
    error TEXT,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    consumed_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_anime_draft_expiry
ON anime_draft(expires_at, consumed_at);

CREATE TABLE IF NOT EXISTS scheduler_state (
    name TEXT PRIMARY KEY,
    heartbeat_at TEXT NOT NULL,
    metadata_json TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS review_notification (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    episode_id INTEGER NOT NULL REFERENCES episode(id) ON DELETE CASCADE,
    channel TEXT NOT NULL,
    candidate_fingerprint TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending', 'sent', 'cancelled')),
    attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts >= 0),
    last_error TEXT,
    next_attempt_at TEXT,
    created_at TEXT NOT NULL,
    sent_at TEXT,
    UNIQUE(episode_id, channel, candidate_fingerprint)
);

CREATE INDEX IF NOT EXISTS idx_review_notification_pending
ON review_notification(status, next_attempt_at, created_at);
