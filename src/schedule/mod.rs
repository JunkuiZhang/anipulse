use std::{
    collections::{HashMap, HashSet},
    env,
    sync::LazyLock,
    time::Duration as StdDuration,
};

use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDate, TimeZone, Timelike, Utc};
use chrono_tz::{Asia::Tokyo, Tz};
use regex::Regex;
use reqwest::Client;
use serde::Deserialize;
use tracing::{info, warn};

use crate::{
    config::ScheduleConfig,
    detector::title::normalize_title,
    domain::{EpisodeNumberMapping, ScheduleUpdate},
    error::{AppError, Result},
    repository::Repository,
};

const MAX_CATALOG_BYTES: u64 = 16 * 1024 * 1024;
const CATALOG_SOURCE: &str = "bangumi-data";
const SOURCE_ALERT_FAILURE_THRESHOLD: i64 = 2;
const STREAM_CONSENSUS_WINDOW_SECS: i64 = 2 * 60 * 60;
const MIN_STREAM_SOURCE_FAMILIES: usize = 2;
const MAX_METADATA_BYTES: u64 = 2 * 1024 * 1024;
const ANIME_SCHEDULE_TOKEN_ENV: &str = "ANIME_SCHEDULE_TOKEN";

static EAST_ASIAN_SEASON: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:第\s*)?([0-9一二两三四五六七八九十]+)\s*[期季]")
        .expect("valid East Asian season regex")
});
static LATIN_SEASON: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:season[\s.\-]*(\d{1,2})|(\d{1,2})(?:st|nd|rd|th)?[\s.\-]*season)")
        .expect("valid Latin season regex")
});

#[derive(Clone)]
pub struct ScheduleProvider {
    client: Client,
    config: ScheduleConfig,
    anime_schedule_token: Option<String>,
}

pub struct ScheduleCatalog {
    items: Vec<BangumiDataItem>,
}

pub struct AutoScheduleRequest<'a> {
    pub title: &'a str,
    pub subject_id: Option<i64>,
    pub next_episode: i64,
    pub episode_mapping: Option<EpisodeNumberMapping>,
    pub anilist_media_id: Option<i64>,
    pub anime_schedule_route: Option<&'a str>,
    pub timezone: &'a str,
}

#[derive(Debug, Clone)]
pub struct ResolvedSchedule {
    pub bangumi_subject_id: i64,
    pub anilist_media_id: Option<i64>,
    pub anime_schedule_route: Option<String>,
    pub total_episodes: Option<i64>,
    pub matched_title: String,
    pub aliases: Vec<String>,
    pub expected_at: Option<DateTime<Utc>>,
    pub expected_weekday: Option<i64>,
    pub expected_time: Option<String>,
    pub timezone: String,
    pub broadcast_pattern: String,
    pub schedule_source: String,
    pub schedule_confidence: String,
    pub schedule_warning: Option<String>,
    source_health_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BangumiPublicRating {
    pub score: Option<f64>,
    pub total: i64,
    pub rank: Option<i64>,
}

#[derive(Clone)]
pub struct ScheduleSynchronizer {
    repository: Repository,
    provider: ScheduleProvider,
    config: ScheduleConfig,
}

#[derive(Debug, Deserialize)]
struct BangumiDataDocument {
    items: Vec<BangumiDataItem>,
}

#[derive(Debug, Deserialize)]
struct BangumiDataItem {
    title: String,
    #[serde(rename = "titleTranslate", default)]
    title_translate: HashMap<String, Vec<String>>,
    #[serde(rename = "type")]
    item_type: String,
    begin: String,
    #[serde(default)]
    broadcast: Option<String>,
    #[serde(default)]
    sites: Vec<BangumiDataSite>,
}

#[derive(Debug, Deserialize)]
struct BangumiDataSite {
    site: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    begin: Option<String>,
    #[serde(default)]
    broadcast: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PagedEpisodes {
    #[serde(default)]
    total: Option<i64>,
    #[serde(default)]
    data: Vec<BangumiEpisode>,
}

struct EpisodeLookup {
    airdate: Option<NaiveDate>,
    total_episodes: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct BangumiEpisode {
    #[serde(default)]
    airdate: Option<String>,
    #[serde(default)]
    sort: f64,
    #[serde(default)]
    ep: f64,
}

#[derive(Debug, Deserialize)]
struct BangumiSubject {
    id: i64,
    name: String,
    #[serde(default)]
    name_cn: String,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    platform: Option<String>,
    #[serde(default)]
    total_episodes: Option<i64>,
    #[serde(default)]
    rating: Option<BangumiRating>,
}

#[derive(Debug, Deserialize)]
struct BangumiRating {
    #[serde(default)]
    total: i64,
    #[serde(default)]
    score: f64,
    #[serde(default)]
    rank: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnimeSchedulePage {
    #[serde(default)]
    anime: Vec<AnimeScheduleAnime>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnimeScheduleAnime {
    title: String,
    route: String,
    #[serde(default)]
    premier: Option<DateTime<Utc>>,
    #[serde(default)]
    month: Option<String>,
    #[serde(default)]
    year: Option<i32>,
    #[serde(default)]
    episode_override: Option<AnimeScheduleEpisodeOverride>,
    #[serde(default)]
    delayed_from: Option<DateTime<Utc>>,
    #[serde(default)]
    delayed_until: Option<DateTime<Utc>>,
    #[serde(default)]
    episodes: Option<i64>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    names: Option<AnimeScheduleNames>,
    #[serde(default)]
    websites: Option<AnimeScheduleWebsites>,
    #[serde(default)]
    media_types: Vec<AnimeScheduleCategory>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AnimeScheduleNames {
    #[serde(default)]
    romaji: Option<String>,
    #[serde(default)]
    english: Option<String>,
    #[serde(default)]
    native: Option<String>,
    #[serde(default)]
    abbreviation: Option<String>,
    #[serde(default)]
    synonyms: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnimeScheduleWebsites {
    #[serde(default)]
    ani_list: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct AnimeScheduleCategory {
    route: String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnimeScheduleEpisodeOverride {
    override_date: DateTime<Utc>,
    override_episode: i64,
    #[serde(default)]
    episodes_aired: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnimeScheduleTimetableEntry {
    route: String,
    episode_date: DateTime<Utc>,
    episode_number: i64,
    #[serde(default)]
    subtracted_episode_number: Option<i64>,
    #[serde(default)]
    delayed_text: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct Recurrence {
    anchor: DateTime<Utc>,
    period: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BroadcastSourceKind {
    Stream,
    Catalog,
}

struct ExpectedSchedule {
    expected_at: Option<DateTime<Utc>>,
    confidence: &'static str,
    warning: Option<String>,
    health_error: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct EpisodeScheduleTarget {
    local_episode: i64,
    mapping: Option<EpisodeNumberMapping>,
}

impl ScheduleProvider {
    pub fn new(config: ScheduleConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(StdDuration::from_secs(config.request_timeout_secs))
            .user_agent(&config.user_agent)
            .build()
            .map_err(|error| {
                AppError::Schedule(format!("cannot build schedule HTTP client: {error}"))
            })?;
        let anime_schedule_token = env::var(ANIME_SCHEDULE_TOKEN_ENV)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        Ok(Self {
            client,
            config,
            anime_schedule_token,
        })
    }

    pub async fn load_catalog(&self) -> Result<ScheduleCatalog> {
        let response = self
            .client
            .get(&self.config.bangumi_data_url)
            .send()
            .await
            .map_err(|error| safe_request_error(error, "bangumi-data"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(AppError::Schedule(format!(
                "bangumi-data returned HTTP {status}"
            )));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_CATALOG_BYTES)
        {
            return Err(AppError::Schedule(format!(
                "bangumi-data response exceeds {} MiB",
                MAX_CATALOG_BYTES / 1024 / 1024
            )));
        }
        let body = response
            .bytes()
            .await
            .map_err(|error| safe_request_error(error, "bangumi-data response"))?;
        let document: BangumiDataDocument = serde_json::from_slice(&body).map_err(|error| {
            AppError::Schedule(format!("bangumi-data returned invalid JSON: {error}"))
        })?;
        if document.items.is_empty() {
            return Err(AppError::Schedule(
                "bangumi-data catalog contains no items".into(),
            ));
        }
        Ok(ScheduleCatalog {
            items: document.items,
        })
    }

    pub async fn resolve(
        &self,
        catalog: &ScheduleCatalog,
        title: &str,
        subject_id: Option<i64>,
        next_episode: i64,
        timezone: &str,
    ) -> Result<ResolvedSchedule> {
        self.resolve_at(
            catalog,
            title,
            subject_id,
            next_episode,
            timezone,
            Utc::now(),
        )
        .await
    }

    pub async fn resolve_with_mapping(
        &self,
        catalog: &ScheduleCatalog,
        title: &str,
        subject_id: Option<i64>,
        next_episode: i64,
        episode_mapping: EpisodeNumberMapping,
        timezone: &str,
    ) -> Result<ResolvedSchedule> {
        self.resolve_mapped_at(
            catalog,
            title,
            subject_id,
            EpisodeScheduleTarget {
                local_episode: next_episode,
                mapping: Some(episode_mapping),
            },
            timezone,
            Utc::now(),
        )
        .await
    }

    /// Resolves from bangumi-data when possible. An explicit Bangumi subject that
    /// is not in the catalog falls back to Bangumi metadata plus AnimeSchedule.
    /// AniList IDs are lookup keys only; AniList itself is never contacted.
    pub async fn resolve_auto(
        &self,
        catalog: &ScheduleCatalog,
        request: AutoScheduleRequest<'_>,
    ) -> Result<ResolvedSchedule> {
        let target = EpisodeScheduleTarget {
            local_episode: request.next_episode,
            mapping: request.episode_mapping,
        };
        if let Some(subject_id) = request.subject_id
            && !catalog
                .items
                .iter()
                .any(|item| item_subject_id(item) == Some(subject_id))
        {
            return self
                .resolve_anime_schedule_fallback(
                    subject_id,
                    target,
                    request.anilist_media_id,
                    request.anime_schedule_route,
                    request.timezone,
                )
                .await;
        }
        self.resolve_mapped_at(
            catalog,
            request.title,
            request.subject_id,
            target,
            request.timezone,
            Utc::now(),
        )
        .await
    }

    async fn resolve_at(
        &self,
        catalog: &ScheduleCatalog,
        title: &str,
        subject_id: Option<i64>,
        next_episode: i64,
        timezone: &str,
        now: DateTime<Utc>,
    ) -> Result<ResolvedSchedule> {
        self.resolve_mapped_at(
            catalog,
            title,
            subject_id,
            EpisodeScheduleTarget {
                local_episode: next_episode,
                mapping: None,
            },
            timezone,
            now,
        )
        .await
    }

    async fn resolve_mapped_at(
        &self,
        catalog: &ScheduleCatalog,
        title: &str,
        subject_id: Option<i64>,
        episode_target: EpisodeScheduleTarget,
        timezone: &str,
        now: DateTime<Utc>,
    ) -> Result<ResolvedSchedule> {
        let next_episode = episode_target.local_episode;
        if next_episode <= 0 {
            return Err(AppError::Schedule(
                "next episode must be greater than zero".into(),
            ));
        }
        let timezone = timezone
            .parse::<Tz>()
            .map_err(|_| AppError::Schedule(format!("invalid timezone: {timezone}")))?;
        let item = match_item(catalog, title, subject_id)?;
        let bangumi_subject_id = item_subject_id(item).ok_or_else(|| {
            AppError::Schedule(format!(
                "matched item '{}' has no Bangumi subject ID",
                item.title
            ))
        })?;
        let (subject_episode_index, bangumi_episode_no) = episode_target
            .mapping
            .map(|mapping| mapping.mapped_numbers(next_episode))
            .transpose()?
            .unwrap_or((next_episode, next_episode));
        let bangumi_origin_episode = episode_target
            .mapping
            .map(|mapping| mapping.bangumi_origin)
            .unwrap_or(1);
        let mut source_health_error = None;
        let (target_airdate, subject_episode_count) = match self
            .episode_airdate(
                bangumi_subject_id,
                subject_episode_index,
                bangumi_episode_no,
            )
            .await
        {
            Ok(value) => (value.airdate, value.total_episodes),
            Err(error) => {
                warn!(bangumi_subject_id, next_episode, bangumi_episode_no, %error, "Bangumi episode date unavailable; using a lower-confidence schedule");
                source_health_error = Some(error.to_string());
                (None, None)
            }
        };
        let origin_airdate = if subject_episode_index == 1 {
            target_airdate
        } else if target_airdate.is_some() {
            match self
                .episode_airdate(bangumi_subject_id, 1, bangumi_origin_episode)
                .await
            {
                Ok(value) => value.airdate,
                Err(error) => {
                    warn!(bangumi_subject_id, bangumi_origin_episode, %error, "Bangumi season origin date unavailable; using a lower-confidence schedule");
                    source_health_error = Some(error.to_string());
                    None
                }
            }
        } else {
            None
        };
        let total_episodes = subject_episode_count
            .filter(|total| *total > 0 && *total <= 10_000)
            .map(|total| {
                episode_target
                    .mapping
                    .map(|mapping| mapping.final_local_episode(total))
                    .transpose()
                    .map_err(|error| AppError::Schedule(error.to_string()))
                    .map(|_| total)
            })
            .transpose()?;
        let selected = select_broadcast(item, &self.config, origin_airdate)?;
        let recurrence = parse_recurrence(&selected.pattern)?;
        let expected = expected_at(
            recurrence,
            subject_episode_index,
            target_airdate,
            origin_airdate,
            selected.kind,
            match selected.kind {
                BroadcastSourceKind::Stream => self.config.max_stream_offset_days,
                BroadcastSourceKind::Catalog => self.config.max_catalog_offset_days,
            },
            now,
        )?;
        let local = expected
            .expected_at
            .map(|expected_at| expected_at.with_timezone(&timezone));
        let schedule_warning = merge_warnings(
            merge_warnings(selected.warning, expected.warning),
            source_health_error
                .as_ref()
                .map(|error| format!("Bangumi 章节日期请求失败：{error}")),
        );
        let source_health_error = expected.health_error.or(source_health_error);

        Ok(ResolvedSchedule {
            bangumi_subject_id,
            anilist_media_id: None,
            anime_schedule_route: None,
            total_episodes,
            matched_title: item.title.clone(),
            aliases: item_aliases(item),
            expected_at: expected.expected_at,
            expected_weekday: local
                .as_ref()
                .map(|value| i64::from(value.weekday().num_days_from_monday())),
            expected_time: local.map(|value| value.format("%H:%M").to_string()),
            timezone: timezone.name().to_string(),
            broadcast_pattern: selected.pattern,
            schedule_source: selected.source,
            schedule_confidence: expected.confidence.into(),
            schedule_warning,
            source_health_error,
        })
    }

    async fn episode_airdate(
        &self,
        subject_id: i64,
        subject_episode_index: i64,
        bangumi_episode_no: i64,
    ) -> Result<EpisodeLookup> {
        let offset = subject_episode_index - 1;
        let response = self
            .client
            .get(format!(
                "{}/v0/episodes",
                self.config.bangumi_api_base_url.trim_end_matches('/')
            ))
            .query(&[
                ("subject_id", subject_id.to_string()),
                ("type", "0".into()),
                ("limit", "1".into()),
                ("offset", offset.to_string()),
            ])
            .send()
            .await
            .map_err(|error| safe_request_error(error, "Bangumi episode API"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(AppError::Schedule(format!(
                "Bangumi episode API returned HTTP {status}"
            )));
        }
        let body = response
            .bytes()
            .await
            .map_err(|error| safe_request_error(error, "Bangumi episode API response"))?;
        let page: PagedEpisodes = serde_json::from_slice(&body).map_err(|error| {
            AppError::Schedule(format!(
                "Bangumi episode API returned invalid JSON: {error}"
            ))
        })?;
        let total_episodes = page.total.filter(|total| *total > 0 && *total <= 10_000);
        let Some(episode) = page.data.first() else {
            return Ok(EpisodeLookup {
                airdate: None,
                total_episodes,
            });
        };
        let returned_number = if episode.ep > 0.0 {
            episode.ep
        } else {
            episode.sort
        };
        if (returned_number - bangumi_episode_no as f64).abs() > 0.01 {
            return Ok(EpisodeLookup {
                airdate: None,
                total_episodes,
            });
        }
        let airdate = parse_metadata_date(
            episode.airdate.as_deref(),
            "Bangumi episode API returned invalid airdate",
        )?;
        Ok(EpisodeLookup {
            airdate,
            total_episodes,
        })
    }

    async fn resolve_anime_schedule_fallback(
        &self,
        subject_id: i64,
        episode_target: EpisodeScheduleTarget,
        anilist_media_id: Option<i64>,
        anime_schedule_route: Option<&str>,
        timezone: &str,
    ) -> Result<ResolvedSchedule> {
        if episode_target.local_episode <= 0 {
            return Err(AppError::Schedule(
                "next episode must be greater than zero".into(),
            ));
        }
        let timezone = timezone
            .parse::<Tz>()
            .map_err(|_| AppError::Schedule(format!("invalid timezone: {timezone}")))?;
        let subject = self.bangumi_subject(subject_id).await?;
        let (subject_episode_index, bangumi_episode_no) = episode_target
            .mapping
            .map(|mapping| mapping.mapped_numbers(episode_target.local_episode))
            .transpose()?
            .unwrap_or((episode_target.local_episode, episode_target.local_episode));
        let episode = self
            .episode_airdate(subject_id, subject_episode_index, bangumi_episode_no)
            .await?;
        let subject_date = parse_metadata_date(
            subject.date.as_deref(),
            "Bangumi subject API returned invalid date",
        )?;
        let media = self
            .resolve_anime_schedule_media(
                &subject,
                subject_date,
                anilist_media_id,
                anime_schedule_route,
            )
            .await?;
        let (mut expected_at, mut confidence, mut timing_warning, query_timetable) =
            anime_schedule_estimate(
                episode.airdate,
                subject_date,
                &media,
                subject_episode_index,
                timezone,
            )?;
        let mut source_health_error = None;
        if query_timetable {
            match self
                .anime_schedule_timetable(expected_at, &media.route, subject_episode_index)
                .await
            {
                Ok(Some(exact)) => {
                    expected_at = exact.episode_date;
                    confidence = "calibrated";
                    timing_warning = exact
                        .delayed_text
                        .map(|detail| format!("AnimeSchedule 标记本集排期有变动：{detail}"));
                }
                Ok(None) => {}
                Err(error) => {
                    source_health_error = Some(error.to_string());
                    timing_warning = merge_warnings(
                        timing_warning,
                        Some(format!(
                            "AnimeSchedule 周排期暂时不可用，已保留首播时间推算值：{error}"
                        )),
                    );
                }
            }
        }
        let batch_release = anime_schedule_is_ona(&media);
        let anchor = if batch_release {
            expected_at
        } else {
            expected_at - Duration::days((subject_episode_index - 1) * 7)
        };
        let local = expected_at.with_timezone(&timezone);
        let total_episodes = episode
            .total_episodes
            .or(subject.total_episodes)
            .or(media.episodes)
            .filter(|total| *total > 0 && *total <= 10_000);
        if let (Some(mapping), Some(total)) = (episode_target.mapping, total_episodes) {
            mapping.final_local_episode(total)?;
        }
        let aliases = anime_schedule_aliases(&subject, &media);
        let resolved_anilist_id = anilist_media_id.or_else(|| anime_schedule_anilist_id(&media));
        let fallback_warning = format!(
            "bangumi-data 尚未收录 Bangumi #{}；已绑定 AnimeSchedule '{}'，系统会每日复查并在正式目录收录后自动切回平台排期。",
            subject.id, media.route
        );
        Ok(ResolvedSchedule {
            bangumi_subject_id: subject.id,
            anilist_media_id: resolved_anilist_id,
            anime_schedule_route: Some(media.route),
            total_episodes,
            matched_title: subject.name,
            aliases,
            expected_at: Some(expected_at),
            expected_weekday: Some(i64::from(local.weekday().num_days_from_monday())),
            expected_time: (confidence != "date_only").then(|| local.format("%H:%M").to_string()),
            timezone: timezone.name().to_string(),
            broadcast_pattern: format!("R/{}/P7D", anchor.to_rfc3339()),
            schedule_source: "anime_schedule".into(),
            schedule_confidence: confidence.into(),
            schedule_warning: merge_warnings(Some(fallback_warning), timing_warning),
            source_health_error,
        })
    }

    async fn bangumi_subject(&self, subject_id: i64) -> Result<BangumiSubject> {
        let url = format!(
            "{}/v0/subjects/{subject_id}",
            self.config.bangumi_api_base_url.trim_end_matches('/')
        );
        self.get_json(&url, "Bangumi subject API").await
    }

    pub async fn bangumi_public_rating(&self, subject_id: i64) -> Result<BangumiPublicRating> {
        let rating = self.bangumi_subject(subject_id).await?.rating;
        let total = rating.as_ref().map(|value| value.total.max(0)).unwrap_or(0);
        let score = rating
            .as_ref()
            .filter(|value| value.total > 0 && value.score.is_finite())
            .map(|value| value.score)
            .filter(|score| *score > 0.0 && *score <= 10.0);
        let rank = rating
            .and_then(|value| value.rank)
            .filter(|value| *value > 0);
        Ok(BangumiPublicRating { score, total, rank })
    }

    async fn resolve_anime_schedule_media(
        &self,
        subject: &BangumiSubject,
        subject_date: Option<NaiveDate>,
        anilist_media_id: Option<i64>,
        persisted_route: Option<&str>,
    ) -> Result<AnimeScheduleAnime> {
        if let Some(route) = persisted_route {
            validate_anime_schedule_route(route)?;
            let media = self.anime_schedule_media(route).await?;
            validate_anime_schedule_mapping(
                subject,
                subject_date,
                &media,
                anilist_media_id,
                self.config.max_stream_offset_days,
            )?;
            return Ok(media);
        }

        let candidates = if let Some(media_id) = anilist_media_id {
            self.anime_schedule_search(None, Some(media_id)).await?
        } else {
            self.anime_schedule_search(Some(&subject.name), None)
                .await?
        };
        select_anime_schedule_candidate(
            subject,
            subject_date,
            anilist_media_id,
            self.config.max_stream_offset_days,
            candidates,
        )
    }

    async fn anime_schedule_search(
        &self,
        title: Option<&str>,
        anilist_media_id: Option<i64>,
    ) -> Result<Vec<AnimeScheduleAnime>> {
        let url = format!(
            "{}/anime",
            self.config.anime_schedule_api_url.trim_end_matches('/')
        );
        let mut request = self.anime_schedule_request(&url);
        if let Some(title) = title {
            request = request.query(&[("q", title)]);
        }
        if let Some(media_id) = anilist_media_id {
            request = request.query(&[("anilist-ids", media_id)]);
        }
        let response = request
            .send()
            .await
            .map_err(|error| safe_request_error(error, "AnimeSchedule API"))?;
        let page: AnimeSchedulePage =
            decode_metadata_response(response, "AnimeSchedule API").await?;
        Ok(page.anime)
    }

    async fn anime_schedule_media(&self, route: &str) -> Result<AnimeScheduleAnime> {
        validate_anime_schedule_route(route)?;
        let url = format!(
            "{}/anime/{route}",
            self.config.anime_schedule_api_url.trim_end_matches('/')
        );
        let response = self
            .anime_schedule_request(&url)
            .send()
            .await
            .map_err(|error| safe_request_error(error, "AnimeSchedule API"))?;
        decode_metadata_response(response, "AnimeSchedule API").await
    }

    async fn anime_schedule_timetable(
        &self,
        expected_at: DateTime<Utc>,
        route: &str,
        target_episode: i64,
    ) -> Result<Option<AnimeScheduleTimetableEntry>> {
        // The upstream query is explicitly UTC, so derive its ISO week in UTC as
        // well. Tokyo and UTC can fall into different weeks around Sunday night.
        let week = expected_at.iso_week();
        let url = format!(
            "{}/timetables/raw",
            self.config.anime_schedule_api_url.trim_end_matches('/')
        );
        let response = self
            .anime_schedule_request(&url)
            .query(&[
                ("year", week.year().to_string()),
                ("week", week.week().to_string()),
                ("tz", "UTC".to_string()),
            ])
            .send()
            .await
            .map_err(|error| safe_request_error(error, "AnimeSchedule timetable API"))?;
        let timetable: Vec<AnimeScheduleTimetableEntry> =
            decode_metadata_response(response, "AnimeSchedule timetable API").await?;
        Ok(timetable.into_iter().find(|entry| {
            let first = entry
                .subtracted_episode_number
                .unwrap_or(entry.episode_number);
            entry.route == route && (first..=entry.episode_number).contains(&target_episode)
        }))
    }

    fn anime_schedule_request(&self, url: &str) -> reqwest::RequestBuilder {
        let request = self.client.get(url).header("Accept", "application/json");
        match self.anime_schedule_token.as_deref() {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, url: &str, label: &str) -> Result<T> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|error| safe_request_error(error, label))?;
        decode_metadata_response(response, label).await
    }
}

async fn decode_metadata_response<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
    label: &str,
) -> Result<T> {
    let status = response.status();
    if !status.is_success() {
        let hint = if label.starts_with("AnimeSchedule") && matches!(status.as_u16(), 401 | 403) {
            format!(
                "; set {ANIME_SCHEDULE_TOKEN_ENV} for direct access, or configure the Cloudflare Worker secret"
            )
        } else if label.starts_with("AnimeSchedule") && status.as_u16() == 502 {
            "; inspect the Worker logs; common causes are an upstream error or stale Worker routes/token"
                .into()
        } else {
            String::new()
        };
        return Err(AppError::Schedule(format!(
            "{label} returned HTTP {status}{hint}"
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_METADATA_BYTES)
    {
        return Err(AppError::Schedule(format!(
            "{label} response exceeds {} MiB",
            MAX_METADATA_BYTES / 1024 / 1024
        )));
    }
    let body = response
        .bytes()
        .await
        .map_err(|error| safe_request_error(error, &format!("{label} response")))?;
    if body.len() as u64 > MAX_METADATA_BYTES {
        return Err(AppError::Schedule(format!(
            "{label} response exceeds {} MiB",
            MAX_METADATA_BYTES / 1024 / 1024
        )));
    }
    serde_json::from_slice(&body)
        .map_err(|error| AppError::Schedule(format!("{label} returned invalid JSON: {error}")))
}

fn anime_schedule_estimate(
    episode_airdate: Option<NaiveDate>,
    subject_date: Option<NaiveDate>,
    media: &AnimeScheduleAnime,
    subject_episode_index: i64,
    timezone: Tz,
) -> Result<(DateTime<Utc>, &'static str, Option<String>, bool)> {
    let premier = anime_schedule_premier(media);
    if let Some(override_entry) = media.episode_override.filter(|entry| {
        let first = entry
            .override_episode
            .saturating_sub(entry.episodes_aired.max(0));
        (first..=entry.override_episode).contains(&subject_episode_index)
    }) {
        return Ok((
            override_entry.override_date,
            "calibrated",
            Some(format!(
                "AnimeSchedule 提供了 EP{subject_episode_index} 的调档时间。"
            )),
            true,
        ));
    }

    if let Some(premier) = premier {
        let mut expected_at = if anime_schedule_is_ona(media) {
            premier
        } else if let Some(airdate) = episode_airdate {
            let tokyo_premier = premier.with_timezone(&Tokyo);
            let source_day_offset = subject_date
                .map(|date| (tokyo_premier.date_naive() - date).num_days())
                .unwrap_or(0);
            let expected_date = airdate
                .checked_add_signed(Duration::days(source_day_offset))
                .ok_or_else(|| AppError::Schedule("episode date calculation overflowed".into()))?;
            let time = tokyo_premier.time();
            Tokyo
                .with_ymd_and_hms(
                    expected_date.year(),
                    expected_date.month(),
                    expected_date.day(),
                    time.hour(),
                    time.minute(),
                    time.second(),
                )
                .single()
                .ok_or_else(|| {
                    AppError::Schedule("cannot combine AnimeSchedule time with Bangumi date".into())
                })?
                .with_timezone(&Utc)
        } else {
            let offset = subject_episode_index
                .checked_sub(1)
                .and_then(|value| value.checked_mul(7))
                .ok_or_else(|| AppError::Schedule("episode date offset overflowed".into()))?;
            premier + Duration::days(offset)
        };
        let mut warning = if subject_episode_index == 1 || anime_schedule_is_ona(media) {
            None
        } else if episode_airdate.is_some() {
            Some("已用 Bangumi 章节日期和 AnimeSchedule 日本首播时刻组合，等待周排期确认。".into())
        } else {
            Some(format!(
                "AnimeSchedule 尚未列出目标周的 EP{subject_episode_index}，暂按首播时刻每周推算。"
            ))
        };
        if let (Some(delayed_from), Some(delayed_until)) = (
            valid_anime_schedule_time(media.delayed_from),
            valid_anime_schedule_time(media.delayed_until),
        ) && (expected_at - delayed_from).num_hours().abs() <= 36
        {
            expected_at = delayed_until;
            warning = Some(format!(
                "AnimeSchedule 标记本集由 {} 调整至 {}。",
                delayed_from.to_rfc3339(),
                delayed_until.to_rfc3339()
            ));
        }
        return Ok((
            expected_at,
            if subject_episode_index == 1 || anime_schedule_is_ona(media) {
                "calibrated"
            } else {
                "estimated"
            },
            warning,
            true,
        ));
    }

    let (start_date, month_only) = match (episode_airdate, subject_date) {
        (Some(airdate), _) => (airdate, false),
        (None, Some(subject_date)) => (subject_date, false),
        (None, None) => (
            anime_schedule_month_start(media)?.ok_or_else(|| {
                AppError::Schedule(format!(
                    "AnimeSchedule '{}' matched '{}', but neither source has a usable premiere date or month",
                    media.route,
                    anime_schedule_display_title(media)
                ))
            })?,
            true,
        ),
    };
    let date = if subject_episode_index == 1 || anime_schedule_is_ona(media) {
        start_date
    } else {
        let day_offset = subject_episode_index
            .checked_sub(1)
            .and_then(|value| value.checked_mul(7))
            .ok_or_else(|| AppError::Schedule("episode date offset overflowed".into()))?;
        start_date
            .checked_add_signed(Duration::days(day_offset))
            .ok_or_else(|| AppError::Schedule("episode date offset overflowed".into()))?
    };
    let local = timezone
        .with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0)
        .single()
        .ok_or_else(|| AppError::Schedule("cannot construct date-only schedule boundary".into()))?;
    let warning = if month_only {
        format!(
            "AnimeSchedule 目前仅公布 {}年{}月，尚无具体日期和时刻；系统将从 {date} 起检查，并每日同步正式排期。",
            start_date.year(),
            start_date.month()
        )
    } else {
        format!(
            "当前来源仅公布 {date} 的开播日期，尚无精确上线时刻；系统将从当天开始检查并每日同步。"
        )
    };
    Ok((
        local.with_timezone(&Utc),
        "date_only",
        Some(warning),
        !month_only,
    ))
}

fn valid_anime_schedule_time(value: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    value.filter(|time| valid_metadata_year(time.year()))
}

fn parse_metadata_date(value: Option<&str>, error_context: &str) -> Result<Option<NaiveDate>> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| AppError::Schedule(format!("{error_context}: {value}")))?;
    Ok(valid_metadata_year(date.year()).then_some(date))
}

fn valid_metadata_year(year: i32) -> bool {
    year >= 1900 && year != 2099
}

fn anime_schedule_month_start(media: &AnimeScheduleAnime) -> Result<Option<NaiveDate>> {
    let (Some(year), Some(month)) = (media.year, media.month.as_deref()) else {
        return Ok(None);
    };
    if !valid_metadata_year(year) {
        return Ok(None);
    }
    let month_number = match month.trim().to_ascii_lowercase().as_str() {
        "1" | "01" | "jan" | "january" => 1,
        "2" | "02" | "feb" | "february" => 2,
        "3" | "03" | "mar" | "march" => 3,
        "4" | "04" | "apr" | "april" => 4,
        "5" | "05" | "may" => 5,
        "6" | "06" | "jun" | "june" => 6,
        "7" | "07" | "jul" | "july" => 7,
        "8" | "08" | "aug" | "august" => 8,
        "9" | "09" | "sep" | "sept" | "september" => 9,
        "10" | "oct" | "october" => 10,
        "11" | "nov" | "november" => 11,
        "12" | "dec" | "december" => 12,
        _ => {
            return Err(AppError::Schedule(format!(
                "AnimeSchedule returned an invalid premiere month: {month}"
            )));
        }
    };
    Ok(NaiveDate::from_ymd_opt(year, month_number, 1))
}

fn anime_schedule_premier(media: &AnimeScheduleAnime) -> Option<DateTime<Utc>> {
    valid_anime_schedule_time(media.premier)
}

fn anime_schedule_premier_date(media: &AnimeScheduleAnime) -> Option<NaiveDate> {
    anime_schedule_premier(media).map(|time| time.with_timezone(&Tokyo).date_naive())
}

fn anime_schedule_is_ona(media: &AnimeScheduleAnime) -> bool {
    media
        .media_types
        .iter()
        .any(|kind| kind.route.eq_ignore_ascii_case("ona"))
}

fn anime_schedule_primary_titles(media: &AnimeScheduleAnime) -> Vec<&str> {
    let mut values = vec![media.title.as_str()];
    if let Some(names) = &media.names {
        values.extend(
            names
                .native
                .iter()
                .chain(names.romaji.iter())
                .chain(names.english.iter())
                .map(String::as_str),
        );
    }
    values.retain(|value| !value.trim().is_empty());
    values
}

fn anime_schedule_titles(media: &AnimeScheduleAnime) -> Vec<&str> {
    let mut values = anime_schedule_primary_titles(media);
    if let Some(names) = &media.names {
        values.extend(names.abbreviation.iter().map(String::as_str));
        if let Some(synonyms) = &names.synonyms {
            values.extend(synonyms.iter().map(String::as_str));
        }
    }
    values.retain(|value| !value.trim().is_empty());
    values
}

fn anime_schedule_display_title(media: &AnimeScheduleAnime) -> &str {
    media
        .names
        .as_ref()
        .and_then(|names| names.native.as_deref())
        .unwrap_or(&media.title)
}

fn anime_schedule_identity_title(media: &AnimeScheduleAnime) -> String {
    let display = anime_schedule_display_title(media);
    if display == media.title {
        display.into()
    } else {
        format!("{display} / {}", media.title)
    }
}

fn anime_schedule_anilist_id(media: &AnimeScheduleAnime) -> Option<i64> {
    media
        .websites
        .as_ref()?
        .ani_list
        .as_deref()?
        .trim_end_matches('/')
        .rsplit('/')
        .next()?
        .parse()
        .ok()
}

fn parse_chinese_number(value: &str) -> Option<i64> {
    let digit = |character| match character {
        '一' => Some(1),
        '二' | '两' => Some(2),
        '三' => Some(3),
        '四' => Some(4),
        '五' => Some(5),
        '六' => Some(6),
        '七' => Some(7),
        '八' => Some(8),
        '九' => Some(9),
        _ => None,
    };
    let characters = value.chars().collect::<Vec<_>>();
    match characters.as_slice() {
        [single] => digit(*single).or_else(|| (*single == '十').then_some(10)),
        [left, '十'] => digit(*left).map(|value| value * 10),
        ['十', right] => digit(*right).map(|value| 10 + value),
        [left, '十', right] => Some(digit(*left)? * 10 + digit(*right)?),
        _ => None,
    }
}

fn season_numbers(values: impl IntoIterator<Item = impl AsRef<str>>) -> HashSet<i64> {
    let mut numbers = HashSet::new();
    for value in values {
        let normalized = normalize_title(value.as_ref());
        for captures in EAST_ASIAN_SEASON.captures_iter(&normalized) {
            let value = &captures[1];
            if let Some(number) = value
                .parse::<i64>()
                .ok()
                .or_else(|| parse_chinese_number(value))
                .filter(|number| (1..=99).contains(number))
            {
                numbers.insert(number);
            }
        }
        for captures in LATIN_SEASON.captures_iter(&normalized) {
            if let Some(number) = captures
                .get(1)
                .or_else(|| captures.get(2))
                .and_then(|value| value.as_str().parse::<i64>().ok())
                .filter(|number| (1..=99).contains(number))
            {
                numbers.insert(number);
            }
        }
    }
    numbers
}

fn title_without_season(value: &str) -> String {
    let normalized = normalize_title(value);
    let without_east_asian = EAST_ASIAN_SEASON.replace_all(&normalized, " ");
    let without_latin = LATIN_SEASON.replace_all(&without_east_asian, " ");
    normalize_title(&without_latin)
}

fn anime_schedule_release_label(media: &AnimeScheduleAnime) -> String {
    if let Some(date) = anime_schedule_premier_date(media) {
        return date.to_string();
    }
    anime_schedule_month_start(media)
        .ok()
        .flatten()
        .map(|date| date.format("%Y-%m").to_string())
        .unwrap_or_else(|| "unknown date".into())
}

fn anime_schedule_release_conflicts(
    subject_date: Option<NaiveDate>,
    media: &AnimeScheduleAnime,
    maximum_offset_days: i64,
) -> bool {
    let Some(subject_date) = subject_date else {
        return false;
    };
    if let Some(media_date) = anime_schedule_premier_date(media) {
        return (subject_date - media_date).num_days().abs() > maximum_offset_days;
    }
    anime_schedule_month_start(media)
        .ok()
        .flatten()
        .is_some_and(|month_start| {
            let next_month = if month_start.month() == 12 {
                NaiveDate::from_ymd_opt(month_start.year() + 1, 1, 1)
            } else {
                NaiveDate::from_ymd_opt(month_start.year(), month_start.month() + 1, 1)
            };
            let month_end = next_month
                .and_then(|date| date.pred_opt())
                .unwrap_or(month_start);
            let distance = if subject_date < month_start {
                (month_start - subject_date).num_days()
            } else if subject_date > month_end {
                (subject_date - month_end).num_days()
            } else {
                0
            };
            distance > maximum_offset_days
        })
}

fn anime_schedule_matches_subject(
    subject: &BangumiSubject,
    subject_date: Option<NaiveDate>,
    media: &AnimeScheduleAnime,
    expected_anilist_id: Option<i64>,
    maximum_release_offset_days: i64,
) -> bool {
    let actual_anilist_id = anime_schedule_anilist_id(media);
    if let (Some(expected), Some(actual)) = (expected_anilist_id, actual_anilist_id)
        && expected != actual
    {
        return false;
    }
    let anilist_id_matches =
        expected_anilist_id.is_some() && expected_anilist_id == actual_anilist_id;
    let subject_titles = [&subject.name, &subject.name_cn]
        .into_iter()
        .map(|value| normalize_title(value))
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    let media_primary_titles = anime_schedule_primary_titles(media)
        .into_iter()
        .map(normalize_title)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    let media_titles = anime_schedule_titles(media)
        .into_iter()
        .map(normalize_title)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if subject_titles.is_empty() || media_titles.is_empty() {
        return false;
    }
    if anime_schedule_release_conflicts(subject_date, media, maximum_release_offset_days) {
        return false;
    }
    let platform = subject.platform.as_deref().unwrap_or_default();
    if platform.eq_ignore_ascii_case("TV")
        && !media.media_types.is_empty()
        && !media
            .media_types
            .iter()
            .any(|kind| matches!(kind.route.to_ascii_lowercase().as_str(), "tv" | "tv-short"))
    {
        return false;
    }
    let subject_seasons = season_numbers([subject.name.as_str(), subject.name_cn.as_str()]);
    let media_seasons = season_numbers(
        anime_schedule_titles(media)
            .into_iter()
            .chain(std::iter::once(media.route.as_str())),
    );
    if subject_seasons.len() > 1
        || media_seasons.len() > 1
        || (!subject_seasons.is_empty()
            && !media_seasons.is_empty()
            && subject_seasons != media_seasons)
    {
        return false;
    }
    let exact_title = subject_titles
        .iter()
        .any(|subject| media_titles.iter().any(|media| subject == media));
    let exact_primary_title = subject_titles
        .iter()
        .any(|subject| media_primary_titles.iter().any(|media| subject == media));
    let equivalent_title = exact_title
        || subject_titles.iter().any(|subject| {
            media_titles
                .iter()
                .any(|media| equivalent_title_spelling(subject, media))
        });
    let media_date = anime_schedule_premier_date(media);
    let exact_date_matches = subject_date
        .zip(media_date)
        .is_some_and(|(left, right)| left == right);
    let only_one_side_declares_season = subject_seasons.is_empty() != media_seasons.is_empty();
    if exact_primary_title
        && (!only_one_side_declares_season || exact_date_matches || anilist_id_matches)
    {
        return true;
    }
    if equivalent_title && exact_date_matches {
        return true;
    }

    let matching_season =
        subject_seasons.len() == 1 && media_seasons.len() == 1 && subject_seasons == media_seasons;
    if !matching_season {
        return false;
    }

    let subject_cores = [&subject.name, &subject.name_cn]
        .into_iter()
        .map(|value| title_without_season(value))
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    let media_cores = anime_schedule_titles(media)
        .into_iter()
        .map(title_without_season)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    subject_cores.iter().any(|subject| {
        media_cores
            .iter()
            .any(|media| subject == media || equivalent_title_spelling(subject, media))
    })
}

fn equivalent_title_spelling(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    let left = left.chars().collect::<Vec<_>>();
    let right = right.chars().collect::<Vec<_>>();
    left.len() >= 5
        && left.len() == right.len()
        && left
            .iter()
            .zip(right.iter())
            .filter(|(left, right)| left != right)
            .count()
            == 1
}

fn validate_anime_schedule_mapping(
    subject: &BangumiSubject,
    subject_date: Option<NaiveDate>,
    media: &AnimeScheduleAnime,
    expected_anilist_id: Option<i64>,
    maximum_release_offset_days: i64,
) -> Result<()> {
    if anime_schedule_matches_subject(
        subject,
        subject_date,
        media,
        expected_anilist_id,
        maximum_release_offset_days,
    ) {
        return Ok(());
    }
    Err(AppError::Schedule(format!(
        "AnimeSchedule '{}' ('{}', {}) does not match Bangumi #{} ('{}', {}); verify the season and route",
        media.route,
        anime_schedule_identity_title(media),
        anime_schedule_release_label(media),
        subject.id,
        subject.name,
        subject_date
            .map(|date| date.to_string())
            .unwrap_or_else(|| "unknown date".into())
    )))
}

fn select_anime_schedule_candidate(
    subject: &BangumiSubject,
    subject_date: Option<NaiveDate>,
    expected_anilist_id: Option<i64>,
    maximum_release_offset_days: i64,
    candidates: Vec<AnimeScheduleAnime>,
) -> Result<AnimeScheduleAnime> {
    let mut matches = candidates
        .iter()
        .filter(|media| {
            anime_schedule_matches_subject(
                subject,
                subject_date,
                media,
                expected_anilist_id,
                maximum_release_offset_days,
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    if matches.len() == 1 {
        return Ok(matches.remove(0));
    }
    let choices = candidates
        .iter()
        .take(8)
        .map(|media| {
            format!(
                "{} {} ({}, {}, {})",
                media.route,
                anime_schedule_identity_title(media),
                anime_schedule_release_label(media),
                media
                    .media_types
                    .first()
                    .map(|kind| kind.route.as_str())
                    .unwrap_or("format unknown"),
                media.status.as_deref().unwrap_or("status unknown")
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    if matches.is_empty() {
        Err(AppError::Schedule(format!(
            "no high-confidence AnimeSchedule match for Bangumi #{} '{}'. Search candidates: {}. Enter the correct AnimeSchedule route manually after checking the season",
            subject.id,
            subject.name,
            if choices.is_empty() { "none" } else { &choices }
        )))
    } else {
        Err(AppError::Schedule(format!(
            "multiple high-confidence AnimeSchedule matches for Bangumi #{} '{}': {}. Enter the correct AnimeSchedule route manually",
            subject.id, subject.name, choices
        )))
    }
}

fn anime_schedule_aliases(subject: &BangumiSubject, media: &AnimeScheduleAnime) -> Vec<String> {
    let mut values = Vec::new();
    values.push(subject.name.clone());
    if !subject.name_cn.trim().is_empty() {
        values.push(subject.name_cn.clone());
    }
    values.extend(anime_schedule_titles(media).into_iter().map(str::to_string));
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|value| {
            let normalized = normalize_title(value);
            !normalized.is_empty() && seen.insert(normalized)
        })
        .collect()
}

fn validate_anime_schedule_route(route: &str) -> Result<()> {
    if route.is_empty()
        || route.len() > 200
        || !route
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err(AppError::InvalidInput(
            "AnimeSchedule route must contain only ASCII letters, digits, and '-'".into(),
        ));
    }
    Ok(())
}

impl ScheduleSynchronizer {
    pub fn new(repository: Repository, config: ScheduleConfig) -> Result<Self> {
        let provider = ScheduleProvider::new(config.clone())?;
        Ok(Self {
            repository,
            provider,
            config,
        })
    }

    pub async fn sync_due(&self) -> Result<()> {
        let ids = self
            .repository
            .auto_schedule_due_ids(self.config.sync_batch_size)
            .await?;
        if ids.is_empty() {
            return Ok(());
        }
        let catalog = match self.load_catalog_with_health().await {
            Ok(catalog) => catalog,
            Err(error) => {
                for anime_id in ids {
                    self.record_failure(anime_id, &error).await?;
                }
                warn!(%error, "automatic schedule catalog refresh failed");
                return Ok(());
            }
        };
        for anime_id in ids {
            if let Err(error) = self.sync_with_catalog(anime_id, &catalog).await {
                self.record_failure(anime_id, &error).await?;
                warn!(anime_id, %error, "automatic schedule refresh failed");
            }
        }
        Ok(())
    }

    pub async fn sync_now(&self, anime_id: i64) -> Result<()> {
        let anime = self.repository.get_anime(anime_id).await?.anime;
        if anime.lifecycle == "archived" {
            return self
                .sync_archive_rating(anime_id, anime.bangumi_subject_id)
                .await;
        }
        let catalog = self.load_catalog_with_health().await?;
        self.sync_with_catalog(anime_id, &catalog).await
    }

    async fn sync_archive_rating(&self, anime_id: i64, subject_id: Option<i64>) -> Result<()> {
        let subject_id = subject_id.ok_or_else(|| {
            AppError::Schedule(format!(
                "archived anime {anime_id} has no Bangumi subject ID"
            ))
        })?;
        let health_source = format!("bangumi-rating:{subject_id}");
        let rating = match self.provider.bangumi_public_rating(subject_id).await {
            Ok(rating) => rating,
            Err(error) => {
                self.repository
                    .record_source_failure(
                        &health_source,
                        &error.to_string(),
                        SOURCE_ALERT_FAILURE_THRESHOLD,
                    )
                    .await?;
                return Err(error);
            }
        };
        self.repository
            .update_anime_archive_bangumi_rating(anime_id, rating.score, rating.total, rating.rank)
            .await?;
        self.repository
            .record_source_success(&health_source)
            .await?;
        info!(
            anime_id,
            bangumi_subject_id = subject_id,
            score = ?rating.score,
            rating_total = rating.total,
            rank = ?rating.rank,
            "archived anime Bangumi rating refreshed"
        );
        Ok(())
    }

    async fn load_catalog_with_health(&self) -> Result<ScheduleCatalog> {
        match self.provider.load_catalog().await {
            Ok(catalog) => {
                self.repository
                    .record_source_success(CATALOG_SOURCE)
                    .await?;
                Ok(catalog)
            }
            Err(error) => {
                self.repository
                    .record_source_failure(
                        CATALOG_SOURCE,
                        &error.to_string(),
                        SOURCE_ALERT_FAILURE_THRESHOLD,
                    )
                    .await?;
                Err(error)
            }
        }
    }

    async fn sync_with_catalog(&self, anime_id: i64, catalog: &ScheduleCatalog) -> Result<()> {
        let anime = self.repository.get_anime(anime_id).await?;
        if !anime.anime.auto_schedule {
            return Err(AppError::Schedule(format!(
                "anime {anime_id} does not use automatic scheduling"
            )));
        }
        let episode = self.repository.active_episode(anime_id).await?;
        let episode_mapping = match (
            anime.anime.local_episode_origin,
            anime.anime.bangumi_episode_origin,
        ) {
            (Some(local_origin), Some(bangumi_origin)) => Some(EpisodeNumberMapping {
                local_origin,
                bangumi_origin,
            }),
            (None, None) => None,
            _ => {
                return Err(AppError::Schedule(
                    "anime has an incomplete episode number mapping".into(),
                ));
            }
        };
        let fallback_health_source = anime
            .anime
            .bangumi_subject_id
            .filter(|subject_id| {
                !catalog
                    .items
                    .iter()
                    .any(|item| item_subject_id(item) == Some(*subject_id))
            })
            .map(|subject_id| format!("anime-schedule:{subject_id}"));
        let resolved = match self
            .provider
            .resolve_auto(
                catalog,
                AutoScheduleRequest {
                    title: &anime.anime.title,
                    subject_id: anime.anime.bangumi_subject_id,
                    next_episode: episode.episode_no,
                    episode_mapping,
                    anilist_media_id: anime.anime.anilist_media_id,
                    anime_schedule_route: anime.anime.anime_schedule_route.as_deref(),
                    timezone: &anime.anime.timezone,
                },
            )
            .await
        {
            Ok(resolved) => resolved,
            Err(error) => {
                if let Some(source) = fallback_health_source.as_deref() {
                    self.repository
                        .record_source_failure(
                            source,
                            &error.to_string(),
                            SOURCE_ALERT_FAILURE_THRESHOLD,
                        )
                        .await?;
                }
                return Err(error);
            }
        };
        let mut update = resolved
            .to_update(Utc::now() + Duration::seconds(self.config.sync_interval_secs as i64));
        if resolved.source_health_error.is_some()
            && resolved.schedule_confidence == "estimated"
            && anime.anime.schedule_confidence.as_deref() == Some("calibrated")
            && episode.expected_at.is_some()
        {
            update.expected_at = episode.expected_at;
            update.expected_weekday = anime.anime.expected_weekday;
            update.expected_time.clone_from(&anime.anime.expected_time);
            if let Some(source) = &anime.anime.schedule_source {
                update.schedule_source.clone_from(source);
            }
            update.schedule_confidence = "stale".into();
            update.schedule_warning = Some(format!(
                "Bangumi 章节日期暂时不可用，已保留上次校准时间。{}",
                resolved.schedule_warning.as_deref().unwrap_or_default()
            ));
        }
        let schedule_health_source = if resolved.schedule_source == "anime_schedule" {
            format!("anime-schedule:{}", resolved.bangumi_subject_id)
        } else {
            format!("bangumi-schedule:{}", resolved.bangumi_subject_id)
        };
        if resolved.schedule_source != "anime_schedule"
            && (anime.anime.anime_schedule_route.is_some()
                || anime.anime.anilist_media_id.is_some())
        {
            self.repository
                .record_source_success(&format!("anime-schedule:{}", resolved.bangumi_subject_id))
                .await?;
        }
        if let Some(error) = &resolved.source_health_error {
            self.repository
                .record_source_failure(
                    &schedule_health_source,
                    error,
                    SOURCE_ALERT_FAILURE_THRESHOLD,
                )
                .await?;
        } else {
            self.repository
                .record_source_success(&schedule_health_source)
                .await?;
        }
        self.repository
            .apply_schedule_update(anime_id, &update)
            .await?;
        info!(
            anime_id,
            bangumi_subject_id = resolved.bangumi_subject_id,
            episode = episode.episode_no,
            bangumi_episode = episode_mapping.and_then(|mapping| mapping.mapped_numbers(episode.episode_no).ok().map(|(_, number)| number)),
            expected_at = ?update.expected_at,
            schedule_source = %update.schedule_source,
            schedule_confidence = %update.schedule_confidence,
            "automatic schedule refreshed"
        );
        Ok(())
    }

    async fn record_failure(&self, anime_id: i64, error: &AppError) -> Result<()> {
        self.repository
            .mark_schedule_sync_failed(
                anime_id,
                &error.to_string(),
                Utc::now() + Duration::seconds(self.config.failure_retry_secs as i64),
            )
            .await
    }
}

impl ResolvedSchedule {
    pub fn to_update(&self, next_sync_at: DateTime<Utc>) -> ScheduleUpdate {
        ScheduleUpdate {
            bangumi_subject_id: self.bangumi_subject_id,
            anilist_media_id: self.anilist_media_id,
            anime_schedule_route: self.anime_schedule_route.clone(),
            total_episodes: self.total_episodes,
            aliases: self.aliases.clone(),
            expected_at: self.expected_at,
            expected_weekday: self.expected_weekday,
            expected_time: self.expected_time.clone(),
            timezone: self.timezone.clone(),
            broadcast_pattern: self.broadcast_pattern.clone(),
            schedule_source: self.schedule_source.clone(),
            schedule_confidence: self.schedule_confidence.clone(),
            schedule_warning: self.schedule_warning.clone(),
            next_sync_at,
        }
    }
}

#[derive(Clone)]
struct SelectedBroadcast {
    pattern: String,
    source: String,
    kind: BroadcastSourceKind,
    warning: Option<String>,
}

struct StreamBroadcastCandidate {
    selected: SelectedBroadcast,
    anchor: DateTime<Utc>,
    priority: usize,
}

fn match_item<'a>(
    catalog: &'a ScheduleCatalog,
    title: &str,
    subject_id: Option<i64>,
) -> Result<&'a BangumiDataItem> {
    if let Some(subject_id) = subject_id {
        return catalog
            .items
            .iter()
            .find(|item| item_subject_id(item) == Some(subject_id))
            .ok_or_else(|| {
                AppError::Schedule(format!(
                    "Bangumi subject {subject_id} is not present in bangumi-data"
                ))
            });
    }

    let normalized = normalize_title(title);
    let matches: Vec<_> = catalog
        .items
        .iter()
        .filter(|item| matches!(item.item_type.as_str(), "tv" | "web"))
        .filter(|item| {
            item_aliases(item)
                .iter()
                .any(|alias| normalize_title(alias) == normalized)
        })
        .collect();
    match matches.as_slice() {
        [] => Err(AppError::Schedule(format!(
            "no exact bangumi-data title match for '{title}'; add aliases manually or specify --bangumi-id"
        ))),
        [item] => Ok(*item),
        _ => {
            let choices = matches
                .iter()
                .take(8)
                .map(|item| {
                    format!(
                        "#{} {} ({})",
                        item_subject_id(item)
                            .map(|id| id.to_string())
                            .unwrap_or_else(|| "?".into()),
                        item.title,
                        item.begin.get(..10).unwrap_or(&item.begin)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            Err(AppError::Schedule(format!(
                "multiple bangumi-data entries match '{title}': {choices}; rerun with --bangumi-id"
            )))
        }
    }
}

fn item_subject_id(item: &BangumiDataItem) -> Option<i64> {
    item.sites
        .iter()
        .find(|site| site.site == "bangumi")
        .and_then(|site| site.id.as_deref())
        .and_then(|id| id.parse().ok())
}

fn item_aliases(item: &BangumiDataItem) -> Vec<String> {
    let mut aliases = vec![item.title.clone()];
    for language in ["zh-Hans", "zh-Hant", "ja", "en"] {
        if let Some(translations) = item.title_translate.get(language) {
            aliases.extend(translations.iter().cloned());
        }
    }
    for translations in item.title_translate.values() {
        aliases.extend(translations.iter().cloned());
    }
    aliases.retain(|alias| !alias.trim().is_empty());
    aliases.dedup();
    aliases
}

fn select_broadcast(
    item: &BangumiDataItem,
    config: &ScheduleConfig,
    origin_airdate: Option<NaiveDate>,
) -> Result<SelectedBroadcast> {
    let excluded_sources = config
        .excluded_stream_sites
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let item_pattern = item
        .broadcast
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .or_else(|| fallback_weekly_pattern(&item.begin));

    if !excluded_sources.contains(config.preferred_site.as_str())
        && let Some(site) = item
            .sites
            .iter()
            .find(|site| site.site == config.preferred_site)
        && let Some(pattern) = site_pattern(site, item_pattern.as_deref())
    {
        return Ok(SelectedBroadcast {
            pattern,
            source: site.site.clone(),
            kind: BroadcastSourceKind::Stream,
            warning: None,
        });
    }

    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    let mut rejected_by_date = Vec::new();
    for (priority, source) in config.stream_site_priority.iter().enumerate() {
        if source == &config.preferred_site
            || excluded_sources.contains(source.as_str())
            || !seen.insert(source.clone())
        {
            continue;
        }
        let Some(site) = item.sites.iter().find(|site| site.site == *source) else {
            continue;
        };
        let Some(pattern) = site_pattern(site, item_pattern.as_deref()) else {
            continue;
        };
        let Ok(recurrence) = parse_recurrence(&pattern) else {
            continue;
        };
        if let Some(origin_airdate) = origin_airdate {
            let source_day_offset =
                (recurrence.anchor.with_timezone(&Tokyo).date_naive() - origin_airdate).num_days();
            if source_day_offset.abs() > config.max_stream_offset_days {
                rejected_by_date.push(site.site.clone());
                continue;
            }
        }
        candidates.push(StreamBroadcastCandidate {
            selected: SelectedBroadcast {
                pattern,
                source: site.site.clone(),
                kind: BroadcastSourceKind::Stream,
                warning: None,
            },
            anchor: recurrence.anchor,
            priority,
        });
    }

    let corroborated = candidates
        .iter()
        .filter(|candidate| {
            candidates
                .iter()
                .filter(|other| {
                    (other.anchor - candidate.anchor).num_seconds().abs()
                        <= STREAM_CONSENSUS_WINDOW_SECS
                })
                .map(|other| stream_source_family(&other.selected.source))
                .collect::<HashSet<_>>()
                .len()
                >= MIN_STREAM_SOURCE_FAMILIES
        })
        .min_by_key(|candidate| (candidate.anchor, candidate.priority));
    if let Some(candidate) = corroborated {
        return Ok(candidate.selected.clone());
    }

    if let Some(candidate) = candidates.iter().min_by_key(|candidate| candidate.priority) {
        let mut selected = candidate.selected.clone();
        selected.warning = Some(format!(
            "没有两个独立网络来源能相互印证，暂按优先级使用 {} 的排期。",
            candidate.selected.source
        ));
        return Ok(selected);
    }

    item_pattern
        .map(|pattern| SelectedBroadcast {
            pattern,
            source: "bangumi-data".into(),
            kind: BroadcastSourceKind::Catalog,
            warning: (!rejected_by_date.is_empty()).then(|| {
                format!(
                    "网络平台排期未通过首集日期安全校验（{}），已回退到 bangumi-data 默认时段。",
                    rejected_by_date.join("、")
                )
            }),
        })
        .ok_or_else(|| AppError::Schedule(format!("'{}' has no broadcast time", item.title)))
}

fn site_pattern(site: &BangumiDataSite, item_pattern: Option<&str>) -> Option<String> {
    site.broadcast
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            site.begin
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .and_then(|begin| site_pattern_from_item(begin, item_pattern))
        })
}

fn stream_source_family(source: &str) -> &str {
    match source {
        "gamer" | "gamer_hk" => "gamer",
        value => value,
    }
}

fn fallback_weekly_pattern(begin: &str) -> Option<String> {
    (!begin.trim().is_empty()).then(|| format!("R/{begin}/P7D"))
}

fn site_pattern_from_item(begin: &str, item_pattern: Option<&str>) -> Option<String> {
    let period = item_pattern
        .and_then(|pattern| pattern.rsplit('/').next())
        .filter(|period| period.starts_with('P'))
        .unwrap_or("P7D");
    Some(format!("R/{begin}/{period}"))
}

fn parse_recurrence(pattern: &str) -> Result<Recurrence> {
    let mut parts = pattern
        .strip_prefix("R/")
        .ok_or_else(|| AppError::Schedule(format!("unsupported broadcast pattern: {pattern}")))?
        .split('/');
    let anchor = parts
        .next()
        .ok_or_else(|| AppError::Schedule(format!("invalid broadcast pattern: {pattern}")))?;
    let period = parts
        .next()
        .ok_or_else(|| AppError::Schedule(format!("invalid broadcast pattern: {pattern}")))?;
    if parts.next().is_some() {
        return Err(AppError::Schedule(format!(
            "invalid broadcast pattern: {pattern}"
        )));
    }
    let anchor = DateTime::parse_from_rfc3339(anchor)
        .map_err(|_| AppError::Schedule(format!("invalid broadcast timestamp: {pattern}")))?
        .with_timezone(&Utc);
    let period = match period {
        "P7D" => Duration::days(7),
        "P1D" => Duration::days(1),
        value => {
            return Err(AppError::Schedule(format!(
                "unsupported broadcast period {value}; configure the schedule manually"
            )));
        }
    };
    Ok(Recurrence { anchor, period })
}

fn expected_at(
    recurrence: Recurrence,
    episode_no: i64,
    target_airdate: Option<NaiveDate>,
    origin_airdate: Option<NaiveDate>,
    source_kind: BroadcastSourceKind,
    maximum_offset_days: i64,
    now: DateTime<Utc>,
) -> Result<ExpectedSchedule> {
    let steps = episode_no - 1;
    let period_seconds = recurrence.period.num_seconds();
    let projected =
        recurrence.anchor
            + Duration::seconds(period_seconds.checked_mul(steps).ok_or_else(|| {
                AppError::Schedule("episode schedule calculation overflowed".into())
            })?);
    if let (Some(target_airdate), Some(origin_airdate)) = (target_airdate, origin_airdate) {
        let source_anchor = recurrence.anchor.with_timezone(&Tokyo);
        let source_day_offset = (source_anchor.date_naive() - origin_airdate).num_days();
        if source_day_offset.abs() > maximum_offset_days {
            let warning = format!(
                "排期来源与 Bangumi 首集日期相差 {source_day_offset} 天，超过 {maximum_offset_days} 天安全阈值；已隐藏预计时间并继续检查 B 站。"
            );
            return Ok(ExpectedSchedule {
                expected_at: None,
                confidence: "unavailable",
                warning: Some(warning.clone()),
                health_error: Some(warning),
            });
        }
        let expected_date = target_airdate
            .checked_add_signed(Duration::days(source_day_offset))
            .ok_or_else(|| AppError::Schedule("episode date calculation overflowed".into()))?;
        let local = expected_date.and_time(source_anchor.time());
        let expected_at = match Tokyo.from_local_datetime(&local) {
            LocalResult::Single(value) => value.with_timezone(&Utc),
            LocalResult::Ambiguous(first, _) => first.with_timezone(&Utc),
            LocalResult::None => {
                return Err(AppError::Schedule(
                    "episode time does not exist in Asia/Tokyo".into(),
                ));
            }
        };
        let (confidence, warning) = match source_kind {
            BroadcastSourceKind::Stream => ("calibrated", None),
            BroadcastSourceKind::Catalog => (
                "estimated",
                Some(
                    "未找到受信网络平台排期，暂用 bangumi-data 默认播出时段；它可能不是网络最早更新时间。"
                        .into(),
                ),
            ),
        };
        return Ok(ExpectedSchedule {
            expected_at: Some(expected_at),
            confidence,
            warning,
            health_error: None,
        });
    }

    let mut expected = projected;
    if expected < now - Duration::days(14) {
        let threshold = now - Duration::hours(6);
        if recurrence.anchor < threshold {
            let elapsed = (threshold - recurrence.anchor).num_seconds();
            let periods = elapsed.div_euclid(period_seconds);
            expected = recurrence.anchor + Duration::seconds(period_seconds * periods);
            if expected < threshold {
                expected += recurrence.period;
            }
        } else {
            expected = recurrence.anchor;
        }
    }
    Ok(ExpectedSchedule {
        expected_at: Some(expected),
        confidence: "estimated",
        warning: Some(
            "Bangumi 缺少目标集或本季首集日期，当前时间仅按周播周期递推；连播、停播或先行配信可能导致偏差。"
                .into(),
        ),
        health_error: None,
    })
}

fn merge_warnings(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(first), Some(second)) if first != second => Some(format!("{first} {second}")),
        (Some(first), _) => Some(first),
        (_, Some(second)) => Some(second),
        (None, None) => None,
    }
}

fn safe_request_error(error: reqwest::Error, context: &str) -> AppError {
    let detail = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "could not connect"
    } else {
        "request failed"
    };
    AppError::Schedule(format!("{context} {detail}"))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;
    use crate::domain::{AutoScheduleMetadata, NewAnime};

    #[test]
    fn anime_schedule_mapping_compares_all_titles_and_allows_small_release_offsets() {
        let subject_date = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        let subject = BangumiSubject {
            id: 513_878,
            name: "Cyberpunk: Edgerunners 2".into(),
            name_cn: "赛博朋克：边缘行者 2".into(),
            date: Some(subject_date.to_string()),
            platform: Some("WEB".into()),
            total_episodes: Some(10),
            rating: None,
        };
        let media = AnimeScheduleAnime {
            title: "Cyberpunk: Edgerunners 2".into(),
            route: "cyberpunk-edgerunners-2".into(),
            premier: Some(
                DateTime::parse_from_rfc3339("2026-10-20T07:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            month: Some("October".into()),
            year: Some(2026),
            episode_override: None,
            delayed_from: None,
            delayed_until: None,
            episodes: Some(10),
            status: Some("Upcoming".into()),
            names: Some(AnimeScheduleNames {
                romaji: Some("Cyberpunk: Edgerunners 2".into()),
                english: Some("Cyberpunk: Edgerunners 2".into()),
                native: Some("サイバーパンク: エッジランナーズ2".into()),
                ..AnimeScheduleNames::default()
            }),
            websites: Some(AnimeScheduleWebsites {
                ani_list: Some("https://anilist.co/anime/195539".into()),
            }),
            media_types: vec![AnimeScheduleCategory {
                route: "ona".into(),
            }],
        };

        assert!(anime_schedule_matches_subject(
            &subject,
            Some(subject_date),
            &media,
            Some(195_539),
            14,
        ));

        let mut nearby_date = media.clone();
        nearby_date.premier = Some(
            DateTime::parse_from_rfc3339("2026-10-21T07:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        assert!(anime_schedule_matches_subject(
            &subject,
            Some(subject_date),
            &nearby_date,
            Some(195_539),
            14,
        ));

        let mut wrong_year = media.clone();
        wrong_year.premier = Some(
            DateTime::parse_from_rfc3339("2027-10-20T07:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        assert!(!anime_schedule_matches_subject(
            &subject,
            Some(subject_date),
            &wrong_year,
            Some(195_539),
            14,
        ));
    }

    #[test]
    fn anime_schedule_mapping_uses_multilingual_core_title_and_explicit_season() {
        let subject = BangumiSubject {
            id: 616_808,
            name: "野生のラスボスが現れた！第2期".into(),
            name_cn: "野生的大魔王出现了 第二季".into(),
            date: Some("2026-10-03".into()),
            platform: Some("TV".into()),
            total_episodes: None,
            rating: None,
        };
        let media = AnimeScheduleAnime {
            title: "Yasei no Last Boss ga Arawareta! 2nd Season".into(),
            route: "yasei-no-last-boss-ga-arawareta-2nd-season".into(),
            premier: Some(
                DateTime::parse_from_rfc3339("2026-09-26T13:30:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            month: Some("September".into()),
            year: Some(2026),
            episode_override: None,
            delayed_from: None,
            delayed_until: None,
            episodes: None,
            status: Some("Upcoming".into()),
            names: Some(AnimeScheduleNames {
                romaji: Some("Yasei no Last Boss ga Arawareta! 2nd Season".into()),
                english: Some("A Wild Last Boss Appeared! 2nd Season".into()),
                native: Some("野生のラスボスが現れた！".into()),
                synonyms: Some(vec!["A Wild Last Boss Appeared! Season 2".into()]),
                ..AnimeScheduleNames::default()
            }),
            websites: Some(AnimeScheduleWebsites {
                ani_list: Some("https://anilist.co/anime/200001".into()),
            }),
            media_types: vec![AnimeScheduleCategory { route: "tv".into() }],
        };

        assert!(anime_schedule_matches_subject(
            &subject,
            None,
            &media,
            Some(200_001),
            14,
        ));
        assert!(anime_schedule_matches_subject(
            &subject,
            NaiveDate::from_ymd_opt(2026, 10, 3),
            &media,
            Some(200_001),
            14,
        ));
        assert!(!anime_schedule_matches_subject(
            &subject,
            NaiveDate::from_ymd_opt(2027, 10, 3),
            &media,
            Some(200_001),
            14,
        ));
        let timezone = "Asia/Shanghai".parse::<Tz>().unwrap();
        let (expected_at, confidence, warning, precise) = anime_schedule_estimate(
            NaiveDate::from_ymd_opt(2026, 10, 3),
            NaiveDate::from_ymd_opt(2026, 10, 3),
            &media,
            1,
            timezone,
        )
        .unwrap();
        assert_eq!(expected_at.to_rfc3339(), "2026-09-26T13:30:00+00:00");
        assert_eq!(confidence, "calibrated");
        assert!(precise);
        assert!(warning.is_none());

        let mut wrong_season = media.clone();
        wrong_season.title = "Yasei no Last Boss ga Arawareta! 3rd Season".into();
        wrong_season.route = "yasei-no-last-boss-ga-arawareta-3rd-season".into();
        wrong_season.names.as_mut().unwrap().romaji =
            Some("Yasei no Last Boss ga Arawareta! 3rd Season".into());
        wrong_season.names.as_mut().unwrap().english =
            Some("A Wild Last Boss Appeared! 3rd Season".into());
        wrong_season.names.as_mut().unwrap().synonyms = None;
        assert!(!anime_schedule_matches_subject(
            &subject,
            NaiveDate::from_ymd_opt(2026, 10, 3),
            &wrong_season,
            Some(200_001),
            14,
        ));

        let mut contradictory_season = wrong_season.clone();
        contradictory_season.names.as_mut().unwrap().native = Some(subject.name.clone());
        assert!(!anime_schedule_matches_subject(
            &subject,
            NaiveDate::from_ymd_opt(2026, 10, 3),
            &contradictory_season,
            Some(200_001),
            14,
        ));

        let error = validate_anime_schedule_mapping(
            &subject,
            NaiveDate::from_ymd_opt(2026, 10, 3),
            &wrong_season,
            Some(200_001),
            14,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("2026-09-26"));
        assert!(error.contains("3rd Season"));
    }

    #[tokio::test]
    async fn resolves_aliases_and_episode_airdate() {
        let (base_url, requests) = mock_server(false).await;
        let provider = provider(&base_url);
        let catalog = provider.load_catalog().await.unwrap();
        let now = DateTime::parse_from_rfc3339("2026-08-20T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let resolved = provider
            .resolve_at(&catalog, "沉默魔女", None, 8, "Asia/Shanghai", now)
            .await
            .unwrap();
        assert_eq!(resolved.bangumi_subject_id, 501_000);
        assert_eq!(resolved.matched_title, "サイレント・ウィッチ");
        assert!(resolved.aliases.iter().any(|alias| alias == "Silent Witch"));
        assert_eq!(
            resolved.expected_at.unwrap().to_rfc3339(),
            "2026-08-22T15:00:00+00:00"
        );
        assert_eq!(resolved.expected_weekday, Some(5));
        assert_eq!(resolved.expected_time.as_deref(), Some("23:00"));
        assert_eq!(resolved.schedule_source, "danime");
        assert_eq!(resolved.schedule_confidence, "calibrated");
        assert_eq!(resolved.total_episodes, Some(12));
        assert_eq!(requests.await.unwrap(), 3);
    }

    #[tokio::test]
    async fn ambiguous_title_requires_subject_id() {
        let (base_url, requests) = mock_server(true).await;
        let provider = provider(&base_url);
        let catalog = provider.load_catalog().await.unwrap();
        let error = provider
            .resolve(&catalog, "沉默魔女", None, 8, "Asia/Shanghai")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("--bangumi-id"));
        assert_eq!(requests.await.unwrap(), 1);
    }

    #[tokio::test]
    async fn unreleased_subject_falls_back_to_a_unique_anime_schedule_match() {
        let (base_url, requests) = unreleased_mock_server().await;
        let provider = ScheduleProvider::new(ScheduleConfig {
            bangumi_data_url: format!("{base_url}/data.json"),
            bangumi_api_base_url: base_url.clone(),
            anime_schedule_api_url: format!("{base_url}/anime-schedule"),
            request_timeout_secs: 5,
            ..ScheduleConfig::default()
        })
        .unwrap();
        let catalog = provider.load_catalog().await.unwrap();

        let resolved = provider
            .resolve_auto(
                &catalog,
                AutoScheduleRequest {
                    title: "药屋少女的呢喃 第三季",
                    subject_id: Some(568_244),
                    next_episode: 1,
                    episode_mapping: None,
                    anilist_media_id: None,
                    anime_schedule_route: None,
                    timezone: "Asia/Shanghai",
                },
            )
            .await
            .unwrap();

        assert_eq!(resolved.bangumi_subject_id, 568_244);
        assert_eq!(resolved.anilist_media_id, Some(195_516));
        assert_eq!(
            resolved.anime_schedule_route.as_deref(),
            Some("kusuriya-no-hitorigoto-3rd-season")
        );
        assert_eq!(resolved.total_episodes, Some(12));
        assert_eq!(resolved.schedule_source, "anime_schedule");
        assert_eq!(resolved.schedule_confidence, "calibrated");
        assert_eq!(resolved.expected_time.as_deref(), Some("22:00"));
        assert_eq!(
            resolved.expected_at.unwrap().to_rfc3339(),
            "2026-10-02T14:00:00+00:00"
        );
        assert!(
            resolved
                .schedule_warning
                .as_deref()
                .is_some_and(|value| value.contains("kusuriya-no-hitorigoto-3rd-season"))
        );
        assert_eq!(requests.await.unwrap(), 5);
    }

    #[tokio::test]
    async fn unreleased_ona_without_airing_timestamp_uses_date_only_schedule() {
        let (base_url, requests) = date_only_anime_schedule_mock_server().await;
        let provider = ScheduleProvider::new(ScheduleConfig {
            bangumi_data_url: format!("{base_url}/data.json"),
            bangumi_api_base_url: base_url.clone(),
            anime_schedule_api_url: format!("{base_url}/anime-schedule"),
            request_timeout_secs: 5,
            ..ScheduleConfig::default()
        })
        .unwrap();
        let catalog = provider.load_catalog().await.unwrap();

        let resolved = provider
            .resolve_auto(
                &catalog,
                AutoScheduleRequest {
                    title: "Cyberpunk: Edgerunners 2",
                    subject_id: Some(513_878),
                    next_episode: 1,
                    episode_mapping: None,
                    anilist_media_id: Some(195_539),
                    anime_schedule_route: None,
                    timezone: "Asia/Shanghai",
                },
            )
            .await
            .unwrap();

        assert_eq!(resolved.anilist_media_id, Some(195_539));
        assert_eq!(resolved.total_episodes, Some(10));
        assert_eq!(resolved.schedule_source, "anime_schedule");
        assert_eq!(
            resolved.anime_schedule_route.as_deref(),
            Some("cyberpunk-edgerunners-2")
        );
        assert_eq!(resolved.schedule_confidence, "date_only");
        assert_eq!(resolved.expected_time, None);
        assert_eq!(
            resolved.expected_at.unwrap().to_rfc3339(),
            "2026-10-19T16:00:00+00:00"
        );
        assert!(
            resolved
                .schedule_warning
                .as_deref()
                .is_some_and(|warning| warning.contains("尚无精确上线时刻"))
        );
        assert_eq!(requests.await.unwrap(), 5);
    }

    #[tokio::test]
    async fn placeholder_dates_use_anime_schedule_month_without_querying_a_future_timetable() {
        let (base_url, requests) = month_only_anime_schedule_mock_server().await;
        let provider = ScheduleProvider::new(ScheduleConfig {
            bangumi_data_url: format!("{base_url}/data.json"),
            bangumi_api_base_url: base_url.clone(),
            anime_schedule_api_url: format!("{base_url}/anime-schedule"),
            request_timeout_secs: 5,
            ..ScheduleConfig::default()
        })
        .unwrap();
        let catalog = provider.load_catalog().await.unwrap();

        let resolved = provider
            .resolve_auto(
                &catalog,
                AutoScheduleRequest {
                    title: "拥有超强装备与宇宙飞船的我",
                    subject_id: Some(536_270),
                    next_episode: 1,
                    episode_mapping: None,
                    anilist_media_id: None,
                    anime_schedule_route: Some(
                        "mezametara-saikyou-soubi-to-uchuusenmochi-datta-node-ikkodate-mezashite-youhei-toshite-jiyuu-ni-ikitai",
                    ),
                    timezone: "Asia/Shanghai",
                },
            )
            .await
            .unwrap();

        assert_eq!(resolved.schedule_confidence, "date_only");
        assert_eq!(resolved.expected_time, None);
        assert_eq!(
            resolved.expected_at.unwrap().to_rfc3339(),
            "2026-09-30T16:00:00+00:00"
        );
        let warning = resolved.schedule_warning.unwrap();
        assert!(warning.contains("2026年10月"));
        assert!(!warning.contains("2099-01-01"));
        assert!(!warning.contains("周排期暂时不可用"));
        assert_eq!(requests.await.unwrap(), 4);
    }

    #[tokio::test]
    async fn sync_preserves_last_calibrated_time_during_episode_api_outage() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("schedule-outage.db");
        let repository = Repository::connect(database.to_str().unwrap())
            .await
            .unwrap();
        let previous = DateTime::parse_from_rfc3339("2026-08-22T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let anime_id = repository
            .add_anime(NewAnime {
                title: "沉默魔女".into(),
                aliases: vec![],
                next_episode: 8,
                expected_at: Some(previous),
                expected_weekday: Some(5),
                expected_time: Some("23:00".into()),
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
        let (base_url, requests) = degraded_mock_server().await;
        let config = ScheduleConfig {
            bangumi_data_url: format!("{base_url}/data.json"),
            bangumi_api_base_url: base_url,
            preferred_site: "bilibili".into(),
            request_timeout_secs: 5,
            ..ScheduleConfig::default()
        };

        ScheduleSynchronizer::new(repository.clone(), config)
            .unwrap()
            .sync_now(anime_id)
            .await
            .unwrap();

        let anime = repository.get_anime(anime_id).await.unwrap();
        assert_eq!(anime.anime.schedule_confidence.as_deref(), Some("stale"));
        assert_eq!(anime.anime.schedule_source.as_deref(), Some("danime"));
        assert!(
            anime
                .anime
                .schedule_warning
                .as_deref()
                .is_some_and(|warning| warning.contains("已保留上次校准时间"))
        );
        assert_eq!(
            repository
                .active_episode(anime_id)
                .await
                .unwrap()
                .expected_at,
            Some(previous)
        );
        assert_eq!(requests.await.unwrap(), 2);
    }

    #[tokio::test]
    async fn archived_sync_refreshes_rating_without_loading_schedule_catalog() {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("archive-rating.db");
        let repository = Repository::connect(database.to_str().unwrap())
            .await
            .unwrap();
        let anime_id = repository
            .add_anime(NewAnime {
                title: "攻壳机动队".into(),
                aliases: vec![],
                next_episode: 1,
                expected_at: None,
                expected_weekday: None,
                expected_time: None,
                timezone: "Asia/Shanghai".into(),
                duration_min_sec: 1_200,
                duration_max_sec: 1_800,
                auto_schedule: Some(AutoScheduleMetadata {
                    bangumi_subject_id: 496_276,
                    anilist_media_id: None,
                    anime_schedule_route: None,
                    total_episodes: Some(10),
                    broadcast_pattern: "R/2026-07-01T15:00:00Z/P7D".into(),
                    schedule_source: "danime".into(),
                    schedule_confidence: "calibrated".into(),
                    schedule_warning: None,
                    next_sync_at: Utc::now(),
                    episode_mapping: None,
                }),
            })
            .await
            .unwrap();
        repository
            .mark_anime_released_complete(anime_id, Some(10))
            .await
            .unwrap();
        repository
            .archive_anime(anime_id, Some(10), "")
            .await
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            let headers = String::from_utf8_lossy(request_headers(&request));
            assert!(headers.contains("/v0/subjects/496276"));
            let body = r#"{"id":496276,"name":"攻殻機動隊","rating":{"rank":88,"total":4321,"score":8.1}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let config = ScheduleConfig {
            bangumi_data_url: "http://127.0.0.1:1/catalog-must-not-be-requested".into(),
            bangumi_api_base_url: format!("http://{address}"),
            request_timeout_secs: 5,
            ..ScheduleConfig::default()
        };

        ScheduleSynchronizer::new(repository.clone(), config)
            .unwrap()
            .sync_now(anime_id)
            .await
            .unwrap();

        let memory = repository.get_anime_archive_memory(anime_id).await.unwrap();
        assert_eq!(memory.bangumi_score, Some(8.1));
        assert_eq!(memory.bangumi_rating_total, Some(4_321));
        assert_eq!(memory.bangumi_rank, Some(88));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn mapped_episode_uses_subject_position_and_bangumi_number() {
        assert!(
            EpisodeNumberMapping {
                local_origin: 12,
                bangumi_origin: 78,
            }
            .mapped_numbers(11)
            .is_err()
        );
        let (base_url, requests) = mapped_mock_server().await;
        let provider = provider(&base_url);
        let catalog = provider.load_catalog().await.unwrap();
        let now = DateTime::parse_from_rfc3339("2026-08-22T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let resolved = provider
            .resolve_mapped_at(
                &catalog,
                "Re：从零开始的异世界生活 第四季 夺还篇",
                Some(633_836),
                EpisodeScheduleTarget {
                    local_episode: 14,
                    mapping: Some(EpisodeNumberMapping {
                        local_origin: 12,
                        bangumi_origin: 78,
                    }),
                },
                "Asia/Shanghai",
                now,
            )
            .await
            .unwrap();

        assert_eq!(
            resolved.expected_at.unwrap().to_rfc3339(),
            "2026-08-26T13:00:00+00:00"
        );
        assert_eq!(resolved.expected_weekday, Some(2));
        assert_eq!(resolved.expected_time.as_deref(), Some("21:00"));
        assert_eq!(resolved.schedule_source, "danime");
        assert_eq!(resolved.schedule_confidence, "calibrated");
        assert_eq!(resolved.total_episodes, Some(8));
        assert_eq!(
            EpisodeNumberMapping {
                local_origin: 12,
                bangumi_origin: 78,
            }
            .final_local_episode(resolved.total_episodes.unwrap())
            .unwrap(),
            19
        );
        assert_eq!(requests.await.unwrap(), 3);
    }

    #[tokio::test]
    async fn episode_count_survives_an_offset_past_the_final_episode() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            let headers = String::from_utf8_lossy(request_headers(&request));
            assert!(headers.contains("offset=12"));
            let response_body = r#"{"total":12,"data":[]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let provider = provider(&format!("http://{address}"));

        let lookup = provider.episode_airdate(633_836, 13, 90).await.unwrap();

        assert!(lookup.airdate.is_none());
        assert_eq!(lookup.total_episodes, Some(12));
        task.await.unwrap();
    }

    #[test]
    fn parses_supported_recurrence() {
        let recurrence = parse_recurrence("R/2026-07-03T15:00:00.000Z/P7D").unwrap();
        assert_eq!(recurrence.period, Duration::days(7));
        assert!(parse_recurrence("R/2026-07-03T15:00:00.000Z/P1M").is_err());
    }

    #[test]
    fn cat_and_dragon_uses_the_earliest_corroborated_stream() {
        let item: BangumiDataItem = serde_json::from_str(
            r#"{
                "title":"猫と竜",
                "titleTranslate":{},
                "type":"tv",
                "begin":"2026-07-04T12:00:00.000Z",
                "broadcast":"R/2026-07-04T12:00:00.000Z/P7D",
                "sites":[
                    {"site":"bangumi","id":"538760"},
                    {"site":"unext","begin":"2026-07-04T12:00:00.000Z"},
                    {"site":"danime","begin":"2026-06-27T12:30:00.000Z"},
                    {"site":"gamer","begin":"2026-06-27T13:00:00.000Z"},
                    {"site":"gamer_hk","begin":"2026-06-27T13:00:00.000Z"}
                ]
            }"#,
        )
        .unwrap();
        let origin = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();

        let selected = select_broadcast(&item, &ScheduleConfig::default(), Some(origin)).unwrap();

        assert_eq!(selected.source, "danime");
        let expected = expected_at(
            parse_recurrence(&selected.pattern).unwrap(),
            10,
            Some(NaiveDate::from_ymd_opt(2026, 9, 5).unwrap()),
            Some(origin),
            BroadcastSourceKind::Stream,
            14,
            Utc::now(),
        )
        .unwrap();
        assert_eq!(
            expected.expected_at.unwrap().to_rfc3339(),
            "2026-08-29T12:30:00+00:00"
        );
    }

    #[test]
    fn explicit_preferred_site_overrides_fallback_consensus() {
        let item: BangumiDataItem = serde_json::from_str(
            r#"{
                "title":"example",
                "titleTranslate":{},
                "type":"tv",
                "begin":"2026-07-04T12:00:00.000Z",
                "sites":[
                    {"site":"bangumi","id":"1"},
                    {"site":"bilibili","begin":"2026-07-04T13:00:00.000Z"},
                    {"site":"danime","begin":"2026-06-27T12:30:00.000Z"},
                    {"site":"gamer","begin":"2026-06-27T13:00:00.000Z"}
                ]
            }"#,
        )
        .unwrap();

        let selected = select_broadcast(
            &item,
            &ScheduleConfig::default(),
            Some(NaiveDate::from_ymd_opt(2026, 7, 4).unwrap()),
        )
        .unwrap();

        assert_eq!(selected.source, "bilibili");
    }

    #[test]
    fn corroborated_cluster_beats_a_single_suspicious_early_source() {
        let item: BangumiDataItem = serde_json::from_str(
            r#"{
                "title":"example",
                "titleTranslate":{},
                "type":"tv",
                "begin":"2026-07-02T15:30:00.000Z",
                "sites":[
                    {"site":"bangumi","id":"1"},
                    {"site":"unext","begin":"2026-06-23T02:11:00.000Z"},
                    {"site":"danime","begin":"2026-07-02T16:00:00.000Z"},
                    {"site":"gamer","begin":"2026-07-02T16:00:00.000Z"},
                    {"site":"gamer_hk","begin":"2026-07-02T16:00:00.000Z"}
                ]
            }"#,
        )
        .unwrap();

        let selected = select_broadcast(
            &item,
            &ScheduleConfig::default(),
            Some(NaiveDate::from_ymd_opt(2026, 7, 2).unwrap()),
        )
        .unwrap();

        assert_eq!(selected.source, "danime");
    }

    #[test]
    fn regional_variants_do_not_count_as_independent_streams() {
        let item: BangumiDataItem = serde_json::from_str(
            r#"{
                "title":"example",
                "titleTranslate":{},
                "type":"tv",
                "begin":"2026-07-04T12:00:00.000Z",
                "sites":[
                    {"site":"bangumi","id":"1"},
                    {"site":"unext","begin":"2026-07-04T12:00:00.000Z"},
                    {"site":"gamer","begin":"2026-06-27T13:00:00.000Z"},
                    {"site":"gamer_hk","begin":"2026-06-27T13:00:00.000Z"}
                ]
            }"#,
        )
        .unwrap();

        let selected = select_broadcast(
            &item,
            &ScheduleConfig::default(),
            Some(NaiveDate::from_ymd_opt(2026, 7, 4).unwrap()),
        )
        .unwrap();

        assert_eq!(selected.source, "gamer");
        assert!(selected.warning.is_some());
    }

    #[test]
    fn excluded_stream_is_not_used_even_when_old_priority_still_lists_it() {
        let item: BangumiDataItem = serde_json::from_str(
            r#"{
                "title":"example",
                "titleTranslate":{},
                "type":"tv",
                "begin":"2026-07-04T12:00:00.000Z",
                "broadcast":"R/2026-07-04T12:00:00.000Z/P7D",
                "sites":[
                    {"site":"bangumi","id":"1"},
                    {"site":"unext","begin":"2026-07-01T01:11:00.000Z"}
                ]
            }"#,
        )
        .unwrap();
        let mut config = ScheduleConfig::default();
        config.stream_site_priority.insert(0, "unext".into());

        let selected = select_broadcast(
            &item,
            &config,
            Some(NaiveDate::from_ymd_opt(2026, 7, 4).unwrap()),
        )
        .unwrap();

        assert_eq!(selected.source, "bangumi-data");
        assert_eq!(selected.kind, BroadcastSourceKind::Catalog);
    }

    #[test]
    fn exact_historical_airdate_is_not_rolled_forward() {
        let recurrence = parse_recurrence("R/2025-07-04T10:03:00.000Z/P7D").unwrap();
        let airdate = NaiveDate::from_ymd_opt(2025, 8, 22).unwrap();
        let origin = NaiveDate::from_ymd_opt(2025, 7, 4).unwrap();
        let now = DateTime::parse_from_rfc3339("2026-08-20T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let expected = expected_at(
            recurrence,
            8,
            Some(airdate),
            Some(origin),
            BroadcastSourceKind::Stream,
            14,
            now,
        )
        .unwrap();

        assert_eq!(
            expected.expected_at.unwrap().to_rfc3339(),
            "2025-08-22T10:03:00+00:00"
        );
    }

    #[test]
    fn network_midnight_is_applied_to_the_episode_airdate() {
        let recurrence = parse_recurrence("R/2026-07-04T15:00:00.000Z/P7D").unwrap();
        let target = NaiveDate::from_ymd_opt(2026, 8, 22).unwrap();
        let origin = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let now = DateTime::parse_from_rfc3339("2026-08-22T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let expected = expected_at(
            recurrence,
            9,
            Some(target),
            Some(origin),
            BroadcastSourceKind::Stream,
            14,
            now,
        )
        .unwrap();

        assert_eq!(
            expected.expected_at.unwrap().to_rfc3339(),
            "2026-08-22T15:00:00+00:00"
        );
        assert_eq!(expected.confidence, "calibrated");
        assert!(expected.warning.is_none());
    }

    #[test]
    fn earlier_network_window_preserves_the_source_date_offset() {
        let recurrence = parse_recurrence("R/2026-06-25T15:00:00.000Z/P7D").unwrap();
        let target = NaiveDate::from_ymd_opt(2026, 8, 26).unwrap();
        let origin = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let now = DateTime::parse_from_rfc3339("2026-08-22T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let expected = expected_at(
            recurrence,
            9,
            Some(target),
            Some(origin),
            BroadcastSourceKind::Stream,
            14,
            now,
        )
        .unwrap();

        assert_eq!(
            expected.expected_at.unwrap().to_rfc3339(),
            "2026-08-20T15:00:00+00:00"
        );
        assert_eq!(expected.confidence, "calibrated");
    }

    #[test]
    fn conflicting_catalog_schedule_is_hidden_instead_of_fabricated() {
        let recurrence = parse_recurrence("R/2026-07-08T20:30:00.000Z/P7D").unwrap();
        let target = NaiveDate::from_ymd_opt(2026, 8, 22).unwrap();
        let origin = NaiveDate::from_ymd_opt(2026, 7, 4).unwrap();
        let now = DateTime::parse_from_rfc3339("2026-08-22T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let expected = expected_at(
            recurrence,
            9,
            Some(target),
            Some(origin),
            BroadcastSourceKind::Catalog,
            1,
            now,
        )
        .unwrap();

        assert!(expected.expected_at.is_none());
        assert_eq!(expected.confidence, "unavailable");
        assert!(expected.health_error.is_some());
    }

    fn provider(base_url: &str) -> ScheduleProvider {
        ScheduleProvider::new(ScheduleConfig {
            bangumi_data_url: format!("{base_url}/data.json"),
            bangumi_api_base_url: base_url.into(),
            preferred_site: "bilibili".into(),
            request_timeout_secs: 5,
            ..ScheduleConfig::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn reads_public_bangumi_rating_from_subject_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            let headers = String::from_utf8_lossy(request_headers(&request));
            assert!(headers.contains("/v0/subjects/496276"));
            let body = r#"{"id":496276,"name":"攻殻機動隊","name_cn":"攻壳机动队","rating":{"rank":321,"total":9876,"score":8.4}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let rating = provider(&format!("http://{address}"))
            .bangumi_public_rating(496_276)
            .await
            .unwrap();
        assert_eq!(
            rating,
            BangumiPublicRating {
                score: Some(8.4),
                total: 9_876,
                rank: Some(321),
            }
        );
        task.await.unwrap();
    }

    async fn mock_server(ambiguous: bool) -> (String, tokio::task::JoinHandle<usize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let expected_requests = if ambiguous { 1 } else { 3 };
        let task = tokio::spawn(async move {
            for index in 0..expected_requests {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                let headers = String::from_utf8_lossy(request_headers(&request));
                let response_body: String = if index == 0 {
                    dataset(ambiguous)
                } else if index == 1 {
                    assert!(headers.contains("/v0/episodes?"));
                    assert!(headers.contains("subject_id=501000"));
                    assert!(headers.contains("offset=7"));
                    r#"{"total":12,"data":[{"airdate":"2026-08-22","sort":8,"ep":8}]}"#.into()
                } else {
                    assert!(headers.contains("/v0/episodes?"));
                    assert!(headers.contains("subject_id=501000"));
                    assert!(headers.contains("offset=0"));
                    r#"{"total":12,"data":[{"airdate":"2026-07-04","sort":1,"ep":1}]}"#.into()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            expected_requests
        });
        (format!("http://{address}"), task)
    }

    async fn mapped_mock_server() -> (String, tokio::task::JoinHandle<usize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            for index in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                let headers = String::from_utf8_lossy(request_headers(&request));
                let response_body: String = if index == 0 {
                    r#"{"items":[{
                        "title":"Re:ゼロから始める異世界生活 4th season 奪還編",
                        "titleTranslate":{"zh-Hans":["Re：从零开始的异世界生活 第四季 夺还篇"]},
                        "type":"tv",
                        "begin":"2026-08-12T13:00:00.000Z",
                        "broadcast":"R/2026-08-12T13:00:00.000Z/P7D",
                        "sites":[
                            {"site":"bangumi","id":"633836"},
                            {"site":"danime","begin":"2026-08-12T13:00:00.000Z"}
                        ]
                    }]}"#
                        .into()
                } else if index == 1 {
                    assert!(headers.contains("/v0/episodes?"));
                    assert!(headers.contains("subject_id=633836"));
                    assert!(headers.contains("offset=2"));
                    r#"{"total":8,"data":[{"airdate":"2026-08-26","sort":80,"ep":80}]}"#.into()
                } else {
                    assert!(headers.contains("/v0/episodes?"));
                    assert!(headers.contains("subject_id=633836"));
                    assert!(headers.contains("offset=0"));
                    r#"{"total":8,"data":[{"airdate":"2026-08-12","sort":78,"ep":78}]}"#.into()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            3
        });
        (format!("http://{address}"), task)
    }

    async fn degraded_mock_server() -> (String, tokio::task::JoinHandle<usize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            for index in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                let headers = String::from_utf8_lossy(request_headers(&request));
                let (status, response_body) = if index == 0 {
                    ("200 OK", dataset(false))
                } else {
                    assert!(headers.contains("/v0/episodes?"));
                    assert!(headers.contains("subject_id=501000"));
                    (
                        "504 Gateway Timeout",
                        r#"{"error":"upstream timeout"}"#.into(),
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            2
        });
        (format!("http://{address}"), task)
    }

    async fn unreleased_mock_server() -> (String, tokio::task::JoinHandle<usize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            for index in 0..5 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                let headers = String::from_utf8_lossy(request_headers(&request));
                let response_body = match index {
                    0 => {
                        r#"{"items":[{
                        "title":"別の作品","titleTranslate":{},"type":"tv",
                        "begin":"2026-10-01T15:00:00.000Z",
                        "sites":[{"site":"bangumi","id":"999999"}]
                    }]}"#
                    }
                    1 => {
                        assert!(headers.contains("/v0/subjects/568244"));
                        r#"{"id":568244,"name":"薬屋のひとりごと 第3期","name_cn":"药屋少女的呢喃 第三季","date":"2026-10-02","platform":"TV","total_episodes":12}"#
                    }
                    2 => {
                        assert!(headers.contains("/v0/episodes?"));
                        assert!(headers.contains("subject_id=568244"));
                        assert!(headers.contains("offset=0"));
                        r#"{"total":12,"data":[{"airdate":"2026-10-02","sort":1,"ep":1}]}"#
                    }
                    3 => {
                        assert!(headers.starts_with("GET /anime-schedule/anime?"));
                        assert!(headers.contains("q="));
                        r#"{"anime":[{"id":"as-1","title":"Kusuriya no Hitorigoto 3rd Season","route":"kusuriya-no-hitorigoto-3rd-season","premier":"2026-10-02T14:00:00Z","episodes":12,"status":"Upcoming","names":{"romaji":"Kusuriya no Hitorigoto 3rd Season","english":"The Apothecary Diaries Season 3","native":"薬屋のひとりごと 第3期","synonyms":["药屋少女的呢喃 第三季"]},"websites":{"aniList":"https://anilist.co/anime/195516"},"mediaTypes":[{"route":"tv"}]}]}"#
                    }
                    _ => {
                        assert!(headers.starts_with("GET /anime-schedule/timetables/raw?"));
                        assert!(headers.contains("tz=UTC"));
                        r#"[{"route":"kusuriya-no-hitorigoto-3rd-season","episodeDate":"2026-10-02T14:00:00Z","episodeNumber":1}]"#
                    }
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            5
        });
        (format!("http://{address}"), task)
    }

    async fn date_only_anime_schedule_mock_server() -> (String, tokio::task::JoinHandle<usize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            for index in 0..5 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                let headers = String::from_utf8_lossy(request_headers(&request));
                let response_body = match index {
                    0 => {
                        r#"{"items":[{"title":"別の作品","titleTranslate":{},"type":"tv","begin":"2026-10-01T15:00:00.000Z","sites":[{"site":"bangumi","id":"999999"}]}]}"#
                    }
                    1 => {
                        assert!(headers.contains("/v0/subjects/513878"));
                        r#"{"id":513878,"name":"Cyberpunk: Edgerunners 2","name_cn":"赛博朋克：边缘行者 2","date":"2026-10-20","platform":"WEB","total_episodes":10}"#
                    }
                    2 => {
                        assert!(headers.contains("/v0/episodes?"));
                        assert!(headers.contains("subject_id=513878"));
                        assert!(headers.contains("offset=0"));
                        r#"{"total":10,"data":[{"airdate":"2026-10-20","sort":1,"ep":1}]}"#
                    }
                    3 => {
                        assert!(headers.starts_with("GET /anime-schedule/anime?"));
                        assert!(headers.contains("anilist-ids=195539"));
                        r#"{"anime":[{"id":"as-2","title":"Cyberpunk: Edgerunners 2","route":"cyberpunk-edgerunners-2","premier":null,"episodes":10,"status":"Upcoming","names":{"romaji":"Cyberpunk: Edgerunners 2","english":"Cyberpunk: Edgerunners 2","native":"サイバーパンク: エッジランナーズ2"},"websites":{"aniList":"https://anilist.co/anime/195539"},"mediaTypes":[{"route":"ona"}]}]}"#
                    }
                    _ => {
                        assert!(headers.starts_with("GET /anime-schedule/timetables/raw?"));
                        r#"[]"#
                    }
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            5
        });
        (format!("http://{address}"), task)
    }

    async fn month_only_anime_schedule_mock_server() -> (String, tokio::task::JoinHandle<usize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            for index in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                let headers = String::from_utf8_lossy(request_headers(&request));
                let response_body = match index {
                    0 => {
                        r#"{"items":[{"title":"別の作品","titleTranslate":{},"type":"tv","begin":"2026-10-01T15:00:00.000Z","sites":[{"site":"bangumi","id":"999999"}]}]}"#
                    }
                    1 => {
                        assert!(headers.contains("/v0/subjects/536270"));
                        r#"{"id":536270,"name":"目覚めたら最強装備と宇宙船持ちだったので","name_cn":"拥有超强装备与宇宙飞船的我","date":"2099-01-01","platform":"TV","total_episodes":null}"#
                    }
                    2 => {
                        assert!(headers.contains("/v0/episodes?"));
                        assert!(headers.contains("subject_id=536270"));
                        r#"{"total":null,"data":[{"airdate":"2099-01-01","sort":1,"ep":1}]}"#
                    }
                    _ => {
                        assert!(headers.starts_with("GET /anime-schedule/anime/mezametara-"));
                        r#"{"title":"目覚めたら最強装備と宇宙船持ちだったので","route":"mezametara-saikyou-soubi-to-uchuusenmochi-datta-node-ikkodate-mezashite-youhei-toshite-jiyuu-ni-ikitai","premier":null,"month":"October","year":2026,"episodes":null,"status":"Upcoming","names":{"english":"Reborn as a Space Mercenary","native":"目覚めたら最強装備と宇宙船持ちだったので"},"mediaTypes":[{"route":"tv"}]}"#
                    }
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            4
        });
        (format!("http://{address}"), task)
    }

    fn dataset(ambiguous: bool) -> String {
        let duplicate = if ambiguous {
            r#",{
                "title":"サイレント・ウィッチ 第二期",
                "titleTranslate":{"zh-Hans":["沉默魔女"]},
                "type":"tv",
                "begin":"2027-01-01T15:00:00.000Z",
                "broadcast":"R/2027-01-01T15:00:00.000Z/P7D",
                "sites":[{"site":"bangumi","id":"601000"}]
            }"#
        } else {
            ""
        };
        format!(
            r#"{{"items":[{{
                "title":"サイレント・ウィッチ",
                "titleTranslate":{{"zh-Hans":["沉默魔女"],"en":["Silent Witch"]}},
                "type":"tv",
                "begin":"2026-07-03T15:00:00.000Z",
                "broadcast":"R/2026-07-03T15:00:00.000Z/P7D",
                "sites":[
                    {{"site":"bangumi","id":"501000"}},
                    {{"site":"danime","begin":"2026-07-04T15:00:00.000Z"}}
                ]
            }}{duplicate}]}}"#
        )
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut chunk = [0_u8; 4_096];
        loop {
            let read = stream.read(&mut chunk).await.unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        request
    }

    fn request_headers(request: &[u8]) -> &[u8] {
        let end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap();
        &request[..end]
    }
}
