ALTER TABLE anime ADD COLUMN anilist_media_id INTEGER;

CREATE INDEX idx_anime_anilist_media_id
    ON anime(anilist_media_id)
    WHERE anilist_media_id IS NOT NULL;
