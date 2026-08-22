CREATE TABLE IF NOT EXISTS source_health (
    source TEXT PRIMARY KEY,
    consecutive_failures INTEGER NOT NULL DEFAULT 0 CHECK(consecutive_failures >= 0),
    first_failed_at TEXT,
    last_checked_at TEXT NOT NULL,
    last_error TEXT,
    alert_attempts INTEGER NOT NULL DEFAULT 0 CHECK(alert_attempts >= 0),
    next_alert_attempt_at TEXT,
    alert_state TEXT NOT NULL DEFAULT 'none' CHECK(alert_state IN (
        'none', 'failure_pending', 'failure_sent', 'recovery_pending'
    ))
);

CREATE INDEX IF NOT EXISTS idx_source_health_pending_alert
ON source_health(alert_state, next_alert_attempt_at, last_checked_at);
