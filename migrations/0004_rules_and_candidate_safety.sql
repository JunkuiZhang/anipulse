CREATE TABLE IF NOT EXISTS blocked_keyword (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    keyword TEXT NOT NULL,
    normalized_keyword TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Older versions left unselected candidates pending after an episode was delivered.
UPDATE candidate
SET state = 'expired'
WHERE state = 'pending'
  AND episode_id IN (
    SELECT id FROM episode WHERE state IN ('confirmed', 'notified')
  );

UPDATE review_notification
SET status = 'cancelled'
WHERE status = 'pending'
  AND episode_id IN (
    SELECT id FROM episode WHERE state IN ('confirmed', 'notified')
  );
