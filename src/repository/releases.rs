use chrono::{DateTime, Utc};
use sqlx::FromRow;
use std::collections::BTreeSet;

use super::Repository;
use crate::{
    domain::{Anime, Episode, ReleaseBatch},
    error::{AppError, Result},
};

#[derive(Debug, Clone, FromRow)]
pub struct ReleaseProgress {
    pub anime_id: i64,
    pub title: String,
    pub bangumi_subject_id: Option<i64>,
    pub total: i64,
    pub official: i64,
    pub found: i64,
    pub watched: i64,
    pub expected_at: Option<DateTime<Utc>>,
}

impl Repository {
    pub async fn apply_release_batches(
        &self,
        anime_id: i64,
        batches: &[ReleaseBatch],
    ) -> Result<()> {
        if batches.is_empty() {
            return Ok(());
        }
        let now = Utc::now();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let anime = sqlx::query_as::<_, Anime>("SELECT * FROM anime WHERE id=?")
            .bind(anime_id)
            .fetch_one(&mut *tx)
            .await?;
        if !anime.auto_schedule || anime.lifecycle == "archived" {
            return Ok(());
        }
        let mut numbers = BTreeSet::new();
        for batch in batches {
            if batch.first_episode <= 0
                || batch.last_episode < batch.first_episode
                || batch.last_episode > 10000
                || batch.source != "anime_schedule"
                || anime
                    .final_episode_no()?
                    .is_some_and(|last| batch.last_episode > last)
            {
                return Err(AppError::InvalidInput(
                    "invalid verified release range".into(),
                ));
            }
            sqlx::query("INSERT INTO release_batch(anime_id,first_episode,last_episode,expected_at,aired,source,verified_at) VALUES(?,?,?,?,?,?,?) ON CONFLICT(anime_id,first_episode,last_episode,source) DO UPDATE SET expected_at=excluded.expected_at,aired=excluded.aired,verified_at=excluded.verified_at")
                .bind(anime_id).bind(batch.first_episode).bind(batch.last_episode)
                .bind(batch.expected_at).bind(batch.aired && batch.expected_at.is_some())
                .bind(&batch.source).bind(now).execute(&mut *tx).await?;
            numbers.extend(batch.first_episode..=batch.last_episode);
        }
        // A narrower per-episode correction takes precedence over a broader
        // premiere batch. Previously confirmed resources and watch timestamps
        // are never rewritten by a schedule synchronization.
        for number in numbers {
            let expected: Option<DateTime<Utc>> = sqlx::query_scalar("SELECT expected_at FROM release_batch WHERE anime_id=? AND first_episode<=? AND last_episode>=? ORDER BY last_episode-first_episode,verified_at DESC LIMIT 1")
                .bind(anime_id).bind(number).bind(number).fetch_one(&mut *tx).await?;
            let check = self.schedule_check_at(expected, now, true);
            sqlx::query("INSERT OR IGNORE INTO episode(anime_id,episode_no,expected_at,state,next_check_at) VALUES(?,?,?,'waiting',?)")
                .bind(anime_id).bind(number).bind(expected).bind(check).execute(&mut *tx).await?;
            sqlx::query("UPDATE episode SET expected_at=?,next_check_at=MIN(next_check_at,?) WHERE anime_id=? AND episode_no=? AND state NOT IN ('confirmed','notified')")
                .bind(expected).bind(check).bind(anime_id).bind(number).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn has_release_batch(&self, anime_id: i64) -> Result<bool> {
        Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM release_batch WHERE anime_id=? AND last_episode>first_episode)")
            .bind(anime_id).fetch_one(&self.pool).await?)
    }

    pub async fn independently_monitorable(&self, anime_id: i64, episode_id: i64) -> Result<bool> {
        Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM episode e JOIN anime a ON a.id=e.anime_id WHERE a.id=? AND e.id=? AND a.lifecycle='tracking' AND e.state NOT IN ('confirmed','notified') AND (e.id=(SELECT id FROM episode WHERE anime_id=a.id AND state NOT IN ('confirmed','notified') ORDER BY episode_no LIMIT 1) OR EXISTS(SELECT 1 FROM release_batch b WHERE b.anime_id=a.id AND e.episode_no BETWEEN b.first_episode AND b.last_episode)))")
            .bind(anime_id).bind(episode_id).fetch_one(&self.pool).await?)
    }

    pub async fn detection_targets(&self, anime_id: i64) -> Result<Vec<Episode>> {
        let planned: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM release_batch WHERE anime_id=?)")
                .bind(anime_id)
                .fetch_one(&self.pool)
                .await?;
        if !planned {
            return Ok(vec![self.active_episode(anime_id).await?]);
        }
        Ok(sqlx::query_as("SELECT * FROM episode WHERE anime_id=? AND state NOT IN ('confirmed','notified') AND next_check_at<=? ORDER BY next_check_at,episode_no LIMIT 100")
            .bind(anime_id).bind(Utc::now()).fetch_all(&self.pool).await?)
    }

    pub async fn release_progress(&self) -> Result<Vec<ReleaseProgress>> {
        Ok(sqlx::query_as(r#"SELECT a.id AS anime_id,a.title,a.bangumi_subject_id,
            COALESCE(a.total_episodes,MAX(e.episode_no)) AS total,
            SUM(CASE WHEN COALESCE((SELECT b.aired FROM release_batch b WHERE b.anime_id=a.id AND e.episode_no BETWEEN b.first_episode AND b.last_episode ORDER BY b.last_episode-b.first_episode,b.verified_at DESC LIMIT 1),0)=1 THEN 1 ELSE 0 END) AS official,
            SUM(CASE WHEN e.state IN ('confirmed','notified') AND EXISTS(SELECT 1 FROM episode_video v WHERE v.episode_id=e.id AND NOT EXISTS(SELECT 1 FROM uploader_trust t WHERE t.anime_id=a.id AND t.uploader_mid=v.uploader_mid AND t.manually_blocked=1)) THEN 1 ELSE 0 END) AS found,
            SUM(CASE WHEN e.watched_at IS NOT NULL THEN 1 ELSE 0 END) AS watched,
            (SELECT MIN(b.expected_at) FROM release_batch b WHERE b.anime_id=a.id AND b.last_episode>b.first_episode) AS expected_at
            FROM anime a JOIN episode e ON e.anime_id=a.id
            WHERE a.lifecycle!='archived' AND EXISTS(SELECT 1 FROM release_batch b WHERE b.anime_id=a.id AND b.last_episode>b.first_episode)
            GROUP BY a.id ORDER BY expected_at,a.id"#).fetch_all(&self.pool).await?)
    }

    pub async fn notification_batch_key(&self, episode_id: i64) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar("SELECT b.id FROM release_batch b JOIN episode e ON e.anime_id=b.anime_id AND e.episode_no BETWEEN b.first_episode AND b.last_episode WHERE e.id=? AND b.last_episode>b.first_episode ORDER BY b.last_episode-b.first_episode LIMIT 1")
            .bind(episode_id).fetch_optional(&self.pool).await?)
    }
}
