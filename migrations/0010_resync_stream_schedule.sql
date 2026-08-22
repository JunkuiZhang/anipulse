-- Source selection now compares all trusted stream schedules instead of taking
-- the first configured fallback. Reconcile every active automatic schedule once.
UPDATE anime
SET schedule_next_sync_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
WHERE auto_schedule = 1 AND lifecycle = 'tracking';
