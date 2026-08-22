use std::{
    collections::{HashMap, HashSet},
    time::Duration as StdDuration,
};

use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDate, TimeZone, Utc};
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

#[derive(Clone)]
pub struct ScheduleProvider {
    client: Client,
    config: ScheduleConfig,
}

pub struct ScheduleCatalog {
    items: Vec<BangumiDataItem>,
}

#[derive(Debug, Clone)]
pub struct ResolvedSchedule {
    pub bangumi_subject_id: i64,
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
    data: Vec<BangumiEpisode>,
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
        let target_airdate = match self
            .episode_airdate(
                bangumi_subject_id,
                subject_episode_index,
                bangumi_episode_no,
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                warn!(bangumi_subject_id, next_episode, bangumi_episode_no, %error, "Bangumi episode date unavailable; using a lower-confidence schedule");
                source_health_error = Some(error.to_string());
                None
            }
        };
        let origin_airdate = if subject_episode_index == 1 {
            target_airdate
        } else if target_airdate.is_some() {
            match self
                .episode_airdate(bangumi_subject_id, 1, bangumi_origin_episode)
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    warn!(bangumi_subject_id, bangumi_origin_episode, %error, "Bangumi season origin date unavailable; using a lower-confidence schedule");
                    source_health_error = Some(error.to_string());
                    None
                }
            }
        } else {
            None
        };
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
    ) -> Result<Option<NaiveDate>> {
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
        let Some(episode) = page.data.first() else {
            return Ok(None);
        };
        let returned_number = if episode.ep > 0.0 {
            episode.ep
        } else {
            episode.sort
        };
        if (returned_number - bangumi_episode_no as f64).abs() > 0.01 {
            return Ok(None);
        }
        episode
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
            .transpose()
    }
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
        let resolved = if let Some(mapping) = episode_mapping {
            self.provider
                .resolve_with_mapping(
                    catalog,
                    &anime.anime.title,
                    anime.anime.bangumi_subject_id,
                    episode.episode_no,
                    mapping,
                    &anime.anime.timezone,
                )
                .await?
        } else {
            self.provider
                .resolve(
                    catalog,
                    &anime.anime.title,
                    anime.anime.bangumi_subject_id,
                    episode.episode_no,
                    &anime.anime.timezone,
                )
                .await?
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
        let schedule_health_source = format!("bangumi-schedule:{}", resolved.bangumi_subject_id);
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
    let item_pattern = item
        .broadcast
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .or_else(|| fallback_weekly_pattern(&item.begin));

    if let Some(site) = item
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
        if source == &config.preferred_site || !seen.insert(source.clone()) {
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
        assert_eq!(resolved.schedule_source, "unext");
        assert_eq!(resolved.schedule_confidence, "calibrated");
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
                    broadcast_pattern: "R/2026-07-04T15:00:00Z/P7D".into(),
                    schedule_source: "unext".into(),
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
        assert_eq!(anime.anime.schedule_source.as_deref(), Some("unext"));
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
        assert_eq!(resolved.schedule_source, "unext");
        assert_eq!(resolved.schedule_confidence, "calibrated");
        assert_eq!(requests.await.unwrap(), 3);
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

        assert_eq!(selected.source, "unext");
        assert!(selected.warning.is_some());
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
                    r#"{"data":[{"airdate":"2026-08-22","sort":8,"ep":8}]}"#.into()
                } else {
                    assert!(headers.contains("/v0/episodes?"));
                    assert!(headers.contains("subject_id=501000"));
                    assert!(headers.contains("offset=0"));
                    r#"{"data":[{"airdate":"2026-07-04","sort":1,"ep":1}]}"#.into()
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
                            {"site":"unext","begin":"2026-08-12T13:00:00.000Z"}
                        ]
                    }]}"#
                        .into()
                } else if index == 1 {
                    assert!(headers.contains("/v0/episodes?"));
                    assert!(headers.contains("subject_id=633836"));
                    assert!(headers.contains("offset=2"));
                    r#"{"data":[{"airdate":"2026-08-26","sort":80,"ep":80}]}"#.into()
                } else {
                    assert!(headers.contains("/v0/episodes?"));
                    assert!(headers.contains("subject_id=633836"));
                    assert!(headers.contains("offset=0"));
                    r#"{"data":[{"airdate":"2026-08-12","sort":78,"ep":78}]}"#.into()
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
                    {{"site":"unext","begin":"2026-07-04T15:00:00.000Z"}}
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
