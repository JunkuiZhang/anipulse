ALTER TABLE anime ADD COLUMN anime_schedule_route TEXT;

CREATE INDEX idx_anime_anime_schedule_route
    ON anime(anime_schedule_route)
    WHERE anime_schedule_route IS NOT NULL;
