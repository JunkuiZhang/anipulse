ALTER TABLE anime ADD COLUMN auto_schedule INTEGER NOT NULL DEFAULT 0;
ALTER TABLE anime ADD COLUMN broadcast_pattern TEXT;
ALTER TABLE anime ADD COLUMN schedule_sync_at TEXT;
ALTER TABLE anime ADD COLUMN schedule_next_sync_at TEXT;
ALTER TABLE anime ADD COLUMN schedule_sync_error TEXT;

CREATE INDEX IF NOT EXISTS idx_anime_schedule_due
ON anime(auto_schedule, enabled, schedule_next_sync_at);
