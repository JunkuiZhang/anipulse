ALTER TABLE episode ADD COLUMN watched_at TEXT;
ALTER TABLE episode_video ADD COLUMN published_at TEXT;

-- Recover the actual Bilibili publication time for videos accepted before this
-- migration. Manually imported history has no candidate row and is treated as
-- already watched so it does not create a surprise backlog after upgrading.
UPDATE episode_video
SET published_at = (
    SELECT c.published_at
    FROM candidate c
    WHERE c.episode_id = episode_video.episode_id
      AND c.bvid = episode_video.bvid
    ORDER BY c.id DESC
    LIMIT 1
)
WHERE published_at IS NULL;

UPDATE episode
SET watched_at = COALESCE(notified_at, confirmed_at)
WHERE state IN ('confirmed', 'notified')
  AND NOT EXISTS (
      SELECT 1 FROM candidate c WHERE c.episode_id = episode.id
  );

CREATE INDEX idx_episode_watch_queue
    ON episode(watched_at, confirmed_at DESC)
    WHERE state IN ('confirmed', 'notified');
