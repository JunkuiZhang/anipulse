use chrono::{DateTime, Duration, Utc};
use sqlx::FromRow;

use super::Repository;
use crate::error::Result;

#[derive(Clone, Copy, Debug, FromRow)]
pub(crate) struct ChapterKey {
    pub subject_id: i64,
    pub subject_index: i64,
    pub bangumi_number: i64,
}

impl Repository {
    pub(crate) async fn next_missing_chapter(&self) -> Result<Option<ChapterKey>> {
        let now = Utc::now();
        Ok(sqlx::query_as(
            r#"SELECT c.subject_id, c.subject_index, c.bangumi_number
               FROM episode_chapter c
               JOIN episode e ON e.id = c.episode_id
               JOIN anime a ON a.id = e.anime_id
               WHERE c.name_cn IS NULL AND (c.retry_at IS NULL OR c.retry_at <= ?)
                 AND a.lifecycle != 'archived' AND e.watched_at IS NULL
                 AND (EXISTS (SELECT 1 FROM monitorable_episode m WHERE m.id = e.id AND m.expected_at < ?)
                      OR (e.state IN ('confirmed', 'notified')
                          AND EXISTS (SELECT 1 FROM episode_video v WHERE v.episode_id = e.id)))
               ORDER BY c.retry_at NULLS FIRST, e.expected_at, e.id LIMIT 1"#,
        )
        .bind(now)
        .bind(now + Duration::days(7))
        .fetch_optional(&self.pool)
        .await?)
    }

    pub(crate) async fn remember_chapter_title(
        &self,
        key: ChapterKey,
        name_cn: Option<&str>,
        name: Option<&str>,
        retry_at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO bangumi_chapter_title (subject_id, subject_index, bangumi_number, name_cn, name, retry_at)
               VALUES (?, ?, ?, ?, ?, ?)
               ON CONFLICT (subject_id, subject_index, bangumi_number) DO UPDATE SET
                   name_cn = COALESCE(excluded.name_cn, bangumi_chapter_title.name_cn),
                   name = COALESCE(excluded.name, bangumi_chapter_title.name),
                   retry_at = excluded.retry_at"#,
        )
        .bind(key.subject_id)
        .bind(key.subject_index)
        .bind(key.bangumi_number)
        .bind(name_cn.map(str::trim).filter(|s| !s.is_empty()))
        .bind(name.map(str::trim).filter(|s| !s.is_empty()))
        .bind(retry_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
