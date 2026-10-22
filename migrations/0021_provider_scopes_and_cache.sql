-- Keep pre-upgrade global cooldowns intact: their original cause is unknown.
ALTER TABLE provider_state ADD COLUMN next_request_at TEXT;
ALTER TABLE provider_state ADD COLUMN probe_until TEXT;

INSERT OR IGNORE INTO provider_state(provider, updated_at) VALUES
    ('bilibili:search', strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    ('bilibili:detail', strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    ('bilibili:device', strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    ('bilibili:session', strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    ('bilibili:followers', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));

CREATE TABLE provider_cache (
    cache_key TEXT PRIMARY KEY,
    payload_json TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX idx_provider_cache_expiry ON provider_cache(expires_at);
