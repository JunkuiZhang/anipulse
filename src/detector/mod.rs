pub mod consensus;
pub mod episode;
pub mod evaluator;
pub mod title;

use std::{collections::HashSet, sync::Arc};

use chrono::Utc;
use rand::RngExt;
use tracing::{info, warn};

use crate::{
    config::AppConfig,
    domain::{CandidateState, DurationMatch, EpisodeMatch, Evaluation},
    error::Result,
    provider::{SearchQuery, VideoSearchProvider},
    repository::Repository,
};

#[derive(Clone)]
pub struct Detector {
    repository: Repository,
    provider: Arc<dyn VideoSearchProvider>,
    config: Arc<AppConfig>,
}

impl Detector {
    pub fn new(
        repository: Repository,
        provider: Arc<dyn VideoSearchProvider>,
        config: Arc<AppConfig>,
    ) -> Self {
        Self {
            repository,
            provider,
            config,
        }
    }

    pub async fn check_anime(&self, anime_id: i64) -> Result<()> {
        let anime = self.repository.get_anime(anime_id).await?;
        let episode = self.repository.active_episode(anime_id).await?;
        info!(
            anime = %anime.anime.title,
            episode = episode.episode_no,
            "checking episode"
        );

        let primary = format!("{} {}", anime.anime.title, episode.episode_no);
        let mut queries = vec![primary];
        if self.config.bilibili.max_search_requests_per_check > 1
            && let Some(alias) = anime
                .aliases
                .iter()
                .find(|alias| !alias.eq_ignore_ascii_case(&anime.anime.title))
        {
            queries.push(format!("{alias} {}", episode.episode_no));
        }
        queries.truncate(self.config.bilibili.max_search_requests_per_check);

        let mut seen_bvids = HashSet::new();
        let mut detail_requests = 0usize;
        let mut useful_results = 0usize;
        for (query_index, keyword) in queries.into_iter().enumerate() {
            if query_index > 0 && useful_results > 0 {
                break;
            }
            info!(query = %keyword, "searching Bilibili");
            let results = self.provider.search(&SearchQuery { keyword }).await?;
            info!(results = results.len(), "search completed");
            for candidate in results {
                if !seen_bvids.insert(candidate.bvid.clone()) {
                    continue;
                }
                let trust = self
                    .repository
                    .uploader_trust(anime_id, candidate.uploader_mid)
                    .await?;
                let preliminary = evaluator::evaluate(
                    &anime,
                    &episode,
                    &candidate,
                    &trust,
                    self.config.confirmation.trusted_confirmed_count,
                );
                if preliminary.hard_reject {
                    self.repository
                        .upsert_candidate(
                            episode.id,
                            &candidate,
                            &preliminary,
                            CandidateState::Rejected,
                        )
                        .await?;
                    continue;
                }

                let detailed = if preliminary.score
                    >= self.config.confirmation.detail_threshold_score
                    && detail_requests < self.config.bilibili.max_detail_requests_per_check
                {
                    detail_requests += 1;
                    match self.provider.enrich(&candidate).await {
                        Ok(detailed) => detailed,
                        Err(error) => {
                            warn!(bvid = %candidate.bvid, %error, "metadata enrichment failed; keeping search metadata pending");
                            candidate
                        }
                    }
                } else {
                    candidate
                };
                let trust = self
                    .repository
                    .uploader_trust(anime_id, detailed.uploader_mid)
                    .await?;
                let evaluation = evaluator::evaluate(
                    &anime,
                    &episode,
                    &detailed,
                    &trust,
                    self.config.confirmation.trusted_confirmed_count,
                );
                let state = if evaluation.hard_reject {
                    CandidateState::Rejected
                } else {
                    useful_results += 1;
                    CandidateState::Pending
                };
                info!(
                    bvid = %detailed.bvid,
                    score = evaluation.score,
                    state = %state,
                    reasons = ?evaluation.reasons,
                    "candidate evaluated"
                );
                self.repository
                    .upsert_candidate(episode.id, &detailed, &evaluation, state)
                    .await?;
                if evaluation.manual_review {
                    self.repository
                        .set_episode_manual_review(episode.id)
                        .await?;
                }
            }
        }

        self.repository
            .expire_candidates(
                Utc::now()
                    - chrono::Duration::seconds(self.config.confirmation.candidate_expire_secs),
            )
            .await?;

        if self.try_confirm(anime_id, episode.id).await? {
            return Ok(());
        }
        let next_check = Utc::now() + self.next_interval(episode.expected_at);
        self.repository
            .reschedule_episode(episode.id, next_check)
            .await?;
        info!(%next_check, "episode remains unconfirmed and was rescheduled");
        Ok(())
    }

    async fn try_confirm(&self, anime_id: i64, episode_id: i64) -> Result<bool> {
        let candidates = self.repository.active_candidates(episode_id).await?;
        for candidate in &candidates {
            let Ok(evaluation) = serde_json::from_str::<Evaluation>(&candidate.evaluation_json)
            else {
                continue;
            };
            let trust = self
                .repository
                .uploader_trust(anime_id, candidate.uploader_mid)
                .await?;
            if trust.is_trusted(self.config.confirmation.trusted_confirmed_count)
                && matches!(
                    evaluation.anime_match,
                    crate::domain::AnimeMatch::Exact | crate::domain::AnimeMatch::Strong
                )
                && evaluation.episode_match == EpisodeMatch::Strong
                && matches!(
                    evaluation.duration_match,
                    DurationMatch::Normal | DurationMatch::Acceptable
                )
                && evaluation.negative_keywords.is_empty()
                && !evaluation.hard_reject
            {
                self.repository
                    .confirm_candidate(
                        episode_id,
                        &candidate.bvid,
                        "trusted_uploader",
                        &self.config.notification.channel,
                        false,
                    )
                    .await?;
                info!(bvid = %candidate.bvid, "episode confirmed by trusted uploader");
                return Ok(true);
            }
        }
        if let Some((bvid, votes)) = consensus::find_consensus(
            &candidates,
            self.config.confirmation.minimum_candidate_score,
            self.config.confirmation.consensus_uploaders,
            self.config.confirmation.max_duration_delta_secs,
            self.config.confirmation.max_publish_delta_secs,
        ) {
            self.repository
                .confirm_candidate(
                    episode_id,
                    &bvid,
                    &format!("consensus:{votes}_uploaders"),
                    &self.config.notification.channel,
                    false,
                )
                .await?;
            info!(%bvid, votes, "episode confirmed by independent consensus");
            return Ok(true);
        }
        Ok(false)
    }

    fn next_interval(&self, expected_at: Option<chrono::DateTime<Utc>>) -> chrono::Duration {
        let seconds = if let Some(expected) = expected_at {
            let delta = (expected - Utc::now()).num_seconds();
            match delta {
                d if d > 172_800 => self.config.polling.far_interval_secs,
                d if d > 43_200 => self.config.polling.two_day_interval_secs,
                d if d > 10_800 => self.config.polling.half_day_interval_secs,
                d if d >= -21_600 => self.config.polling.release_interval_secs,
                d if d >= -86_400 => self.config.polling.day_after_interval_secs,
                d if d >= -259_200 => self.config.polling.three_day_interval_secs,
                _ => self.config.polling.far_interval_secs,
            }
        } else {
            self.config.polling.two_day_interval_secs
        };
        let jitter = (seconds as f64 * self.config.polling.jitter_ratio) as i64;
        let jittered = if jitter > 0 {
            rand::rng().random_range((seconds as i64 - jitter)..=(seconds as i64 + jitter))
        } else {
            seconds as i64
        };
        chrono::Duration::seconds(jittered.max(60))
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        domain::{NewAnime, VideoCandidate},
        provider::{ProviderResult, SearchQuery},
    };

    struct MockProvider {
        candidates: Vec<VideoCandidate>,
    }

    #[async_trait]
    impl VideoSearchProvider for MockProvider {
        async fn search(&self, _query: &SearchQuery) -> ProviderResult<Vec<VideoCandidate>> {
            Ok(self.candidates.clone())
        }

        async fn enrich(&self, candidate: &VideoCandidate) -> ProviderResult<VideoCandidate> {
            let mut detailed = candidate.clone();
            detailed.enriched = true;
            Ok(detailed)
        }
    }

    async fn fixture(mids: &[i64]) -> (TempDir, Repository, Detector, i64) {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("detector.db");
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
            })
            .await
            .unwrap();
        let now = Utc::now();
        let candidates = mids
            .iter()
            .enumerate()
            .map(|(index, mid)| VideoCandidate {
                bvid: format!("BVmock{index:05}"),
                title: "Silent Witch EP08".into(),
                description: None,
                uploader_mid: *mid,
                uploader_name: format!("up{mid}"),
                duration_sec: 1_420,
                published_at: now,
                url: format!("https://www.bilibili.com/video/BVmock{index:05}"),
                tags: vec![],
                page_count: Some(1),
                discovered_at: now,
                enriched: false,
            })
            .collect();
        let mut config = AppConfig::default();
        config.bilibili.max_search_requests_per_check = 1;
        let detector = Detector::new(
            repository.clone(),
            Arc::new(MockProvider { candidates }),
            Arc::new(config),
        );
        (directory, repository, detector, anime_id)
    }

    #[tokio::test]
    async fn detector_confirms_two_independent_uploaders() {
        let (_directory, repository, detector, anime_id) = fixture(&[100, 200]).await;
        let episode = repository.active_episode(anime_id).await.unwrap();
        detector.check_anime(anime_id).await.unwrap();
        assert_eq!(
            repository.episode(episode.id).await.unwrap().state,
            "confirmed"
        );
        assert_eq!(repository.pending_notifications().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn detector_does_not_treat_same_mid_as_consensus() {
        let (_directory, repository, detector, anime_id) = fixture(&[100, 100]).await;
        let episode = repository.active_episode(anime_id).await.unwrap();
        detector.check_anime(anime_id).await.unwrap();
        assert_ne!(
            repository.episode(episode.id).await.unwrap().state,
            "confirmed"
        );
        assert!(repository.pending_notifications().await.unwrap().is_empty());
    }
}
