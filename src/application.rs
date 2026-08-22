use std::sync::Arc;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::{
    config::AppConfig,
    detector::{evaluator, title::normalize_title},
    domain::{Anime, AutoScheduleMetadata, CandidateState, NewAnime, VideoCandidate},
    error::{AppError, Result},
    provider::{BilibiliProvider, VideoSearchProvider},
    repository::{EpisodeRepairSummary, Repository},
    schedule::ScheduleProvider,
};

#[derive(Debug, Clone, Copy)]
pub enum ManagementJobKind {
    CheckAnime,
    SyncSchedule,
    NotificationTest,
    AcceptBilibiliUrl,
    ResolveAnimeDraft,
}

impl ManagementJobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CheckAnime => "check_anime",
            Self::SyncSchedule => "sync_schedule",
            Self::NotificationTest => "notification_test",
            Self::AcceptBilibiliUrl => "accept_bilibili_url",
            Self::ResolveAnimeDraft => "resolve_anime_draft",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimeDraftRequest {
    pub title: String,
    pub next_episode: i64,
    pub timezone: String,
    pub duration_min_sec: i64,
    pub duration_max_sec: i64,
    pub auto_schedule: bool,
    pub bangumi_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimeDraftResolution {
    pub title: String,
    pub matched_title: Option<String>,
    pub aliases: Vec<String>,
    pub next_episode: i64,
    pub expected_at: Option<DateTime<Utc>>,
    pub expected_weekday: Option<i64>,
    pub expected_time: Option<String>,
    pub timezone: String,
    pub duration_min_sec: i64,
    pub duration_max_sec: i64,
    pub bangumi_subject_id: Option<i64>,
    pub broadcast_pattern: Option<String>,
    pub warning: Option<String>,
}

impl AnimeDraftResolution {
    fn into_new_anime(self, sync_interval_secs: u64) -> NewAnime {
        NewAnime {
            title: self.title,
            aliases: self.aliases,
            next_episode: self.next_episode,
            expected_at: self.expected_at,
            expected_weekday: self.expected_weekday,
            expected_time: self.expected_time,
            timezone: self.timezone,
            duration_min_sec: self.duration_min_sec,
            duration_max_sec: self.duration_max_sec,
            auto_schedule: self.bangumi_subject_id.zip(self.broadcast_pattern).map(
                |(bangumi_subject_id, broadcast_pattern)| AutoScheduleMetadata {
                    bangumi_subject_id,
                    broadcast_pattern,
                    next_sync_at: Utc::now() + chrono::Duration::seconds(sync_interval_secs as i64),
                },
            ),
        }
    }
}

#[derive(Clone)]
pub struct ApplicationService {
    repository: Repository,
    config: Arc<AppConfig>,
}

impl ApplicationService {
    pub fn new(repository: Repository, config: Arc<AppConfig>) -> Self {
        Self { repository, config }
    }

    pub fn repository(&self) -> &Repository {
        &self.repository
    }

    pub async fn set_anime_enabled(&self, anime_id: i64, enabled: bool) -> Result<()> {
        self.repository.set_anime_enabled(anime_id, enabled).await
    }

    pub async fn rename_anime(&self, anime_id: i64, title: &str) -> Result<Anime> {
        self.repository.rename_anime(anime_id, title).await
    }

    pub async fn delete_anime(&self, anime_id: i64) -> Result<Anime> {
        self.repository.delete_anime(anime_id).await
    }

    pub async fn delete_anime_checked(
        &self,
        anime_id: i64,
        expected_title: &str,
        expected_updated_at: DateTime<Utc>,
    ) -> Result<Anime> {
        self.repository
            .delete_anime_checked(anime_id, expected_title, expected_updated_at)
            .await
    }

    pub async fn accept_candidate(&self, bvid: &str) -> Result<()> {
        let (candidate, _) = self.repository.candidate_context(bvid).await?;
        self.accept_candidate_for_episode(candidate.episode_id, bvid)
            .await
    }

    pub async fn accept_candidate_for_episode(&self, episode_id: i64, bvid: &str) -> Result<()> {
        self.repository
            .candidate_context_for_episode(episode_id, bvid)
            .await?;
        self.repository
            .confirm_candidate(
                episode_id,
                bvid,
                "manual_confirmation",
                &self.config.notification.channel,
                true,
            )
            .await
    }

    pub async fn reject_candidate(&self, bvid: &str) -> Result<()> {
        self.repository.reject_candidate(bvid, true).await
    }

    pub async fn reject_candidate_for_episode(&self, episode_id: i64, bvid: &str) -> Result<()> {
        self.repository
            .reject_candidate_for_episode(episode_id, bvid, true)
            .await
    }

    pub async fn reject_all_candidates(&self, anime_id: i64) -> Result<(i64, u64)> {
        let episode = self.repository.active_episode(anime_id).await?;
        let rejected = self
            .repository
            .reject_all_candidates(episode.id, true)
            .await?;
        Ok((episode.episode_no, rejected))
    }

    pub async fn set_uploader_flag(
        &self,
        anime_id: i64,
        mid: i64,
        trusted: bool,
        blocked: bool,
    ) -> Result<()> {
        self.repository
            .set_uploader_flag(anime_id, mid, trusted, blocked)
            .await
    }

    pub async fn block_uploader(&self, anime_id: i64, mid: i64) -> Result<u64> {
        self.repository.block_uploader(anime_id, mid).await
    }

    pub async fn add_blocked_keyword(&self, keyword: &str) -> Result<i64> {
        let (keyword, normalized) = validate_blocked_keyword(keyword)?;
        self.repository
            .add_blocked_keyword(&keyword, &normalized)
            .await
    }

    pub async fn update_blocked_keyword(&self, id: i64, keyword: &str) -> Result<()> {
        let (keyword, normalized) = validate_blocked_keyword(keyword)?;
        self.repository
            .update_blocked_keyword(id, &keyword, &normalized)
            .await
    }

    pub async fn delete_blocked_keyword(&self, id: i64) -> Result<()> {
        self.repository.delete_blocked_keyword(id).await
    }

    pub async fn add_trusted_uploader(&self, anime_id: i64, mid: i64, name: &str) -> Result<()> {
        let name = validate_uploader_input(mid, name)?;
        self.repository
            .add_manually_trusted_uploader(anime_id, mid, &name)
            .await
    }

    pub async fn update_trusted_uploader(
        &self,
        old_anime_id: i64,
        old_mid: i64,
        anime_id: i64,
        mid: i64,
        name: &str,
    ) -> Result<()> {
        let name = validate_uploader_input(mid, name)?;
        self.repository
            .update_manually_trusted_uploader(old_anime_id, old_mid, anime_id, mid, &name)
            .await
    }

    pub async fn remove_trusted_uploader(&self, anime_id: i64, mid: i64) -> Result<()> {
        self.repository
            .remove_manual_uploader_trust(anime_id, mid)
            .await
    }

    pub async fn repair_current_episode(
        &self,
        anime_id: i64,
        expected_current_episode_id: i64,
        target_episode_no: i64,
    ) -> Result<EpisodeRepairSummary> {
        self.repository
            .repair_current_episode(anime_id, expected_current_episode_id, target_episode_no)
            .await
    }

    pub async fn enqueue_job(
        &self,
        kind: ManagementJobKind,
        target_type: Option<&str>,
        target_id: Option<&str>,
        payload_json: &str,
        requested_by: Option<i64>,
        dedupe_key: Option<&str>,
    ) -> Result<i64> {
        self.repository
            .enqueue_management_job(
                kind.as_str(),
                target_type,
                target_id,
                payload_json,
                requested_by,
                dedupe_key,
            )
            .await
    }

    pub async fn accept_bilibili_url(&self, anime_id: i64, input: &str) -> Result<String> {
        let episode = self.repository.active_episode(anime_id).await?;
        let bvid = self
            .import_bilibili_url_for_episode(anime_id, episode.id, input)
            .await?;
        self.repository
            .confirm_candidate(
                episode.id,
                &bvid,
                "manual_url_confirmation",
                &self.config.notification.channel,
                true,
            )
            .await?;
        Ok(bvid)
    }

    pub async fn import_bilibili_url(&self, anime_id: i64, input: &str) -> Result<String> {
        let episode = self.repository.active_episode(anime_id).await?;
        self.import_bilibili_url_for_episode(anime_id, episode.id, input)
            .await
    }

    pub async fn import_bilibili_url_for_episode(
        &self,
        anime_id: i64,
        episode_id: i64,
        input: &str,
    ) -> Result<String> {
        let bvid = parse_bilibili_bvid(input)?;
        let anime = self.repository.get_anime(anime_id).await?;
        let episode = self.repository.episode(episode_id).await?;
        if episode.anime_id != anime_id
            || matches!(episode.state.as_str(), "confirmed" | "notified")
        {
            return Err(AppError::InvalidInput(
                "the review episode is no longer active; open the current episode review page"
                    .into(),
            ));
        }
        let now = Utc::now();
        let seed = VideoCandidate {
            bvid: bvid.clone(),
            title: bvid.clone(),
            description: None,
            uploader_mid: 0,
            uploader_name: "unknown".into(),
            duration_sec: 0,
            published_at: now,
            url: format!("https://www.bilibili.com/video/{bvid}"),
            tags: Vec::new(),
            page_count: None,
            discovered_at: now,
            enriched: false,
        };
        let provider =
            BilibiliProvider::new(self.config.bilibili.clone(), self.repository.clone())?;
        let candidate = provider.enrich(&seed).await?;
        let trust = self
            .repository
            .uploader_trust(anime_id, candidate.uploader_mid)
            .await?;
        let blocked_keywords = self
            .repository
            .list_blocked_keywords()
            .await?
            .into_iter()
            .map(|row| row.normalized_keyword)
            .collect::<Vec<_>>();
        let evaluation = evaluator::evaluate(
            &anime,
            &episode,
            &candidate,
            &trust,
            self.config.confirmation.trusted_confirmed_count,
            &blocked_keywords,
        );
        self.repository
            .upsert_candidate(episode.id, &candidate, &evaluation, CandidateState::Pending)
            .await?;
        Ok(bvid)
    }

    pub async fn create_anime_draft(
        &self,
        admin_id: i64,
        session_token_hmac: &[u8],
        request: AnimeDraftRequest,
    ) -> Result<String> {
        validate_anime_draft(&request)?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let payload_json = serde_json::to_string(&request)
            .map_err(|_| AppError::InvalidInput("cannot encode anime draft".into()))?;
        if request.auto_schedule {
            self.repository
                .create_anime_draft(
                    &id,
                    admin_id,
                    session_token_hmac,
                    &payload_json,
                    "queued",
                    None,
                )
                .await?;
            self.enqueue_job(
                ManagementJobKind::ResolveAnimeDraft,
                Some("anime_draft"),
                Some(&id),
                "{}",
                Some(admin_id),
                Some(&format!("resolve_anime_draft:{id}")),
            )
            .await?;
        } else {
            let resolved = AnimeDraftResolution {
                title: request.title.clone(),
                matched_title: None,
                aliases: Vec::new(),
                next_episode: request.next_episode,
                expected_at: None,
                expected_weekday: None,
                expected_time: None,
                timezone: request.timezone.clone(),
                duration_min_sec: request.duration_min_sec,
                duration_max_sec: request.duration_max_sec,
                bangumi_subject_id: None,
                broadcast_pattern: None,
                warning: None,
            };
            self.repository
                .create_anime_draft(
                    &id,
                    admin_id,
                    session_token_hmac,
                    &payload_json,
                    "ready",
                    Some(&serde_json::to_string(&resolved).map_err(|_| {
                        AppError::InvalidInput("cannot encode anime draft result".into())
                    })?),
                )
                .await?;
        }
        Ok(id)
    }

    pub async fn resolve_anime_draft(&self, draft_id: &str) -> Result<()> {
        let draft = self
            .repository
            .anime_draft_payload_for_resolution(draft_id)
            .await?;
        let request: AnimeDraftRequest = serde_json::from_str(&draft)
            .map_err(|_| AppError::InvalidInput("anime draft payload is invalid".into()))?;
        let provider = ScheduleProvider::new(self.config.schedule.clone())?;
        let catalog = provider.load_catalog().await?;
        let resolved = provider
            .resolve(
                &catalog,
                &request.title,
                request.bangumi_id,
                request.next_episode,
                &request.timezone,
            )
            .await?;
        let input = normalize_title(&request.title);
        let matched = std::iter::once(&resolved.matched_title)
            .chain(resolved.aliases.iter())
            .any(|value| normalize_title(value) == input);
        let warning = (!matched).then(|| {
            format!(
                "输入标题与 Bangumi #{} 的标题差异较大，请确认没有选错季度或作品。",
                resolved.bangumi_subject_id
            )
        });
        let resolution = AnimeDraftResolution {
            title: request.title,
            matched_title: Some(resolved.matched_title),
            aliases: resolved.aliases,
            next_episode: request.next_episode,
            expected_at: Some(resolved.expected_at),
            expected_weekday: Some(resolved.expected_weekday),
            expected_time: Some(resolved.expected_time),
            timezone: resolved.timezone,
            duration_min_sec: request.duration_min_sec,
            duration_max_sec: request.duration_max_sec,
            bangumi_subject_id: Some(resolved.bangumi_subject_id),
            broadcast_pattern: Some(resolved.broadcast_pattern),
            warning,
        };
        let json = serde_json::to_string(&resolution)
            .map_err(|_| AppError::InvalidInput("cannot encode anime draft result".into()))?;
        self.repository
            .mark_anime_draft_ready(draft_id, &json)
            .await
    }

    pub async fn confirm_anime_draft(
        &self,
        draft_id: &str,
        admin_id: i64,
        session_token_hmac: &[u8],
        warning_confirmed: bool,
    ) -> Result<i64> {
        let draft = self
            .repository
            .claim_anime_draft(draft_id, admin_id, session_token_hmac)
            .await?;
        let resolution: AnimeDraftResolution = draft
            .resolved_json
            .as_deref()
            .ok_or_else(|| AppError::InvalidInput("anime draft has no resolution".into()))
            .and_then(|value| {
                serde_json::from_str(value)
                    .map_err(|_| AppError::InvalidInput("anime draft result is invalid".into()))
            })?;
        if resolution.warning.is_some() && !warning_confirmed {
            self.repository.release_anime_draft(draft_id).await?;
            return Err(AppError::InvalidInput(
                "the Bangumi mismatch warning must be explicitly confirmed".into(),
            ));
        }
        let result = self
            .repository
            .add_anime(resolution.into_new_anime(self.config.schedule.sync_interval_secs))
            .await;
        if result.is_err() {
            self.repository.release_anime_draft(draft_id).await?;
        }
        result
    }
}

fn validate_anime_draft(request: &AnimeDraftRequest) -> Result<()> {
    if request.title.trim().is_empty() || request.title.chars().count() > 200 {
        return Err(AppError::InvalidInput("anime title is invalid".into()));
    }
    if request.next_episode <= 0 || request.next_episode > 10_000 {
        return Err(AppError::InvalidInput("next episode is invalid".into()));
    }
    if request.duration_min_sec < 60
        || request.duration_max_sec < request.duration_min_sec
        || request.duration_max_sec > 21_600
    {
        return Err(AppError::InvalidInput("duration range is invalid".into()));
    }
    request
        .timezone
        .parse::<Tz>()
        .map_err(|_| AppError::InvalidInput("timezone is invalid".into()))?;
    if !request.auto_schedule && request.bangumi_id.is_some() {
        return Err(AppError::InvalidInput(
            "Bangumi ID requires automatic scheduling".into(),
        ));
    }
    Ok(())
}

fn validate_blocked_keyword(value: &str) -> Result<(String, String)> {
    let keyword = value.trim().to_string();
    if keyword.is_empty() || keyword.chars().count() > 80 {
        return Err(AppError::InvalidInput(
            "blocked keyword must contain between 1 and 80 characters".into(),
        ));
    }
    let normalized = normalize_title(&keyword);
    if normalized.is_empty() {
        return Err(AppError::InvalidInput(
            "blocked keyword must contain searchable characters".into(),
        ));
    }
    Ok((keyword, normalized))
}

fn validate_uploader_input(mid: i64, value: &str) -> Result<String> {
    if mid <= 0 {
        return Err(AppError::InvalidInput(
            "Bilibili uploader UID must be greater than zero".into(),
        ));
    }
    let name = value.trim();
    if name.chars().count() > 100 {
        return Err(AppError::InvalidInput(
            "uploader display name must not exceed 100 characters".into(),
        ));
    }
    Ok(if name.is_empty() {
        format!("UID {mid}")
    } else {
        name.to_string()
    })
}

pub fn parse_bilibili_bvid(value: &str) -> Result<String> {
    let value = value.trim();
    if valid_bvid(value) {
        return Ok(value.to_string());
    }

    let url = url::Url::parse(value)
        .map_err(|_| AppError::InvalidInput("expected a BV ID or Bilibili video URL".into()))?;
    if url.scheme() != "https"
        || !matches!(url.host_str(), Some("www.bilibili.com" | "m.bilibili.com"))
    {
        return Err(AppError::InvalidInput(
            "only canonical HTTPS Bilibili video URLs are accepted".into(),
        ));
    }
    let mut segments = url.path_segments().into_iter().flatten();
    if segments.next() != Some("video") {
        return Err(AppError::InvalidInput(
            "Bilibili URL must use /video/BV...".into(),
        ));
    }
    let bvid = segments.next().unwrap_or_default();
    if !valid_bvid(bvid) {
        return Err(AppError::InvalidInput(
            "Bilibili URL contains an invalid BV ID".into(),
        ));
    }
    Ok(bvid.to_string())
}

fn valid_bvid(value: &str) -> bool {
    value.len() == 12
        && value.starts_with("BV")
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_canonical_bilibili_video_inputs() {
        assert_eq!(
            parse_bilibili_bvid("https://www.bilibili.com/video/BV1Es8A6UEnr?p=1").unwrap(),
            "BV1Es8A6UEnr"
        );
        assert_eq!(parse_bilibili_bvid("BV1Es8A6UEnr").unwrap(), "BV1Es8A6UEnr");
        assert!(parse_bilibili_bvid("https://example.com/video/BV1Es8A6UEnr").is_err());
    }
}
