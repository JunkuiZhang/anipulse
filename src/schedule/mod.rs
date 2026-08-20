use std::{collections::HashMap, time::Duration as StdDuration};

use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDate, TimeZone, Utc};
use chrono_tz::{Asia::Tokyo, Tz};
use reqwest::Client;
use serde::Deserialize;
use tracing::{info, warn};

use crate::{
    config::ScheduleConfig,
    detector::title::normalize_title,
    domain::ScheduleUpdate,
    error::{AppError, Result},
    repository::Repository,
};

const MAX_CATALOG_BYTES: u64 = 16 * 1024 * 1024;

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
    pub expected_at: DateTime<Utc>,
    pub expected_weekday: i64,
    pub expected_time: String,
    pub timezone: String,
    pub broadcast_pattern: String,
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

    async fn resolve_at(
        &self,
        catalog: &ScheduleCatalog,
        title: &str,
        subject_id: Option<i64>,
        next_episode: i64,
        timezone: &str,
        now: DateTime<Utc>,
    ) -> Result<ResolvedSchedule> {
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
        let selected = select_broadcast(item, &self.config.preferred_site)?;
        let recurrence = parse_recurrence(&selected.pattern)?;
        let airdate = if selected.use_episode_airdate {
            match self.episode_airdate(bangumi_subject_id, next_episode).await {
                Ok(value) => value,
                Err(error) => {
                    warn!(bangumi_subject_id, next_episode, %error, "Bangumi episode date unavailable; using broadcast recurrence");
                    None
                }
            }
        } else {
            None
        };
        let expected_at = expected_at(recurrence, next_episode, airdate, now)?;
        let local = expected_at.with_timezone(&timezone);

        Ok(ResolvedSchedule {
            bangumi_subject_id,
            matched_title: item.title.clone(),
            aliases: item_aliases(item),
            expected_at,
            expected_weekday: i64::from(local.weekday().num_days_from_monday()),
            expected_time: local.format("%H:%M").to_string(),
            timezone: timezone.name().to_string(),
            broadcast_pattern: selected.pattern,
        })
    }

    async fn episode_airdate(&self, subject_id: i64, episode_no: i64) -> Result<Option<NaiveDate>> {
        let offset = episode_no - 1;
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
        if (returned_number - episode_no as f64).abs() > 0.01 {
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
        let catalog = match self.provider.load_catalog().await {
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
        let catalog = self.provider.load_catalog().await?;
        self.sync_with_catalog(anime_id, &catalog).await
    }

    async fn sync_with_catalog(&self, anime_id: i64, catalog: &ScheduleCatalog) -> Result<()> {
        let anime = self.repository.get_anime(anime_id).await?;
        if !anime.anime.auto_schedule {
            return Err(AppError::Schedule(format!(
                "anime {anime_id} does not use automatic scheduling"
            )));
        }
        let episode = self.repository.active_episode(anime_id).await?;
        let resolved = self
            .provider
            .resolve(
                catalog,
                &anime.anime.title,
                anime.anime.bangumi_subject_id,
                episode.episode_no,
                &anime.anime.timezone,
            )
            .await?;
        let update = resolved
            .to_update(Utc::now() + Duration::seconds(self.config.sync_interval_secs as i64));
        self.repository
            .apply_schedule_update(anime_id, &update)
            .await?;
        info!(
            anime_id,
            bangumi_subject_id = resolved.bangumi_subject_id,
            episode = episode.episode_no,
            expected_at = %resolved.expected_at,
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
            next_sync_at,
        }
    }
}

struct SelectedBroadcast {
    pattern: String,
    use_episode_airdate: bool,
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

fn select_broadcast(item: &BangumiDataItem, preferred_site: &str) -> Result<SelectedBroadcast> {
    let item_pattern = item
        .broadcast
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .or_else(|| fallback_weekly_pattern(&item.begin));
    if let Some(site) = item.sites.iter().find(|site| {
        site.site == preferred_site
            && (site
                .broadcast
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
                || site
                    .begin
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty()))
    }) {
        let pattern = site
            .broadcast
            .clone()
            .or_else(|| {
                site.begin
                    .as_deref()
                    .and_then(|begin| site_pattern_from_item(begin, item_pattern.as_deref()))
            })
            .ok_or_else(|| AppError::Schedule("preferred site has no usable schedule".into()))?;
        return Ok(SelectedBroadcast {
            pattern,
            use_episode_airdate: true,
        });
    }
    item_pattern
        .map(|pattern| SelectedBroadcast {
            pattern,
            use_episode_airdate: true,
        })
        .ok_or_else(|| AppError::Schedule(format!("'{}' has no broadcast time", item.title)))
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
    airdate: Option<NaiveDate>,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    let has_exact_airdate = airdate.is_some();
    let steps = episode_no - 1;
    let period_seconds = recurrence.period.num_seconds();
    let projected =
        recurrence.anchor
            + Duration::seconds(period_seconds.checked_mul(steps).ok_or_else(|| {
                AppError::Schedule("episode schedule calculation overflowed".into())
            })?);
    let mut expected = if let Some(airdate) = airdate {
        let tokyo_time = projected.with_timezone(&Tokyo).time();
        let local = airdate.and_time(tokyo_time);
        match Tokyo.from_local_datetime(&local) {
            LocalResult::Single(value) => value.with_timezone(&Utc),
            LocalResult::Ambiguous(first, _) => first.with_timezone(&Utc),
            LocalResult::None => projected,
        }
    } else {
        projected
    };

    if !has_exact_airdate && expected < now - Duration::days(14) {
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
    Ok(expected)
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
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

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
            resolved.expected_at.to_rfc3339(),
            "2026-08-21T15:00:00+00:00"
        );
        assert_eq!(resolved.expected_weekday, 4);
        assert_eq!(resolved.expected_time, "23:00");
        assert_eq!(requests.await.unwrap(), 2);
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

    #[test]
    fn parses_supported_recurrence() {
        let recurrence = parse_recurrence("R/2026-07-03T15:00:00.000Z/P7D").unwrap();
        assert_eq!(recurrence.period, Duration::days(7));
        assert!(parse_recurrence("R/2026-07-03T15:00:00.000Z/P1M").is_err());
    }

    #[test]
    fn exact_historical_airdate_is_not_rolled_forward() {
        let recurrence = parse_recurrence("R/2025-07-04T10:03:00.000Z/P7D").unwrap();
        let airdate = NaiveDate::from_ymd_opt(2025, 8, 22).unwrap();
        let now = DateTime::parse_from_rfc3339("2026-08-20T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let expected = expected_at(recurrence, 8, Some(airdate), now).unwrap();

        assert_eq!(expected.to_rfc3339(), "2025-08-22T10:03:00+00:00");
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
        let expected_requests = if ambiguous { 1 } else { 2 };
        let task = tokio::spawn(async move {
            for index in 0..expected_requests {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                let headers = String::from_utf8_lossy(request_headers(&request));
                let response_body = if index == 0 {
                    dataset(ambiguous)
                } else {
                    assert!(headers.contains("/v0/episodes?"));
                    assert!(headers.contains("subject_id=501000"));
                    assert!(headers.contains("offset=7"));
                    r#"{"data":[{"airdate":"2026-08-22","sort":8,"ep":8}]}"#.into()
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
                "sites":[{{"site":"bangumi","id":"501000"}}]
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
