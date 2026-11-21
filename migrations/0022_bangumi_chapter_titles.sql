CREATE TABLE bangumi_chapter_title (
    subject_id INTEGER NOT NULL,
    subject_index INTEGER NOT NULL,
    bangumi_number INTEGER NOT NULL,
    name_cn TEXT,
    name TEXT,
    retry_at TEXT NOT NULL,
    PRIMARY KEY (subject_id, subject_index, bangumi_number)
);

-- Resolve the current mapping on read, including for episodes created before this migration.
CREATE VIEW episode_chapter AS
SELECT k.*, COALESCE(t.name_cn, t.name) AS title, t.name_cn, t.retry_at
FROM (
    SELECT e.id AS episode_id, a.bangumi_subject_id AS subject_id,
           e.episode_no - COALESCE(a.local_episode_origin, 1) + 1 AS subject_index,
           e.episode_no - COALESCE(a.local_episode_origin, 1)
               + COALESCE(a.bangumi_episode_origin, 1) AS bangumi_number
    FROM episode e JOIN anime a ON a.id = e.anime_id
    WHERE a.bangumi_subject_id IS NOT NULL
      AND e.episode_no >= COALESCE(a.local_episode_origin, 1)
) k LEFT JOIN bangumi_chapter_title t USING (subject_id, subject_index, bangumi_number);
