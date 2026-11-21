use chrono::{DateTime, Utc};
use sqlx::FromRow;

use super::Repository;
use crate::error::Result;

#[derive(Clone, Copy, Debug, FromRow)]
pub(crate) struct ChapterKey {
    pub subject_id: i64,
    pub subject_index: i64,
    pub bangumi_number: i64,
}

#[derive(FromRow)]
pub(crate) struct TmdbChapter {
    #[sqlx(flatten)]
    pub key: ChapterKey,
    pub name: Option<String>,
}

#[derive(Default)]
pub(crate) struct ChapterTranslations {
    pub simplified: Option<String>,
    pub traditional: Option<String>,
    pub original: Option<String>,
}

impl Repository {
    pub(crate) async fn next_missing_chapter(&self) -> Result<Option<ChapterKey>> {
        Ok(sqlx::query_as(
            r#"SELECT subject_id, subject_index, bangumi_number FROM missing_chapter_translation
               WHERE retry_at IS NULL OR retry_at <= ?
               ORDER BY retry_at NULLS FIRST, expected_at, episode_id LIMIT 1"#,
        )
        .bind(Utc::now())
        .fetch_optional(&self.pool)
        .await?)
    }

    pub(crate) async fn next_tmdb_chapter(&self) -> Result<Option<TmdbChapter>> {
        Ok(sqlx::query_as(
            r#"SELECT subject_id, subject_index, bangumi_number, name FROM missing_chapter_translation
               WHERE retry_at IS NOT NULL AND tmdb_simplified IS NULL
                 AND (tmdb_retry_at IS NULL OR tmdb_retry_at <= ?)
               ORDER BY tmdb_retry_at NULLS FIRST, name IS NULL, expected_at, episode_id LIMIT 1"#,
        )
        .bind(Utc::now())
        .fetch_optional(&self.pool)
        .await?)
    }

    pub(crate) async fn remember_tmdb_titles(
        &self,
        chapter: &TmdbChapter,
        titles: &ChapterTranslations,
        retry_at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            r#"UPDATE bangumi_chapter_title SET
                   tmdb_simplified = COALESCE(?, tmdb_simplified),
                   tmdb_traditional = COALESCE(?, tmdb_traditional),
                   tmdb_original = COALESCE(?, tmdb_original), tmdb_retry_at = ?
               WHERE subject_id = ? AND subject_index = ? AND bangumi_number = ? AND name_cn IS NULL AND name IS ?"#,
        )
        .bind(&titles.simplified)
        .bind(&titles.traditional)
        .bind(&titles.original)
        .bind(retry_at)
        .bind(chapter.key.subject_id)
        .bind(chapter.key.subject_index)
        .bind(chapter.key.bangumi_number)
        .bind(&chapter.name)
        .execute(&self.pool).await?;
        Ok(())
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
