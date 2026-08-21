use std::{collections::HashSet, path::Path, str::FromStr, time::Duration};

use chrono::{DateTime, Days, NaiveDate, Utc};
use serde_json::to_string;
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction, sqlite::SqliteConnectOptions};

use crate::{
    domain::{
        Anime, AnimeWithAliases, CandidateState, Episode, EpisodeState, Evaluation, NewAnime,
        PendingNotification, ScheduleUpdate, StoredCandidate, UploaderTrust, VideoCandidate,
    },
    error::{AppError, Result},
};

#[derive(Debug, Clone)]
pub struct Repository {
    pool: SqlitePool,
}

#[derive(Debug, Clone, FromRow)]
pub struct CandidateListRow {
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
        }
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePool::connect_with(options).await?;
        sqlx::migrate!()
            .run(&pool)
            .await
            .map_err(|error| sqlx::Error::Migrate(Box::new(error)))?;
        Ok(Self { pool })
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
                r#"SELECT c.bvid, a.title AS anime_title, e.episode_no,
                          c.uploader_mid, c.uploader_name, c.title, c.duration_sec,
                          c.published_at, c.score, c.state, c.seen_count, c.evaluation_json
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
                r#"SELECT c.bvid, a.title AS anime_title, e.episode_no,
                          c.uploader_mid, c.uploader_name, c.title, c.duration_sec,
                          c.published_at, c.score, c.state, c.seen_count, c.evaluation_json
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
}
