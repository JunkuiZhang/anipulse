-- U-NEXT timestamps have proven unreliable for release monitoring. Existing
-- configurations inherit the new exclusion and affected active schedules are
-- refreshed as soon as the upgraded service starts.
UPDATE episode
SET expected_at = NULL,
    next_check_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
WHERE anime_id IN (
    SELECT id FROM anime
    WHERE auto_schedule = 1
      AND lifecycle = 'tracking'
      AND schedule_source = 'unext'
)
AND state IN ('waiting', 'watching', 'candidate_found', 'needs_manual_review');

UPDATE anime
SET schedule_next_sync_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
    schedule_source = NULL,
    schedule_confidence = 'unavailable',
    schedule_warning = 'U-NEXT 排期已停用，等待使用其他来源重新同步。'
WHERE auto_schedule = 1
  AND lifecycle = 'tracking'
  AND schedule_source = 'unext';
