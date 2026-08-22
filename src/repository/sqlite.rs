use std::{collections::HashSet, path::Path, str::FromStr, time::Duration};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Days, NaiveDate, Utc};
use serde_json::to_string;
use sha2::{Digest, Sha256};
use sqlx::{
    FromRow, Sqlite, SqlitePool, Transaction,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

use crate::{
    domain::{
        Anime, AnimeWithAliases, CandidateState, Episode, EpisodeState, Evaluation, NewAnime,
        PendingNotification, PendingReviewNotification, ReviewCandidateSummary, ScheduleUpdate,
        StoredCandidate, UploaderTrust, VideoCandidate,
    },
    error::{AppError, Result},
};

#[derive(Debug, Clone)]
pub struct Repository {
    pool: SqlitePool,
}

#[derive(Debug, Clone, FromRow)]
pub struct CandidateListRow {
    pub episode_id: i64,
    pub anime_id: i64,
    pub bvid: String,
    pub anime_title: String,
    pub episode_no: i64,
    pub uploader_mid: i64,
    pub uploader_name: String,
    pub title: String,
    pub duration_sec: i64,
    pub published_at: DateTime<Utc>,
    pub score: i64,
    pub state: String,
    pub seen_count: i64,
    pub evaluation_json: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct DashboardStats {
    pub anime_count: i64,
    pub enabled_anime_count: i64,
    pub pending_candidate_count: i64,
    pub pending_notification_count: i64,
    pub failed_notification_count: i64,
    pub queued_job_count: i64,
    pub failed_job_count: i64,
    pub scheduler_heartbeat: Option<DateTime<Utc>>,
    pub provider_backoff_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, FromRow)]
pub struct ManagementJob {
    pub id: i64,
    pub kind: String,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    pub payload_json: String,
    pub state: String,
    pub requested_by: Option<i64>,
    pub dedupe_key: Option<String>,
    pub attempts: i64,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct WebAdmin {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    pub role: String,
    pub disabled: bool,
    pub created_at: DateTime<Utc>,
    pub password_changed_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, FromRow)]
pub struct AuthenticatedSession {
    pub token_hmac: Vec<u8>,
    pub admin_id: i64,
    pub username: String,
    pub role: String,
    pub created_at: DateTime<Utc>,
    pub renewed_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub idle_expires_at: DateTime<Utc>,
    pub absolute_expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct AuditEventRow {
    pub id: i64,
    pub actor_type: String,
    pub actor_admin_id: Option<i64>,
    pub action: String,
    pub entity_type: Option<String>,
    pub entity_id: Option<String>,
    pub outcome: String,
    pub request_id: Option<String>,
    pub metadata_json: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct AnimeDraftRow {
    pub id: String,
    pub admin_id: i64,
    pub session_token_hmac: Vec<u8>,
    pub payload_json: String,
    pub state: String,
    pub resolved_json: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub consumed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, FromRow)]
struct ProviderState {
    backoff_until: Option<DateTime<Utc>>,
    daily_date: Option<NaiveDate>,
    daily_count: i64,
}

impl Repository {
    pub async fn connect(path: &str) -> Result<Self> {
        if path != ":memory:" {
            let path = Path::new(path);
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                std::fs::create_dir_all(parent).map_err(|error| {
                    AppError::Config(format!(
                        "cannot create database directory {}: {error}",
                        parent.display()
                    ))
                })?;
            }
        }
        let options = if path == ":memory:" {
            SqliteConnectOptions::from_str("sqlite::memory:")?
        } else {
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal)
                .synchronous(SqliteSynchronous::Normal)
        }
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(if path == ":memory:" { 1 } else { 4 })
            .min_connections(1)
            .connect_with(options)
            .await?;
        sqlx::migrate!()
            .run(&pool)
            .await
            .map_err(|error| sqlx::Error::Migrate(Box::new(error)))?;
        Ok(Self { pool })
    }

    pub async fn migrate(path: &str) -> Result<()> {
        let repository = Self::connect(path).await?;
        repository.pool.close().await;
        Ok(())
    }

    pub async fn add_anime(&self, new: NewAnime) -> Result<i64> {
        if new.title.trim().is_empty() {
            return Err(AppError::InvalidInput("title cannot be empty".into()));
        }
        if new.next_episode <= 0 {
            return Err(AppError::InvalidInput(
                "next episode must be greater than zero".into(),
            ));
        }
        if new.duration_min_sec <= 0 || new.duration_max_sec < new.duration_min_sec {
            return Err(AppError::InvalidInput("duration range is invalid".into()));
        }

        let now = Utc::now();
        let (bangumi_subject_id, auto_schedule, broadcast_pattern, schedule_sync_at, next_sync_at) =
            new.auto_schedule
                .as_ref()
                .map(|metadata| {
                    (
                        Some(metadata.bangumi_subject_id),
                        true,
                        Some(metadata.broadcast_pattern.as_str()),
                        Some(now),
                        Some(metadata.next_sync_at),
                    )
                })
                .unwrap_or((None, false, None, None, None));
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            r#"INSERT INTO anime(
                title, bangumi_subject_id, expected_weekday, expected_time, timezone,
                duration_min_sec, duration_max_sec, enabled, created_at, updated_at,
                auto_schedule, broadcast_pattern, schedule_sync_at, schedule_next_sync_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(new.title.trim())
        .bind(bangumi_subject_id)
        .bind(new.expected_weekday)
        .bind(new.expected_time.as_deref())
        .bind(&new.timezone)
        .bind(new.duration_min_sec)
        .bind(new.duration_max_sec)
        .bind(now)
        .bind(now)
        .bind(auto_schedule)
        .bind(broadcast_pattern)
        .bind(schedule_sync_at)
        .bind(next_sync_at)
        .execute(&mut *tx)
        .await?;
        let anime_id = result.last_insert_rowid();

        let mut seen_aliases = HashSet::new();
        for (priority, alias) in std::iter::once(new.title)
            .chain(new.aliases)
            .map(|alias| alias.trim().to_string())
            .filter(|alias| !alias.is_empty() && seen_aliases.insert(alias.clone()))
            .enumerate()
        {
            sqlx::query(
                "INSERT OR IGNORE INTO anime_alias(anime_id, alias, priority) VALUES (?, ?, ?)",
            )
            .bind(anime_id)
            .bind(alias)
            .bind(priority as i64)
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query(
            r#"INSERT INTO episode(
                anime_id, episode_no, expected_at, state, next_check_at
            ) VALUES (?, ?, ?, ?, ?)"#,
        )
        .bind(anime_id)
        .bind(new.next_episode)
        .bind(new.expected_at)
        .bind(EpisodeState::Watching.to_string())
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(anime_id)
    }

    pub async fn list_anime(&self) -> Result<Vec<Anime>> {
        Ok(
            sqlx::query_as::<_, Anime>("SELECT * FROM anime ORDER BY id")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub async fn has_bangumi_subject_id(&self, subject_id: i64) -> Result<bool> {
        Ok(sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM anime WHERE bangumi_subject_id = ?)",
        )
        .bind(subject_id)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn get_anime(&self, anime_id: i64) -> Result<AnimeWithAliases> {
        let anime = sqlx::query_as::<_, Anime>("SELECT * FROM anime WHERE id = ?")
            .bind(anime_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("anime {anime_id}")))?;
        let aliases = sqlx::query_scalar::<_, String>(
            "SELECT alias FROM anime_alias WHERE anime_id = ? AND enabled = 1 ORDER BY priority, id",
        )
        .bind(anime_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(AnimeWithAliases { anime, aliases })
    }

    pub async fn delete_anime(&self, anime_id: i64) -> Result<Anime> {
        let mut tx = self.pool.begin().await?;
        let anime = sqlx::query_as::<_, Anime>("SELECT * FROM anime WHERE id = ?")
            .bind(anime_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("anime {anime_id}")))?;
        let result = sqlx::query("DELETE FROM anime WHERE id = ?")
            .bind(anime_id)
            .execute(&mut *tx)
            .await?;
        if result.rows_affected() != 1 {
            return Err(AppError::NotFound(format!("anime {anime_id}")));
        }
        tx.commit().await?;
        Ok(anime)
    }

    pub async fn delete_anime_checked(
        &self,
        anime_id: i64,
        expected_title: &str,
        expected_updated_at: DateTime<Utc>,
    ) -> Result<Anime> {
        let mut tx = self.pool.begin().await?;
        let anime = sqlx::query_as::<_, Anime>("SELECT * FROM anime WHERE id = ?")
            .bind(anime_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("anime {anime_id}")))?;
        if anime.enabled {
            return Err(AppError::InvalidInput(
                "anime must be disabled before permanent deletion".into(),
            ));
        }
        if anime.title != expected_title {
            return Err(AppError::InvalidInput(
                "typed title does not exactly match the current anime title".into(),
            ));
        }
        if anime.updated_at != expected_updated_at {
            return Err(AppError::InvalidInput(
                "anime changed after the deletion page was opened; review it again".into(),
            ));
        }
        sqlx::query("DELETE FROM anime WHERE id = ?")
            .bind(anime_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(anime)
    }

    pub async fn rename_anime(&self, anime_id: i64, title: &str) -> Result<Anime> {
        let title = title.trim();
        if title.is_empty() || title.chars().count() > 200 {
            return Err(AppError::InvalidInput(
                "title must contain between 1 and 200 characters".into(),
            ));
        }

        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        let previous = sqlx::query_as::<_, Anime>("SELECT * FROM anime WHERE id = ?")
            .bind(anime_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("anime {anime_id}")))?;

        sqlx::query("UPDATE anime SET title = ?, updated_at = ? WHERE id = ?")
            .bind(title)
            .bind(now)
            .bind(anime_id)
            .execute(&mut *tx)
            .await?;

        let next_priority = sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(MAX(priority), -1) + 1 FROM anime_alias WHERE anime_id = ?",
        )
        .bind(anime_id)
        .fetch_one(&mut *tx)
        .await?;
        for (offset, alias) in [previous.title.as_str(), title].into_iter().enumerate() {
            sqlx::query(
                r#"INSERT INTO anime_alias(anime_id, alias, priority, enabled)
                   VALUES (?, ?, ?, 1)
                   ON CONFLICT(anime_id, alias) DO UPDATE SET enabled = 1"#,
            )
            .bind(anime_id)
            .bind(alias)
            .bind(next_priority + offset as i64)
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query(
            r#"UPDATE episode SET next_check_at = ?
               WHERE anime_id = ?
                 AND state IN ('waiting','watching','candidate_found','needs_manual_review')"#,
        )
        .bind(now)
        .bind(anime_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(previous)
    }

    pub async fn set_anime_enabled(&self, anime_id: i64, enabled: bool) -> Result<()> {
        let result = sqlx::query("UPDATE anime SET enabled = ?, updated_at = ? WHERE id = ?")
            .bind(enabled)
            .bind(Utc::now())
            .bind(anime_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(AppError::NotFound(format!("anime {anime_id}")));
        }
        Ok(())
    }

    pub async fn active_episode(&self, anime_id: i64) -> Result<Episode> {
        sqlx::query_as::<_, Episode>(
            r#"SELECT * FROM episode
               WHERE anime_id = ? AND state NOT IN ('notified', 'confirmed')
               ORDER BY episode_no LIMIT 1"#,
        )
        .bind(anime_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("active episode for anime {anime_id}")))
    }

    pub async fn episode(&self, episode_id: i64) -> Result<Episode> {
        sqlx::query_as::<_, Episode>("SELECT * FROM episode WHERE id = ?")
            .bind(episode_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("episode {episode_id}")))
    }

    pub async fn due_anime_ids(&self, limit: i64) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar::<_, i64>(
            r#"SELECT e.anime_id FROM episode e
               JOIN anime a ON a.id = e.anime_id
               WHERE a.enabled = 1
                 AND e.state IN ('waiting','watching','candidate_found','needs_manual_review')
                 AND e.next_check_at <= ?
               ORDER BY e.next_check_at LIMIT ?"#,
        )
        .bind(Utc::now())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn enabled_anime_ids(&self) -> Result<Vec<i64>> {
        Ok(
            sqlx::query_scalar::<_, i64>("SELECT id FROM anime WHERE enabled = 1 ORDER BY id")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub async fn auto_schedule_due_ids(&self, limit: i64) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar::<_, i64>(
            r#"SELECT id FROM anime
               WHERE enabled = 1 AND auto_schedule = 1
                 AND (schedule_next_sync_at IS NULL OR schedule_next_sync_at <= ?)
               ORDER BY COALESCE(schedule_next_sync_at, created_at) LIMIT ?"#,
        )
        .bind(Utc::now())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn apply_schedule_update(
        &self,
        anime_id: i64,
        update: &ScheduleUpdate,
    ) -> Result<()> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            r#"UPDATE anime SET
                bangumi_subject_id = ?, expected_weekday = ?, expected_time = ?, timezone = ?,
                broadcast_pattern = ?, schedule_sync_at = ?, schedule_next_sync_at = ?,
                schedule_sync_error = NULL, updated_at = ?
               WHERE id = ? AND auto_schedule = 1"#,
        )
        .bind(update.bangumi_subject_id)
        .bind(update.expected_weekday)
        .bind(&update.expected_time)
        .bind(&update.timezone)
        .bind(&update.broadcast_pattern)
        .bind(now)
        .bind(update.next_sync_at)
        .bind(now)
        .bind(anime_id)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            return Err(AppError::NotFound(format!(
                "auto-scheduled anime {anime_id}"
            )));
        }

        let mut priority = sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(MAX(priority), -1) + 1 FROM anime_alias WHERE anime_id = ?",
        )
        .bind(anime_id)
        .fetch_one(&mut *tx)
        .await?;
        let mut seen_aliases = HashSet::new();
        for alias in update
            .aliases
            .iter()
            .map(|alias| alias.trim())
            .filter(|alias| !alias.is_empty() && seen_aliases.insert((*alias).to_string()))
        {
            let inserted = sqlx::query(
                "INSERT OR IGNORE INTO anime_alias(anime_id, alias, priority) VALUES (?, ?, ?)",
            )
            .bind(anime_id)
            .bind(alias)
            .bind(priority)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if inserted > 0 {
                priority += 1;
            }
        }

        sqlx::query(
            r#"UPDATE episode SET expected_at = ?, next_check_at = ?
               WHERE anime_id = ? AND state IN
                   ('waiting','watching','candidate_found','needs_manual_review')"#,
        )
        .bind(update.expected_at)
        .bind(now)
        .bind(anime_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn mark_schedule_sync_failed(
        &self,
        anime_id: i64,
        error: &str,
        next_sync_at: DateTime<Utc>,
    ) -> Result<()> {
        let safe_error: String = error.chars().take(500).collect();
        sqlx::query(
            r#"UPDATE anime SET schedule_sync_error = ?, schedule_next_sync_at = ?, updated_at = ?
               WHERE id = ? AND auto_schedule = 1"#,
        )
        .bind(safe_error)
        .bind(next_sync_at)
        .bind(Utc::now())
        .bind(anime_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn reschedule_episode(
        &self,
        episode_id: i64,
        next_check_at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query("UPDATE episode SET next_check_at = ? WHERE id = ?")
            .bind(next_check_at)
            .bind(episode_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_episode_manual_review(&self, episode_id: i64) -> Result<()> {
        sqlx::query(
            "UPDATE episode SET state = 'needs_manual_review', first_candidate_at = COALESCE(first_candidate_at, ?) WHERE id = ? AND state NOT IN ('confirmed','notified')",
        )
        .bind(Utc::now())
        .bind(episode_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn upsert_candidate(
        &self,
        episode_id: i64,
        candidate: &VideoCandidate,
        evaluation: &Evaluation,
        state: CandidateState,
    ) -> Result<()> {
        let now = Utc::now();
        let evaluation_json = to_string(evaluation)
            .map_err(|e| AppError::InvalidInput(format!("cannot encode evaluation: {e}")))?;
        let tags_json = to_string(&candidate.tags)
            .map_err(|e| AppError::InvalidInput(format!("cannot encode tags: {e}")))?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"INSERT INTO candidate(
                episode_id, bvid, uploader_mid, uploader_name, title, description,
                duration_sec, published_at, url, tags_json, page_count, score, state,
                first_seen_at, last_seen_at, evaluation_json
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(episode_id, bvid) DO UPDATE SET
                uploader_mid = excluded.uploader_mid,
                uploader_name = excluded.uploader_name,
                title = excluded.title,
                description = excluded.description,
                duration_sec = excluded.duration_sec,
                published_at = excluded.published_at,
                url = excluded.url,
                tags_json = excluded.tags_json,
                page_count = excluded.page_count,
                score = excluded.score,
                state = CASE
                    WHEN candidate.state IN ('confirmed','rejected') THEN candidate.state
                    ELSE excluded.state
                END,
                last_seen_at = excluded.last_seen_at,
                seen_count = candidate.seen_count + 1,
                evaluation_json = excluded.evaluation_json"#,
        )
        .bind(episode_id)
        .bind(&candidate.bvid)
        .bind(candidate.uploader_mid)
        .bind(&candidate.uploader_name)
        .bind(&candidate.title)
        .bind(candidate.description.as_deref())
        .bind(candidate.duration_sec)
        .bind(candidate.published_at)
        .bind(&candidate.url)
        .bind(tags_json)
        .bind(candidate.page_count)
        .bind(evaluation.score)
        .bind(state.to_string())
        .bind(now)
        .bind(now)
        .bind(evaluation_json)
        .execute(&mut *tx)
        .await?;
        if state == CandidateState::Pending {
            sqlx::query(
                "UPDATE episode SET state = CASE WHEN state IN ('waiting','watching') THEN 'candidate_found' ELSE state END, first_candidate_at = COALESCE(first_candidate_at, ?) WHERE id = ?",
            )
            .bind(now)
            .bind(episode_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn active_candidates(&self, episode_id: i64) -> Result<Vec<StoredCandidate>> {
        Ok(sqlx::query_as::<_, StoredCandidate>(
            "SELECT * FROM candidate WHERE episode_id = ? AND state = 'pending' ORDER BY score DESC, first_seen_at",
        )
        .bind(episode_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn list_candidates(&self, state: Option<&str>) -> Result<Vec<CandidateListRow>> {
        let rows = if let Some(state) = state {
            sqlx::query_as::<_, CandidateListRow>(
                r#"SELECT c.episode_id, e.anime_id, c.bvid, a.title AS anime_title, e.episode_no,
                          c.uploader_mid, c.uploader_name, c.title, c.duration_sec,
                          c.published_at, c.score, c.state, c.seen_count, c.evaluation_json, c.url
                   FROM candidate c
                   JOIN episode e ON e.id = c.episode_id
                   JOIN anime a ON a.id = e.anime_id
                   WHERE c.state = ? ORDER BY c.last_seen_at DESC"#,
            )
            .bind(state)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, CandidateListRow>(
                r#"SELECT c.episode_id, e.anime_id, c.bvid, a.title AS anime_title, e.episode_no,
                          c.uploader_mid, c.uploader_name, c.title, c.duration_sec,
                          c.published_at, c.score, c.state, c.seen_count, c.evaluation_json, c.url
                   FROM candidate c
                   JOIN episode e ON e.id = c.episode_id
                   JOIN anime a ON a.id = e.anime_id
                   ORDER BY c.last_seen_at DESC"#,
            )
            .fetch_all(&self.pool)
            .await?
        };
        Ok(rows)
    }

    pub async fn candidate_context(&self, bvid: &str) -> Result<(StoredCandidate, Episode)> {
        let candidate = sqlx::query_as::<_, StoredCandidate>(
            "SELECT * FROM candidate WHERE bvid = ? ORDER BY last_seen_at DESC LIMIT 1",
        )
        .bind(bvid)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("candidate {bvid}")))?;
        let episode = self.episode(candidate.episode_id).await?;
        Ok((candidate, episode))
    }

    pub async fn uploader_trust(&self, anime_id: i64, mid: i64) -> Result<UploaderTrust> {
        Ok(sqlx::query_as::<_, UploaderTrust>(
            "SELECT * FROM uploader_trust WHERE anime_id = ? AND uploader_mid = ?",
        )
        .bind(anime_id)
        .bind(mid)
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or(UploaderTrust {
            anime_id,
            uploader_mid: mid,
            ..UploaderTrust::default()
        }))
    }

    pub async fn set_uploader_flag(
        &self,
        anime_id: i64,
        mid: i64,
        trusted: bool,
        blocked: bool,
    ) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO uploader_trust(
                anime_id, uploader_mid, manually_trusted, manually_blocked
            ) VALUES (?, ?, ?, ?)
            ON CONFLICT(anime_id, uploader_mid) DO UPDATE SET
                manually_trusted = excluded.manually_trusted,
                manually_blocked = excluded.manually_blocked"#,
        )
        .bind(anime_id)
        .bind(mid)
        .bind(trusted)
        .bind(blocked)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn block_uploader(&self, anime_id: i64, mid: i64) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let anime_exists =
            sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM anime WHERE id = ?)")
                .bind(anime_id)
                .fetch_one(&mut *tx)
                .await?;
        if !anime_exists {
            return Err(AppError::NotFound(format!("anime {anime_id}")));
        }
        let uploader_name = sqlx::query_scalar::<_, String>(
            r#"SELECT c.uploader_name
               FROM candidate c JOIN episode e ON e.id = c.episode_id
               WHERE e.anime_id = ? AND c.uploader_mid = ?
               ORDER BY c.last_seen_at DESC LIMIT 1"#,
        )
        .bind(anime_id)
        .bind(mid)
        .fetch_optional(&mut *tx)
        .await?;
        sqlx::query(
            r#"INSERT INTO uploader_trust(
                anime_id, uploader_mid, uploader_name, manually_trusted, manually_blocked
            ) VALUES (?, ?, ?, 0, 1)
            ON CONFLICT(anime_id, uploader_mid) DO UPDATE SET
                uploader_name = COALESCE(excluded.uploader_name, uploader_trust.uploader_name),
                manually_trusted = 0,
                manually_blocked = 1"#,
        )
        .bind(anime_id)
        .bind(mid)
        .bind(uploader_name)
        .execute(&mut *tx)
        .await?;
        let rejected = sqlx::query(
            r#"UPDATE candidate SET state = 'rejected'
               WHERE state = 'pending' AND uploader_mid = ?
                 AND episode_id IN (SELECT id FROM episode WHERE anime_id = ?)"#,
        )
        .bind(mid)
        .bind(anime_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sqlx::query(
            r#"UPDATE episode SET state = 'watching'
               WHERE anime_id = ? AND state IN ('candidate_found','needs_manual_review')
                 AND NOT EXISTS (
                   SELECT 1 FROM candidate c
                   WHERE c.episode_id = episode.id AND c.state = 'pending'
                 )"#,
        )
        .bind(anime_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"UPDATE review_notification SET status = 'cancelled'
               WHERE status = 'pending' AND episode_id IN (
                 SELECT e.id FROM episode e
                 WHERE e.anime_id = ? AND NOT EXISTS (
                   SELECT 1 FROM candidate c
                   WHERE c.episode_id = e.id AND c.state = 'pending'
                 )
               )"#,
        )
        .bind(anime_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rejected)
    }

    pub async fn confirm_candidate(
        &self,
        episode_id: i64,
        bvid: &str,
        reason: &str,
        channel: &str,
        user_confirmed: bool,
    ) -> Result<()> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        let (candidate_id, previous_state) = sqlx::query_as::<_, (i64, String)>(
            "SELECT id, state FROM candidate WHERE episode_id = ? AND bvid = ?",
        )
        .bind(episode_id)
        .bind(bvid)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("candidate {bvid}")))?;
        sqlx::query("UPDATE candidate SET state = 'confirmed' WHERE id = ?")
            .bind(candidate_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE episode SET state = 'confirmed', confirmed_at = COALESCE(confirmed_at, ?) WHERE id = ? AND state != 'notified'",
        )
        .bind(now)
        .bind(episode_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"INSERT OR IGNORE INTO notification(
                episode_id, candidate_id, channel, confirmation_reason, status, created_at
            ) VALUES (?, ?, ?, ?, 'pending', ?)"#,
        )
        .bind(episode_id)
        .bind(candidate_id)
        .bind(channel)
        .bind(reason)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE review_notification SET status = 'cancelled' WHERE episode_id = ? AND status = 'pending'",
        )
        .bind(episode_id)
        .execute(&mut *tx)
        .await?;
        if user_confirmed && previous_state != CandidateState::Confirmed.to_string() {
            Self::increment_trust_tx(&mut tx, candidate_id, true).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn reject_candidate(&self, bvid: &str, user_rejected: bool) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let (candidate_id, previous_state) = sqlx::query_as::<_, (i64, String)>(
            "SELECT id, state FROM candidate WHERE bvid = ? ORDER BY last_seen_at DESC LIMIT 1",
        )
        .bind(bvid)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("candidate {bvid}")))?;
        sqlx::query("UPDATE candidate SET state = 'rejected' WHERE id = ?")
            .bind(candidate_id)
            .execute(&mut *tx)
            .await?;
        if user_rejected && previous_state != CandidateState::Rejected.to_string() {
            Self::increment_trust_tx(&mut tx, candidate_id, false).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn reject_all_candidates(&self, episode_id: i64, user_rejected: bool) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let candidate_ids = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM candidate WHERE episode_id = ? AND state = 'pending' ORDER BY id",
        )
        .bind(episode_id)
        .fetch_all(&mut *tx)
        .await?;
        if user_rejected {
            for candidate_id in &candidate_ids {
                Self::increment_trust_tx(&mut tx, *candidate_id, false).await?;
            }
        }
        let result = sqlx::query(
            "UPDATE candidate SET state = 'rejected' WHERE episode_id = ? AND state = 'pending'",
        )
        .bind(episode_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"UPDATE episode SET state = 'watching'
               WHERE id = ? AND state IN ('candidate_found','needs_manual_review')"#,
        )
        .bind(episode_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }

    pub async fn pending_candidate_fingerprint(&self, episode_id: i64) -> Result<String> {
        let bvids = sqlx::query_scalar::<_, String>(
            "SELECT bvid FROM candidate WHERE episode_id = ? AND state = 'pending' ORDER BY bvid",
        )
        .bind(episode_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(candidate_fingerprint(&bvids))
    }

    pub async fn reject_all_candidates_checked(
        &self,
        episode_id: i64,
        expected_fingerprint: &str,
    ) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query_as::<_, (i64, String)>(
            "SELECT id, bvid FROM candidate WHERE episode_id = ? AND state = 'pending' ORDER BY bvid",
        )
        .bind(episode_id)
        .fetch_all(&mut *tx)
        .await?;
        let bvids: Vec<String> = rows.iter().map(|(_, bvid)| bvid.clone()).collect();
        if candidate_fingerprint(&bvids) != expected_fingerprint {
            return Err(AppError::InvalidInput(
                "candidate set changed; review the episode again".into(),
            ));
        }
        for (candidate_id, _) in &rows {
            Self::increment_trust_tx(&mut tx, *candidate_id, false).await?;
        }
        let result = sqlx::query(
            "UPDATE candidate SET state = 'rejected' WHERE episode_id = ? AND state = 'pending'",
        )
        .bind(episode_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"UPDATE episode SET state = 'watching'
               WHERE id = ? AND state IN ('candidate_found','needs_manual_review')"#,
        )
        .bind(episode_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }

    async fn increment_trust_tx(
        tx: &mut Transaction<'_, Sqlite>,
        candidate_id: i64,
        confirmed: bool,
    ) -> Result<()> {
        #[derive(FromRow)]
        struct TrustSource {
            anime_id: i64,
            uploader_mid: i64,
            uploader_name: String,
        }
        let source = sqlx::query_as::<_, TrustSource>(
            r#"SELECT e.anime_id, c.uploader_mid, c.uploader_name
               FROM candidate c JOIN episode e ON e.id = c.episode_id WHERE c.id = ?"#,
        )
        .bind(candidate_id)
        .fetch_one(&mut **tx)
        .await?;
        let (confirmed_delta, rejected_delta) = if confirmed { (1, 0) } else { (0, 1) };
        sqlx::query(
            r#"INSERT INTO uploader_trust(
                anime_id, uploader_mid, uploader_name, confirmed_count, rejected_count
            ) VALUES (?, ?, ?, ?, ?)
            ON CONFLICT(anime_id, uploader_mid) DO UPDATE SET
                uploader_name = excluded.uploader_name,
                confirmed_count = uploader_trust.confirmed_count + excluded.confirmed_count,
                rejected_count = uploader_trust.rejected_count + excluded.rejected_count"#,
        )
        .bind(source.anime_id)
        .bind(source.uploader_mid)
        .bind(source.uploader_name)
        .bind(confirmed_delta)
        .bind(rejected_delta)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    pub async fn expire_candidates(&self, expire_before: DateTime<Utc>) -> Result<u64> {
        Ok(sqlx::query(
            "UPDATE candidate SET state = 'expired' WHERE state = 'pending' AND last_seen_at < ?",
        )
        .bind(expire_before)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    pub async fn pending_notifications(&self) -> Result<Vec<PendingNotification>> {
        Ok(sqlx::query_as::<_, PendingNotification>(
            r#"SELECT n.id, n.episode_id, n.channel, n.attempts,
                      a.title AS anime_title, e.episode_no,
                      c.bvid, c.uploader_name, c.duration_sec, c.url,
                      n.confirmation_reason
               FROM notification n
               JOIN episode e ON e.id = n.episode_id
               JOIN anime a ON a.id = e.anime_id
               LEFT JOIN candidate c ON c.id = n.candidate_id
               WHERE n.status = 'pending'
                 AND (n.next_attempt_at IS NULL OR n.next_attempt_at <= ?)
               ORDER BY n.created_at"#,
        )
        .bind(Utc::now())
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn enqueue_review_notification(
        &self,
        episode_id: i64,
        channel: &str,
        candidate_fingerprint: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"INSERT OR IGNORE INTO review_notification(
                   episode_id, channel, candidate_fingerprint, status, created_at
               ) VALUES (?, ?, ?, 'pending', ?)"#,
        )
        .bind(episode_id)
        .bind(channel)
        .bind(candidate_fingerprint)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn pending_review_notifications(
        &self,
        public_url: &str,
    ) -> Result<Vec<PendingReviewNotification>> {
        #[derive(FromRow)]
        struct ReviewRow {
            id: i64,
            episode_id: i64,
            channel: String,
            attempts: i64,
            anime_title: String,
            episode_no: i64,
            candidate_fingerprint: String,
            anime_updated_at: DateTime<Utc>,
        }
        let rows = sqlx::query_as::<_, ReviewRow>(
            r#"SELECT r.id, r.episode_id, r.channel, r.attempts,
                      a.title AS anime_title, e.episode_no, r.candidate_fingerprint,
                      a.updated_at AS anime_updated_at
               FROM review_notification r
               JOIN episode e ON e.id = r.episode_id
               JOIN anime a ON a.id = e.anime_id
               WHERE r.status = 'pending'
                 AND (r.next_attempt_at IS NULL OR r.next_attempt_at <= ?)
               ORDER BY r.created_at, r.id"#,
        )
        .bind(Utc::now())
        .fetch_all(&self.pool)
        .await?;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let stored_candidates = sqlx::query_as::<_, StoredCandidate>(
                "SELECT * FROM candidate WHERE episode_id = ? AND state = 'pending' ORDER BY score DESC, id LIMIT 5",
            )
            .bind(row.episode_id)
            .fetch_all(&self.pool)
            .await?;
            let current_bvids = sqlx::query_scalar::<_, String>(
                "SELECT bvid FROM candidate WHERE episode_id = ? AND state = 'pending' ORDER BY bvid",
            )
            .bind(row.episode_id)
            .fetch_all(&self.pool)
            .await?;
            let obsolete = if row.candidate_fingerprint.starts_with("none:") {
                !current_bvids.is_empty()
                    || row.candidate_fingerprint
                        != format!("none:{}", row.anime_updated_at.timestamp())
            } else {
                candidate_fingerprint(&current_bvids) != row.candidate_fingerprint
            };
            if obsolete {
                sqlx::query(
                    "UPDATE review_notification SET status = 'cancelled' WHERE id = ? AND status = 'pending'",
                )
                .bind(row.id)
                .execute(&self.pool)
                .await?;
                continue;
            }
            let candidates = stored_candidates
                .into_iter()
                .map(|candidate| ReviewCandidateSummary {
                    bvid: candidate.bvid,
                    title: candidate.title,
                    uploader_name: candidate.uploader_name,
                    duration_sec: candidate.duration_sec,
                    score: candidate.score,
                    url: candidate.url,
                })
                .collect();
            events.push(PendingReviewNotification {
                id: row.id,
                episode_id: row.episode_id,
                channel: row.channel,
                attempts: row.attempts,
                anime_title: row.anime_title,
                episode_no: row.episode_no,
                candidate_fingerprint: row.candidate_fingerprint,
                review_url: format!(
                    "{}/review/episodes/{}",
                    public_url.trim_end_matches('/'),
                    row.episode_id
                ),
                candidates,
            });
        }
        Ok(events)
    }

    pub async fn mark_review_notification_sent(&self, id: i64) -> Result<()> {
        sqlx::query(
            "UPDATE review_notification SET status = 'sent', attempts = attempts + 1, sent_at = ?, last_error = NULL WHERE id = ?",
        )
        .bind(Utc::now())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_review_notification_failed(&self, id: i64, error: &str) -> Result<()> {
        let safe_error: String = error.chars().take(500).collect();
        let attempts =
            sqlx::query_scalar::<_, i64>("SELECT attempts FROM review_notification WHERE id = ?")
                .bind(id)
                .fetch_one(&self.pool)
                .await?;
        let exponent = attempts.clamp(0, 6) as u32;
        let retry_at = Utc::now() + chrono::Duration::seconds(60 * 2_i64.pow(exponent));
        sqlx::query(
            "UPDATE review_notification SET attempts = attempts + 1, last_error = ?, next_attempt_at = ? WHERE id = ?",
        )
        .bind(safe_error)
        .bind(retry_at)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_notification_failed(&self, id: i64, error: &str) -> Result<()> {
        let safe_error: String = error.chars().take(500).collect();
        let attempts =
            sqlx::query_scalar::<_, i64>("SELECT attempts FROM notification WHERE id = ?")
                .bind(id)
                .fetch_one(&self.pool)
                .await?;
        let exponent = attempts.clamp(0, 6) as u32;
        let retry_at = Utc::now() + chrono::Duration::seconds(60 * 2_i64.pow(exponent));
        sqlx::query(
            "UPDATE notification SET attempts = attempts + 1, last_error = ?, next_attempt_at = ? WHERE id = ?",
        )
            .bind(safe_error)
            .bind(retry_at)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn mark_notification_sent(&self, id: i64) -> Result<()> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        let episode = sqlx::query_as::<_, Episode>(
            "SELECT e.* FROM episode e JOIN notification n ON n.episode_id = e.id WHERE n.id = ?",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE notification SET status = 'sent', attempts = attempts + 1, sent_at = ?, last_error = NULL WHERE id = ?",
        )
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE episode SET state = 'notified', notified_at = ? WHERE id = ?")
            .bind(now)
            .bind(episode.id)
            .execute(&mut *tx)
            .await?;

        let next_expected = episode
            .expected_at
            .and_then(|at| at.checked_add_days(Days::new(7)));
        let next_check = now + chrono::Duration::hours(6);
        sqlx::query(
            r#"INSERT OR IGNORE INTO episode(
                anime_id, episode_no, expected_at, state, next_check_at
            ) VALUES (?, ?, ?, 'waiting', ?)"#,
        )
        .bind(episode.anime_id)
        .bind(episode.episode_no + 1)
        .bind(next_expected)
        .bind(next_check)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE anime SET schedule_next_sync_at = ? WHERE id = ? AND auto_schedule = 1",
        )
        .bind(now)
        .bind(episode.anime_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn web_admin_count(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM web_admin")
            .fetch_one(&self.pool)
            .await?)
    }

    pub async fn web_admin_by_username(&self, username: &str) -> Result<Option<WebAdmin>> {
        Ok(
            sqlx::query_as::<_, WebAdmin>("SELECT * FROM web_admin WHERE username = ?")
                .bind(username)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn create_web_admin(&self, username: &str, password_hash: &str) -> Result<i64> {
        let now = Utc::now();
        let result = sqlx::query(
            r#"INSERT INTO web_admin(
                   username, password_hash, role, disabled, created_at, password_changed_at
               ) VALUES (?, ?, 'owner', 0, ?, ?)"#,
        )
        .bind(username)
        .bind(password_hash)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(result.last_insert_rowid())
    }

    pub async fn reset_web_admin_password(
        &self,
        username: &str,
        password_hash: &str,
    ) -> Result<()> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        let admin_id = sqlx::query_scalar::<_, i64>("SELECT id FROM web_admin WHERE username = ?")
            .bind(username)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("web admin {username}")))?;
        sqlx::query("UPDATE web_admin SET password_hash = ?, password_changed_at = ? WHERE id = ?")
            .bind(password_hash)
            .bind(now)
            .bind(admin_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE web_session SET revoked_at = ? WHERE admin_id = ? AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(admin_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_web_admin_disabled(&self, username: &str, disabled: bool) -> Result<()> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        let admin_id = sqlx::query_scalar::<_, i64>("SELECT id FROM web_admin WHERE username = ?")
            .bind(username)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("web admin {username}")))?;
        sqlx::query("UPDATE web_admin SET disabled = ? WHERE id = ?")
            .bind(disabled)
            .bind(admin_id)
            .execute(&mut *tx)
            .await?;
        if disabled {
            sqlx::query(
                "UPDATE web_session SET revoked_at = ? WHERE admin_id = ? AND revoked_at IS NULL",
            )
            .bind(now)
            .bind(admin_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn revoke_web_admin_sessions(&self, username: &str) -> Result<u64> {
        let admin_id = sqlx::query_scalar::<_, i64>("SELECT id FROM web_admin WHERE username = ?")
            .bind(username)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("web admin {username}")))?;
        Ok(sqlx::query(
            "UPDATE web_session SET revoked_at = ? WHERE admin_id = ? AND revoked_at IS NULL",
        )
        .bind(Utc::now())
        .bind(admin_id)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_web_session(
        &self,
        token_hmac: &[u8],
        admin_id: i64,
        idle_expires_at: DateTime<Utc>,
        absolute_expires_at: DateTime<Utc>,
        user_agent_hash: Option<&[u8]>,
        source_ip_hash: Option<&[u8]>,
    ) -> Result<()> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"INSERT INTO web_session(
                   token_hmac, admin_id, created_at, renewed_at, last_seen_at,
                   idle_expires_at, absolute_expires_at, user_agent_hash, source_ip_hash
               ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(token_hmac)
        .bind(admin_id)
        .bind(now)
        .bind(now)
        .bind(now)
        .bind(idle_expires_at)
        .bind(absolute_expires_at)
        .bind(user_agent_hash)
        .bind(source_ip_hash)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE web_admin SET last_login_at = ? WHERE id = ?")
            .bind(now)
            .bind(admin_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn authenticated_session(
        &self,
        token_hmac: &[u8],
    ) -> Result<Option<AuthenticatedSession>> {
        let now = Utc::now();
        Ok(sqlx::query_as::<_, AuthenticatedSession>(
            r#"SELECT s.token_hmac, s.admin_id, a.username, a.role,
                      s.created_at, s.renewed_at, s.last_seen_at,
                      s.idle_expires_at, s.absolute_expires_at
               FROM web_session s
               JOIN web_admin a ON a.id = s.admin_id
               WHERE s.token_hmac = ?
                 AND s.revoked_at IS NULL
                 AND a.disabled = 0
                 AND s.idle_expires_at > ?
                 AND s.absolute_expires_at > ?"#,
        )
        .bind(token_hmac)
        .bind(now)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn touch_web_session(
        &self,
        token_hmac: &[u8],
        idle_expires_at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE web_session SET last_seen_at = ?, idle_expires_at = ? WHERE token_hmac = ? AND revoked_at IS NULL",
        )
        .bind(Utc::now())
        .bind(idle_expires_at)
        .bind(token_hmac)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn rotate_web_session(
        &self,
        old_token_hmac: &[u8],
        new_token_hmac: &[u8],
        idle_expires_at: DateTime<Utc>,
    ) -> Result<()> {
        let result = sqlx::query(
            r#"UPDATE web_session
               SET token_hmac = ?, renewed_at = ?, last_seen_at = ?, idle_expires_at = ?
               WHERE token_hmac = ? AND revoked_at IS NULL"#,
        )
        .bind(new_token_hmac)
        .bind(Utc::now())
        .bind(Utc::now())
        .bind(idle_expires_at)
        .bind(old_token_hmac)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(AppError::NotFound("active web session".into()));
        }
        Ok(())
    }

    pub async fn revoke_web_session(&self, token_hmac: &[u8]) -> Result<()> {
        sqlx::query(
            "UPDATE web_session SET revoked_at = ? WHERE token_hmac = ? AND revoked_at IS NULL",
        )
        .bind(Utc::now())
        .bind(token_hmac)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn auth_throttle_blocked_until(
        &self,
        key_hmac: &[u8],
    ) -> Result<Option<DateTime<Utc>>> {
        Ok(
            sqlx::query_scalar("SELECT blocked_until FROM auth_throttle WHERE key_hmac = ?")
                .bind(key_hmac)
                .fetch_optional(&self.pool)
                .await?
                .flatten(),
        )
    }

    pub async fn record_auth_failure(
        &self,
        key_hmac: &[u8],
        window_secs: i64,
        max_failures: i64,
    ) -> Result<Option<DateTime<Utc>>> {
        #[derive(FromRow)]
        struct ThrottleRow {
            window_started_at: DateTime<Utc>,
            failure_count: i64,
        }
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        let existing = sqlx::query_as::<_, ThrottleRow>(
            "SELECT window_started_at, failure_count FROM auth_throttle WHERE key_hmac = ?",
        )
        .bind(key_hmac)
        .fetch_optional(&mut *tx)
        .await?;
        let (window_started_at, failure_count) = match existing {
            Some(row) if row.window_started_at + chrono::Duration::seconds(window_secs) > now => {
                (row.window_started_at, row.failure_count + 1)
            }
            _ => (now, 1),
        };
        let blocked_until = (failure_count >= max_failures).then(|| {
            let exponent = (failure_count - max_failures).clamp(0, 6) as u32;
            now + chrono::Duration::seconds(
                window_secs.saturating_mul(2_i64.pow(exponent)).min(86_400),
            )
        });
        sqlx::query(
            r#"INSERT INTO auth_throttle(
                   key_hmac, window_started_at, failure_count, blocked_until, updated_at
               ) VALUES (?, ?, ?, ?, ?)
               ON CONFLICT(key_hmac) DO UPDATE SET
                   window_started_at = excluded.window_started_at,
                   failure_count = excluded.failure_count,
                   blocked_until = excluded.blocked_until,
                   updated_at = excluded.updated_at"#,
        )
        .bind(key_hmac)
        .bind(window_started_at)
        .bind(failure_count)
        .bind(blocked_until)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(blocked_until)
    }

    pub async fn clear_auth_throttle(&self, keys: &[Vec<u8>]) -> Result<()> {
        for key in keys {
            sqlx::query("DELETE FROM auth_throttle WHERE key_hmac = ?")
                .bind(key)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    pub async fn create_anime_draft(
        &self,
        id: &str,
        admin_id: i64,
        session_token_hmac: &[u8],
        payload_json: &str,
        state: &str,
        resolved_json: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now();
        sqlx::query(
            r#"INSERT INTO anime_draft(
                   id, admin_id, session_token_hmac, payload_json, state,
                   resolved_json, created_at, expires_at
               ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(id)
        .bind(admin_id)
        .bind(session_token_hmac)
        .bind(payload_json)
        .bind(state)
        .bind(resolved_json)
        .bind(now)
        .bind(now + chrono::Duration::minutes(15))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn anime_draft(
        &self,
        id: &str,
        admin_id: i64,
        session_token_hmac: &[u8],
    ) -> Result<AnimeDraftRow> {
        sqlx::query_as::<_, AnimeDraftRow>(
            r#"SELECT * FROM anime_draft
               WHERE id = ? AND admin_id = ? AND session_token_hmac = ?
                 AND consumed_at IS NULL AND expires_at > ?"#,
        )
        .bind(id)
        .bind(admin_id)
        .bind(session_token_hmac)
        .bind(Utc::now())
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("active anime draft {id}")))
    }

    pub async fn anime_draft_payload_for_resolution(&self, id: &str) -> Result<String> {
        sqlx::query_scalar(
            r#"SELECT payload_json FROM anime_draft
               WHERE id = ? AND state = 'queued' AND consumed_at IS NULL AND expires_at > ?"#,
        )
        .bind(id)
        .bind(Utc::now())
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("queued anime draft {id}")))
    }

    pub async fn mark_anime_draft_ready(&self, id: &str, resolved_json: &str) -> Result<()> {
        sqlx::query(
            "UPDATE anime_draft SET state = 'ready', resolved_json = ?, error = NULL WHERE id = ? AND consumed_at IS NULL",
        )
        .bind(resolved_json)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_anime_draft_failed(&self, id: &str, error: &str) -> Result<()> {
        let safe_error: String = error.chars().take(500).collect();
        sqlx::query(
            "UPDATE anime_draft SET state = 'failed', error = ? WHERE id = ? AND consumed_at IS NULL",
        )
        .bind(safe_error)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn claim_anime_draft(
        &self,
        id: &str,
        admin_id: i64,
        session_token_hmac: &[u8],
    ) -> Result<AnimeDraftRow> {
        sqlx::query_as::<_, AnimeDraftRow>(
            r#"UPDATE anime_draft SET consumed_at = ?
               WHERE id = ? AND admin_id = ? AND session_token_hmac = ?
                 AND state = 'ready' AND consumed_at IS NULL AND expires_at > ?
               RETURNING *"#,
        )
        .bind(Utc::now())
        .bind(id)
        .bind(admin_id)
        .bind(session_token_hmac)
        .bind(Utc::now())
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("ready anime draft {id}")))
    }

    pub async fn release_anime_draft(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE anime_draft SET consumed_at = NULL WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn create_action_nonce(
        &self,
        nonce_hmac: &[u8],
        admin_id: i64,
        action: &str,
        entity_id: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now();
        sqlx::query(
            r#"INSERT INTO web_action_nonce(
                   nonce_hmac, admin_id, action, entity_id, created_at, expires_at
               ) VALUES (?, ?, ?, ?, ?, ?)"#,
        )
        .bind(nonce_hmac)
        .bind(admin_id)
        .bind(action)
        .bind(entity_id)
        .bind(now)
        .bind(now + chrono::Duration::minutes(10))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn consume_action_nonce(
        &self,
        nonce_hmac: &[u8],
        admin_id: i64,
        action: &str,
        entity_id: Option<&str>,
    ) -> Result<bool> {
        Ok(sqlx::query(
            r#"UPDATE web_action_nonce SET consumed_at = ?
               WHERE nonce_hmac = ? AND admin_id = ? AND action = ?
                 AND entity_id IS ? AND consumed_at IS NULL AND expires_at > ?"#,
        )
        .bind(Utc::now())
        .bind(nonce_hmac)
        .bind(admin_id)
        .bind(action)
        .bind(entity_id)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    pub async fn enqueue_management_job(
        &self,
        kind: &str,
        target_type: Option<&str>,
        target_id: Option<&str>,
        payload_json: &str,
        requested_by: Option<i64>,
        dedupe_key: Option<&str>,
    ) -> Result<i64> {
        let now = Utc::now();
        let result = sqlx::query(
            r#"INSERT OR IGNORE INTO management_job(
                   kind, target_type, target_id, payload_json, state,
                   requested_by, dedupe_key, created_at
               ) VALUES (?, ?, ?, ?, 'queued', ?, ?, ?)"#,
        )
        .bind(kind)
        .bind(target_type)
        .bind(target_id)
        .bind(payload_json)
        .bind(requested_by)
        .bind(dedupe_key)
        .bind(now)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 1 {
            return Ok(result.last_insert_rowid());
        }
        let dedupe_key = dedupe_key.ok_or_else(|| {
            AppError::Database(sqlx::Error::Protocol(
                "management job insert was ignored without a dedupe key".into(),
            ))
        })?;
        sqlx::query_scalar::<_, i64>(
            r#"SELECT id FROM management_job
               WHERE dedupe_key = ? AND state IN ('queued','running')
               ORDER BY id DESC LIMIT 1"#,
        )
        .bind(dedupe_key)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("active management job {dedupe_key}")))
    }

    pub async fn claim_management_jobs(&self, limit: i64) -> Result<Vec<ManagementJob>> {
        let now = Utc::now();
        let mut jobs = sqlx::query_as::<_, ManagementJob>(
            r#"UPDATE management_job
               SET state = 'running', started_at = ?, attempts = attempts + 1, error = NULL
               WHERE id IN (
                   SELECT id FROM management_job
                   WHERE state = 'queued'
                   ORDER BY created_at, id
                   LIMIT ?
               ) AND state = 'queued'
               RETURNING *"#,
        )
        .bind(now)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await?;
        jobs.sort_by_key(|job| job.id);
        Ok(jobs)
    }

    pub async fn recover_stale_management_jobs(&self, stale_before: DateTime<Utc>) -> Result<u64> {
        Ok(sqlx::query(
            r#"UPDATE management_job
               SET state = 'queued', started_at = NULL, finished_at = NULL,
                   error = 'recovered after scheduler interruption'
               WHERE state = 'running' AND started_at < ?"#,
        )
        .bind(stale_before)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    pub async fn complete_management_job(&self, id: i64) -> Result<()> {
        sqlx::query(
            "UPDATE management_job SET state = 'completed', finished_at = ?, error = NULL WHERE id = ? AND state = 'running'",
        )
        .bind(Utc::now())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn fail_management_job(&self, id: i64, error: &str) -> Result<()> {
        let safe_error: String = error.chars().take(500).collect();
        sqlx::query(
            "UPDATE management_job SET state = 'failed', finished_at = ?, error = ? WHERE id = ? AND state = 'running'",
        )
        .bind(Utc::now())
        .bind(safe_error)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_management_jobs(&self, limit: i64) -> Result<Vec<ManagementJob>> {
        Ok(sqlx::query_as::<_, ManagementJob>(
            "SELECT * FROM management_job ORDER BY created_at DESC, id DESC LIMIT ?",
        )
        .bind(limit.clamp(1, 200))
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn update_scheduler_heartbeat(&self) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO scheduler_state(name, heartbeat_at, metadata_json)
               VALUES ('main', ?, '{}')
               ON CONFLICT(name) DO UPDATE SET heartbeat_at = excluded.heartbeat_at"#,
        )
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn scheduler_heartbeat(&self) -> Result<Option<DateTime<Utc>>> {
        Ok(
            sqlx::query_scalar("SELECT heartbeat_at FROM scheduler_state WHERE name = 'main'")
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn record_audit(
        &self,
        actor_type: &str,
        actor_admin_id: Option<i64>,
        action: &str,
        entity_type: Option<&str>,
        entity_id: Option<&str>,
        outcome: &str,
        request_id: Option<&str>,
        source_ip_hash: Option<&[u8]>,
        metadata_json: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO audit_event(
                   actor_type, actor_admin_id, action, entity_type, entity_id,
                   outcome, request_id, source_ip_hash, metadata_json, created_at
               ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(actor_type)
        .bind(actor_admin_id)
        .bind(action)
        .bind(entity_type)
        .bind(entity_id)
        .bind(outcome)
        .bind(request_id)
        .bind(source_ip_hash)
        .bind(metadata_json)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_audit_events(&self, limit: i64) -> Result<Vec<AuditEventRow>> {
        Ok(sqlx::query_as::<_, AuditEventRow>(
            r#"SELECT id, actor_type, actor_admin_id, action, entity_type, entity_id,
                      outcome, request_id, metadata_json, created_at
               FROM audit_event ORDER BY created_at DESC, id DESC LIMIT ?"#,
        )
        .bind(limit.clamp(1, 200))
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn cleanup_web_ephemera(&self) -> Result<()> {
        let now = Utc::now();
        let retained_since = now - chrono::Duration::days(30);
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"DELETE FROM web_session
               WHERE idle_expires_at <= ? OR absolute_expires_at <= ?
                  OR (revoked_at IS NOT NULL AND revoked_at <= ?)"#,
        )
        .bind(now)
        .bind(now)
        .bind(retained_since)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM web_action_nonce WHERE expires_at <= ? OR consumed_at IS NOT NULL",
        )
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM anime_draft WHERE expires_at <= ? OR consumed_at IS NOT NULL")
            .bind(now)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM auth_throttle WHERE updated_at <= ?")
            .bind(retained_since)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn dashboard_stats(&self) -> Result<DashboardStats> {
        let anime_count = sqlx::query_scalar("SELECT COUNT(*) FROM anime")
            .fetch_one(&self.pool)
            .await?;
        let enabled_anime_count =
            sqlx::query_scalar("SELECT COUNT(*) FROM anime WHERE enabled = 1")
                .fetch_one(&self.pool)
                .await?;
        let pending_candidate_count =
            sqlx::query_scalar("SELECT COUNT(*) FROM candidate WHERE state = 'pending'")
                .fetch_one(&self.pool)
                .await?;
        let pending_notification_count =
            sqlx::query_scalar("SELECT COUNT(*) FROM notification WHERE status = 'pending'")
                .fetch_one(&self.pool)
                .await?;
        let failed_notification_count = sqlx::query_scalar(
            "SELECT COUNT(*) FROM notification WHERE status = 'pending' AND last_error IS NOT NULL",
        )
        .fetch_one(&self.pool)
        .await?;
        let queued_job_count =
            sqlx::query_scalar("SELECT COUNT(*) FROM management_job WHERE state = 'queued'")
                .fetch_one(&self.pool)
                .await?;
        let failed_job_count =
            sqlx::query_scalar("SELECT COUNT(*) FROM management_job WHERE state = 'failed'")
                .fetch_one(&self.pool)
                .await?;
        let scheduler_heartbeat = self.scheduler_heartbeat().await?;
        let provider_backoff_until = sqlx::query_scalar(
            "SELECT backoff_until FROM provider_state WHERE provider = 'bilibili'",
        )
        .fetch_optional(&self.pool)
        .await?
        .flatten();
        Ok(DashboardStats {
            anime_count,
            enabled_anime_count,
            pending_candidate_count,
            pending_notification_count,
            failed_notification_count,
            queued_job_count,
            failed_job_count,
            scheduler_heartbeat,
            provider_backoff_until,
        })
    }

    pub async fn reserve_provider_request(&self, max_per_day: u32) -> Result<()> {
        let now = Utc::now();
        let today = now.date_naive();
        let mut tx = self.pool.begin().await?;
        let state = sqlx::query_as::<_, ProviderState>(
            "SELECT backoff_until, daily_date, daily_count FROM provider_state WHERE provider = 'bilibili'",
        )
        .fetch_one(&mut *tx)
        .await?;
        if let Some(until) = state.backoff_until.filter(|until| *until > now) {
            return Err(AppError::InvalidInput(format!(
                "Bilibili global backoff active until {until}"
            )));
        }
        let count = if state.daily_date == Some(today) {
            state.daily_count
        } else {
            0
        };
        if count >= i64::from(max_per_day) {
            return Err(AppError::InvalidInput(format!(
                "Bilibili daily safety budget exhausted ({max_per_day})"
            )));
        }
        sqlx::query(
            "UPDATE provider_state SET daily_date = ?, daily_count = ?, updated_at = ? WHERE provider = 'bilibili'",
        )
        .bind(today)
        .bind(count + 1)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn record_provider_failure(
        &self,
        base_seconds: i64,
        max_seconds: i64,
    ) -> Result<DateTime<Utc>> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        let failures = sqlx::query_scalar::<_, i64>(
            "SELECT consecutive_failures FROM provider_state WHERE provider = 'bilibili'",
        )
        .fetch_one(&mut *tx)
        .await?
            + 1;
        let exponent = (failures - 1).clamp(0, 8) as u32;
        let delay = base_seconds
            .saturating_mul(2_i64.pow(exponent))
            .min(max_seconds);
        let until = now + chrono::Duration::seconds(delay);
        sqlx::query(
            "UPDATE provider_state SET backoff_until = ?, consecutive_failures = ?, updated_at = ? WHERE provider = 'bilibili'",
        )
        .bind(until)
        .bind(failures)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(until)
    }

    pub async fn clear_provider_failures(&self) -> Result<()> {
        sqlx::query(
            "UPDATE provider_state SET backoff_until = NULL, consecutive_failures = 0, updated_at = ? WHERE provider = 'bilibili'",
        )
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

fn candidate_fingerprint(bvids: &[String]) -> String {
    let mut hasher = Sha256::new();
    for bvid in bvids {
        hasher.update(bvid.as_bytes());
        hasher.update([0]);
    }
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use tempfile::TempDir;

    use super::*;
    use crate::domain::{
        AnimeMatch, AutoScheduleMetadata, DurationMatch, EpisodeMatch, ScheduleUpdate,
    };

    async fn fixture() -> (TempDir, Repository, i64, Episode) {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("test.db");
        let repository = Repository::connect(path.to_str().unwrap()).await.unwrap();
        let anime_id = repository
            .add_anime(NewAnime {
                title: "Silent Witch".into(),
                aliases: vec!["沉默魔女".into()],
                next_episode: 8,
                expected_at: Some(Utc::now()),
                expected_weekday: None,
                expected_time: None,
                timezone: "Asia/Shanghai".into(),
                duration_min_sec: 1_200,
                duration_max_sec: 1_680,
                auto_schedule: None,
            })
            .await
            .unwrap();
        let episode = repository.active_episode(anime_id).await.unwrap();
        (directory, repository, anime_id, episode)
    }

    fn candidate() -> (VideoCandidate, Evaluation) {
        let candidate = VideoCandidate {
            bvid: "BVtest00001".into(),
            title: "Silent Witch EP08".into(),
            description: None,
            uploader_mid: 100,
            uploader_name: "test up".into(),
            duration_sec: 1_420,
            published_at: Utc::now(),
            url: "https://www.bilibili.com/video/BVtest00001".into(),
            tags: vec!["动画".into()],
            page_count: Some(1),
            discovered_at: Utc::now(),
            enriched: true,
        };
        let evaluation = Evaluation {
            anime_match: AnimeMatch::Strong,
            episode_match: EpisodeMatch::Strong,
            duration_match: DurationMatch::Normal,
            expected_time_delta_sec: Some(0),
            trusted_uploader: false,
            blocked_uploader: false,
            negative_keywords: vec![],
            metadata_enriched: true,
            score: 75,
            hard_reject: false,
            manual_review: false,
            reasons: vec![],
        };
        (candidate, evaluation)
    }

    #[tokio::test]
    async fn duplicate_candidate_increments_seen_count_only() {
        let (_directory, repository, _anime_id, episode) = fixture().await;
        let (candidate, evaluation) = candidate();
        for _ in 0..3 {
            repository
                .upsert_candidate(episode.id, &candidate, &evaluation, CandidateState::Pending)
                .await
                .unwrap();
        }
        let stored = repository.active_candidates(episode.id).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].seen_count, 3);
    }

    #[tokio::test]
    async fn confirmation_and_feedback_are_idempotent() {
        let (_directory, repository, anime_id, episode) = fixture().await;
        let (candidate, evaluation) = candidate();
        repository
            .upsert_candidate(episode.id, &candidate, &evaluation, CandidateState::Pending)
            .await
            .unwrap();
        for _ in 0..2 {
            repository
                .confirm_candidate(episode.id, &candidate.bvid, "manual", "default", true)
                .await
                .unwrap();
        }
        assert_eq!(repository.pending_notifications().await.unwrap().len(), 1);
        let trust = repository
            .uploader_trust(anime_id, candidate.uploader_mid)
            .await
            .unwrap();
        assert_eq!(trust.confirmed_count, 1);
    }

    #[tokio::test]
    async fn rejecting_all_pending_candidates_records_feedback() {
        let (_directory, repository, anime_id, episode) = fixture().await;
        let (candidate, evaluation) = candidate();
        repository
            .upsert_candidate(episode.id, &candidate, &evaluation, CandidateState::Pending)
            .await
            .unwrap();

        assert_eq!(
            repository
                .reject_all_candidates(episode.id, true)
                .await
                .unwrap(),
            1
        );
        assert!(
            repository
                .active_candidates(episode.id)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            repository.episode(episode.id).await.unwrap().state,
            "watching"
        );
        assert_eq!(
            repository
                .uploader_trust(anime_id, candidate.uploader_mid)
                .await
                .unwrap()
                .rejected_count,
            1
        );
    }

    #[tokio::test]
    async fn provider_budget_and_backoff_are_global() {
        let (_directory, repository, _anime_id, _episode) = fixture().await;
        repository.reserve_provider_request(1).await.unwrap();
        assert!(repository.reserve_provider_request(1).await.is_err());

        repository.clear_provider_failures().await.unwrap();
        repository.record_provider_failure(60, 60).await.unwrap();
        assert!(repository.reserve_provider_request(500).await.is_err());
    }

    #[tokio::test]
    async fn automatic_schedule_metadata_updates_active_episode() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("schedule.db");
        let repository = Repository::connect(path.to_str().unwrap()).await.unwrap();
        let initial_expected = Utc::now() + chrono::Duration::days(1);
        let anime_id = repository
            .add_anime(NewAnime {
                title: "Silent Witch".into(),
                aliases: vec![],
                next_episode: 8,
                expected_at: Some(initial_expected),
                expected_weekday: Some(4),
                expected_time: Some("23:00".into()),
                timezone: "Asia/Shanghai".into(),
                duration_min_sec: 1_200,
                duration_max_sec: 1_680,
                auto_schedule: Some(AutoScheduleMetadata {
                    bangumi_subject_id: 506_677,
                    broadcast_pattern: "R/2025-07-04T15:00:00Z/P7D".into(),
                    next_sync_at: Utc::now() + chrono::Duration::days(1),
                }),
            })
            .await
            .unwrap();
        let updated_expected = Utc::now() + chrono::Duration::days(2);
        repository
            .apply_schedule_update(
                anime_id,
                &ScheduleUpdate {
                    bangumi_subject_id: 506_677,
                    aliases: vec!["沉默魔女".into(), "サイレント・ウィッチ".into()],
                    expected_at: updated_expected,
                    expected_weekday: 5,
                    expected_time: "00:00".into(),
                    timezone: "Asia/Shanghai".into(),
                    broadcast_pattern: "R/2025-07-05T16:00:00Z/P7D".into(),
                    next_sync_at: Utc::now() + chrono::Duration::days(1),
                },
            )
            .await
            .unwrap();

        let anime = repository.get_anime(anime_id).await.unwrap();
        assert!(anime.anime.auto_schedule);
        assert_eq!(anime.anime.bangumi_subject_id, Some(506_677));
        assert!(anime.aliases.iter().any(|alias| alias == "沉默魔女"));
        let episode = repository.active_episode(anime_id).await.unwrap();
        assert_eq!(episode.expected_at, Some(updated_expected));
    }

    #[tokio::test]
    async fn renaming_anime_keeps_old_title_and_schedules_check() {
        let (_directory, repository, anime_id, episode) = fixture().await;
        let future = Utc::now() + chrono::Duration::days(1);
        repository
            .reschedule_episode(episode.id, future)
            .await
            .unwrap();

        let previous = repository
            .rename_anime(anime_id, "  无职转生 第三季  ")
            .await
            .unwrap();
        assert_eq!(previous.title, "Silent Witch");

        let anime = repository.get_anime(anime_id).await.unwrap();
        assert_eq!(anime.anime.title, "无职转生 第三季");
        assert!(anime.aliases.iter().any(|alias| alias == "Silent Witch"));
        assert!(anime.aliases.iter().any(|alias| alias == "无职转生 第三季"));
        assert!(
            repository
                .active_episode(anime_id)
                .await
                .unwrap()
                .next_check_at
                < future
        );
    }

    #[tokio::test]
    async fn management_jobs_are_deduplicated_claimed_once_and_recovered() {
        let (_directory, repository, anime_id, _episode) = fixture().await;
        let target = anime_id.to_string();
        let first = repository
            .enqueue_management_job(
                "check_anime",
                Some("anime"),
                Some(&target),
                "{}",
                None,
                Some("check:1"),
            )
            .await
            .unwrap();
        let duplicate = repository
            .enqueue_management_job(
                "check_anime",
                Some("anime"),
                Some(&target),
                "{}",
                None,
                Some("check:1"),
            )
            .await
            .unwrap();
        assert_eq!(first, duplicate);

        let claimed = repository.claim_management_jobs(10).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert!(
            repository
                .claim_management_jobs(10)
                .await
                .unwrap()
                .is_empty()
        );
        sqlx::query("UPDATE management_job SET started_at = ? WHERE id = ?")
            .bind(Utc::now() - chrono::Duration::minutes(20))
            .bind(first)
            .execute(&repository.pool)
            .await
            .unwrap();
        assert_eq!(
            repository
                .recover_stale_management_jobs(Utc::now() - chrono::Duration::minutes(10))
                .await
                .unwrap(),
            1
        );
        assert_eq!(repository.claim_management_jobs(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn action_nonces_are_bound_and_consumed_once() {
        let (_directory, repository, anime_id, _episode) = fixture().await;
        let admin_id = repository
            .create_web_admin("admin", "test-password-hash")
            .await
            .unwrap();
        let entity = anime_id.to_string();
        repository
            .create_action_nonce(b"nonce-hmac", admin_id, "anime.delete", Some(&entity))
            .await
            .unwrap();
        assert!(
            !repository
                .consume_action_nonce(b"nonce-hmac", admin_id, "candidate.accept", Some(&entity),)
                .await
                .unwrap()
        );
        assert!(
            repository
                .consume_action_nonce(b"nonce-hmac", admin_id, "anime.delete", Some(&entity),)
                .await
                .unwrap()
        );
        assert!(
            !repository
                .consume_action_nonce(b"nonce-hmac", admin_id, "anime.delete", Some(&entity),)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn checked_delete_requires_disabled_unchanged_exact_target() {
        let (_directory, repository, anime_id, _episode) = fixture().await;
        let original = repository.get_anime(anime_id).await.unwrap().anime;
        assert!(
            repository
                .delete_anime_checked(anime_id, &original.title, original.updated_at)
                .await
                .is_err()
        );
        repository.set_anime_enabled(anime_id, false).await.unwrap();
        let current = repository.get_anime(anime_id).await.unwrap().anime;
        assert!(
            repository
                .delete_anime_checked(anime_id, "wrong title", current.updated_at)
                .await
                .is_err()
        );
        assert!(
            repository
                .delete_anime_checked(anime_id, &current.title, original.updated_at)
                .await
                .is_err()
        );
        repository
            .delete_anime_checked(anime_id, &current.title, current.updated_at)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn checked_reject_all_does_not_reject_new_candidates() {
        let (_directory, repository, _anime_id, episode) = fixture().await;
        let (first, evaluation) = candidate();
        repository
            .upsert_candidate(episode.id, &first, &evaluation, CandidateState::Pending)
            .await
            .unwrap();
        let fingerprint = repository
            .pending_candidate_fingerprint(episode.id)
            .await
            .unwrap();
        let mut second = first.clone();
        second.bvid = "BVtest00002".into();
        second.url = "https://www.bilibili.com/video/BVtest00002".into();
        repository
            .upsert_candidate(episode.id, &second, &evaluation, CandidateState::Pending)
            .await
            .unwrap();

        assert!(
            repository
                .reject_all_candidates_checked(episode.id, &fingerprint)
                .await
                .is_err()
        );
        assert_eq!(
            repository
                .active_candidates(episode.id)
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn deleting_anime_cascades_related_state() {
        let (_directory, repository, anime_id, episode) = fixture().await;
        let (candidate, evaluation) = candidate();
        repository
            .upsert_candidate(episode.id, &candidate, &evaluation, CandidateState::Pending)
            .await
            .unwrap();
        repository
            .confirm_candidate(episode.id, &candidate.bvid, "manual", "default", true)
            .await
            .unwrap();

        let deleted = repository.delete_anime(anime_id).await.unwrap();
        assert_eq!(deleted.title, "Silent Witch");
        assert!(matches!(
            repository.get_anime(anime_id).await,
            Err(AppError::NotFound(_))
        ));
        assert!(matches!(
            repository.episode(episode.id).await,
            Err(AppError::NotFound(_))
        ));
        assert!(repository.list_candidates(None).await.unwrap().is_empty());
        assert!(repository.pending_notifications().await.unwrap().is_empty());
        let trust = repository
            .uploader_trust(anime_id, candidate.uploader_mid)
            .await
            .unwrap();
        assert_eq!(trust.confirmed_count, 0);
        assert_eq!(trust.rejected_count, 0);
        assert!(!trust.manually_trusted);
        assert!(!trust.manually_blocked);
        assert!(matches!(
            repository.delete_anime(anime_id).await,
            Err(AppError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn blocking_uploader_rejects_existing_pending_candidates() {
        let (_directory, repository, anime_id, episode) = fixture().await;
        let (candidate, evaluation) = candidate();
        repository
            .upsert_candidate(episode.id, &candidate, &evaluation, CandidateState::Pending)
            .await
            .unwrap();
        repository
            .set_episode_manual_review(episode.id)
            .await
            .unwrap();

        let rejected = repository
            .block_uploader(anime_id, candidate.uploader_mid)
            .await
            .unwrap();

        assert_eq!(rejected, 1);
        assert!(
            repository
                .active_candidates(episode.id)
                .await
                .unwrap()
                .is_empty()
        );
        let trust = repository
            .uploader_trust(anime_id, candidate.uploader_mid)
            .await
            .unwrap();
        assert!(trust.manually_blocked);
        assert!(!trust.manually_trusted);
        assert_eq!(trust.uploader_name.as_deref(), Some("test up"));
        assert_eq!(
            repository.episode(episode.id).await.unwrap().state,
            "watching"
        );
    }
}
