ALTER TABLE anime ADD COLUMN lifecycle TEXT NOT NULL DEFAULT 'tracking'
    CHECK(lifecycle IN ('tracking', 'released_complete', 'archived'));
ALTER TABLE anime ADD COLUMN summary TEXT NOT NULL DEFAULT '';
ALTER TABLE anime ADD COLUMN total_episodes INTEGER CHECK(total_episodes IS NULL OR total_episodes > 0);
ALTER TABLE anime ADD COLUMN released_completed_at TEXT;
ALTER TABLE anime ADD COLUMN archived_at TEXT;

CREATE INDEX idx_anime_lifecycle ON anime(lifecycle, id);
