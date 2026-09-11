ALTER TABLE anime_archive_memory ADD COLUMN bangumi_score REAL
    CHECK(bangumi_score IS NULL OR bangumi_score BETWEEN 0 AND 10);

ALTER TABLE anime_archive_memory ADD COLUMN bangumi_rating_total INTEGER
    CHECK(bangumi_rating_total IS NULL OR bangumi_rating_total >= 0);

ALTER TABLE anime_archive_memory ADD COLUMN bangumi_rank INTEGER
    CHECK(bangumi_rank IS NULL OR bangumi_rank > 0);

ALTER TABLE anime_archive_memory ADD COLUMN bangumi_rating_updated_at TEXT;

-- Existing collections should gain their public rating after the scheduler
-- starts, without making the first archive page request wait on the network.
INSERT OR IGNORE INTO management_job(
    kind, target_type, target_id, payload_json, state,
    requested_by, dedupe_key, created_at
)
SELECT 'sync_schedule', 'anime', CAST(id AS TEXT), '{}', 'queued',
       NULL, 'archive_rating:' || id,
       strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
FROM anime
WHERE lifecycle = 'archived' AND bangumi_subject_id IS NOT NULL;
