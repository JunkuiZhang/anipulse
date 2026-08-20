PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS anime (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL,
    bangumi_subject_id INTEGER,
    expected_weekday INTEGER,
    expected_time TEXT,
    timezone TEXT NOT NULL DEFAULT 'Asia/Shanghai',
    duration_min_sec INTEGER NOT NULL CHECK(duration_min_sec > 0),
    duration_max_sec INTEGER NOT NULL CHECK(duration_max_sec >= duration_min_sec),
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS anime_alias (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    anime_id INTEGER NOT NULL REFERENCES anime(id) ON DELETE CASCADE,
    alias TEXT NOT NULL,
    priority INTEGER NOT NULL DEFAULT 0,
    enabled INTEGER NOT NULL DEFAULT 1,
    UNIQUE(anime_id, alias)
);

CREATE TABLE IF NOT EXISTS episode (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    anime_id INTEGER NOT NULL REFERENCES anime(id) ON DELETE CASCADE,
    episode_no INTEGER NOT NULL CHECK(episode_no > 0),
    expected_at TEXT,
    state TEXT NOT NULL,
    next_check_at TEXT NOT NULL,
    first_candidate_at TEXT,
    confirmed_at TEXT,
    notified_at TEXT,
    UNIQUE(anime_id, episode_no)
);

CREATE INDEX IF NOT EXISTS idx_episode_due ON episode(state, next_check_at);

CREATE TABLE IF NOT EXISTS candidate (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    episode_id INTEGER NOT NULL REFERENCES episode(id) ON DELETE CASCADE,
    bvid TEXT NOT NULL,
    uploader_mid INTEGER NOT NULL,
    uploader_name TEXT NOT NULL,
    title TEXT NOT NULL,
    description TEXT,
    duration_sec INTEGER NOT NULL,
    published_at TEXT NOT NULL,
    url TEXT NOT NULL,
    tags_json TEXT NOT NULL DEFAULT '[]',
    page_count INTEGER,
    score INTEGER NOT NULL,
    state TEXT NOT NULL,
    first_seen_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    seen_count INTEGER NOT NULL DEFAULT 1,
    evaluation_json TEXT NOT NULL,
    UNIQUE(episode_id, bvid)
);

CREATE INDEX IF NOT EXISTS idx_candidate_episode_state ON candidate(episode_id, state);

CREATE TABLE IF NOT EXISTS uploader_trust (
    anime_id INTEGER NOT NULL REFERENCES anime(id) ON DELETE CASCADE,
    uploader_mid INTEGER NOT NULL,
    uploader_name TEXT,
    confirmed_count INTEGER NOT NULL DEFAULT 0,
    rejected_count INTEGER NOT NULL DEFAULT 0,
    manually_trusted INTEGER NOT NULL DEFAULT 0,
    manually_blocked INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(anime_id, uploader_mid)
);

CREATE TABLE IF NOT EXISTS notification (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    episode_id INTEGER NOT NULL REFERENCES episode(id) ON DELETE CASCADE,
    candidate_id INTEGER REFERENCES candidate(id) ON DELETE SET NULL,
    channel TEXT NOT NULL,
    confirmation_reason TEXT,
    status TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    next_attempt_at TEXT,
    created_at TEXT NOT NULL,
    sent_at TEXT,
    UNIQUE(episode_id, channel)
);

CREATE TABLE IF NOT EXISTS provider_state (
    provider TEXT PRIMARY KEY,
    backoff_until TEXT,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    daily_date TEXT,
    daily_count INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL
);

INSERT OR IGNORE INTO provider_state(provider, updated_at)
VALUES ('bilibili', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
