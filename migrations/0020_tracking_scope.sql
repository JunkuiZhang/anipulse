-- Episode IDs are monotonic. Recover the earliest surviving tracking record,
-- not MIN(episode_no), which the release-backfill regression has contaminated.
-- This establishes a monitoring boundary, never an inferred watched status.
ALTER TABLE anime ADD COLUMN tracking_start_episode INTEGER NOT NULL DEFAULT 1
    CHECK(tracking_start_episode BETWEEN 1 AND 10000);
UPDATE anime SET tracking_start_episode = COALESCE(
    (SELECT e.episode_no FROM episode e WHERE e.anime_id=anime.id ORDER BY e.id LIMIT 1),
    local_episode_origin, 1
);

CREATE VIEW monitorable_episode AS
SELECT e.* FROM episode e JOIN anime a ON a.id=e.anime_id
WHERE a.lifecycle='tracking' AND e.episode_no>=a.tracking_start_episode
  AND e.state NOT IN ('confirmed','notified')
  AND (
    e.episode_no=(SELECT MIN(current.episode_no) FROM episode current
                 WHERE current.anime_id=a.id
                   AND current.episode_no>=a.tracking_start_episode
                   AND current.state NOT IN ('confirmed','notified'))
    OR EXISTS(SELECT 1 FROM release_batch b WHERE b.anime_id=a.id
              AND b.last_episode>b.first_episode
              AND e.episode_no BETWEEN b.first_episode AND b.last_episode)
  );
