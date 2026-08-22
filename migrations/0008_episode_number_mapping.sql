ALTER TABLE anime ADD COLUMN local_episode_origin INTEGER
    CHECK(local_episode_origin IS NULL OR local_episode_origin > 0);
ALTER TABLE anime ADD COLUMN bangumi_episode_origin INTEGER
    CHECK(bangumi_episode_origin IS NULL OR bangumi_episode_origin > 0);
