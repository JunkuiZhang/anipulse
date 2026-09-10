CREATE TABLE anime_archive_memory (
    anime_id INTEGER PRIMARY KEY REFERENCES anime(id) ON DELETE CASCADE,
    watched_episodes INTEGER
        CHECK(watched_episodes IS NULL OR watched_episodes >= 0),
    approximate_watch_seconds INTEGER
        CHECK(approximate_watch_seconds IS NULL OR approximate_watch_seconds >= 0),
    tracking_started_at TEXT NOT NULL,
    first_confirmed_at TEXT,
    first_watched_at TEXT,
    completed_at TEXT NOT NULL,
    rating INTEGER CHECK(rating IS NULL OR rating BETWEEN 1 AND 10),
    short_review TEXT NOT NULL DEFAULT '',
    tags_json TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(tags_json)),
    history_available INTEGER NOT NULL DEFAULT 1 CHECK(history_available IN (0, 1)),
    snapshot_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Older collections have already lost their episode rows. Preserve the dates
-- and duration estimate that can still be derived without pretending the
-- missing per-episode history can be reconstructed.
INSERT INTO anime_archive_memory(
    anime_id, watched_episodes, approximate_watch_seconds,
    tracking_started_at, completed_at, history_available,
    snapshot_at, updated_at
)
SELECT id, NULL,
       CASE
           WHEN total_episodes IS NOT NULL
           THEN total_episodes * ((duration_min_sec + duration_max_sec) / 2)
           ELSE NULL
       END,
       created_at, COALESCE(archived_at, updated_at), 0,
       COALESCE(archived_at, updated_at), updated_at
FROM anime
WHERE lifecycle = 'archived';
