CREATE TABLE release_batch (
    id INTEGER PRIMARY KEY,
    anime_id INTEGER NOT NULL REFERENCES anime(id) ON DELETE CASCADE,
    first_episode INTEGER NOT NULL CHECK(first_episode > 0),
    last_episode INTEGER NOT NULL CHECK(last_episode >= first_episode AND last_episode <= 10000),
    expected_at TEXT,
    aired INTEGER NOT NULL DEFAULT 0 CHECK(aired IN (0,1)),
    source TEXT NOT NULL,
    verified_at TEXT NOT NULL,
    UNIQUE(anime_id,first_episode,last_episode,source)
);
CREATE INDEX idx_release_batch_anime ON release_batch(anime_id,first_episode,last_episode);

ALTER TABLE episode_video ADD COLUMN page_no INTEGER;
ALTER TABLE episode_video ADD COLUMN cid INTEGER;

CREATE TABLE collection_draft (
    id TEXT PRIMARY KEY,
    anime_id INTEGER NOT NULL REFERENCES anime(id) ON DELETE CASCADE,
    admin_id INTEGER NOT NULL REFERENCES web_admin(id),
    url TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'queued' CHECK(state IN ('queued','ready','importing','completed','failed')),
    metadata_json TEXT,
    error TEXT,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);
CREATE INDEX idx_collection_draft_expiry ON collection_draft(expires_at);

-- Reconcile existing entries through their authenticated upstream mappings;
-- never infer a batch from ONA, a video title, or an old weekly recurrence.
UPDATE anime SET schedule_next_sync_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
WHERE auto_schedule = 1 AND lifecycle = 'tracking';
