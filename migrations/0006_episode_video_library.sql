CREATE TABLE episode_video (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    episode_id INTEGER NOT NULL REFERENCES episode(id) ON DELETE CASCADE,
    bvid TEXT NOT NULL,
    title TEXT NOT NULL,
    uploader_mid INTEGER NOT NULL CHECK(uploader_mid > 0),
    uploader_name TEXT NOT NULL,
    duration_sec INTEGER NOT NULL CHECK(duration_sec >= 0),
    score INTEGER NOT NULL,
    is_preferred INTEGER NOT NULL DEFAULT 0 CHECK(is_preferred IN (0, 1)),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(episode_id, bvid)
);

CREATE INDEX idx_episode_video_episode_score
    ON episode_video(episode_id, score DESC, updated_at DESC);

-- Preserve videos that were accepted before the historical library existed.
INSERT OR IGNORE INTO episode_video(
    episode_id, bvid, title, uploader_mid, uploader_name,
    duration_sec, score, is_preferred, created_at, updated_at
)
SELECT
    c.episode_id, c.bvid, c.title, c.uploader_mid, c.uploader_name,
    c.duration_sec, c.score, 0, c.first_seen_at, c.last_seen_at
FROM candidate c
JOIN episode e ON e.id = c.episode_id
WHERE e.state IN ('confirmed', 'notified')
  AND c.state IN ('confirmed', 'expired')
  AND c.uploader_mid > 0;

-- An accepted candidate is the user's existing choice. Pick one deterministically
-- if old data happens to contain more than one confirmed candidate for an episode.
UPDATE episode_video
SET is_preferred = 1
WHERE id IN (
    SELECT MIN(ev.id)
    FROM episode_video ev
    JOIN candidate c
      ON c.episode_id = ev.episode_id AND c.bvid = ev.bvid
    WHERE c.state = 'confirmed'
    GROUP BY ev.episode_id
);

CREATE UNIQUE INDEX idx_episode_video_one_preferred
    ON episode_video(episode_id)
    WHERE is_preferred = 1;
