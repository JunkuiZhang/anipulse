use std::{
    collections::{HashMap, HashSet},
    time::Duration as StdDuration,
};

use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDate, TimeZone, Timelike, Utc};
use chrono_tz::{Asia::Tokyo, Tz};
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
const ANILIST_SEARCH_QUERY: &str = r#"query AniPulseSearch($search: String!, $seasonYear: Int) {
  Page(page: 1, perPage: 10) {
    media(search: $search, seasonYear: $seasonYear, type: ANIME, sort: SEARCH_MATCH) {
      id title { romaji english native } synonyms format status
      startDate { year month day } episodes
      nextAiringEpisode { episode airingAt }
    }
  }
}"#;
const ANILIST_MEDIA_QUERY: &str = r#"query AniPulseMedia($id: Int!) {
  Media(id: $id, type: ANIME) {
    id title { romaji english native } synonyms format status
    startDate { year month day } episodes
    nextAiringEpisode { episode airingAt }
  }
}"#;

#[derive(Clone)]
pub struct ScheduleProvider {
    client: Client,
    config: ScheduleConfig,
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
    pub timezone: &'a str,
}

#[derive(Debug, Clone)]
pub struct ResolvedSchedule {
    pub bangumi_subject_id: i64,
    pub anilist_media_id: Option<i64>,
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
}

#[derive(Debug, Deserialize)]
struct AniListResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<AniListError>,
}

#[derive(Debug, Deserialize)]
struct AniListError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct AniListSearchData {
    #[serde(rename = "Page")]
    page: AniListPage,
}

#[derive(Debug, Deserialize)]
struct AniListMediaData {
    #[serde(rename = "Media")]
    media: Option<AniListMedia>,
}

#[derive(Debug, Deserialize)]
struct AniListPage {
    #[serde(default)]
    media: Vec<AniListMedia>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AniListMedia {
    id: i64,
    title: AniListTitle,
    #[serde(default)]
    synonyms: Vec<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    start_date: AniListDate,
    #[serde(default)]
    episodes: Option<i64>,
    #[serde(default)]
    next_airing_episode: Option<AniListAiring>,
}

#[derive(Debug, Clone, Deserialize)]
struct AniListTitle {
    #[serde(default)]
    romaji: Option<String>,
    #[serde(default)]
    english: Option<String>,
    #[serde(default)]
    native: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AniListDate {
    year: Option<i32>,
    month: Option<u32>,
    day: Option<u32>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AniListAiring {
    episode: i64,
    airing_at: i64,
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
        Ok(Self { client, config })
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
    /// is not in the catalog falls back to Bangumi subject metadata plus AniList.
    /// `anilist_media_id` is a previously persisted or explicitly selected mapping.
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
                .resolve_anilist_fallback(
                    subject_id,
                    target,
                    request.anilist_media_id,
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
        let airdate = episode
            .airdate
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(|value| {
                NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| {
                    AppError::Schedule(format!(
                        "Bangumi episode API returned invalid airdate: {value}"
                    ))
                })
            })
            .transpose()?;
        Ok(EpisodeLookup {
            airdate,
            total_episodes,
        })
    }

    async fn resolve_anilist_fallback(
        &self,
        subject_id: i64,
        episode_target: EpisodeScheduleTarget,
        anilist_media_id: Option<i64>,
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
        let subject_date = subject
            .date
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(|value| {
                NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| {
                    AppError::Schedule(format!(
                        "Bangumi subject API returned invalid date: {value}"
                    ))
                })
            })
            .transpose()?;
        let media = match anilist_media_id {
            Some(media_id) => {
                let media = self.anilist_media(media_id).await?;
                validate_anilist_mapping(&subject, subject_date, &media)?;
                media
            }
            None => {
                let candidates = self
                    .anilist_search(&subject.name, subject_date.map(|date| date.year()))
                    .await?;
                select_anilist_candidate(&subject, subject_date, candidates)?
            }
        };
        let (expected_at, confidence, timing_warning) = if let Some(airing) =
            media.next_airing_episode
        {
            let reported_at = Utc
                .timestamp_opt(airing.airing_at, 0)
                .single()
                .ok_or_else(|| {
                    AppError::Schedule(format!(
                        "AniList #{} returned an invalid airing timestamp",
                        media.id
                    ))
                })?;
            if airing.episode == subject_episode_index {
                let warning = episode.airdate.and_then(|airdate| {
                        let reported_date = reported_at.with_timezone(&Tokyo).date_naive();
                        (reported_date != airdate).then(|| {
                            format!(
                                "Bangumi 章节日期为 {airdate}，AniList 精确时刻对应日本日期 {reported_date}；已采用 AniList 时刻。"
                            )
                        })
                    });
                (reported_at, "calibrated", warning)
            } else if let Some(airdate) = episode.airdate {
                let tokyo_time = reported_at.with_timezone(&Tokyo).time();
                let local = Tokyo
                    .with_ymd_and_hms(
                        airdate.year(),
                        airdate.month(),
                        airdate.day(),
                        tokyo_time.hour(),
                        tokyo_time.minute(),
                        tokyo_time.second(),
                    )
                    .single()
                    .ok_or_else(|| {
                        AppError::Schedule("cannot construct AniList airing time".into())
                    })?;
                (
                    local.with_timezone(&Utc),
                    "estimated",
                    Some(format!(
                        "AniList 当前报告 EP{}，目标为 Bangumi EP{}；已用 Bangumi 章节日期和 AniList 的日本时刻组合，待后续同步校准。",
                        airing.episode, subject_episode_index
                    )),
                )
            } else {
                let offset = subject_episode_index
                    .checked_sub(airing.episode)
                    .and_then(|delta| delta.checked_mul(7))
                    .ok_or_else(|| AppError::Schedule("episode offset overflowed".into()))?;
                (
                    reported_at + Duration::days(offset),
                    "estimated",
                    Some(format!(
                        "Bangumi 暂无目标章节日期；已从 AniList EP{} 按周推算 EP{}，每日同步会继续校准。",
                        airing.episode, subject_episode_index
                    )),
                )
            }
        } else {
            anilist_date_only_schedule(
                episode.airdate,
                subject_date,
                &media,
                subject_episode_index,
                timezone,
            )?
        };
        let batch_release = confidence == "date_only" && media.format.as_deref() == Some("ONA");
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
        let aliases = anilist_aliases(&subject, &media);
        let fallback_warning = format!(
            "bangumi-data 尚未收录 Bangumi #{}；已绑定 AniList #{}，系统会每日复查并在正式目录收录后自动切回平台排期。",
            subject.id, media.id
        );
        Ok(ResolvedSchedule {
            bangumi_subject_id: subject.id,
            anilist_media_id: Some(media.id),
            total_episodes,
            matched_title: subject.name,
            aliases,
            expected_at: Some(expected_at),
            expected_weekday: Some(i64::from(local.weekday().num_days_from_monday())),
            expected_time: (confidence != "date_only").then(|| local.format("%H:%M").to_string()),
            timezone: timezone.name().to_string(),
            broadcast_pattern: format!("R/{}/P7D", anchor.to_rfc3339()),
            schedule_source: "anilist".into(),
            schedule_confidence: confidence.into(),
            schedule_warning: merge_warnings(Some(fallback_warning), timing_warning),
            source_health_error: None,
        })
    }

    async fn bangumi_subject(&self, subject_id: i64) -> Result<BangumiSubject> {
        let url = format!(
            "{}/v0/subjects/{subject_id}",
            self.config.bangumi_api_base_url.trim_end_matches('/')
        );
        self.get_json(&url, "Bangumi subject API").await
    }

    async fn anilist_search(&self, title: &str, year: Option<i32>) -> Result<Vec<AniListMedia>> {
        let body = serde_json::json!({
            "operationName": "AniPulseSearch",
            "query": ANILIST_SEARCH_QUERY,
            "variables": { "search": title, "seasonYear": year }
        });
        let response: AniListResponse<AniListSearchData> = self
            .post_json(&self.config.anilist_api_url, &body, "AniList API")
            .await?;
        let data = anilist_data(response)?;
        Ok(data.page.media)
    }

    async fn anilist_media(&self, media_id: i64) -> Result<AniListMedia> {
        let body = serde_json::json!({
            "operationName": "AniPulseMedia",
            "query": ANILIST_MEDIA_QUERY,
            "variables": { "id": media_id }
        });
        let response: AniListResponse<AniListMediaData> = self
            .post_json(&self.config.anilist_api_url, &body, "AniList API")
            .await?;
        anilist_data(response)?
            .media
            .ok_or_else(|| AppError::Schedule(format!("AniList media #{media_id} was not found")))
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

    async fn post_json<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
        body: &serde_json::Value,
        label: &str,
    ) -> Result<T> {
        let response = self
            .client
            .post(url)
            .json(body)
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
        return Err(AppError::Schedule(format!(
            "{label} returned HTTP {status}"
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

fn anilist_data<T>(response: AniListResponse<T>) -> Result<T> {
    if let Some(data) = response.data {
        return Ok(data);
    }
    let errors = response
        .errors
        .into_iter()
        .map(|error| error.message)
        .collect::<Vec<_>>()
        .join("; ");
    Err(AppError::Schedule(if errors.is_empty() {
        "AniList API returned no data".into()
    } else {
        format!("AniList API error: {errors}")
    }))
}

fn anilist_date(value: &AniListDate) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(value.year?, value.month?, value.day?)
}

fn anilist_date_only_schedule(
    episode_airdate: Option<NaiveDate>,
    subject_date: Option<NaiveDate>,
    media: &AniListMedia,
    subject_episode_index: i64,
    timezone: Tz,
) -> Result<(DateTime<Utc>, &'static str, Option<String>)> {
    let start_date = subject_date.or_else(|| anilist_date(&media.start_date));
    let date = if let Some(airdate) = episode_airdate {
        airdate
    } else {
        let start_date = start_date.ok_or_else(|| {
            AppError::Schedule(format!(
                "AniList #{} matched '{}', but it has neither a next airing timestamp nor a start date",
                media.id,
                anilist_display_title(media)
            ))
        })?;
        if subject_episode_index == 1 || media.format.as_deref() == Some("ONA") {
            start_date
        } else {
            let day_offset = subject_episode_index
                .checked_sub(1)
                .and_then(|value| value.checked_mul(7))
                .ok_or_else(|| AppError::Schedule("episode date offset overflowed".into()))?;
            start_date
                .checked_add_signed(Duration::days(day_offset))
                .ok_or_else(|| AppError::Schedule("episode date offset overflowed".into()))?
        }
    };
    let local = timezone
        .with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0)
        .single()
        .ok_or_else(|| AppError::Schedule("cannot construct date-only schedule boundary".into()))?;
    Ok((
        local.with_timezone(&Utc),
        "date_only",
        Some(format!(
            "当前来源仅公布 {date} 的开播日期，尚无精确上线时刻；系统将从当天开始检查并每日同步，获得精确时刻后会自动校准。"
        )),
    ))
}

fn anilist_titles(media: &AniListMedia) -> Vec<&str> {
    media
        .title
        .native
        .iter()
        .chain(media.title.romaji.iter())
        .chain(media.title.english.iter())
        .chain(media.synonyms.iter())
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .collect()
}

fn anilist_display_title(media: &AniListMedia) -> &str {
    media
        .title
        .native
        .as_deref()
        .or(media.title.romaji.as_deref())
        .or(media.title.english.as_deref())
        .unwrap_or("unknown title")
}

fn anilist_matches_subject(
    subject: &BangumiSubject,
    subject_date: Option<NaiveDate>,
    media: &AniListMedia,
) -> bool {
    let subject_titles = [&subject.name, &subject.name_cn]
        .into_iter()
        .map(|value| normalize_title(value))
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    let media_primary_titles = media
        .title
        .native
        .iter()
        .chain(media.title.romaji.iter())
        .chain(media.title.english.iter())
        .map(|value| normalize_title(value))
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    let media_titles = media_primary_titles
        .iter()
        .cloned()
        .chain(media.synonyms.iter().map(|value| normalize_title(value)))
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if subject_titles.is_empty() || media_titles.is_empty() {
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
    if !equivalent_title {
        return false;
    }
    let media_date = anilist_date(&media.start_date);
    // Synonyms and one-character spelling variants are accepted only with an
    // exact date match. This covers Japanese old/new glyph variants such as
    // 藥/薬 without allowing a nearby season title to bind when either source
    // has incomplete metadata.
    if !exact_primary_title
        && subject_date
            .zip(media_date)
            .is_none_or(|(left, right)| left != right)
    {
        return false;
    }
    if let (Some(subject_date), Some(media_date)) = (subject_date, media_date)
        && subject_date != media_date
    {
        return false;
    }
    let platform = subject.platform.as_deref().unwrap_or_default();
    if platform.eq_ignore_ascii_case("TV")
        && !matches!(media.format.as_deref(), Some("TV" | "TV_SHORT"))
    {
        return false;
    }
    true
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

fn validate_anilist_mapping(
    subject: &BangumiSubject,
    subject_date: Option<NaiveDate>,
    media: &AniListMedia,
) -> Result<()> {
    if anilist_matches_subject(subject, subject_date, media) {
        return Ok(());
    }
    Err(AppError::Schedule(format!(
        "AniList #{} ('{}', {}) does not match Bangumi #{} ('{}', {}); verify the season and ID",
        media.id,
        anilist_display_title(media),
        anilist_date(&media.start_date)
            .map(|date| date.to_string())
            .unwrap_or_else(|| "unknown date".into()),
        subject.id,
        subject.name,
        subject_date
            .map(|date| date.to_string())
            .unwrap_or_else(|| "unknown date".into())
    )))
}

fn select_anilist_candidate(
    subject: &BangumiSubject,
    subject_date: Option<NaiveDate>,
    candidates: Vec<AniListMedia>,
) -> Result<AniListMedia> {
    let mut matches = candidates
        .iter()
        .filter(|media| anilist_matches_subject(subject, subject_date, media))
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
                "#{} {} ({}, {}, {})",
                media.id,
                anilist_display_title(media),
                anilist_date(&media.start_date)
                    .map(|date| date.to_string())
                    .unwrap_or_else(|| "date unknown".into()),
                media.format.as_deref().unwrap_or("format unknown"),
                media.status.as_deref().unwrap_or("status unknown")
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    if matches.is_empty() {
        Err(AppError::Schedule(format!(
            "no high-confidence AniList match for Bangumi #{} '{}'. Search candidates: {}. Enter the correct AniList ID manually after checking the season",
            subject.id,
            subject.name,
            if choices.is_empty() { "none" } else { &choices }
        )))
    } else {
        Err(AppError::Schedule(format!(
            "multiple high-confidence AniList matches for Bangumi #{} '{}': {}. Enter the correct AniList ID manually",
            subject.id, subject.name, choices
        )))
    }
}

fn anilist_aliases(subject: &BangumiSubject, media: &AniListMedia) -> Vec<String> {
    let mut values = Vec::new();
    values.push(subject.name.clone());
    if !subject.name_cn.trim().is_empty() {
        values.push(subject.name_cn.clone());
    }
    values.extend(anilist_titles(media).into_iter().map(str::to_string));
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|value| {
            let normalized = normalize_title(value);
            !normalized.is_empty() && seen.insert(normalized)
        })
        .collect()
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
        let catalog = self.load_catalog_with_health().await?;
        self.sync_with_catalog(anime_id, &catalog).await
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
            .map(|subject_id| format!("anilist-schedule:{subject_id}"));
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
        let schedule_health_source = if resolved.schedule_source == "anilist" {
            format!("anilist-schedule:{}", resolved.bangumi_subject_id)
        } else {
            format!("bangumi-schedule:{}", resolved.bangumi_subject_id)
        };
        if resolved.schedule_source != "anilist" && anime.anime.anilist_media_id.is_some() {
            self.repository
                .record_source_success(&format!("anilist-schedule:{}", resolved.bangumi_subject_id))
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
    fn anilist_mapping_compares_all_titles_without_weakening_date_checks() {
        let subject_date = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        let subject = BangumiSubject {
            id: 513_878,
            name: "Cyberpunk: Edgerunners 2".into(),
            name_cn: "赛博朋克：边缘行者 2".into(),
            date: Some(subject_date.to_string()),
            platform: Some("WEB".into()),
            total_episodes: Some(10),
        };
        let media = AniListMedia {
            id: 195_539,
            title: AniListTitle {
                romaji: Some("Cyberpunk: Edgerunners 2".into()),
                english: Some("Cyberpunk: Edgerunners 2".into()),
                native: Some("サイバーパンク: エッジランナーズ2".into()),
            },
            synonyms: Vec::new(),
            format: Some("ONA".into()),
            status: Some("NOT_YET_RELEASED".into()),
            start_date: AniListDate {
                year: Some(2026),
                month: Some(10),
                day: Some(20),
            },
            episodes: Some(10),
            next_airing_episode: None,
        };

        assert!(anilist_matches_subject(
            &subject,
            Some(subject_date),
            &media
        ));

        let mut wrong_date = media.clone();
        wrong_date.start_date.day = Some(21);
        assert!(!anilist_matches_subject(
            &subject,
            Some(subject_date),
            &wrong_date
        ));
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
    async fn unreleased_subject_falls_back_to_a_unique_anilist_match() {
        let (base_url, requests) = unreleased_mock_server().await;
        let provider = ScheduleProvider::new(ScheduleConfig {
            bangumi_data_url: format!("{base_url}/data.json"),
            bangumi_api_base_url: base_url.clone(),
            anilist_api_url: format!("{base_url}/anilist/graphql"),
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
                    timezone: "Asia/Shanghai",
                },
            )
            .await
            .unwrap();

        assert_eq!(resolved.bangumi_subject_id, 568_244);
        assert_eq!(resolved.anilist_media_id, Some(195_516));
        assert_eq!(resolved.total_episodes, Some(12));
        assert_eq!(resolved.schedule_source, "anilist");
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
                .is_some_and(|value| value.contains("AniList #195516"))
        );
        assert_eq!(requests.await.unwrap(), 4);
    }

    #[tokio::test]
    async fn unreleased_ona_without_airing_timestamp_uses_date_only_schedule() {
        let (base_url, requests) = date_only_anilist_mock_server().await;
        let provider = ScheduleProvider::new(ScheduleConfig {
            bangumi_data_url: format!("{base_url}/data.json"),
            bangumi_api_base_url: base_url.clone(),
            anilist_api_url: format!("{base_url}/anilist/graphql"),
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
                    timezone: "Asia/Shanghai",
                },
            )
            .await
            .unwrap();

        assert_eq!(resolved.anilist_media_id, Some(195_539));
        assert_eq!(resolved.total_episodes, Some(10));
        assert_eq!(resolved.schedule_source, "anilist");
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
            for index in 0..4 {
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
                    _ => {
                        assert!(headers.starts_with("POST /anilist/graphql"));
                        r#"{"data":{"Page":{"media":[{"id":195516,"title":{"romaji":"Kusuriya no Hitorigoto 3rd Season","english":"The Apothecary Diaries Season 3","native":"藥屋のひとりごと 第3期"},"synonyms":[],"format":"TV","status":"NOT_YET_RELEASED","startDate":{"year":2026,"month":10,"day":2},"episodes":null,"nextAiringEpisode":{"episode":1,"airingAt":1790949600}}]}}}"#
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

    async fn date_only_anilist_mock_server() -> (String, tokio::task::JoinHandle<usize>) {
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
                        assert!(headers.contains("/v0/subjects/513878"));
                        r#"{"id":513878,"name":"Cyberpunk: Edgerunners 2","name_cn":"赛博朋克：边缘行者 2","date":"2026-10-20","platform":"WEB","total_episodes":10}"#
                    }
                    2 => {
                        assert!(headers.contains("/v0/episodes?"));
                        assert!(headers.contains("subject_id=513878"));
                        assert!(headers.contains("offset=0"));
                        r#"{"total":10,"data":[{"airdate":"2026-10-20","sort":1,"ep":1}]}"#
                    }
                    _ => {
                        assert!(headers.starts_with("POST /anilist/graphql"));
                        r#"{"data":{"Media":{"id":195539,"title":{"romaji":"Cyberpunk: Edgerunners 2","english":"Cyberpunk: Edgerunners 2","native":"サイバーパンク: エッジランナーズ2"},"synonyms":[],"format":"ONA","status":"NOT_YET_RELEASED","startDate":{"year":2026,"month":10,"day":20},"episodes":10,"nextAiringEpisode":null}}}"#
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
