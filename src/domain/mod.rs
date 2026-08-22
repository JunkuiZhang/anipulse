use std::{fmt, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::error::{AppError, Result};

macro_rules! string_enum {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let value = match self { $(Self::$variant => $value),+ };
                f.write_str(value)
            }
        }

        impl FromStr for $name {
            type Err = AppError;
            fn from_str(value: &str) -> Result<Self> {
                match value {
                    $($value => Ok(Self::$variant),)+
                    _ => Err(AppError::InvalidInput(format!("unknown {}: {value}", stringify!($name)))),
                }
            }
        }
    };
}

#[derive(Debug, Clone, FromRow)]
pub struct Anime {
    pub id: i64,
    pub title: String,
    pub bangumi_subject_id: Option<i64>,
    pub expected_weekday: Option<i64>,
    pub expected_time: Option<String>,
    pub timezone: String,
    pub duration_min_sec: i64,
    pub duration_max_sec: i64,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub auto_schedule: bool,
    pub broadcast_pattern: Option<String>,
    pub schedule_sync_at: Option<DateTime<Utc>>,
    pub schedule_next_sync_at: Option<DateTime<Utc>>,
    pub schedule_sync_error: Option<String>,
    pub schedule_source: Option<String>,
    pub schedule_confidence: Option<String>,
    pub schedule_warning: Option<String>,
    pub local_episode_origin: Option<i64>,
    pub bangumi_episode_origin: Option<i64>,
    pub lifecycle: String,
    pub summary: String,
    pub total_episodes: Option<i64>,
    pub released_completed_at: Option<DateTime<Utc>>,
    pub archived_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct AnimeWithAliases {
    pub anime: Anime,
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct NewAnime {
    pub title: String,
    pub aliases: Vec<String>,
    pub next_episode: i64,
    pub expected_at: Option<DateTime<Utc>>,
    pub expected_weekday: Option<i64>,
    pub expected_time: Option<String>,
    pub timezone: String,
    pub duration_min_sec: i64,
    pub duration_max_sec: i64,
    pub auto_schedule: Option<AutoScheduleMetadata>,
}

#[derive(Debug, Clone)]
pub struct AutoScheduleMetadata {
    pub bangumi_subject_id: i64,
    pub broadcast_pattern: String,
    pub schedule_source: String,
    pub schedule_confidence: String,
    pub schedule_warning: Option<String>,
    pub next_sync_at: DateTime<Utc>,
    pub episode_mapping: Option<EpisodeNumberMapping>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpisodeNumberMapping {
    pub local_origin: i64,
    pub bangumi_origin: i64,
}

impl EpisodeNumberMapping {
    pub fn mapped_numbers(self, local_episode: i64) -> Result<(i64, i64)> {
        if self.local_origin <= 0 || self.bangumi_origin <= 0 {
            return Err(AppError::InvalidInput(
                "episode mapping origins must be greater than zero".into(),
            ));
        }
        if local_episode < self.local_origin {
            return Err(AppError::InvalidInput(
                "next episode must not be earlier than the mapped local origin".into(),
            ));
        }
        let delta = local_episode
            .checked_sub(self.local_origin)
            .ok_or_else(|| {
                AppError::InvalidInput(
                    "next episode must not be earlier than the mapped local origin".into(),
                )
            })?;
        let subject_index = delta.checked_add(1).ok_or_else(|| {
            AppError::InvalidInput("episode mapping calculation overflowed".into())
        })?;
        let bangumi_episode = self.bangumi_origin.checked_add(delta).ok_or_else(|| {
            AppError::InvalidInput("episode mapping calculation overflowed".into())
        })?;
        Ok((subject_index, bangumi_episode))
    }
}

#[derive(Debug, Clone)]
pub struct ScheduleUpdate {
    pub bangumi_subject_id: i64,
    pub aliases: Vec<String>,
    pub expected_at: Option<DateTime<Utc>>,
    pub expected_weekday: Option<i64>,
    pub expected_time: Option<String>,
    pub timezone: String,
    pub broadcast_pattern: String,
    pub schedule_source: String,
    pub schedule_confidence: String,
    pub schedule_warning: Option<String>,
    pub next_sync_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeState {
    Waiting,
    Watching,
    CandidateFound,
    Confirmed,
    Notified,
    NeedsManualReview,
}

string_enum!(EpisodeState {
    Waiting => "waiting",
    Watching => "watching",
    CandidateFound => "candidate_found",
    Confirmed => "confirmed",
    Notified => "notified",
    NeedsManualReview => "needs_manual_review"
});

#[derive(Debug, Clone, FromRow)]
pub struct Episode {
    pub id: i64,
    pub anime_id: i64,
    pub episode_no: i64,
    pub expected_at: Option<DateTime<Utc>>,
    pub state: String,
    pub next_check_at: DateTime<Utc>,
    pub first_candidate_at: Option<DateTime<Utc>>,
    pub confirmed_at: Option<DateTime<Utc>>,
    pub notified_at: Option<DateTime<Utc>>,
}

impl Episode {
    pub fn parsed_state(&self) -> Result<EpisodeState> {
        self.state.parse()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateState {
    Pending,
    Confirmed,
    Rejected,
    Expired,
}

string_enum!(CandidateState {
    Pending => "pending",
    Confirmed => "confirmed",
    Rejected => "rejected",
    Expired => "expired"
});

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoCandidate {
    pub bvid: String,
    pub title: String,
    pub description: Option<String>,
    pub uploader_mid: i64,
    pub uploader_name: String,
    pub duration_sec: i64,
    pub published_at: DateTime<Utc>,
    pub url: String,
    pub tags: Vec<String>,
    pub page_count: Option<i64>,
    pub discovered_at: DateTime<Utc>,
    pub enriched: bool,
}

#[derive(Debug, Clone, FromRow)]
pub struct StoredCandidate {
    pub id: i64,
    pub episode_id: i64,
    pub bvid: String,
    pub uploader_mid: i64,
    pub uploader_name: String,
    pub title: String,
    pub description: Option<String>,
    pub duration_sec: i64,
    pub published_at: DateTime<Utc>,
    pub url: String,
    pub tags_json: String,
    pub page_count: Option<i64>,
    pub score: i64,
    pub state: String,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub seen_count: i64,
    pub evaluation_json: String,
}

#[derive(Debug, Clone, Default, FromRow)]
pub struct UploaderTrust {
    pub anime_id: i64,
    pub uploader_mid: i64,
    pub uploader_name: Option<String>,
    pub confirmed_count: i64,
    pub rejected_count: i64,
    pub manually_trusted: bool,
    pub manually_blocked: bool,
}

impl UploaderTrust {
    pub fn is_trusted(&self, required_count: i64) -> bool {
        self.manually_trusted
            || (self.confirmed_count >= required_count && self.rejected_count == 0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnimeMatch {
    Exact,
    Strong,
    Weak,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeMatch {
    Strong,
    Weak,
    None,
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurationMatch {
    Normal,
    Acceptable,
    Suspicious,
    TooShort,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evaluation {
    pub anime_match: AnimeMatch,
    pub episode_match: EpisodeMatch,
    pub duration_match: DurationMatch,
    pub expected_time_delta_sec: Option<i64>,
    pub trusted_uploader: bool,
    pub blocked_uploader: bool,
    pub negative_keywords: Vec<String>,
    pub metadata_enriched: bool,
    pub score: i32,
    pub hard_reject: bool,
    pub manual_review: bool,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct PendingNotification {
    pub id: i64,
    pub episode_id: i64,
    pub channel: String,
    pub attempts: i64,
    pub anime_title: String,
    pub episode_no: i64,
    pub bvid: Option<String>,
    pub uploader_name: Option<String>,
    pub duration_sec: Option<i64>,
    pub url: Option<String>,
    pub confirmation_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReviewCandidateSummary {
    pub bvid: String,
    pub title: String,
    pub uploader_name: String,
    pub duration_sec: i64,
    pub score: i64,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct PendingReviewNotification {
    pub id: i64,
    pub episode_id: i64,
    pub channel: String,
    pub attempts: i64,
    pub anime_title: String,
    pub episode_no: i64,
    pub candidate_fingerprint: String,
    pub review_url: String,
    pub candidates: Vec<ReviewCandidateSummary>,
}
