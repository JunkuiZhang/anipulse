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
        Anime, AnimeWithAliases, CandidateState, Episode, EpisodeNumberMapping, EpisodeState,
        Evaluation, NewAnime, PendingNotification, PendingReviewNotification,
        ReviewCandidateSummary, ScheduleUpdate, StoredCandidate, UploaderTrust, VideoCandidate,
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
    pub manually_trusted: bool,
    pub globally_trusted: bool,
}

#[derive(Debug, Clone, FromRow)]
pub struct EpisodeVideoRow {
    pub id: i64,
    pub episode_id: i64,
    pub episode_no: i64,
    pub bvid: String,
    pub title: String,
    pub uploader_mid: i64,
    pub uploader_name: String,
    pub duration_sec: i64,
    pub score: i64,
    pub is_preferred: bool,
    pub updated_at: DateTime<Utc>,
    pub confirmed_count: i64,
    pub rejected_count: i64,
    pub manually_trusted: bool,
    pub globally_trusted: bool,
    pub manually_blocked: bool,
}

#[derive(Debug, Clone, FromRow)]
pub struct BlockedKeywordRow {
    pub id: i64,
    pub keyword: String,
    pub normalized_keyword: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct TrustedUploaderRow {
    pub anime_id: i64,
    pub anime_title: String,
    pub uploader_mid: i64,
    pub uploader_name: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct GlobalTrustedUploaderRow {
    pub uploader_mid: i64,
    pub uploader_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EpisodeRepairSummary {
    pub episode_no: i64,
    pub expired_candidates: u64,
    pub removed_notifications: u64,
    pub removed_future_episodes: u64,
}

#[derive(Debug, Clone)]
pub struct AnimeArchiveSummary {
    pub total_episodes: i64,
    pub removed_episodes: u64,
    pub removed_candidates: i64,
    pub removed_notifications: i64,
    pub removed_jobs: u64,
}

#[derive(Debug, Clone)]
pub struct EpisodeWatchedResult {
    pub anime_id: i64,
    pub episode_no: i64,
    pub auto_archive: Option<AnimeArchiveSummary>,
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
pub struct UpcomingReleaseRow {
    pub anime_id: i64,
    pub title: String,
    pub bangumi_subject_id: Option<i64>,
    pub total_episodes: Option<i64>,
    pub schedule_confidence: Option<String>,
    pub enabled: bool,
    pub episode_no: i64,
    pub expected_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct WatchQueueRow {
    pub episode_id: i64,
    pub anime_id: i64,
    pub anime_title: String,
    pub bangumi_subject_id: Option<i64>,
    pub episode_no: i64,
    pub bvid: String,
    pub video_title: String,
    pub released_at: DateTime<Utc>,
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
pub struct ManagementJobListRow {
    pub id: i64,
    pub kind: String,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    pub state: String,
    pub created_at: DateTime<Utc>,
    pub error: Option<String>,
    pub anime_title: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct PendingSourceAlert {
    pub source: String,
    pub alert_state: String,
    pub consecutive_failures: i64,
    pub first_failed_at: Option<DateTime<Utc>>,
    pub last_checked_at: DateTime<Utc>,
    pub last_error: Option<String>,
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
        if let Some(mapping) = new
            .auto_schedule
            .as_ref()
            .and_then(|metadata| metadata.episode_mapping)
        {
            mapping.mapped_numbers(new.next_episode)?;
        }

        let now = Utc::now();
        let (
            bangumi_subject_id,
            anilist_media_id,
            anime_schedule_route,
            total_episodes,
            auto_schedule,
            broadcast_pattern,
            schedule_sync_at,
            next_sync_at,
            schedule_source,
            schedule_confidence,
            schedule_warning,
            local_episode_origin,
            bangumi_episode_origin,
        ) = new
            .auto_schedule
            .as_ref()
            .map(|metadata| {
                let mapping = metadata.episode_mapping;
                (
                    Some(metadata.bangumi_subject_id),
                    metadata.anilist_media_id,
                    metadata.anime_schedule_route.as_deref(),
                    metadata.total_episodes,
                    true,
                    Some(metadata.broadcast_pattern.as_str()),
                    Some(now),
                    Some(metadata.next_sync_at),
                    Some(metadata.schedule_source.as_str()),
                    Some(metadata.schedule_confidence.as_str()),
                    metadata.schedule_warning.as_deref(),
                    mapping.map(|value| value.local_origin),
                    mapping.map(|value| value.bangumi_origin),
                )
            })
            .unwrap_or((
                None, None, None, None, false, None, None, None, None, None, None, None, None,
            ));
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            r#"INSERT INTO anime(
                title, bangumi_subject_id, anilist_media_id, anime_schedule_route,
                expected_weekday, expected_time, timezone,
                duration_min_sec, duration_max_sec, enabled, created_at, updated_at,
                auto_schedule, broadcast_pattern, schedule_sync_at, schedule_next_sync_at,
                schedule_source, schedule_confidence, schedule_warning,
                local_episode_origin, bangumi_episode_origin, total_episodes
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(new.title.trim())
        .bind(bangumi_subject_id)
        .bind(anilist_media_id)
        .bind(anime_schedule_route)
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
        .bind(schedule_source)
        .bind(schedule_confidence)
        .bind(schedule_warning)
        .bind(local_episode_origin)
        .bind(bangumi_episode_origin)
        .bind(total_episodes)
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

        let initial_check_at = new
            .expected_at
            .filter(|expected_at| *expected_at > now)
            .unwrap_or(now);
        sqlx::query(
            r#"INSERT INTO episode(
                anime_id, episode_no, expected_at, state, next_check_at
            ) VALUES (?, ?, ?, ?, ?)"#,
        )
        .bind(anime_id)
        .bind(new.next_episode)
        .bind(new.expected_at)
        .bind(EpisodeState::Watching.to_string())
        .bind(initial_check_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(anime_id)
    }

    pub async fn list_anime(&self) -> Result<Vec<Anime>> {
        Ok(sqlx::query_as::<_, Anime>(
            "SELECT * FROM anime WHERE lifecycle != 'archived' ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn list_all_anime(&self) -> Result<Vec<Anime>> {
        Ok(
            sqlx::query_as::<_, Anime>("SELECT * FROM anime ORDER BY id")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub async fn list_archived_anime(&self) -> Result<Vec<Anime>> {
        Ok(sqlx::query_as::<_, Anime>(
            "SELECT * FROM anime WHERE lifecycle = 'archived' ORDER BY archived_at DESC, id DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn upcoming_releases(
        &self,
        from: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<UpcomingReleaseRow>> {
        Ok(sqlx::query_as::<_, UpcomingReleaseRow>(
            r#"SELECT a.id AS anime_id, a.title, a.bangumi_subject_id,
                      a.total_episodes, a.schedule_confidence, a.enabled,
                      e.episode_no, e.expected_at
               FROM anime a
               JOIN episode e ON e.anime_id = a.id
               WHERE a.lifecycle = 'tracking'
                 AND e.state NOT IN ('notified', 'confirmed')
                 AND e.expected_at >= ?
                 AND e.expected_at < ?
               ORDER BY e.expected_at, a.id"#,
        )
        .bind(from)
        .bind(until)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn watch_queue(&self, limit: i64) -> Result<Vec<WatchQueueRow>> {
        let limit = limit.clamp(1, 100);
        Ok(sqlx::query_as::<_, WatchQueueRow>(
            r#"WITH ranked_videos AS (
                   SELECT ev.episode_id, ev.bvid, ev.title, ev.published_at,
                          ROW_NUMBER() OVER (
                              PARTITION BY ev.episode_id
                              ORDER BY ev.is_preferred DESC, ev.score DESC,
                                       ev.updated_at DESC, ev.id ASC
                          ) AS rank
                   FROM episode_video ev
                   JOIN episode e ON e.id = ev.episode_id
                   LEFT JOIN uploader_trust ut
                     ON ut.anime_id = e.anime_id AND ut.uploader_mid = ev.uploader_mid
                   WHERE COALESCE(ut.manually_blocked, 0) = 0
               )
               SELECT e.id AS episode_id, a.id AS anime_id,
                      a.title AS anime_title, a.bangumi_subject_id,
                      e.episode_no, rv.bvid, rv.title AS video_title,
                      COALESCE(rv.published_at, e.confirmed_at, e.notified_at, e.next_check_at)
                          AS released_at
               FROM episode e
               JOIN anime a ON a.id = e.anime_id
               JOIN ranked_videos rv ON rv.episode_id = e.id AND rv.rank = 1
               WHERE e.state IN ('confirmed', 'notified')
                 AND e.watched_at IS NULL
                 AND a.lifecycle != 'archived'
               ORDER BY released_at DESC, e.id DESC
               LIMIT ?"#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn mark_episode_watched(&self, episode_id: i64) -> Result<EpisodeWatchedResult> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        let episode = sqlx::query_as::<_, Episode>("SELECT * FROM episode WHERE id = ?")
            .bind(episode_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("episode {episode_id}")))?;
        if !matches!(episode.state.as_str(), "confirmed" | "notified") {
            return Err(AppError::InvalidInput(
                "only a released episode can be marked watched".into(),
            ));
        }
        sqlx::query("UPDATE episode SET watched_at = COALESCE(watched_at, ?) WHERE id = ?")
            .bind(now)
            .bind(episode_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        let anime = self.get_anime(episode.anime_id).await?.anime;
        let auto_archive = if let Some(final_episode_no) = anime.final_episode_no()? {
            let ready = sqlx::query_scalar::<_, bool>(
                r#"SELECT
                       EXISTS(
                           SELECT 1 FROM episode
                           WHERE anime_id = ? AND episode_no = ?
                             AND state IN ('confirmed','notified') AND watched_at IS NOT NULL
                       )
                       AND NOT EXISTS(
                           SELECT 1 FROM episode
                           WHERE anime_id = ? AND state IN ('confirmed','notified')
                             AND watched_at IS NULL
                       )"#,
            )
            .bind(episode.anime_id)
            .bind(final_episode_no)
            .bind(episode.anime_id)
            .fetch_one(&self.pool)
            .await?;
            if ready {
                if anime.lifecycle == "tracking" {
                    self.mark_anime_released_complete(episode.anime_id, anime.total_episodes)
                        .await?;
                }
                Some(
                    self.archive_anime(episode.anime_id, anime.total_episodes, &anime.summary)
                        .await?,
                )
            } else {
                None
            }
        } else {
            None
        };
        Ok(EpisodeWatchedResult {
            anime_id: episode.anime_id,
            episode_no: episode.episode_no,
            auto_archive,
        })
    }

    pub async fn anime_display_number(&self, anime_id: i64) -> Result<i64> {
        let lifecycle = sqlx::query_scalar::<_, String>("SELECT lifecycle FROM anime WHERE id = ?")
            .bind(anime_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("anime {anime_id}")))?;
        if lifecycle == "archived" {
            return Err(AppError::InvalidInput(
                "archived anime has no active display number".into(),
            ));
        }
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM anime WHERE lifecycle != 'archived' AND id <= ?",
        )
        .bind(anime_id)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn has_bangumi_subject_id(&self, subject_id: i64) -> Result<bool> {
        Ok(sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM anime WHERE bangumi_subject_id = ?)",
        )
        .bind(subject_id)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn list_blocked_keywords(&self) -> Result<Vec<BlockedKeywordRow>> {
        Ok(sqlx::query_as::<_, BlockedKeywordRow>(
            "SELECT * FROM blocked_keyword ORDER BY normalized_keyword, id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn add_blocked_keyword(&self, keyword: &str, normalized: &str) -> Result<i64> {
        let now = Utc::now();
        let result = sqlx::query(
            "INSERT INTO blocked_keyword(keyword, normalized_keyword, created_at, updated_at) VALUES (?, ?, ?, ?)",
        )
        .bind(keyword)
        .bind(normalized)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(map_unique_rule_error)?;
        Ok(result.last_insert_rowid())
    }

    pub async fn update_blocked_keyword(
        &self,
        id: i64,
        keyword: &str,
        normalized: &str,
    ) -> Result<()> {
        let result = sqlx::query(
            "UPDATE blocked_keyword SET keyword = ?, normalized_keyword = ?, updated_at = ? WHERE id = ?",
        )
        .bind(keyword)
        .bind(normalized)
        .bind(Utc::now())
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(map_unique_rule_error)?;
        if result.rows_affected() != 1 {
            return Err(AppError::NotFound(format!("blocked keyword {id}")));
        }
        Ok(())
    }

    pub async fn delete_blocked_keyword(&self, id: i64) -> Result<()> {
        let result = sqlx::query("DELETE FROM blocked_keyword WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() != 1 {
            return Err(AppError::NotFound(format!("blocked keyword {id}")));
        }
        Ok(())
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
        let result = sqlx::query(
            "UPDATE anime SET enabled = ?, updated_at = ? WHERE id = ? AND lifecycle = 'tracking'",
        )
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

    pub async fn set_episode_number_mapping(
        &self,
        anime_id: i64,
        mapping: Option<EpisodeNumberMapping>,
    ) -> Result<()> {
        let anime = self.get_anime(anime_id).await?;
        if !anime.anime.auto_schedule || anime.anime.lifecycle != "tracking" {
            return Err(AppError::InvalidInput(
                "episode mapping requires a tracking anime with automatic scheduling".into(),
            ));
        }
        let episode = self.active_episode(anime_id).await?;
        if let Some(mapping) = mapping {
            mapping.mapped_numbers(episode.episode_no)?;
        }
        let now = Utc::now();
        sqlx::query(
            r#"UPDATE anime SET local_episode_origin = ?, bangumi_episode_origin = ?,
                   total_episodes = NULL, schedule_next_sync_at = ?,
                   schedule_sync_error = NULL, updated_at = ?
               WHERE id = ? AND auto_schedule = 1 AND lifecycle = 'tracking'"#,
        )
        .bind(mapping.map(|value| value.local_origin))
        .bind(mapping.map(|value| value.bangumi_origin))
        .bind(now)
        .bind(now)
        .bind(anime_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_anime_released_complete(
        &self,
        anime_id: i64,
        total_episodes: Option<i64>,
    ) -> Result<i64> {
        if total_episodes.is_some_and(|value| value <= 0 || value > 10_000) {
            return Err(AppError::InvalidInput(
                "total episodes must be between 1 and 10000".into(),
            ));
        }
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        let anime = sqlx::query_as::<_, Anime>("SELECT * FROM anime WHERE id = ?")
            .bind(anime_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("anime {anime_id}")))?;
        if anime.lifecycle == "archived" {
            return Err(AppError::InvalidInput(
                "archived anime cannot be marked released complete".into(),
            ));
        }
        let completed_max = sqlx::query_scalar::<_, i64>(
            r#"SELECT COALESCE(MAX(episode_no), 0) FROM episode
               WHERE anime_id = ? AND state IN ('confirmed', 'notified')"#,
        )
        .bind(anime_id)
        .fetch_one(&mut *tx)
        .await?;
        let inferred_total = total_episodes
            .or(anime.total_episodes)
            .unwrap_or(completed_max);
        if inferred_total <= 0 {
            return Err(AppError::InvalidInput(
                "cannot infer total episodes; enter the completed episode count".into(),
            ));
        }
        let final_episode_no = match (anime.local_episode_origin, anime.bangumi_episode_origin) {
            (Some(local_origin), Some(bangumi_origin)) => EpisodeNumberMapping {
                local_origin,
                bangumi_origin,
            }
            .final_local_episode(inferred_total)?,
            (None, None) => inferred_total,
            _ => {
                return Err(AppError::InvalidInput(
                    "anime has an incomplete episode number mapping".into(),
                ));
            }
        };
        if final_episode_no < completed_max {
            return Err(AppError::InvalidInput(format!(
                "the mapped final episode EP{final_episode_no} cannot be lower than completed EP{completed_max}"
            )));
        }
        sqlx::query(
            r#"DELETE FROM episode
               WHERE anime_id = ? AND state NOT IN ('confirmed', 'notified')"#,
        )
        .bind(anime_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"DELETE FROM management_job
               WHERE target_type = 'anime' AND target_id = ? AND state != 'running'"#,
        )
        .bind(anime_id.to_string())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"UPDATE anime SET lifecycle = 'released_complete', enabled = 0,
                   total_episodes = ?, released_completed_at = COALESCE(released_completed_at, ?),
                   schedule_next_sync_at = NULL, updated_at = ?
               WHERE id = ?"#,
        )
        .bind(inferred_total)
        .bind(now)
        .bind(now)
        .bind(anime_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(inferred_total)
    }

    pub async fn resume_anime_tracking(&self, anime_id: i64, next_episode: i64) -> Result<()> {
        if next_episode <= 0 || next_episode > 10_000 {
            return Err(AppError::InvalidInput(
                "next episode must be between 1 and 10000".into(),
            ));
        }
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        let lifecycle = sqlx::query_scalar::<_, String>("SELECT lifecycle FROM anime WHERE id = ?")
            .bind(anime_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("anime {anime_id}")))?;
        if lifecycle != "released_complete" {
            return Err(AppError::InvalidInput(
                "only a released-complete anime can resume tracking".into(),
            ));
        }
        let existing = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM episode WHERE anime_id = ? AND episode_no = ?)",
        )
        .bind(anime_id)
        .bind(next_episode)
        .fetch_one(&mut *tx)
        .await?;
        if existing {
            return Err(AppError::InvalidInput(
                "the selected next episode already exists in history".into(),
            ));
        }
        sqlx::query(
            r#"INSERT INTO episode(anime_id, episode_no, state, next_check_at)
               VALUES (?, ?, 'watching', ?)"#,
        )
        .bind(anime_id)
        .bind(next_episode)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"UPDATE anime SET lifecycle = 'tracking', enabled = 1,
                   total_episodes = NULL, released_completed_at = NULL,
                   schedule_next_sync_at = CASE WHEN auto_schedule = 1 THEN ? ELSE schedule_next_sync_at END,
                   updated_at = ? WHERE id = ?"#,
        )
        .bind(now)
        .bind(now)
        .bind(anime_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn archive_anime(
        &self,
        anime_id: i64,
        total_episodes: Option<i64>,
        summary: &str,
    ) -> Result<AnimeArchiveSummary> {
        if total_episodes.is_some_and(|value| value <= 0 || value > 10_000) {
            return Err(AppError::InvalidInput(
                "total episodes must be between 1 and 10000".into(),
            ));
        }
        let summary = summary.trim();
        if summary.chars().count() > 4_000 {
            return Err(AppError::InvalidInput(
                "archive summary must not exceed 4000 characters".into(),
            ));
        }
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        let anime = sqlx::query_as::<_, Anime>("SELECT * FROM anime WHERE id = ?")
            .bind(anime_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("anime {anime_id}")))?;
        if anime.lifecycle != "released_complete" {
            return Err(AppError::InvalidInput(
                "mark the anime as released complete before archiving it".into(),
            ));
        }
        let completed_max = sqlx::query_scalar::<_, i64>(
            r#"SELECT COALESCE(MAX(episode_no), 0) FROM episode
               WHERE anime_id = ? AND state IN ('confirmed', 'notified')"#,
        )
        .bind(anime_id)
        .fetch_one(&mut *tx)
        .await?;
        let total_episodes = total_episodes
            .or(anime.total_episodes)
            .unwrap_or(completed_max);
        let final_episode_no = match (anime.local_episode_origin, anime.bangumi_episode_origin) {
            (Some(local_origin), Some(bangumi_origin)) => EpisodeNumberMapping {
                local_origin,
                bangumi_origin,
            }
            .final_local_episode(total_episodes)?,
            (None, None) => total_episodes,
            _ => {
                return Err(AppError::InvalidInput(
                    "anime has an incomplete episode number mapping".into(),
                ));
            }
        };
        if total_episodes <= 0 || final_episode_no < completed_max {
            return Err(AppError::InvalidInput(
                "archive total episodes is lower than the completed history".into(),
            ));
        }
        let removed_candidates = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM candidate WHERE episode_id IN (SELECT id FROM episode WHERE anime_id = ?)",
        )
        .bind(anime_id)
        .fetch_one(&mut *tx)
        .await?;
        let removed_notifications = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM notification WHERE episode_id IN (SELECT id FROM episode WHERE anime_id = ?)",
        )
        .bind(anime_id)
        .fetch_one(&mut *tx)
        .await?;
        let removed_episodes = sqlx::query("DELETE FROM episode WHERE anime_id = ?")
            .bind(anime_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        sqlx::query("DELETE FROM uploader_trust WHERE anime_id = ?")
            .bind(anime_id)
            .execute(&mut *tx)
            .await?;
        let removed_jobs = sqlx::query(
            "DELETE FROM management_job WHERE target_type = 'anime' AND target_id = ? AND state != 'running'",
        )
        .bind(anime_id.to_string())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sqlx::query(
            r#"UPDATE anime SET lifecycle = 'archived', enabled = 0,
                   summary = ?, total_episodes = ?, archived_at = ?,
                   schedule_next_sync_at = NULL, schedule_sync_error = NULL, updated_at = ?
               WHERE id = ?"#,
        )
        .bind(summary)
        .bind(total_episodes)
        .bind(now)
        .bind(now)
        .bind(anime_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(AnimeArchiveSummary {
            total_episodes,
            removed_episodes,
            removed_candidates,
            removed_notifications,
            removed_jobs,
        })
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

    pub async fn ensure_historical_episode(
        &self,
        anime_id: i64,
        episode_no: i64,
    ) -> Result<Episode> {
        if episode_no <= 0 || episode_no > 10_000 {
            return Err(AppError::InvalidInput(
                "episode must be between 1 and 10000".into(),
            ));
        }
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        let anime = sqlx::query_as::<_, Anime>("SELECT * FROM anime WHERE id = ?")
            .bind(anime_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("anime {anime_id}")))?;
        if anime.lifecycle == "archived" {
            return Err(AppError::InvalidInput(
                "archived anime no longer accepts episode videos".into(),
            ));
        }
        let exclusive_limit = if anime.lifecycle == "released_complete" {
            anime
                .final_episode_no()?
                .ok_or_else(|| AppError::InvalidInput("anime total episodes is unknown".into()))?
                + 1
        } else {
            sqlx::query_scalar::<_, i64>(
                r#"SELECT episode_no FROM episode
                   WHERE anime_id = ? AND state NOT IN ('notified', 'confirmed')
                   ORDER BY episode_no LIMIT 1"#,
            )
            .bind(anime_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::InvalidInput("anime has no active episode".into()))?
        };
        if episode_no >= exclusive_limit {
            return Err(AppError::InvalidInput(format!(
                "historical episode must be earlier than EP{exclusive_limit}"
            )));
        }
        sqlx::query(
            r#"INSERT OR IGNORE INTO episode(
                   anime_id, episode_no, state, next_check_at,
                   confirmed_at, notified_at, watched_at
               ) VALUES (?, ?, 'notified', ?, ?, ?, ?)"#,
        )
        .bind(anime_id)
        .bind(episode_no)
        .bind(now)
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let episode = sqlx::query_as::<_, Episode>(
            "SELECT * FROM episode WHERE anime_id = ? AND episode_no = ?",
        )
        .bind(anime_id)
        .bind(episode_no)
        .fetch_one(&mut *tx)
        .await?;
        if !matches!(episode.state.as_str(), "confirmed" | "notified") {
            return Err(AppError::InvalidInput(
                "episode is still active and cannot be added to history".into(),
            ));
        }
        tx.commit().await?;
        Ok(episode)
    }

    pub async fn list_episode_videos(&self, anime_id: i64) -> Result<Vec<EpisodeVideoRow>> {
        Ok(sqlx::query_as::<_, EpisodeVideoRow>(
            r#"SELECT ev.id, ev.episode_id, e.episode_no, ev.bvid, ev.title,
                      ev.uploader_mid, ev.uploader_name, ev.duration_sec, ev.score,
                      ev.is_preferred, ev.updated_at,
                      COALESCE(ut.confirmed_count, 0) AS confirmed_count,
                      COALESCE(ut.rejected_count, 0) AS rejected_count,
                      COALESCE(ut.manually_trusted, 0) AS manually_trusted,
                      gut.uploader_mid IS NOT NULL AS globally_trusted,
                      COALESCE(ut.manually_blocked, 0) AS manually_blocked
               FROM episode_video ev
               JOIN episode e ON e.id = ev.episode_id
               LEFT JOIN uploader_trust ut
                 ON ut.anime_id = e.anime_id AND ut.uploader_mid = ev.uploader_mid
               LEFT JOIN global_uploader_trust gut
                 ON gut.uploader_mid = ev.uploader_mid
               WHERE e.anime_id = ?
               ORDER BY e.episode_no DESC, ev.is_preferred DESC, ev.score DESC,
                        ev.updated_at DESC, ev.id ASC"#,
        )
        .bind(anime_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn upsert_episode_video(
        &self,
        anime_id: i64,
        episode_id: i64,
        replace_video_id: Option<i64>,
        candidate: &VideoCandidate,
        score: i64,
    ) -> Result<i64> {
        if candidate.uploader_mid <= 0 {
            return Err(AppError::InvalidInput(
                "video uploader information is unavailable".into(),
            ));
        }
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        let state = sqlx::query_scalar::<_, String>(
            "SELECT state FROM episode WHERE id = ? AND anime_id = ?",
        )
        .bind(episode_id)
        .bind(anime_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("episode {episode_id}")))?;
        if !matches!(state.as_str(), "confirmed" | "notified") {
            return Err(AppError::InvalidInput(
                "only completed episodes can be added to video history".into(),
            ));
        }

        let video_id = if let Some(video_id) = replace_video_id {
            let existing_episode_id =
                sqlx::query_scalar::<_, i64>("SELECT episode_id FROM episode_video WHERE id = ?")
                    .bind(video_id)
                    .fetch_optional(&mut *tx)
                    .await?
                    .ok_or_else(|| AppError::NotFound(format!("episode video {video_id}")))?;
            if existing_episode_id != episode_id {
                return Err(AppError::InvalidInput(
                    "video does not belong to this episode".into(),
                ));
            }
            let duplicate = sqlx::query_scalar::<_, i64>(
                "SELECT id FROM episode_video WHERE episode_id = ? AND bvid = ? AND id != ?",
            )
            .bind(episode_id)
            .bind(&candidate.bvid)
            .bind(video_id)
            .fetch_optional(&mut *tx)
            .await?;
            if duplicate.is_some() {
                return Err(AppError::InvalidInput(
                    "this Bilibili video is already saved for the episode".into(),
                ));
            }
            sqlx::query(
                r#"UPDATE episode_video SET bvid = ?, title = ?, uploader_mid = ?,
                       uploader_name = ?, duration_sec = ?, score = ?,
                       published_at = ?, updated_at = ?
                   WHERE id = ?"#,
            )
            .bind(&candidate.bvid)
            .bind(&candidate.title)
            .bind(candidate.uploader_mid)
            .bind(&candidate.uploader_name)
            .bind(candidate.duration_sec)
            .bind(score)
            .bind(candidate.published_at)
            .bind(now)
            .bind(video_id)
            .execute(&mut *tx)
            .await?;
            video_id
        } else {
            sqlx::query(
                r#"INSERT INTO episode_video(
                       episode_id, bvid, title, uploader_mid, uploader_name,
                       duration_sec, score, published_at, created_at, updated_at
                   ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                   ON CONFLICT(episode_id, bvid) DO UPDATE SET
                       title = excluded.title,
                       uploader_mid = excluded.uploader_mid,
                       uploader_name = excluded.uploader_name,
                       duration_sec = excluded.duration_sec,
                       score = excluded.score,
                       published_at = excluded.published_at,
                       updated_at = excluded.updated_at"#,
            )
            .bind(episode_id)
            .bind(&candidate.bvid)
            .bind(&candidate.title)
            .bind(candidate.uploader_mid)
            .bind(&candidate.uploader_name)
            .bind(candidate.duration_sec)
            .bind(score)
            .bind(candidate.published_at)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            sqlx::query_scalar::<_, i64>(
                "SELECT id FROM episode_video WHERE episode_id = ? AND bvid = ?",
            )
            .bind(episode_id)
            .bind(&candidate.bvid)
            .fetch_one(&mut *tx)
            .await?
        };
        sqlx::query(
            r#"UPDATE episode_video SET is_preferred = 0
               WHERE id = ? AND EXISTS (
                   SELECT 1
                   FROM episode e
                   JOIN uploader_trust ut
                     ON ut.anime_id = e.anime_id
                    AND ut.uploader_mid = episode_video.uploader_mid
                   WHERE e.id = episode_video.episode_id
                     AND ut.manually_blocked = 1
               )"#,
        )
        .bind(video_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(video_id)
    }

    pub async fn set_episode_video_preferred(
        &self,
        anime_id: i64,
        episode_id: i64,
        video_id: Option<i64>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let owner = sqlx::query_scalar::<_, i64>("SELECT anime_id FROM episode WHERE id = ?")
            .bind(episode_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("episode {episode_id}")))?;
        if owner != anime_id {
            return Err(AppError::InvalidInput(
                "episode does not belong to this anime".into(),
            ));
        }
        if let Some(video_id) = video_id {
            let blocked = sqlx::query_scalar::<_, bool>(
                r#"SELECT COALESCE(ut.manually_blocked, 0)
                   FROM episode_video ev
                   JOIN episode e ON e.id = ev.episode_id
                   LEFT JOIN uploader_trust ut
                     ON ut.anime_id = e.anime_id AND ut.uploader_mid = ev.uploader_mid
                   WHERE ev.id = ? AND ev.episode_id = ?"#,
            )
            .bind(video_id)
            .bind(episode_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("episode video {video_id}")))?;
            if blocked {
                return Err(AppError::InvalidInput(
                    "a blocked uploader cannot be selected as best video".into(),
                ));
            }
        }
        sqlx::query("UPDATE episode_video SET is_preferred = 0 WHERE episode_id = ?")
            .bind(episode_id)
            .execute(&mut *tx)
            .await?;
        if let Some(video_id) = video_id {
            sqlx::query(
                "UPDATE episode_video SET is_preferred = 1, updated_at = ? WHERE id = ? AND episode_id = ?",
            )
            .bind(Utc::now())
            .bind(video_id)
            .bind(episode_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn repair_current_episode(
        &self,
        anime_id: i64,
        expected_current_episode_id: i64,
        target_episode_no: i64,
    ) -> Result<EpisodeRepairSummary> {
        if target_episode_no <= 0 || target_episode_no > 10_000 {
            return Err(AppError::InvalidInput(
                "target episode must be between 1 and 10000".into(),
            ));
        }
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        let enabled = sqlx::query_scalar::<_, bool>("SELECT enabled FROM anime WHERE id = ?")
            .bind(anime_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("anime {anime_id}")))?;
        if enabled {
            return Err(AppError::InvalidInput(
                "anime must be disabled before repairing its episode state".into(),
            ));
        }
        let current = sqlx::query_as::<_, Episode>(
            r#"SELECT * FROM episode
               WHERE anime_id = ? AND state NOT IN ('notified', 'confirmed')
               ORDER BY episode_no LIMIT 1"#,
        )
        .bind(anime_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("active episode for anime {anime_id}")))?;
        if current.id != expected_current_episode_id {
            return Err(AppError::InvalidInput(
                "current episode changed; inspect the anime and retry".into(),
            ));
        }
        if target_episode_no > current.episode_no {
            return Err(AppError::InvalidInput(
                "repair can only reset to the current or an earlier existing episode".into(),
            ));
        }
        let target = sqlx::query_as::<_, Episode>(
            "SELECT * FROM episode WHERE anime_id = ? AND episode_no = ?",
        )
        .bind(anime_id)
        .bind(target_episode_no)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!("anime {anime_id} episode {target_episode_no}"))
        })?;
        let expired_candidates = sqlx::query(
            "UPDATE candidate SET state = 'expired' WHERE episode_id = ? AND state IN ('pending','confirmed')",
        )
        .bind(target.id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let removed_notifications = sqlx::query("DELETE FROM notification WHERE episode_id = ?")
            .bind(target.id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        sqlx::query("DELETE FROM episode_video WHERE episode_id = ?")
            .bind(target.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE review_notification SET status = 'cancelled' WHERE episode_id = ? AND status = 'pending'",
        )
        .bind(target.id)
        .execute(&mut *tx)
        .await?;
        let removed_future_episodes =
            sqlx::query("DELETE FROM episode WHERE anime_id = ? AND episode_no > ?")
                .bind(anime_id)
                .bind(target_episode_no)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        sqlx::query(
            r#"UPDATE episode SET state = 'waiting', next_check_at = ?,
                   first_candidate_at = NULL, confirmed_at = NULL,
                   notified_at = NULL, watched_at = NULL
               WHERE id = ?"#,
        )
        .bind(now)
        .bind(target.id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE anime SET updated_at = ?, schedule_next_sync_at = CASE WHEN auto_schedule = 1 THEN ? ELSE schedule_next_sync_at END WHERE id = ?",
        )
        .bind(now)
        .bind(now)
        .bind(anime_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(EpisodeRepairSummary {
            episode_no: target_episode_no,
            expired_candidates,
            removed_notifications,
            removed_future_episodes,
        })
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
                 AND a.lifecycle = 'tracking'
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
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT id FROM anime WHERE enabled = 1 AND lifecycle = 'tracking' ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn auto_schedule_due_ids(&self, limit: i64) -> Result<Vec<i64>> {
        Ok(sqlx::query_scalar::<_, i64>(
            r#"SELECT id FROM anime
               WHERE enabled = 1 AND auto_schedule = 1
                 AND lifecycle = 'tracking'
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
                bangumi_subject_id = ?, anilist_media_id = COALESCE(?, anilist_media_id),
                anime_schedule_route = COALESCE(?, anime_schedule_route),
                expected_weekday = ?, expected_time = ?, timezone = ?,
                broadcast_pattern = ?, schedule_sync_at = ?, schedule_next_sync_at = ?,
                schedule_source = ?, schedule_confidence = ?, schedule_warning = ?,
                total_episodes = COALESCE(?, total_episodes),
                schedule_sync_error = NULL, updated_at = ?
               WHERE id = ? AND auto_schedule = 1 AND lifecycle = 'tracking'"#,
        )
        .bind(update.bangumi_subject_id)
        .bind(update.anilist_media_id)
        .bind(update.anime_schedule_route.as_deref())
        .bind(update.expected_weekday)
        .bind(&update.expected_time)
        .bind(&update.timezone)
        .bind(&update.broadcast_pattern)
        .bind(now)
        .bind(update.next_sync_at)
        .bind(&update.schedule_source)
        .bind(&update.schedule_confidence)
        .bind(update.schedule_warning.as_deref())
        .bind(update.total_episodes)
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

        let next_check_at = update
            .expected_at
            .filter(|expected_at| *expected_at > now)
            .unwrap_or(now);
        sqlx::query(
            r#"UPDATE episode SET expected_at = ?, next_check_at = ?
               WHERE anime_id = ? AND state IN
                   ('waiting','watching','candidate_found','needs_manual_review')"#,
        )
        .bind(update.expected_at)
        .bind(next_check_at)
        .bind(anime_id)
        .execute(&mut *tx)
        .await?;
        transition_released_complete_if_final_released(&mut tx, anime_id, now).await?;
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
                          c.published_at, c.score, c.state, c.seen_count, c.evaluation_json, c.url,
                          COALESCE(ut.manually_trusted, 0) AS manually_trusted,
                          gut.uploader_mid IS NOT NULL AS globally_trusted
                   FROM candidate c
                   JOIN episode e ON e.id = c.episode_id
                   JOIN anime a ON a.id = e.anime_id
                   LEFT JOIN uploader_trust ut
                     ON ut.anime_id = e.anime_id AND ut.uploader_mid = c.uploader_mid
                   LEFT JOIN global_uploader_trust gut
                     ON gut.uploader_mid = c.uploader_mid
                   WHERE c.state = ?
                     AND (? != 'pending' OR e.state NOT IN ('confirmed', 'notified'))
                   ORDER BY c.last_seen_at DESC"#,
            )
            .bind(state)
            .bind(state)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, CandidateListRow>(
                r#"SELECT c.episode_id, e.anime_id, c.bvid, a.title AS anime_title, e.episode_no,
                          c.uploader_mid, c.uploader_name, c.title, c.duration_sec,
                          c.published_at, c.score, c.state, c.seen_count, c.evaluation_json, c.url,
                          COALESCE(ut.manually_trusted, 0) AS manually_trusted,
                          gut.uploader_mid IS NOT NULL AS globally_trusted
                   FROM candidate c
                   JOIN episode e ON e.id = c.episode_id
                   JOIN anime a ON a.id = e.anime_id
                   LEFT JOIN uploader_trust ut
                     ON ut.anime_id = e.anime_id AND ut.uploader_mid = c.uploader_mid
                   LEFT JOIN global_uploader_trust gut
                     ON gut.uploader_mid = c.uploader_mid
                   ORDER BY c.last_seen_at DESC"#,
            )
            .fetch_all(&self.pool)
            .await?
        };
        Ok(rows)
    }

    pub async fn candidate_context(&self, bvid: &str) -> Result<(StoredCandidate, Episode)> {
        let candidates = sqlx::query_as::<_, StoredCandidate>(
            r#"SELECT c.* FROM candidate c JOIN episode e ON e.id = c.episode_id
               WHERE c.bvid = ? AND c.state = 'pending'
                 AND e.state NOT IN ('confirmed', 'notified')
               ORDER BY c.last_seen_at DESC"#,
        )
        .bind(bvid)
        .fetch_all(&self.pool)
        .await?;
        let candidate = match candidates.as_slice() {
            [] => return Err(AppError::NotFound(format!("pending candidate {bvid}"))),
            [candidate] => candidate.clone(),
            _ => {
                return Err(AppError::InvalidInput(format!(
                    "candidate {bvid} belongs to multiple active episodes; use the web review page"
                )));
            }
        };
        let episode = self.episode(candidate.episode_id).await?;
        Ok((candidate, episode))
    }

    pub async fn candidate_context_for_episode(
        &self,
        episode_id: i64,
        bvid: &str,
    ) -> Result<(StoredCandidate, Episode)> {
        let candidate = sqlx::query_as::<_, StoredCandidate>(
            "SELECT * FROM candidate WHERE episode_id = ? AND bvid = ?",
        )
        .bind(episode_id)
        .bind(bvid)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("candidate {episode_id}:{bvid}")))?;
        let episode = self.episode(episode_id).await?;
        Ok((candidate, episode))
    }

    pub async fn uploader_trust(&self, anime_id: i64, mid: i64) -> Result<UploaderTrust> {
        Ok(sqlx::query_as::<_, UploaderTrust>(
            r#"SELECT ? AS anime_id, ? AS uploader_mid,
                      COALESCE(ut.uploader_name, gut.uploader_name) AS uploader_name,
                      COALESCE(ut.confirmed_count, 0) AS confirmed_count,
                      COALESCE(ut.rejected_count, 0) AS rejected_count,
                      COALESCE(ut.manually_trusted, 0) AS manually_trusted,
                      gut.uploader_mid IS NOT NULL AS globally_trusted,
                      COALESCE(ut.manually_blocked, 0) AS manually_blocked
               FROM (SELECT 1) seed
               LEFT JOIN uploader_trust ut
                 ON ut.anime_id = ? AND ut.uploader_mid = ?
               LEFT JOIN global_uploader_trust gut
                 ON gut.uploader_mid = ?"#,
        )
        .bind(anime_id)
        .bind(mid)
        .bind(anime_id)
        .bind(mid)
        .bind(mid)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn list_manually_trusted_uploaders(&self) -> Result<Vec<TrustedUploaderRow>> {
        Ok(sqlx::query_as::<_, TrustedUploaderRow>(
            r#"SELECT u.anime_id, a.title AS anime_title, u.uploader_mid, u.uploader_name
               FROM uploader_trust u JOIN anime a ON a.id = u.anime_id
               WHERE u.manually_trusted = 1
               ORDER BY a.title, u.uploader_name, u.uploader_mid"#,
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn list_globally_trusted_uploaders(&self) -> Result<Vec<GlobalTrustedUploaderRow>> {
        Ok(sqlx::query_as::<_, GlobalTrustedUploaderRow>(
            r#"SELECT uploader_mid, uploader_name
               FROM global_uploader_trust
               ORDER BY uploader_name, uploader_mid"#,
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn add_globally_trusted_uploader(&self, mid: i64, name: &str) -> Result<()> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"INSERT INTO global_uploader_trust(
                   uploader_mid, uploader_name, created_at, updated_at
               ) VALUES (?, ?, ?, ?)
               ON CONFLICT(uploader_mid) DO UPDATE SET
                   uploader_name = excluded.uploader_name,
                   updated_at = excluded.updated_at"#,
        )
        .bind(mid)
        .bind(name)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE uploader_trust SET manually_trusted = 0 WHERE uploader_mid = ?")
            .bind(mid)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn promote_uploader_to_global(&self, mid: i64, name: &str) -> Result<()> {
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"INSERT INTO global_uploader_trust(
                   uploader_mid, uploader_name, created_at, updated_at
               ) VALUES (?, ?, ?, ?)
               ON CONFLICT(uploader_mid) DO UPDATE SET
                   uploader_name = excluded.uploader_name,
                   updated_at = excluded.updated_at"#,
        )
        .bind(mid)
        .bind(name)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE uploader_trust SET manually_trusted = 0 WHERE uploader_mid = ?")
            .bind(mid)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn promote_uploader_from_anime(&self, anime_id: i64, mid: i64) -> Result<()> {
        let uploader_name = sqlx::query_scalar::<_, String>(
            r#"SELECT uploader_name FROM (
                   SELECT u.uploader_name, '9999-12-31T23:59:59Z' AS seen_at
                   FROM uploader_trust u
                   WHERE u.anime_id = ? AND u.uploader_mid = ?
                     AND u.uploader_name IS NOT NULL
                   UNION ALL
                   SELECT c.uploader_name, c.last_seen_at AS seen_at
                   FROM candidate c JOIN episode e ON e.id = c.episode_id
                   WHERE e.anime_id = ? AND c.uploader_mid = ?
                   UNION ALL
                   SELECT ev.uploader_name, ev.updated_at AS seen_at
                   FROM episode_video ev JOIN episode e ON e.id = ev.episode_id
                   WHERE e.anime_id = ? AND ev.uploader_mid = ?
               ) ORDER BY seen_at DESC LIMIT 1"#,
        )
        .bind(anime_id)
        .bind(mid)
        .bind(anime_id)
        .bind(mid)
        .bind(anime_id)
        .bind(mid)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("uploader {anime_id}:{mid}")))?;
        self.promote_uploader_to_global(mid, &uploader_name).await
    }

    pub async fn update_globally_trusted_uploader(
        &self,
        old_mid: i64,
        mid: i64,
        name: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM global_uploader_trust WHERE uploader_mid = ?)",
        )
        .bind(old_mid)
        .fetch_one(&mut *tx)
        .await?;
        if !exists {
            return Err(AppError::NotFound(format!(
                "globally trusted uploader {old_mid}"
            )));
        }
        let now = Utc::now();
        if old_mid == mid {
            sqlx::query(
                "UPDATE global_uploader_trust SET uploader_name = ?, updated_at = ? WHERE uploader_mid = ?",
            )
            .bind(name)
            .bind(now)
            .bind(mid)
            .execute(&mut *tx)
            .await?;
        } else {
            let result = sqlx::query(
                r#"UPDATE global_uploader_trust
                   SET uploader_mid = ?, uploader_name = ?, updated_at = ?
                   WHERE uploader_mid = ?"#,
            )
            .bind(mid)
            .bind(name)
            .bind(now)
            .bind(old_mid)
            .execute(&mut *tx)
            .await;
            match result {
                Ok(_) => {}
                Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
                    return Err(AppError::InvalidInput(format!(
                        "globally trusted uploader {mid} already exists"
                    )));
                }
                Err(error) => return Err(error.into()),
            }
        }
        sqlx::query("UPDATE uploader_trust SET manually_trusted = 0 WHERE uploader_mid = ?")
            .bind(mid)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn remove_global_uploader_trust(&self, mid: i64) -> Result<()> {
        let result = sqlx::query("DELETE FROM global_uploader_trust WHERE uploader_mid = ?")
            .bind(mid)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() != 1 {
            return Err(AppError::NotFound(format!(
                "globally trusted uploader {mid}"
            )));
        }
        Ok(())
    }

    pub async fn add_manually_trusted_uploader(
        &self,
        anime_id: i64,
        mid: i64,
        name: &str,
    ) -> Result<()> {
        let globally_trusted = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM global_uploader_trust WHERE uploader_mid = ?)",
        )
        .bind(mid)
        .fetch_one(&self.pool)
        .await?;
        if globally_trusted {
            return Err(AppError::InvalidInput(format!(
                "uploader {mid} is already globally trusted"
            )));
        }
        let result = sqlx::query(
            r#"INSERT INTO uploader_trust(
                anime_id, uploader_mid, uploader_name, manually_trusted, manually_blocked
            ) VALUES (?, ?, ?, 1, 0)
            ON CONFLICT(anime_id, uploader_mid) DO UPDATE SET
                uploader_name = excluded.uploader_name,
                manually_trusted = 1,
                manually_blocked = 0"#,
        )
        .bind(anime_id)
        .bind(mid)
        .bind(name)
        .execute(&self.pool)
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(error)) if error.is_foreign_key_violation() => {
                Err(AppError::NotFound(format!("anime {anime_id}")))
            }
            Err(error) => Err(error.into()),
        }
    }

    pub async fn update_manually_trusted_uploader(
        &self,
        old_anime_id: i64,
        old_mid: i64,
        anime_id: i64,
        mid: i64,
        name: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM uploader_trust WHERE anime_id = ? AND uploader_mid = ? AND manually_trusted = 1)",
        )
        .bind(old_anime_id)
        .bind(old_mid)
        .fetch_one(&mut *tx)
        .await?;
        if !exists {
            return Err(AppError::NotFound(format!(
                "trusted uploader {old_anime_id}:{old_mid}"
            )));
        }
        let globally_trusted = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM global_uploader_trust WHERE uploader_mid = ?)",
        )
        .bind(mid)
        .fetch_one(&mut *tx)
        .await?;
        if globally_trusted {
            return Err(AppError::InvalidInput(format!(
                "uploader {mid} is already globally trusted"
            )));
        }
        if old_anime_id == anime_id && old_mid == mid {
            sqlx::query(
                "UPDATE uploader_trust SET uploader_name = ? WHERE anime_id = ? AND uploader_mid = ?",
            )
            .bind(name)
            .bind(anime_id)
            .bind(mid)
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query(
                r#"UPDATE uploader_trust SET manually_trusted = 0
                   WHERE anime_id = ? AND uploader_mid = ?"#,
            )
            .bind(old_anime_id)
            .bind(old_mid)
            .execute(&mut *tx)
            .await?;
            let result = sqlx::query(
                r#"INSERT INTO uploader_trust(
                    anime_id, uploader_mid, uploader_name, manually_trusted, manually_blocked
                ) VALUES (?, ?, ?, 1, 0)
                ON CONFLICT(anime_id, uploader_mid) DO UPDATE SET
                    uploader_name = excluded.uploader_name,
                    manually_trusted = 1,
                    manually_blocked = 0"#,
            )
            .bind(anime_id)
            .bind(mid)
            .bind(name)
            .execute(&mut *tx)
            .await;
            match result {
                Ok(_) => {}
                Err(sqlx::Error::Database(error)) if error.is_foreign_key_violation() => {
                    return Err(AppError::NotFound(format!("anime {anime_id}")));
                }
                Err(error) => return Err(error.into()),
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn remove_manual_uploader_trust(&self, anime_id: i64, mid: i64) -> Result<()> {
        let result = sqlx::query(
            r#"UPDATE uploader_trust SET manually_trusted = 0
               WHERE anime_id = ? AND uploader_mid = ? AND manually_trusted = 1"#,
        )
        .bind(anime_id)
        .bind(mid)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(AppError::NotFound(format!(
                "trusted uploader {anime_id}:{mid}"
            )));
        }
        Ok(())
    }

    pub async fn set_uploader_flag(
        &self,
        anime_id: i64,
        mid: i64,
        trusted: bool,
        blocked: bool,
    ) -> Result<()> {
        let globally_trusted = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM global_uploader_trust WHERE uploader_mid = ?)",
        )
        .bind(mid)
        .fetch_one(&self.pool)
        .await?;
        let manually_trusted = trusted && !globally_trusted;
        let uploader_name = sqlx::query_scalar::<_, String>(
            r#"SELECT uploader_name FROM (
                   SELECT c.uploader_name, c.last_seen_at AS seen_at
                   FROM candidate c JOIN episode e ON e.id = c.episode_id
                   WHERE e.anime_id = ? AND c.uploader_mid = ?
                   UNION ALL
                   SELECT ev.uploader_name, ev.updated_at AS seen_at
                   FROM episode_video ev JOIN episode e ON e.id = ev.episode_id
                   WHERE e.anime_id = ? AND ev.uploader_mid = ?
               ) ORDER BY seen_at DESC LIMIT 1"#,
        )
        .bind(anime_id)
        .bind(mid)
        .bind(anime_id)
        .bind(mid)
        .fetch_optional(&self.pool)
        .await?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"INSERT INTO uploader_trust(
                anime_id, uploader_mid, uploader_name, manually_trusted, manually_blocked
            ) VALUES (?, ?, ?, ?, ?)
            ON CONFLICT(anime_id, uploader_mid) DO UPDATE SET
                uploader_name = COALESCE(excluded.uploader_name, uploader_trust.uploader_name),
                manually_trusted = excluded.manually_trusted,
                manually_blocked = excluded.manually_blocked"#,
        )
        .bind(anime_id)
        .bind(mid)
        .bind(uploader_name)
        .bind(manually_trusted)
        .bind(blocked)
        .execute(&mut *tx)
        .await?;
        if blocked {
            sqlx::query(
                r#"UPDATE episode_video SET is_preferred = 0
                   WHERE uploader_mid = ? AND episode_id IN (
                       SELECT id FROM episode WHERE anime_id = ?
                   )"#,
            )
            .bind(mid)
            .bind(anime_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
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
            r#"SELECT uploader_name FROM (
                   SELECT c.uploader_name, c.last_seen_at AS seen_at
                   FROM candidate c JOIN episode e ON e.id = c.episode_id
                   WHERE e.anime_id = ? AND c.uploader_mid = ?
                   UNION ALL
                   SELECT ev.uploader_name, ev.updated_at AS seen_at
                   FROM episode_video ev JOIN episode e ON e.id = ev.episode_id
                   WHERE e.anime_id = ? AND ev.uploader_mid = ?
               ) ORDER BY seen_at DESC LIMIT 1"#,
        )
        .bind(anime_id)
        .bind(mid)
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
            r#"UPDATE episode_video SET is_preferred = 0
               WHERE uploader_mid = ? AND episode_id IN (
                   SELECT id FROM episode WHERE anime_id = ?
               )"#,
        )
        .bind(mid)
        .bind(anime_id)
        .execute(&mut *tx)
        .await?;
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
        let (candidate_id, previous_state, anime_id, episode_state) =
            sqlx::query_as::<_, (i64, String, i64, String)>(
                r#"SELECT c.id, c.state, e.anime_id, e.state
               FROM candidate c JOIN episode e ON e.id = c.episode_id
               WHERE c.episode_id = ? AND c.bvid = ?"#,
            )
            .bind(episode_id)
            .bind(bvid)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("candidate {bvid}")))?;
        if previous_state == CandidateState::Confirmed.to_string()
            && episode_state == EpisodeState::Confirmed.to_string()
        {
            return Ok(());
        }
        if previous_state != CandidateState::Pending.to_string() {
            return Err(AppError::InvalidInput(
                "candidate is no longer pending; refresh the review page".into(),
            ));
        }
        if matches!(episode_state.as_str(), "confirmed" | "notified") {
            return Err(AppError::InvalidInput(
                "candidate belongs to an episode that is already complete".into(),
            ));
        }
        let current_episode_id = sqlx::query_scalar::<_, i64>(
            r#"SELECT id FROM episode
               WHERE anime_id = ? AND state NOT IN ('notified', 'confirmed')
               ORDER BY episode_no LIMIT 1"#,
        )
        .bind(anime_id)
        .fetch_optional(&mut *tx)
        .await?;
        if current_episode_id != Some(episode_id) {
            return Err(AppError::InvalidInput(
                "candidate no longer belongs to the current episode; refresh the review page"
                    .into(),
            ));
        }
        sqlx::query("UPDATE candidate SET state = 'confirmed' WHERE id = ?")
            .bind(candidate_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE candidate SET state = 'expired' WHERE episode_id = ? AND id != ? AND state = 'pending'",
        )
        .bind(episode_id)
        .bind(candidate_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE episode SET state = 'confirmed', confirmed_at = ? WHERE id = ?")
            .bind(now)
            .bind(episode_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE episode_video SET is_preferred = 0 WHERE episode_id = ?")
            .bind(episode_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            r#"INSERT INTO episode_video(
                   episode_id, bvid, title, uploader_mid, uploader_name,
                   duration_sec, score, is_preferred, published_at, created_at, updated_at
               )
               SELECT episode_id, bvid, title, uploader_mid, uploader_name,
                      duration_sec, score, 1, published_at, first_seen_at, ?
               FROM candidate WHERE id = ?
               ON CONFLICT(episode_id, bvid) DO UPDATE SET
                   title = excluded.title,
                   uploader_mid = excluded.uploader_mid,
                   uploader_name = excluded.uploader_name,
                   duration_sec = excluded.duration_sec,
                   score = excluded.score,
                   is_preferred = 1,
                   published_at = excluded.published_at,
                   updated_at = excluded.updated_at"#,
        )
        .bind(now)
        .bind(candidate_id)
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
        let (candidate, _) = self.candidate_context(bvid).await?;
        self.reject_candidate_for_episode(candidate.episode_id, bvid, user_rejected)
            .await
    }

    pub async fn reject_candidate_for_episode(
        &self,
        episode_id: i64,
        bvid: &str,
        user_rejected: bool,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let (candidate_id, previous_state, anime_id, episode_state) =
            sqlx::query_as::<_, (i64, String, i64, String)>(
                r#"SELECT c.id, c.state, e.anime_id, e.state
                   FROM candidate c JOIN episode e ON e.id = c.episode_id
                   WHERE c.episode_id = ? AND c.bvid = ?"#,
            )
            .bind(episode_id)
            .bind(bvid)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("candidate {bvid}")))?;
        if previous_state == CandidateState::Rejected.to_string() {
            return Ok(());
        }
        if previous_state != CandidateState::Pending.to_string() {
            return Err(AppError::InvalidInput(
                "candidate is no longer pending; refresh the review page".into(),
            ));
        }
        if matches!(episode_state.as_str(), "confirmed" | "notified") {
            return Err(AppError::InvalidInput(
                "candidate belongs to an episode that is already complete".into(),
            ));
        }
        let current_episode_id = sqlx::query_scalar::<_, i64>(
            r#"SELECT id FROM episode
               WHERE anime_id = ? AND state NOT IN ('notified', 'confirmed')
               ORDER BY episode_no LIMIT 1"#,
        )
        .bind(anime_id)
        .fetch_optional(&mut *tx)
        .await?;
        if current_episode_id != Some(episode_id) {
            return Err(AppError::InvalidInput(
                "candidate no longer belongs to the current episode; refresh the review page"
                    .into(),
            ));
        }
        sqlx::query("UPDATE candidate SET state = 'rejected' WHERE id = ?")
            .bind(candidate_id)
            .execute(&mut *tx)
            .await?;
        if user_rejected && previous_state != CandidateState::Rejected.to_string() {
            Self::increment_trust_tx(&mut tx, candidate_id, false).await?;
        }
        sqlx::query(
            r#"UPDATE episode SET state = 'watching'
               WHERE id = ? AND state IN ('candidate_found','needs_manual_review')
                 AND NOT EXISTS (
                   SELECT 1 FROM candidate c
                   WHERE c.episode_id = episode.id AND c.state = 'pending'
                 )"#,
        )
        .bind(episode_id)
        .execute(&mut *tx)
        .await?;
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
                 AND e.state NOT IN ('confirmed', 'notified')
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

        let released_complete =
            transition_released_complete_if_final_released(&mut tx, episode.anime_id, now).await?;
        if !released_complete {
            let next_expected = episode
                .expected_at
                .and_then(|at| at.checked_add_days(Days::new(7)));
            let next_check = now + chrono::Duration::hours(6);
            sqlx::query(
                r#"INSERT OR IGNORE INTO episode(
                       anime_id, episode_no, expected_at, state, next_check_at
                   ) SELECT ?, ?, ?, 'waiting', ?
                     WHERE EXISTS (
                       SELECT 1 FROM anime
                       WHERE id = ? AND lifecycle = 'tracking'
                         AND (
                           total_episodes IS NULL OR ? < CASE
                             WHEN local_episode_origin IS NOT NULL
                              AND bangumi_episode_origin IS NOT NULL
                             THEN local_episode_origin + total_episodes - 1
                             ELSE total_episodes
                           END
                         )
                     )"#,
            )
            .bind(episode.anime_id)
            .bind(episode.episode_no + 1)
            .bind(next_expected)
            .bind(next_check)
            .bind(episode.anime_id)
            .bind(episode.episode_no)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "UPDATE candidate SET state = 'expired' WHERE episode_id = ? AND state = 'pending'",
        )
        .bind(episode.id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE review_notification SET status = 'cancelled' WHERE episode_id = ? AND status = 'pending'",
        )
        .bind(episode.id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE anime SET schedule_next_sync_at = ? WHERE id = ? AND auto_schedule = 1 AND lifecycle = 'tracking'",
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

    pub async fn list_management_jobs_with_targets(
        &self,
        limit: i64,
    ) -> Result<Vec<ManagementJobListRow>> {
        Ok(sqlx::query_as::<_, ManagementJobListRow>(
            r#"SELECT job.id, job.kind, job.target_type, job.target_id,
                      job.state, job.created_at, job.error,
                      anime.title AS anime_title
               FROM management_job AS job
               LEFT JOIN anime
                 ON job.target_type = 'anime'
                AND job.target_id = CAST(anime.id AS TEXT)
               ORDER BY job.created_at DESC, job.id DESC
               LIMIT ?"#,
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

    pub async fn record_source_failure(
        &self,
        source: &str,
        error: &str,
        alert_after_failures: i64,
    ) -> Result<()> {
        let now = Utc::now();
        let safe_error: String = error.chars().take(500).collect();
        sqlx::query(
            r#"INSERT INTO source_health(
                   source, consecutive_failures, first_failed_at, last_checked_at,
                   last_error, alert_attempts, next_alert_attempt_at, alert_state
               ) VALUES (?, 1, ?, ?, ?, 0, NULL,
                   CASE WHEN ? <= 1 THEN 'failure_pending' ELSE 'none' END)
               ON CONFLICT(source) DO UPDATE SET
                   consecutive_failures = source_health.consecutive_failures + 1,
                   first_failed_at = COALESCE(source_health.first_failed_at, excluded.first_failed_at),
                   last_checked_at = excluded.last_checked_at,
                   last_error = excluded.last_error,
                   alert_attempts = CASE
                       WHEN source_health.alert_state IN ('none', 'recovery_pending') THEN 0
                       ELSE source_health.alert_attempts
                   END,
                   next_alert_attempt_at = CASE
                       WHEN source_health.alert_state IN ('none', 'recovery_pending') THEN NULL
                       ELSE source_health.next_alert_attempt_at
                   END,
                   alert_state = CASE
                       WHEN source_health.alert_state = 'recovery_pending' THEN 'failure_sent'
                       WHEN source_health.alert_state = 'none'
                            AND source_health.consecutive_failures + 1 >= ?
                           THEN 'failure_pending'
                       ELSE source_health.alert_state
                   END"#,
        )
        .bind(source)
        .bind(now)
        .bind(now)
        .bind(safe_error)
        .bind(alert_after_failures)
        .bind(alert_after_failures)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn record_source_success(&self, source: &str) -> Result<()> {
        let now = Utc::now();
        sqlx::query(
            r#"INSERT INTO source_health(
                   source, consecutive_failures, first_failed_at, last_checked_at,
                   last_error, alert_attempts, next_alert_attempt_at, alert_state
               ) VALUES (?, 0, NULL, ?, NULL, 0, NULL, 'none')
               ON CONFLICT(source) DO UPDATE SET
                   consecutive_failures = CASE
                       WHEN source_health.alert_state IN ('failure_sent', 'recovery_pending')
                           THEN source_health.consecutive_failures
                       ELSE 0
                   END,
                   first_failed_at = CASE
                       WHEN source_health.alert_state IN ('failure_sent', 'recovery_pending')
                           THEN source_health.first_failed_at
                       ELSE NULL
                   END,
                   last_checked_at = excluded.last_checked_at,
                   last_error = CASE
                       WHEN source_health.alert_state IN ('failure_sent', 'recovery_pending')
                           THEN source_health.last_error
                       ELSE NULL
                   END,
                   alert_attempts = CASE
                       WHEN source_health.alert_state = 'recovery_pending'
                           THEN source_health.alert_attempts
                       ELSE 0
                   END,
                   next_alert_attempt_at = CASE
                       WHEN source_health.alert_state = 'recovery_pending'
                           THEN source_health.next_alert_attempt_at
                       ELSE NULL
                   END,
                   alert_state = CASE
                       WHEN source_health.alert_state = 'failure_sent' THEN 'recovery_pending'
                       WHEN source_health.alert_state = 'recovery_pending' THEN 'recovery_pending'
                       ELSE 'none'
                   END"#,
        )
        .bind(source)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn pending_source_alerts(&self) -> Result<Vec<PendingSourceAlert>> {
        Ok(sqlx::query_as::<_, PendingSourceAlert>(
            r#"SELECT source, alert_state, consecutive_failures, first_failed_at,
                      last_checked_at, last_error
               FROM source_health
               WHERE alert_state IN ('failure_pending', 'recovery_pending')
                 AND (next_alert_attempt_at IS NULL OR next_alert_attempt_at <= ?)
               ORDER BY last_checked_at, source"#,
        )
        .bind(Utc::now())
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn mark_source_alert_sent(&self, source: &str, alert_state: &str) -> Result<()> {
        let result = match alert_state {
            "failure_pending" => {
                sqlx::query(
                    "UPDATE source_health SET alert_state = 'failure_sent', alert_attempts = 0, next_alert_attempt_at = NULL WHERE source = ? AND alert_state = 'failure_pending'",
                )
                .bind(source)
                .execute(&self.pool)
                .await?
            }
            "recovery_pending" => {
                sqlx::query(
                    r#"UPDATE source_health SET alert_state = 'none', consecutive_failures = 0,
                           first_failed_at = NULL, last_error = NULL, alert_attempts = 0,
                           next_alert_attempt_at = NULL
                       WHERE source = ? AND alert_state = 'recovery_pending'"#,
                )
                .bind(source)
                .execute(&self.pool)
                .await?
            }
            _ => {
                return Err(AppError::InvalidInput(format!(
                    "unsupported source alert state: {alert_state}"
                )));
            }
        };
        if result.rows_affected() == 0 {
            return Err(AppError::NotFound(format!(
                "pending source alert {source}:{alert_state}"
            )));
        }
        Ok(())
    }

    pub async fn mark_source_alert_failed(&self, source: &str, error: &str) -> Result<()> {
        let safe_error: String = error.chars().take(500).collect();
        let attempts = sqlx::query_scalar::<_, i64>(
            "SELECT alert_attempts FROM source_health WHERE source = ? AND alert_state IN ('failure_pending', 'recovery_pending')",
        )
        .bind(source)
        .fetch_one(&self.pool)
        .await?;
        let exponent = attempts.clamp(0, 6) as u32;
        let retry_at = Utc::now() + chrono::Duration::seconds(60 * 2_i64.pow(exponent));
        sqlx::query(
            r#"UPDATE source_health
               SET alert_attempts = alert_attempts + 1,
                   next_alert_attempt_at = ?,
                   last_error = CASE
                       WHEN alert_state = 'failure_pending' THEN last_error
                       ELSE ?
                   END
               WHERE source = ? AND alert_state IN ('failure_pending', 'recovery_pending')"#,
        )
        .bind(retry_at)
        .bind(safe_error)
        .bind(source)
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
        let anime_count =
            sqlx::query_scalar("SELECT COUNT(*) FROM anime WHERE lifecycle != 'archived'")
                .fetch_one(&self.pool)
                .await?;
        let enabled_anime_count = sqlx::query_scalar(
            "SELECT COUNT(*) FROM anime WHERE enabled = 1 AND lifecycle = 'tracking'",
        )
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

async fn transition_released_complete_if_final_released(
    tx: &mut Transaction<'_, Sqlite>,
    anime_id: i64,
    now: DateTime<Utc>,
) -> Result<bool> {
    let anime = sqlx::query_as::<_, Anime>("SELECT * FROM anime WHERE id = ?")
        .bind(anime_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("anime {anime_id}")))?;
    if anime.lifecycle != "tracking" {
        return Ok(anime.lifecycle == "released_complete");
    }
    let Some(final_episode_no) = anime.final_episode_no()? else {
        return Ok(false);
    };
    let final_released = sqlx::query_scalar::<_, bool>(
        r#"SELECT EXISTS(
               SELECT 1 FROM episode
               WHERE anime_id = ? AND episode_no = ?
                 AND state IN ('confirmed','notified')
           )"#,
    )
    .bind(anime_id)
    .bind(final_episode_no)
    .fetch_one(&mut **tx)
    .await?;
    if !final_released {
        return Ok(false);
    }
    sqlx::query(
        r#"DELETE FROM episode
           WHERE anime_id = ? AND episode_no > ?
             AND state NOT IN ('confirmed','notified')"#,
    )
    .bind(anime_id)
    .bind(final_episode_no)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        r#"UPDATE anime SET lifecycle = 'released_complete', enabled = 0,
               released_completed_at = COALESCE(released_completed_at, ?),
               schedule_next_sync_at = NULL, updated_at = ?
           WHERE id = ? AND lifecycle = 'tracking'"#,
    )
    .bind(now)
    .bind(now)
    .bind(anime_id)
    .execute(&mut **tx)
    .await?;
    Ok(true)
}

fn map_unique_rule_error(error: sqlx::Error) -> AppError {
    match error {
        sqlx::Error::Database(error) if error.is_unique_violation() => {
            AppError::InvalidInput("an equivalent blocked keyword already exists".into())
        }
        error => error.into(),
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

    #[tokio::test]
    async fn date_only_schedule_confidence_is_persisted() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("date-only-schedule.db");
        let repository = Repository::connect(path.to_str().unwrap()).await.unwrap();
        let anime_id = repository
            .add_anime(NewAnime {
                title: "一觉醒来坐拥神装和飞船".into(),
                aliases: Vec::new(),
                next_episode: 1,
                expected_at: Some(Utc::now()),
                expected_weekday: None,
                expected_time: None,
                timezone: "Asia/Shanghai".into(),
                duration_min_sec: 1_200,
                duration_max_sec: 2_400,
                auto_schedule: Some(AutoScheduleMetadata {
                    bangumi_subject_id: 536_270,
                    anilist_media_id: Some(186_541),
                    anime_schedule_route: Some(
                        "mezametara-saikyou-soubi-to-uchuusenmochi-datta-node-ikkodate-mezashite-youhei-toshite-jiyuu-ni-ikitai".into(),
                    ),
                    total_episodes: None,
                    broadcast_pattern: "R/2026-10-01T00:00:00Z/P1D".into(),
                    schedule_source: "anime_schedule".into(),
                    schedule_confidence: "date_only".into(),
                    schedule_warning: Some("仅公布月份".into()),
                    next_sync_at: Utc::now(),
                    episode_mapping: None,
                }),
            })
            .await
            .unwrap();

        let anime = repository.get_anime(anime_id).await.unwrap();
        assert_eq!(
            anime.anime.schedule_confidence.as_deref(),
            Some("date_only")
        );
    }

    #[tokio::test]
    async fn upcoming_releases_are_ordered_windowed_and_keep_paused_schedules() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("upcoming.db");
        let repository = Repository::connect(path.to_str().unwrap()).await.unwrap();
        let now = Utc::now();
        let add = |title: &str, expected_at| NewAnime {
            title: title.into(),
            aliases: Vec::new(),
            next_episode: 1,
            expected_at: Some(expected_at),
            expected_weekday: None,
            expected_time: None,
            timezone: "Asia/Shanghai".into(),
            duration_min_sec: 1_200,
            duration_max_sec: 1_800,
            auto_schedule: None,
        };

        let later = repository
            .add_anime(add("两天后", now + chrono::Duration::days(2)))
            .await
            .unwrap();
        sqlx::query("UPDATE anime SET total_episodes = 12 WHERE id = ?")
            .bind(later)
            .execute(&repository.pool)
            .await
            .unwrap();
        let paused = repository
            .add_anime(add("明天", now + chrono::Duration::days(1)))
            .await
            .unwrap();
        repository.set_anime_enabled(paused, false).await.unwrap();
        repository
            .add_anime(add("窗口外", now + chrono::Duration::days(8)))
            .await
            .unwrap();
        let completed = repository
            .add_anime(add("已经播完", now + chrono::Duration::days(3)))
            .await
            .unwrap();
        repository
            .mark_anime_released_complete(completed, Some(1))
            .await
            .unwrap();

        let releases = repository
            .upcoming_releases(now, now + chrono::Duration::days(7))
            .await
            .unwrap();
        assert_eq!(releases.len(), 2);
        assert_eq!(releases[0].anime_id, paused);
        assert!(!releases[0].enabled);
        assert_eq!(releases[1].anime_id, later);
        assert_eq!(releases[1].total_episodes, Some(12));
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
            view_count: Some(10_000),
            reply_count: Some(20),
            uploader_follower_count: Some(1_000),
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
            view_count: Some(10_000),
            reply_count: Some(20),
            uploader_follower_count: Some(1_000),
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
        let videos = repository.list_episode_videos(anime_id).await.unwrap();
        assert_eq!(videos.len(), 1);
        assert_eq!(videos[0].bvid, candidate.bvid);
        assert!(videos[0].is_preferred);
    }

    #[tokio::test]
    async fn confirmed_episode_stays_in_watch_queue_until_marked_watched() {
        let (_directory, repository, anime_id, episode) = fixture().await;
        let (mut candidate, evaluation) = candidate();
        candidate.published_at = Utc::now() - chrono::Duration::minutes(35);
        repository
            .upsert_candidate(episode.id, &candidate, &evaluation, CandidateState::Pending)
            .await
            .unwrap();
        repository
            .confirm_candidate(episode.id, &candidate.bvid, "manual", "default", true)
            .await
            .unwrap();

        let queued = repository.watch_queue(50).await.unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].episode_id, episode.id);
        assert_eq!(queued[0].anime_id, anime_id);
        assert_eq!(queued[0].episode_no, episode.episode_no);
        assert_eq!(queued[0].bvid, candidate.bvid);
        assert_eq!(queued[0].released_at, candidate.published_at);

        let watched = repository.mark_episode_watched(episode.id).await.unwrap();
        assert_eq!(watched.anime_id, anime_id);
        assert_eq!(watched.episode_no, episode.episode_no);
        assert!(watched.auto_archive.is_none());
        repository.mark_episode_watched(episode.id).await.unwrap();
        assert!(repository.watch_queue(50).await.unwrap().is_empty());
        assert!(
            repository
                .episode(episode.id)
                .await
                .unwrap()
                .watched_at
                .is_some()
        );
    }

    #[tokio::test]
    async fn historical_video_library_supports_ranking_preference_replacement_and_blocking() {
        let (_directory, repository, anime_id, current_episode) = fixture().await;
        assert!(
            repository
                .ensure_historical_episode(anime_id, current_episode.episode_no)
                .await
                .is_err()
        );
        let historical = repository
            .ensure_historical_episode(anime_id, 7)
            .await
            .unwrap();
        assert_eq!(historical.state, "notified");
        assert!(historical.watched_at.is_some());

        let (low, _) = candidate();
        let low_id = repository
            .upsert_episode_video(anime_id, historical.id, None, &low, 40)
            .await
            .unwrap();
        let mut high = low.clone();
        high.bvid = "BVhistory002".into();
        high.title = "Silent Witch EP07 high score".into();
        high.uploader_mid = 200;
        high.uploader_name = "high score up".into();
        high.url = "https://www.bilibili.com/video/BVhistory002".into();
        repository
            .upsert_episode_video(anime_id, historical.id, None, &high, 90)
            .await
            .unwrap();

        let ranked = repository.list_episode_videos(anime_id).await.unwrap();
        assert_eq!(ranked[0].bvid, high.bvid);
        assert!(!ranked[0].is_preferred);
        repository
            .set_episode_video_preferred(anime_id, historical.id, Some(low_id))
            .await
            .unwrap();
        let preferred = repository.list_episode_videos(anime_id).await.unwrap();
        assert_eq!(preferred[0].id, low_id);
        assert!(preferred[0].is_preferred);

        let mut replacement = low.clone();
        replacement.bvid = "BVreplace003".into();
        replacement.title = "replacement".into();
        repository
            .upsert_episode_video(anime_id, historical.id, Some(low_id), &replacement, 55)
            .await
            .unwrap();
        let replaced = repository.list_episode_videos(anime_id).await.unwrap();
        let replaced = replaced.iter().find(|video| video.id == low_id).unwrap();
        assert_eq!(replaced.bvid, replacement.bvid);
        assert!(replaced.is_preferred);

        repository
            .block_uploader(anime_id, replacement.uploader_mid)
            .await
            .unwrap();
        let blocked = repository.list_episode_videos(anime_id).await.unwrap();
        let blocked = blocked.iter().find(|video| video.id == low_id).unwrap();
        assert!(blocked.manually_blocked);
        assert!(!blocked.is_preferred);
    }

    #[tokio::test]
    async fn bocchi_release_completion_waits_for_manual_archive_and_compacts_runtime_data() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("bocchi-archive.db");
        let repository = Repository::connect(path.to_str().unwrap()).await.unwrap();
        let anime_id = repository
            .add_anime(NewAnime {
                title: "孤独摇滚！".into(),
                aliases: vec!["ぼっち・ざ・ろっく！".into(), "Bocchi the Rock!".into()],
                next_episode: 12,
                expected_at: Some(Utc::now()),
                expected_weekday: None,
                expected_time: None,
                timezone: "Asia/Shanghai".into(),
                duration_min_sec: 1_200,
                duration_max_sec: 1_680,
                auto_schedule: Some(AutoScheduleMetadata {
                    bangumi_subject_id: 328_609,
                    anilist_media_id: None,
                    anime_schedule_route: None,
                    total_episodes: None,
                    broadcast_pattern: "R/2022-10-08T15:00:00Z/P7D".into(),
                    next_sync_at: Utc::now(),
                    episode_mapping: None,
                    schedule_source: "unext".into(),
                    schedule_confidence: "calibrated".into(),
                    schedule_warning: None,
                }),
            })
            .await
            .unwrap();
        let episode_twelve = repository.active_episode(anime_id).await.unwrap();
        let (mut video, evaluation) = candidate();
        video.title = "孤独摇滚！ EP12".into();
        repository
            .upsert_candidate(
                episode_twelve.id,
                &video,
                &evaluation,
                CandidateState::Pending,
            )
            .await
            .unwrap();
        repository
            .confirm_candidate(episode_twelve.id, &video.bvid, "manual", "default", true)
            .await
            .unwrap();
        let notification_id = repository.pending_notifications().await.unwrap()[0].id;
        repository
            .mark_notification_sent(notification_id)
            .await
            .unwrap();
        assert_eq!(
            repository
                .active_episode(anime_id)
                .await
                .unwrap()
                .episode_no,
            13
        );

        assert_eq!(
            repository
                .mark_anime_released_complete(anime_id, Some(12))
                .await
                .unwrap(),
            12
        );
        let released = repository.get_anime(anime_id).await.unwrap().anime;
        assert_eq!(released.lifecycle, "released_complete");
        assert!(!released.enabled);
        assert_eq!(released.total_episodes, Some(12));
        assert!(repository.active_episode(anime_id).await.is_err());
        assert_eq!(
            repository
                .list_episode_videos(anime_id)
                .await
                .unwrap()
                .len(),
            1
        );

        repository
            .resume_anime_tracking(anime_id, 13)
            .await
            .unwrap();
        assert_eq!(
            repository
                .active_episode(anime_id)
                .await
                .unwrap()
                .episode_no,
            13
        );
        assert_eq!(
            repository
                .get_anime(anime_id)
                .await
                .unwrap()
                .anime
                .lifecycle,
            "tracking"
        );
        repository
            .mark_anime_released_complete(anime_id, Some(12))
            .await
            .unwrap();

        repository
            .enqueue_management_job(
                "check_anime",
                Some("anime"),
                Some(&anime_id.to_string()),
                "{}",
                None,
                Some("bocchi:archive:test"),
            )
            .await
            .unwrap();
        let archived = repository
            .archive_anime(anime_id, Some(12), "乐队成长故事；已看完。")
            .await
            .unwrap();
        assert_eq!(archived.total_episodes, 12);
        assert_eq!(archived.removed_episodes, 1);
        assert_eq!(archived.removed_candidates, 1);
        assert_eq!(archived.removed_notifications, 1);
        assert_eq!(archived.removed_jobs, 1);

        assert!(repository.list_anime().await.unwrap().is_empty());
        assert_eq!(repository.list_archived_anime().await.unwrap().len(), 1);
        assert_eq!(repository.list_all_anime().await.unwrap().len(), 1);
        let collection = repository.get_anime(anime_id).await.unwrap().anime;
        assert_eq!(collection.lifecycle, "archived");
        assert_eq!(collection.summary, "乐队成长故事；已看完。");
        assert_eq!(collection.bangumi_subject_id, Some(328_609));
        assert!(collection.archived_at.is_some());
        assert!(
            repository
                .list_episode_videos(anime_id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(repository.list_candidates(None).await.unwrap().is_empty());
        assert!(repository.pending_notifications().await.unwrap().is_empty());
        assert!(
            repository
                .list_management_jobs(10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn active_display_numbers_close_gaps_after_an_archive() {
        let (_directory, repository, first_id, _episode) = fixture().await;
        let add = |title: &str| NewAnime {
            title: title.into(),
            aliases: vec![],
            next_episode: 1,
            expected_at: None,
            expected_weekday: None,
            expected_time: None,
            timezone: "Asia/Shanghai".into(),
            duration_min_sec: 1_200,
            duration_max_sec: 1_680,
            auto_schedule: None,
        };
        let second_id = repository.add_anime(add("孤独摇滚！")).await.unwrap();
        let third_id = repository.add_anime(add("轻音少女")).await.unwrap();
        repository
            .mark_anime_released_complete(second_id, Some(12))
            .await
            .unwrap();
        repository
            .archive_anime(second_id, Some(12), "")
            .await
            .unwrap();

        let current = repository.list_anime().await.unwrap();
        assert_eq!(
            current.iter().map(|anime| anime.id).collect::<Vec<_>>(),
            vec![first_id, third_id]
        );
        assert_eq!(repository.anime_display_number(first_id).await.unwrap(), 1);
        assert_eq!(repository.anime_display_number(third_id).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn final_notification_does_not_create_a_next_episode_after_release_completion() {
        let (_directory, repository, anime_id, episode) = fixture().await;
        let (video, evaluation) = candidate();
        repository
            .upsert_candidate(episode.id, &video, &evaluation, CandidateState::Pending)
            .await
            .unwrap();
        repository
            .confirm_candidate(episode.id, &video.bvid, "manual", "default", true)
            .await
            .unwrap();
        let notification_id = repository.pending_notifications().await.unwrap()[0].id;
        repository
            .mark_anime_released_complete(anime_id, Some(8))
            .await
            .unwrap();
        repository
            .mark_notification_sent(notification_id)
            .await
            .unwrap();

        assert!(repository.active_episode(anime_id).await.is_err());
        assert_eq!(
            repository
                .get_anime(anime_id)
                .await
                .unwrap()
                .anime
                .lifecycle,
            "released_complete"
        );
        assert_eq!(
            repository
                .list_episode_videos(anime_id)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn known_final_episode_completes_then_archives_when_watched() {
        let (_directory, repository, anime_id, episode) = fixture().await;
        sqlx::query("UPDATE anime SET total_episodes = ? WHERE id = ?")
            .bind(episode.episode_no)
            .bind(anime_id)
            .execute(&repository.pool)
            .await
            .unwrap();
        let (video, evaluation) = candidate();
        repository
            .upsert_candidate(episode.id, &video, &evaluation, CandidateState::Pending)
            .await
            .unwrap();
        repository
            .confirm_candidate(episode.id, &video.bvid, "manual", "default", true)
            .await
            .unwrap();
        let notification_id = repository.pending_notifications().await.unwrap()[0].id;
        repository
            .mark_notification_sent(notification_id)
            .await
            .unwrap();

        let completed = repository.get_anime(anime_id).await.unwrap().anime;
        assert_eq!(completed.lifecycle, "released_complete");
        assert!(!completed.enabled);
        assert!(repository.active_episode(anime_id).await.is_err());
        assert_eq!(repository.watch_queue(50).await.unwrap().len(), 1);

        let watched = repository.mark_episode_watched(episode.id).await.unwrap();
        let archived = watched.auto_archive.expect("final episode auto archives");
        assert_eq!(archived.total_episodes, episode.episode_no);
        assert_eq!(
            repository
                .get_anime(anime_id)
                .await
                .unwrap()
                .anime
                .lifecycle,
            "archived"
        );
        assert!(repository.watch_queue(50).await.unwrap().is_empty());
        assert!(repository.episode(episode.id).await.is_err());
    }

    #[tokio::test]
    async fn schedule_sync_repairs_a_previously_created_episode_past_the_finale() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("late-total.db");
        let repository = Repository::connect(path.to_str().unwrap()).await.unwrap();
        let anime_id = repository
            .add_anime(NewAnime {
                title: "Silent Witch".into(),
                aliases: vec![],
                next_episode: 8,
                expected_at: Some(Utc::now()),
                expected_weekday: None,
                expected_time: None,
                timezone: "Asia/Shanghai".into(),
                duration_min_sec: 1_200,
                duration_max_sec: 1_800,
                auto_schedule: Some(AutoScheduleMetadata {
                    bangumi_subject_id: 501_000,
                    anilist_media_id: None,
                    anime_schedule_route: None,
                    total_episodes: None,
                    broadcast_pattern: "R/2026-07-04T15:00:00Z/P7D".into(),
                    schedule_source: "danime".into(),
                    schedule_confidence: "calibrated".into(),
                    schedule_warning: None,
                    next_sync_at: Utc::now(),
                    episode_mapping: None,
                }),
            })
            .await
            .unwrap();
        let episode = repository.active_episode(anime_id).await.unwrap();
        let (video, evaluation) = candidate();
        repository
            .upsert_candidate(episode.id, &video, &evaluation, CandidateState::Pending)
            .await
            .unwrap();
        repository
            .confirm_candidate(episode.id, &video.bvid, "manual", "default", true)
            .await
            .unwrap();
        let notification_id = repository.pending_notifications().await.unwrap()[0].id;
        repository
            .mark_notification_sent(notification_id)
            .await
            .unwrap();
        assert_eq!(
            repository
                .active_episode(anime_id)
                .await
                .unwrap()
                .episode_no,
            9
        );

        repository
            .apply_schedule_update(
                anime_id,
                &ScheduleUpdate {
                    bangumi_subject_id: 501_000,
                    anilist_media_id: None,
                    anime_schedule_route: None,
                    total_episodes: Some(8),
                    aliases: vec![],
                    expected_at: None,
                    expected_weekday: None,
                    expected_time: None,
                    timezone: "Asia/Shanghai".into(),
                    broadcast_pattern: "R/2026-07-04T15:00:00Z/P7D".into(),
                    schedule_source: "danime".into(),
                    schedule_confidence: "calibrated".into(),
                    schedule_warning: None,
                    next_sync_at: Utc::now() + chrono::Duration::days(1),
                },
            )
            .await
            .unwrap();

        let anime = repository.get_anime(anime_id).await.unwrap().anime;
        assert_eq!(anime.total_episodes, Some(8));
        assert_eq!(anime.lifecycle, "released_complete");
        assert!(repository.active_episode(anime_id).await.is_err());
    }

    #[tokio::test]
    async fn mapped_subject_count_uses_the_final_local_episode() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("mapped-final.db");
        let repository = Repository::connect(path.to_str().unwrap()).await.unwrap();
        let anime_id = repository
            .add_anime(NewAnime {
                title: "Re:Zero fourth season second cour".into(),
                aliases: vec![],
                next_episode: 19,
                expected_at: Some(Utc::now()),
                expected_weekday: None,
                expected_time: None,
                timezone: "Asia/Shanghai".into(),
                duration_min_sec: 1_200,
                duration_max_sec: 1_800,
                auto_schedule: Some(AutoScheduleMetadata {
                    bangumi_subject_id: 633_836,
                    anilist_media_id: None,
                    anime_schedule_route: None,
                    total_episodes: Some(8),
                    broadcast_pattern: "R/2026-08-12T13:00:00Z/P7D".into(),
                    schedule_source: "danime".into(),
                    schedule_confidence: "calibrated".into(),
                    schedule_warning: None,
                    next_sync_at: Utc::now(),
                    episode_mapping: Some(EpisodeNumberMapping {
                        local_origin: 12,
                        bangumi_origin: 78,
                    }),
                }),
            })
            .await
            .unwrap();
        let episode = repository.active_episode(anime_id).await.unwrap();
        assert_eq!(
            repository
                .get_anime(anime_id)
                .await
                .unwrap()
                .anime
                .final_episode_no()
                .unwrap(),
            Some(19)
        );
        let (mut video, evaluation) = candidate();
        video.title = "Re:Zero EP19".into();
        repository
            .upsert_candidate(episode.id, &video, &evaluation, CandidateState::Pending)
            .await
            .unwrap();
        repository
            .confirm_candidate(episode.id, &video.bvid, "manual", "default", true)
            .await
            .unwrap();
        let notification_id = repository.pending_notifications().await.unwrap()[0].id;
        repository
            .mark_notification_sent(notification_id)
            .await
            .unwrap();
        assert_eq!(
            repository
                .get_anime(anime_id)
                .await
                .unwrap()
                .anime
                .lifecycle,
            "released_complete"
        );
        let archived = repository
            .mark_episode_watched(episode.id)
            .await
            .unwrap()
            .auto_archive
            .unwrap();
        assert_eq!(archived.total_episodes, 8);
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
    async fn source_health_alerts_once_per_outage_and_once_on_recovery() {
        let (_directory, repository, _anime_id, _episode) = fixture().await;

        repository
            .record_source_failure("bangumi-data", "first timeout", 2)
            .await
            .unwrap();
        assert!(repository.pending_source_alerts().await.unwrap().is_empty());

        repository
            .record_source_failure("bangumi-data", "second timeout", 2)
            .await
            .unwrap();
        let pending = repository.pending_source_alerts().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].alert_state, "failure_pending");
        assert_eq!(pending[0].consecutive_failures, 2);
        assert_eq!(pending[0].last_error.as_deref(), Some("second timeout"));

        repository
            .mark_source_alert_failed("bangumi-data", "Feishu timeout")
            .await
            .unwrap();
        assert!(repository.pending_source_alerts().await.unwrap().is_empty());
        sqlx::query(
            "UPDATE source_health SET next_alert_attempt_at = ? WHERE source = 'bangumi-data'",
        )
        .bind(Utc::now() - chrono::Duration::seconds(1))
        .execute(&repository.pool)
        .await
        .unwrap();
        assert_eq!(repository.pending_source_alerts().await.unwrap().len(), 1);

        repository
            .mark_source_alert_sent("bangumi-data", "failure_pending")
            .await
            .unwrap();
        repository
            .record_source_failure("bangumi-data", "third timeout", 2)
            .await
            .unwrap();
        assert!(repository.pending_source_alerts().await.unwrap().is_empty());

        repository
            .record_source_success("bangumi-data")
            .await
            .unwrap();
        let recovered = repository.pending_source_alerts().await.unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].alert_state, "recovery_pending");
        assert_eq!(recovered[0].consecutive_failures, 3);

        repository
            .record_source_success("bangumi-data")
            .await
            .unwrap();
        assert_eq!(repository.pending_source_alerts().await.unwrap().len(), 1);
        repository
            .mark_source_alert_sent("bangumi-data", "recovery_pending")
            .await
            .unwrap();
        assert!(repository.pending_source_alerts().await.unwrap().is_empty());

        repository
            .record_source_failure("bangumi-data", "new outage one", 2)
            .await
            .unwrap();
        repository
            .record_source_failure("bangumi-data", "new outage two", 2)
            .await
            .unwrap();
        assert_eq!(repository.pending_source_alerts().await.unwrap().len(), 1);
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
                    anilist_media_id: Some(195_516),
                    anime_schedule_route: Some("silent-witch".into()),
                    total_episodes: None,
                    broadcast_pattern: "R/2025-07-04T15:00:00Z/P7D".into(),
                    next_sync_at: Utc::now() + chrono::Duration::days(1),
                    episode_mapping: Some(EpisodeNumberMapping {
                        local_origin: 8,
                        bangumi_origin: 80,
                    }),
                    schedule_source: "unext".into(),
                    schedule_confidence: "calibrated".into(),
                    schedule_warning: None,
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
                    anilist_media_id: None,
                    anime_schedule_route: None,
                    total_episodes: Some(12),
                    aliases: vec!["沉默魔女".into(), "サイレント・ウィッチ".into()],
                    expected_at: Some(updated_expected),
                    expected_weekday: Some(5),
                    expected_time: Some("00:00".into()),
                    timezone: "Asia/Shanghai".into(),
                    broadcast_pattern: "R/2025-07-05T16:00:00Z/P7D".into(),
                    schedule_source: "abema".into(),
                    schedule_confidence: "calibrated".into(),
                    schedule_warning: None,
                    next_sync_at: Utc::now() + chrono::Duration::days(1),
                },
            )
            .await
            .unwrap();

        let anime = repository.get_anime(anime_id).await.unwrap();
        assert!(anime.anime.auto_schedule);
        assert_eq!(anime.anime.bangumi_subject_id, Some(506_677));
        assert_eq!(anime.anime.anilist_media_id, Some(195_516));
        assert_eq!(
            anime.anime.anime_schedule_route.as_deref(),
            Some("silent-witch")
        );
        assert_eq!(anime.anime.local_episode_origin, Some(8));
        assert_eq!(anime.anime.bangumi_episode_origin, Some(80));
        assert_eq!(anime.anime.total_episodes, Some(12));
        assert_eq!(anime.anime.schedule_source.as_deref(), Some("abema"));
        assert_eq!(
            anime.anime.schedule_confidence.as_deref(),
            Some("calibrated")
        );
        assert!(anime.aliases.iter().any(|alias| alias == "沉默魔女"));
        let episode = repository.active_episode(anime_id).await.unwrap();
        assert_eq!(episode.expected_at, Some(updated_expected));
        assert_eq!(episode.next_check_at, updated_expected);

        repository
            .apply_schedule_update(
                anime_id,
                &ScheduleUpdate {
                    bangumi_subject_id: 506_677,
                    anilist_media_id: None,
                    anime_schedule_route: None,
                    total_episodes: None,
                    aliases: vec![],
                    expected_at: None,
                    expected_weekday: None,
                    expected_time: None,
                    timezone: "Asia/Shanghai".into(),
                    broadcast_pattern: "R/2025-07-05T16:00:00Z/P7D".into(),
                    schedule_source: "bangumi-data".into(),
                    schedule_confidence: "unavailable".into(),
                    schedule_warning: Some("source conflict".into()),
                    next_sync_at: Utc::now() + chrono::Duration::days(1),
                },
            )
            .await
            .unwrap();
        let anime = repository.get_anime(anime_id).await.unwrap();
        assert_eq!(
            anime.anime.schedule_confidence.as_deref(),
            Some("unavailable")
        );
        assert_eq!(
            anime.anime.schedule_warning.as_deref(),
            Some("source conflict")
        );
        assert_eq!(
            repository
                .active_episode(anime_id)
                .await
                .unwrap()
                .expected_at,
            None
        );
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

        let listed = repository
            .list_management_jobs_with_targets(10)
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].target_id.as_deref(), Some(target.as_str()));
        assert_eq!(listed[0].anime_title.as_deref(), Some("Silent Witch"));

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
        assert!(
            repository
                .list_episode_videos(anime_id)
                .await
                .unwrap()
                .is_empty()
        );
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

    #[tokio::test]
    async fn completed_episode_candidates_cannot_confirm_the_next_episode() {
        let (_directory, repository, _anime_id, episode_eight) = fixture().await;
        let (confirmed, evaluation) = candidate();
        let mut stale = confirmed.clone();
        stale.bvid = "BVstale0001".into();
        stale.url = "https://www.bilibili.com/video/BVstale0001".into();
        repository
            .upsert_candidate(
                episode_eight.id,
                &confirmed,
                &evaluation,
                CandidateState::Pending,
            )
            .await
            .unwrap();
        repository
            .upsert_candidate(
                episode_eight.id,
                &stale,
                &evaluation,
                CandidateState::Pending,
            )
            .await
            .unwrap();
        repository
            .confirm_candidate(episode_eight.id, &confirmed.bvid, "manual", "default", true)
            .await
            .unwrap();
        let notification_id = repository.pending_notifications().await.unwrap()[0].id;
        repository
            .mark_notification_sent(notification_id)
            .await
            .unwrap();

        let stale_old = repository
            .candidate_context_for_episode(episode_eight.id, &stale.bvid)
            .await
            .unwrap()
            .0;
        assert_eq!(stale_old.state, "expired");

        let episode_nine = repository
            .active_episode(episode_eight.anime_id)
            .await
            .unwrap();
        stale.title = "Silent Witch EP09".into();
        repository
            .upsert_candidate(
                episode_nine.id,
                &stale,
                &evaluation,
                CandidateState::Pending,
            )
            .await
            .unwrap();

        assert!(
            repository
                .confirm_candidate(episode_eight.id, &stale.bvid, "manual", "default", true,)
                .await
                .is_err()
        );
        assert_eq!(
            repository
                .active_episode(episode_eight.anime_id)
                .await
                .unwrap()
                .episode_no,
            9
        );
        repository
            .confirm_candidate(episode_nine.id, &stale.bvid, "manual", "default", true)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn repair_episode_rewinds_wrong_notification_and_later_episode() {
        let (_directory, repository, anime_id, episode_eight) = fixture().await;
        let (mut video, evaluation) = candidate();
        repository
            .upsert_candidate(
                episode_eight.id,
                &video,
                &evaluation,
                CandidateState::Pending,
            )
            .await
            .unwrap();
        repository
            .confirm_candidate(episode_eight.id, &video.bvid, "manual", "default", true)
            .await
            .unwrap();
        let first_notification = repository.pending_notifications().await.unwrap()[0].id;
        repository
            .mark_notification_sent(first_notification)
            .await
            .unwrap();

        let episode_nine = repository.active_episode(anime_id).await.unwrap();
        video.bvid = "BVwrongEP09".into();
        video.title = "Silent Witch EP08 stale".into();
        video.url = "https://www.bilibili.com/video/BVwrongEP09".into();
        repository
            .upsert_candidate(
                episode_nine.id,
                &video,
                &evaluation,
                CandidateState::Pending,
            )
            .await
            .unwrap();
        repository
            .confirm_candidate(episode_nine.id, &video.bvid, "manual", "default", true)
            .await
            .unwrap();
        let wrong_notification = repository.pending_notifications().await.unwrap()[0].id;
        repository
            .mark_notification_sent(wrong_notification)
            .await
            .unwrap();
        let episode_ten = repository.active_episode(anime_id).await.unwrap();

        repository.set_anime_enabled(anime_id, false).await.unwrap();
        let summary = repository
            .repair_current_episode(anime_id, episode_ten.id, 9)
            .await
            .unwrap();

        assert_eq!(summary.episode_no, 9);
        assert_eq!(summary.removed_notifications, 1);
        assert_eq!(summary.removed_future_episodes, 1);
        let repaired = repository.active_episode(anime_id).await.unwrap();
        assert_eq!(repaired.id, episode_nine.id);
        assert_eq!(repaired.episode_no, 9);
        assert_eq!(repaired.state, "waiting");
        assert!(matches!(
            repository.episode(episode_ten.id).await,
            Err(AppError::NotFound(_))
        ));
        assert_eq!(
            repository
                .candidate_context_for_episode(episode_nine.id, &video.bvid)
                .await
                .unwrap()
                .0
                .state,
            "expired"
        );
        assert!(repository.pending_notifications().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn blocked_keywords_and_manual_trust_are_editable() {
        let (_directory, repository, anime_id, episode) = fixture().await;
        let keyword_id = repository
            .add_blocked_keyword("有声漫画", "有声漫画")
            .await
            .unwrap();
        repository
            .update_blocked_keyword(keyword_id, "有声小说", "有声小说")
            .await
            .unwrap();
        assert_eq!(
            repository.list_blocked_keywords().await.unwrap()[0].keyword,
            "有声小说"
        );
        repository.delete_blocked_keyword(keyword_id).await.unwrap();
        assert!(repository.list_blocked_keywords().await.unwrap().is_empty());

        let (video, evaluation) = candidate();
        repository
            .upsert_candidate(episode.id, &video, &evaluation, CandidateState::Pending)
            .await
            .unwrap();
        repository
            .set_uploader_flag(anime_id, video.uploader_mid, true, false)
            .await
            .unwrap();
        assert_eq!(
            repository.list_manually_trusted_uploaders().await.unwrap()[0]
                .uploader_name
                .as_deref(),
            Some("test up")
        );
        repository
            .add_manually_trusted_uploader(anime_id, 100, "可信 UP")
            .await
            .unwrap();
        repository
            .update_manually_trusted_uploader(anime_id, 100, anime_id, 101, "新名字")
            .await
            .unwrap();
        let trusted = repository.list_manually_trusted_uploaders().await.unwrap();
        assert_eq!(trusted.len(), 1);
        assert_eq!(trusted[0].uploader_mid, 101);
        assert_eq!(trusted[0].uploader_name.as_deref(), Some("新名字"));
        repository
            .remove_manual_uploader_trust(anime_id, 101)
            .await
            .unwrap();
        assert!(
            repository
                .list_manually_trusted_uploaders()
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn global_uploader_trust_applies_everywhere_and_local_block_wins() {
        let (_directory, repository, anime_id, _episode) = fixture().await;
        let other_anime_id = repository
            .add_anime(NewAnime {
                title: "另一部番".into(),
                aliases: Vec::new(),
                next_episode: 1,
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
        let mid = 778_899;
        repository
            .add_manually_trusted_uploader(anime_id, mid, "跨番上传者")
            .await
            .unwrap();
        repository
            .add_globally_trusted_uploader(mid, "跨番上传者")
            .await
            .unwrap();

        assert!(
            repository
                .list_manually_trusted_uploaders()
                .await
                .unwrap()
                .is_empty()
        );
        let global = repository.list_globally_trusted_uploaders().await.unwrap();
        assert_eq!(global.len(), 1);
        assert_eq!(global[0].uploader_mid, mid);
        assert!(matches!(
            repository
                .add_manually_trusted_uploader(other_anime_id, mid, "重复信任")
                .await,
            Err(AppError::InvalidInput(_))
        ));

        let trust = repository.uploader_trust(anime_id, mid).await.unwrap();
        assert!(trust.globally_trusted);
        assert!(!trust.manually_trusted);
        assert!(trust.is_trusted(2));
        let other_trust = repository
            .uploader_trust(other_anime_id, mid)
            .await
            .unwrap();
        assert!(other_trust.globally_trusted);
        assert!(other_trust.is_trusted(2));

        repository
            .set_uploader_flag(anime_id, mid, false, true)
            .await
            .unwrap();
        let blocked = repository.uploader_trust(anime_id, mid).await.unwrap();
        assert!(blocked.globally_trusted);
        assert!(blocked.manually_blocked);
        assert!(!blocked.is_trusted(2));

        repository
            .add_manually_trusted_uploader(anime_id, mid + 1, "即将升级")
            .await
            .unwrap();
        repository
            .update_globally_trusted_uploader(mid, mid + 1, "新 UID")
            .await
            .unwrap();
        assert!(
            repository
                .list_manually_trusted_uploaders()
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            !repository
                .uploader_trust(anime_id, mid)
                .await
                .unwrap()
                .globally_trusted
        );
        assert!(
            repository
                .uploader_trust(anime_id, mid + 1)
                .await
                .unwrap()
                .globally_trusted
        );
        repository
            .remove_global_uploader_trust(mid + 1)
            .await
            .unwrap();
        assert!(
            repository
                .list_globally_trusted_uploaders()
                .await
                .unwrap()
                .is_empty()
        );
    }
}
