CREATE TABLE IF NOT EXISTS global_uploader_trust (
    uploader_mid INTEGER PRIMARY KEY CHECK (uploader_mid > 0),
    uploader_name TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
