ALTER TABLE bangumi_chapter_title ADD COLUMN tmdb_simplified TEXT;
ALTER TABLE bangumi_chapter_title ADD COLUMN tmdb_traditional TEXT;
ALTER TABLE bangumi_chapter_title ADD COLUMN tmdb_original TEXT;
ALTER TABLE bangumi_chapter_title ADD COLUMN tmdb_retry_at TEXT;

-- A corrected Bangumi original invalidates a translation verified against the old title.
CREATE TRIGGER invalidate_tmdb_chapter_title AFTER UPDATE OF name ON bangumi_chapter_title
WHEN NEW.name IS NOT OLD.name AND OLD.tmdb_retry_at IS NOT NULL
BEGIN
    UPDATE bangumi_chapter_title SET tmdb_simplified=NULL, tmdb_traditional=NULL,
           tmdb_original=NULL, tmdb_retry_at=NULL
    WHERE subject_id=NEW.subject_id AND subject_index=NEW.subject_index AND bangumi_number=NEW.bangumi_number;
END;

DROP VIEW episode_chapter;
CREATE VIEW episode_chapter AS
SELECT k.*, COALESCE(t.name_cn, t.tmdb_simplified, t.tmdb_traditional, t.name, t.tmdb_original) AS title,
       t.name_cn, t.name, t.retry_at, t.tmdb_simplified, t.tmdb_retry_at
FROM (
    SELECT e.id AS episode_id, a.bangumi_subject_id AS subject_id,
           e.episode_no - COALESCE(a.local_episode_origin, 1) + 1 AS subject_index,
           e.episode_no - COALESCE(a.local_episode_origin, 1)
               + COALESCE(a.bangumi_episode_origin, 1) AS bangumi_number
    FROM episode e JOIN anime a ON a.id = e.anime_id
    WHERE a.bangumi_subject_id IS NOT NULL
      AND e.episode_no >= COALESCE(a.local_episode_origin, 1)
) k LEFT JOIN bangumi_chapter_title t USING (subject_id, subject_index, bangumi_number);

-- Both providers backfill the same dashboard episodes, with independent retries.
CREATE VIEW missing_chapter_translation AS
SELECT c.*, e.expected_at
FROM episode_chapter c
JOIN episode e ON e.id = c.episode_id
JOIN anime a ON a.id = e.anime_id
WHERE c.name_cn IS NULL AND a.lifecycle != 'archived' AND e.watched_at IS NULL
  AND (EXISTS (SELECT 1 FROM monitorable_episode m WHERE m.id = e.id
               AND julianday(m.expected_at) < julianday('now', '+7 days'))
       OR (e.state IN ('confirmed', 'notified')
           AND EXISTS (SELECT 1 FROM episode_video v WHERE v.episode_id = e.id)));
