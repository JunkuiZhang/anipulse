use super::Repository;
use crate::{
    domain::{Anime, CollectionSelection, VideoCollection},
    error::{AppError, Result},
};
use chrono::{DateTime, Duration, Utc};
use sqlx::FromRow;
use std::collections::HashSet;

#[derive(Debug, Clone, FromRow)]
pub struct CollectionDraft {
    pub id: String,
    pub anime_id: i64,
    pub admin_id: i64,
    pub url: String,
    pub state: String,
    pub metadata_json: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl Repository {
    pub async fn create_collection_draft(
        &self,
        id: &str,
        anime_id: i64,
        admin_id: i64,
        url: &str,
    ) -> Result<()> {
        let anime = self.get_anime(anime_id).await?;
        if anime.anime.lifecycle == "archived" {
            return Err(AppError::InvalidInput("归档番剧不能导入合集".into()));
        }
        sqlx::query("INSERT INTO collection_draft(id,anime_id,admin_id,url,created_at,expires_at) VALUES(?,?,?,?,?,?)")
            .bind(id).bind(anime_id).bind(admin_id).bind(url).bind(Utc::now()).bind(Utc::now()+Duration::hours(2))
            .execute(&self.pool).await?;
        Ok(())
    }
    pub async fn collection_draft(&self, id: &str) -> Result<CollectionDraft> {
        sqlx::query_as("SELECT * FROM collection_draft WHERE id=? AND expires_at>?")
            .bind(id)
            .bind(Utc::now())
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| AppError::NotFound("合集预览已过期，请重新读取".into()))
    }
    pub async fn ready_collection_draft(&self, id: &str, metadata: &VideoCollection) -> Result<()> {
        let json = serde_json::to_string(metadata)
            .map_err(|_| AppError::InvalidInput("cannot serialize collection".into()))?;
        sqlx::query("UPDATE collection_draft SET state='ready',metadata_json=?,error=NULL,expires_at=? WHERE id=? AND state='queued'")
            .bind(json).bind(Utc::now()+Duration::minutes(30)).bind(id).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn fail_collection_draft(&self, id: &str, error: &str) -> Result<()> {
        sqlx::query(
            "UPDATE collection_draft SET state='failed',error=? WHERE id=? AND state!='completed'",
        )
        .bind(error.chars().take(500).collect::<String>())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// All selections are validated before any writes. A missing/reordered P
    /// cannot partially confirm a season. Existing preferences/watch dates win.
    pub async fn import_collection(
        &self,
        draft_id: &str,
        collection: &VideoCollection,
        selections: &[CollectionSelection],
        channel: &str,
    ) -> Result<()> {
        if selections.is_empty() || selections.len() > 1000 {
            return Err(AppError::InvalidInput("请选择至少一集".into()));
        }
        // Network metadata has already been read. Acquire the write lock before
        // reading the snapshot so a concurrent web write cannot invalidate it.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let draft = sqlx::query_as::<_, CollectionDraft>(
            "SELECT * FROM collection_draft WHERE id=? AND expires_at>?",
        )
        .bind(draft_id)
        .bind(Utc::now())
        .fetch_one(&mut *tx)
        .await?;
        if draft.state == "completed" {
            return Ok(());
        }
        if draft.state != "ready" && draft.state != "importing" {
            return Err(AppError::InvalidInput("合集尚未准备好".into()));
        }
        let anime = sqlx::query_as::<_, Anime>("SELECT * FROM anime WHERE id=?")
            .bind(draft.anime_id)
            .fetch_one(&mut *tx)
            .await?;
        if anime.lifecycle == "archived" {
            return Err(AppError::InvalidInput("番剧已经归档".into()));
        }
        let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM uploader_trust WHERE anime_id=? AND uploader_mid=? AND manually_blocked=1)")
            .bind(anime.id).bind(collection.video.uploader_mid).fetch_one(&mut *tx).await?;
        if blocked || collection.video.uploader_mid <= 0 {
            return Err(AppError::InvalidInput("合集 UP 已被屏蔽或不可用".into()));
        }
        let mut episodes = HashSet::new();
        let mut cids = HashSet::new();
        for selection in selections {
            if selection.episode_no < anime.local_episode_origin.unwrap_or(1)
                || selection.episode_no > 10000
                || anime
                    .final_episode_no()?
                    .is_some_and(|last| selection.episode_no > last)
                || !episodes.insert(selection.episode_no)
                || !cids.insert(selection.cid)
                || !collection.parts.iter().any(|p| p.cid == selection.cid)
            {
                return Err(AppError::InvalidInput(
                    "集数重复、超出范围或分 P 已变化，请重新预览".into(),
                ));
            }
        }
        let now = Utc::now();
        let mut first_new = None;
        let mut newly_found = Vec::new();
        for selection in selections {
            let part = collection
                .parts
                .iter()
                .find(|p| p.cid == selection.cid)
                .unwrap();
            let expected: Option<DateTime<Utc>>=sqlx::query_scalar("SELECT expected_at FROM release_batch WHERE anime_id=? AND ? BETWEEN first_episode AND last_episode ORDER BY last_episode-first_episode,verified_at DESC LIMIT 1")
                .bind(anime.id).bind(selection.episode_no).fetch_optional(&mut *tx).await?.flatten();
            sqlx::query("INSERT OR IGNORE INTO episode(anime_id,episode_no,expected_at,state,next_check_at) VALUES(?,?,?,'waiting',?)")
                .bind(anime.id).bind(selection.episode_no).bind(expected).bind(now).execute(&mut *tx).await?;
            let (episode_id, state): (i64, String) =
                sqlx::query_as("SELECT id,state FROM episode WHERE anime_id=? AND episode_no=?")
                    .bind(anime.id)
                    .bind(selection.episode_no)
                    .fetch_one(&mut *tx)
                    .await?;
            let title = format!("{} · {}", collection.video.title, part.title);
            sqlx::query("INSERT INTO episode_video(episode_id,bvid,title,uploader_mid,uploader_name,duration_sec,score,is_preferred,published_at,created_at,updated_at,page_no,cid) VALUES(?,?,?,?,?,?,100,0,?,?,?,?,?) ON CONFLICT(episode_id,bvid) DO UPDATE SET page_no=excluded.page_no,cid=excluded.cid,title=excluded.title,duration_sec=excluded.duration_sec,updated_at=excluded.updated_at")
                .bind(episode_id).bind(&collection.video.bvid).bind(title).bind(collection.video.uploader_mid)
                .bind(&collection.video.uploader_name).bind(part.duration_sec).bind(collection.video.published_at)
                .bind(now).bind(now).bind(part.page).bind(part.cid).execute(&mut *tx).await?;
            if !matches!(state.as_str(), "confirmed" | "notified") {
                sqlx::query("UPDATE episode SET state='confirmed',confirmed_at=COALESCE(confirmed_at,?) WHERE id=?")
                    .bind(now).bind(episode_id).execute(&mut *tx).await?;
                first_new.get_or_insert(episode_id);
                newly_found.push(selection.episode_no);
            }
            sqlx::query(
                "UPDATE candidate SET state='expired' WHERE episode_id=? AND state='pending'",
            )
            .bind(episode_id)
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE review_notification SET status='cancelled' WHERE episode_id=? AND status='pending'")
                .bind(episode_id).execute(&mut *tx).await?;
        }
        // One import, one notification. Each episode remains independently
        // available to watch; no watched_at is inferred from this operation.
        if let Some(id) = first_new {
            let mut numbers = newly_found;
            numbers.sort();
            let reason = format!(
                "batch:{}",
                numbers
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            );
            sqlx::query("INSERT OR IGNORE INTO notification(episode_id,channel,confirmation_reason,status,created_at) VALUES(?,?,?,'pending',?)")
                .bind(id).bind(channel).bind(reason).bind(now).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE collection_draft SET state='completed',error=NULL WHERE id=?")
            .bind(draft_id)
            .execute(&mut *tx)
            .await?;
        super::sqlite::transition_released_complete_if_final_released(&mut tx, anime.id, now)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}
