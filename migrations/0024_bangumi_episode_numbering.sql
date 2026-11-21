-- Store provider numbering separately so manual mappings always win, including 1 ↔ 1.
CREATE TABLE bangumi_episode_numbering (
    subject_id INTEGER PRIMARY KEY,
    first_number INTEGER NOT NULL CHECK (first_number BETWEEN 1 AND 10000)
);

DROP VIEW episode_chapter;
CREATE VIEW episode_chapter AS
SELECT k.*, COALESCE(t.name_cn, t.tmdb_simplified, t.tmdb_traditional, t.name, t.tmdb_original) AS title,
       t.name_cn, t.name, t.retry_at, t.tmdb_simplified, t.tmdb_retry_at
FROM (
    SELECT a.bangumi_episode_origin IS NOT NULL AS manually_mapped, e.id AS episode_id, a.bangumi_subject_id AS subject_id,
           e.episode_no - COALESCE(a.local_episode_origin, 1) + 1 AS subject_index,
           e.episode_no - COALESCE(a.local_episode_origin, 1)
               + COALESCE(a.bangumi_episode_origin, n.first_number, 1) AS bangumi_number
    FROM episode e JOIN anime a ON a.id = e.anime_id
    LEFT JOIN bangumi_episode_numbering n ON n.subject_id = a.bangumi_subject_id
    WHERE a.bangumi_subject_id IS NOT NULL
      AND e.episode_no >= COALESCE(a.local_episode_origin, 1)
) k LEFT JOIN bangumi_chapter_title t USING (subject_id, subject_index, bangumi_number);

-- Retry previously empty default lookups on upgrade. The background worker picks visible episodes.
UPDATE bangumi_chapter_title SET retry_at = '1970-01-01T00:00:00Z'
WHERE subject_index = bangumi_number AND name IS NULL AND name_cn IS NULL
  AND EXISTS (SELECT 1 FROM anime a WHERE a.bangumi_subject_id = bangumi_chapter_title.subject_id
              AND a.lifecycle != 'archived' AND a.bangumi_episode_origin IS NULL);
