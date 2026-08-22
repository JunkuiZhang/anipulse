ALTER TABLE anime ADD COLUMN schedule_source TEXT;
ALTER TABLE anime ADD COLUMN schedule_confidence TEXT
    CHECK(schedule_confidence IS NULL OR schedule_confidence IN (
        'calibrated', 'stale', 'estimated', 'unavailable'
    ));
ALTER TABLE anime ADD COLUMN schedule_warning TEXT;

-- Reconcile every existing automatic schedule once after the upgraded binary starts.
-- Existing expected_at remains available until that refresh succeeds.
UPDATE anime
SET schedule_next_sync_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
WHERE auto_schedule = 1 AND lifecycle = 'tracking';
