//! TMDB only enriches the display title. Schedule and release decisions use their own sources.
use super::{
    BangumiSubject, EAST_ASIAN_PART, EAST_ASIAN_SEASON, LATIN_PART, LATIN_SEASON,
    ScheduleSynchronizer, decode_metadata_response, normalize_title, safe_request_error,
};
use crate::{
    error::{AppError, Result},
    repository::{ChapterTranslations, TmdbChapter},
};
use chrono::{NaiveDate, TimeDelta, Utc};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{env, sync::LazyLock, time::Duration};
use tracing::warn;

const PRIMARY: &str = "https://api.themoviedb.org/3";
const ALTERNATE: &str = "https://api.tmdb.org/3";
static NUMBER_ONLY: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)^(?:(?:episode|ep|集|話|话)\s*\.?\s*)?[0-9]+$|^第\s*[0-9零〇一二两兩三四五六七八九十百千]+\s*[集話话回]$")
        .expect("valid episode placeholder regex")
});

struct Tmdb<'a> {
    client: &'a Client,
    base: &'a str,
    token: String,
    cooldown: Duration,
}

#[derive(Deserialize)]
struct SearchPage {
    results: Vec<Series>,
}
#[derive(Deserialize)]
struct Series {
    id: i64,
    name: String,
    original_name: String,
    original_language: String,
}
#[derive(Deserialize)]
struct SeriesDetails {
    seasons: Vec<Season>,
}
#[derive(Deserialize)]
struct Season {
    season_number: i64,
    air_date: Option<NaiveDate>,
}
#[derive(Deserialize)]
struct SeasonDetails {
    episodes: Vec<SeasonEpisode>,
}
#[derive(Deserialize)]
struct SeasonEpisode {
    episode_number: i64,
    air_date: Option<NaiveDate>,
}

#[derive(Serialize, Deserialize)]
struct Binding {
    series_id: i64,
    season: i64,
    first_episode: i64, // The TMDB episode corresponding to this Bangumi subject's first regular episode.
    original_language: String,
}

#[derive(Deserialize)]
struct Translations {
    translations: Vec<Translation>,
}
#[derive(Deserialize)]
struct Translation {
    iso_639_1: String,
    iso_3166_1: String,
    data: TranslationData,
}
#[derive(Deserialize)]
struct TranslationData {
    name: Option<String>,
}

impl Translations {
    fn titles(&self, original_language: &str) -> ChapterTranslations {
        let find = |language: &str, region: Option<&str>| {
            self.translations
                .iter()
                .filter(|r| r.iso_639_1 == language && region.is_none_or(|c| c == r.iso_3166_1))
                .filter_map(|r| r.data.name.as_deref().map(str::trim))
                .find(|s| !s.is_empty() && !NUMBER_ONLY.is_match(&normalize_title(s)))
                .map(str::to_owned)
        };
        ChapterTranslations {
            simplified: ["CN", "SG"].into_iter().find_map(|r| find("zh", Some(r))),
            traditional: ["TW", "HK", "MO"]
                .into_iter()
                .find_map(|r| find("zh", Some(r))),
            original: find(original_language, None),
        }
    }
}

fn series_query(title: &str) -> &str {
    // TMDB usually groups sequels/cours under the original series. Strip the installment
    // suffix for discovery; the actual season and cour will be verified by premiere date.
    let end = [
        &*EAST_ASIAN_SEASON,
        &*LATIN_SEASON,
        &*EAST_ASIAN_PART,
        &*LATIN_PART,
    ]
    .into_iter()
    .filter_map(|re| re.find(title))
    .map(|m| m.start())
    .min()
    .unwrap_or(title.len());
    let base = title[..end].trim();
    if base.is_empty() { title.trim() } else { base }
}

fn compact(title: &str) -> String {
    normalize_title(title)
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

fn date_distance(a: NaiveDate, b: NaiveDate) -> i64 {
    (a - b).num_days().abs()
}

fn first_episode(episodes: &[SeasonEpisode], premiere: NaiveDate) -> Option<i64> {
    let distance = episodes
        .iter()
        .filter(|e| e.episode_number > 0)
        .filter_map(|e| e.air_date)
        .map(|d| date_distance(d, premiere))
        .min()
        .filter(|d| *d <= 1)?;
    let matches: Vec<_> = episodes
        .iter()
        .filter(|e| {
            e.episode_number > 0
                && e.air_date
                    .is_some_and(|d| date_distance(d, premiere) == distance)
        })
        .collect();
    let first = matches.first()?;
    if matches.len() == 1 {
        return Some(first.episode_number);
    }
    // A same-day batch premiere starts at episode 1. A midseason batch or a tie
    // between adjacent dates cannot reliably identify the start of a cour.
    (matches.iter().all(|e| e.air_date == first.air_date)
        && matches.iter().any(|e| e.episode_number == 1))
    .then_some(1)
}

impl Tmdb<'_> {
    async fn get<T: DeserializeOwned>(
        &mut self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Option<T>> {
        let response = self
            .client
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .query(query)
            .send()
            .await
            .map_err(|e| safe_request_error(e, "TMDB"))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            let seconds = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(60);
            self.cooldown = Duration::from_secs(u64::from(seconds.max(1)));
        }
        decode_metadata_response(response, "TMDB").await.map(Some)
    }

    async fn resolve(
        &mut self,
        subject: &BangumiSubject,
        bangumi_first_number: i64,
    ) -> Result<Option<Binding>> {
        let Some(premiere) = subject
            .date
            .as_deref()
            .and_then(|v| v.parse::<NaiveDate>().ok())
        else {
            return Ok(None);
        };
        let queries = [series_query(&subject.name), series_query(&subject.name_cn)];
        let names = queries.map(compact);
        let mut series = None;
        for (idx, &query) in queries.iter().enumerate() {
            if query.is_empty() || queries[..idx].contains(&query) {
                continue;
            }
            let Some(page) = self
                .get::<SearchPage>("/search/tv", &[("query", query), ("language", "zh-CN")])
                .await?
            else {
                continue;
            };
            let mut exact = page.results.into_iter().filter(|s| {
                [&s.name, &s.original_name]
                    .iter()
                    .map(|s| compact(s))
                    .any(|n| !n.is_empty() && names.contains(&n))
            });
            if let Some(hit) = exact.next() {
                if exact.next().is_some() {
                    return Ok(None);
                }
                series = Some(hit);
                break;
            }
        }
        let Some(series) = series else {
            return Ok(None);
        };
        let path = format!("/tv/{}", series.id);
        let Some(details) = self.get::<SeriesDetails>(&path, &[]).await? else {
            return Ok(None);
        };
        let mut seasons: Vec<_> = details
            .seasons
            .into_iter()
            .filter_map(|s| {
                let date = s.air_date?;
                (s.season_number > 0 && (date - premiere).num_days() <= 1)
                    .then_some((date, s.season_number))
            })
            .collect();
        seasons.sort_unstable_by(|a, b| b.cmp(a));
        let Some(&(date, season)) = seasons.first() else {
            return Ok(None);
        };
        let expected_seasons = super::season_numbers([&subject.name, &subject.name_cn]);
        if seasons.get(1).is_some_and(|s| s.0 == date) {
            return Ok(None);
        }
        let Some(list) = self
            .get::<SeasonDetails>(&format!("{path}/season/{season}"), &[])
            .await?
        else {
            return Ok(None);
        };
        let Some(first_episode) = first_episode(&list.episodes, premiere) else {
            return Ok(None);
        };
        // TMDB sometimes combines multiple broadcast seasons into season 1. The premiere
        // date and the verified series-wide episode number must both pinpoint its start.
        let merged_season =
            season == 1 && first_episode > 1 && first_episode == bangumi_first_number;
        if !expected_seasons.is_empty() && !expected_seasons.contains(&season) && !merged_season {
            return Ok(None);
        }
        Ok(Some(Binding {
            series_id: series.id,
            season,
            first_episode,
            original_language: series.original_language,
        }))
    }
}

impl ScheduleSynchronizer {
    /// A separate, low-frequency worker keeps metadata latency off the polling and web paths.
    pub async fn run_tmdb_title_updates(&self, tick: Duration) {
        let Ok(token) = env::var("TMDB_READ_ACCESS_TOKEN") else {
            return;
        };
        if token.trim().is_empty() {
            return;
        }
        let base = match env::var("TMDB_API_HOST").as_deref() {
            Err(_) | Ok("api.themoviedb.org") => PRIMARY,
            Ok("api.tmdb.org") => ALTERNATE,
            Ok(_) => {
                warn!("unsupported TMDB_API_HOST; title fallback disabled");
                return;
            }
        };
        let mut tmdb = Tmdb {
            client: &self.provider.client,
            token,
            base,
            cooldown: Duration::ZERO,
        };
        loop {
            if let Err(error) = self.refresh_tmdb_title(&mut tmdb).await {
                warn!(%error, "TMDB chapter title refresh failed");
            }
            tokio::time::sleep(tick.max(std::mem::take(&mut tmdb.cooldown))).await;
        }
    }

    async fn refresh_tmdb_title(&self, tmdb: &mut Tmdb<'_>) -> Result<()> {
        let Some(chapter) = self.repository.next_tmdb_chapter().await? else {
            return Ok(());
        };
        let result = self.lookup_tmdb_title(tmdb, &chapter).await;
        let retry = if result.is_ok() {
            self.config.sync_interval_secs
        } else {
            self.config.failure_retry_secs
        };
        let empty = ChapterTranslations::default();
        self.repository
            .remember_tmdb_titles(
                &chapter,
                result.as_ref().unwrap_or(&empty),
                Utc::now() + TimeDelta::seconds(retry as i64),
            )
            .await?;
        result.map(|_| ())
    }

    async fn lookup_tmdb_title(
        &self,
        tmdb: &mut Tmdb<'_>,
        chapter: &TmdbChapter,
    ) -> Result<ChapterTranslations> {
        let subject_id = chapter.key.subject_id;
        let first_number = chapter.key.bangumi_number - chapter.key.subject_index + 1;
        let cache_key = format!("tmdb:subject:{subject_id}:first:{first_number}");
        let binding: Option<Binding> =
            if let Some(cached) = self.repository.cached_provider_value(&cache_key).await? {
                serde_json::from_value(cached)
                    .map_err(|_| AppError::Schedule("invalid cached TMDB binding".into()))?
            } else {
                let subject = self.provider.bangumi_subject(subject_id).await?;
                if subject.id != subject_id {
                    return Err(AppError::Schedule("unexpected Bangumi subject ID".into()));
                }
                let binding = tmdb.resolve(&subject, first_number).await?;
                let ttl = if binding.is_some() {
                    7 * 86400
                } else {
                    self.config.sync_interval_secs as i64
                };
                self.repository
                    .cache_provider_value(&cache_key, &serde_json::json!(binding), ttl)
                    .await?;
                binding
            };
        let Some(binding) = binding else {
            return Ok(ChapterTranslations::default());
        };
        let Some(episode) = binding
            .first_episode
            .checked_add(chapter.key.subject_index - 1)
        else {
            return Ok(ChapterTranslations::default());
        };
        let path = format!(
            "/tv/{}/season/{}/episode/{episode}/translations",
            binding.series_id, binding.season
        );
        let Some(localized) = tmdb.get::<Translations>(&path, &[]).await? else {
            return Ok(ChapterTranslations::default());
        };
        let titles = localized.titles(&binding.original_language);
        if let (Some(bangumi), Some(original)) = (&chapter.name, &titles.original)
            && compact(bangumi) != compact(original)
        {
            // Keep the upstream original if the two catalogs disagree about this episode.
            return Ok(ChapterTranslations::default());
        }
        Ok(titles)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::ScheduleConfig,
        domain::{AutoScheduleMetadata, EpisodeNumberMapping, NewAnime},
        repository::{ChapterKey, Repository},
    };
    use axum::{
        Json, Router,
        http::{HeaderMap, Uri},
    };
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn a_cour_start_must_identify_one_episode_or_a_batch_premiere() {
        let premiere = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
        let episode = |episode_number, days| SeasonEpisode {
            episode_number,
            air_date: Some(premiere + TimeDelta::days(days)),
        };
        assert_eq!(
            first_episode(&[episode(12, -1), episode(13, 1)], premiere),
            None
        );
        assert_eq!(
            first_episode(&[episode(2, 0), episode(1, 0)], premiere),
            Some(1)
        );
        assert_eq!(
            first_episode(&[episode(13, 0), episode(12, 0)], premiere),
            None
        );
        assert_eq!(
            first_episode(&[episode(12, 0), episode(13, 7)], premiere),
            Some(12)
        );
    }

    #[test]
    fn localized_names_skip_placeholders_and_empty_translations() {
        let rows: Translations = serde_json::from_value(json!({"translations":[
            {"iso_639_1":"zh","iso_3166_1":"HK","data":{"name":"港譯"}},
            {"iso_639_1":"zh","iso_3166_1":"TW","data":{"name":"邁向目標"}},
            {"iso_639_1":"zh","iso_3166_1":"CN","data":{"name":"第 ８ 集"}},
            {"iso_639_1":"ja","iso_3166_1":"JP","data":{"name":"目指す先"}}
        ]}))
        .unwrap();
        let titles = rows.titles("ja");
        assert!(titles.simplified.is_none());
        assert_eq!(titles.traditional.as_deref(), Some("邁向目標"));
        assert_eq!(titles.original.as_deref(), Some("目指す先"));
    }

    #[tokio::test]
    async fn merged_season_requires_matching_premiere_and_cumulative_number() {
        let app = Router::new().fallback(|uri: Uri| async move {
            Json(match uri.path() {
                "/search/tv" => json!({"results":[{"id":12,"name":"旅路","original_name":"旅路","original_language":"ja"}]}),
                "/tv/12" => json!({"seasons":[{"season_number":1,"air_date":"2025-10-04"}]}),
                "/tv/12/season/1" => json!({"episodes":[{"episode_number":1,"air_date":"2025-10-04"},{"episode_number":13,"air_date":"2026-10-03"}]}),
                path => panic!("unexpected path: {path}"),
            })
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut tmdb = Tmdb {
            client: &Client::new(),
            base: &base,
            token: "fixture".into(),
            cooldown: Duration::ZERO,
        };
        let mut subject: BangumiSubject =
            serde_json::from_value(json!({"id":100,"name":"旅路 第2期","date":"2026-10-03"}))
                .unwrap();
        assert!(tmdb.resolve(&subject, 1).await.unwrap().is_none());
        let binding = tmdb.resolve(&subject, 13).await.unwrap().unwrap();
        assert_eq!((binding.season, binding.first_episode), (1, 13));
        subject.date = Some("2026-10-10".into());
        assert!(tmdb.resolve(&subject, 13).await.unwrap().is_none());
        task.abort();
    }

    #[tokio::test]
    async fn mapped_episode_backfills_then_yields_to_bangumi_without_another_request() {
        let calls = Arc::new(AtomicUsize::new(0));
        let requests = calls.clone();
        let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap| {
            let requests = requests.clone();
            async move {
                requests.fetch_add(1, Ordering::Relaxed);
                let auth = headers.get("authorization");
                if uri.path().starts_with("/tmdb/") {
                    assert_eq!(auth.unwrap(), "Bearer fixture");
                } else {
                    assert!(auth.is_none());
                }
                Json(match uri.path() {
                    "/v0/subjects/100" => json!({"id":100,"name":"旅路 第4期 第2部分","date":"2026-08-12"}),
                    "/tmdb/search/tv" => json!({"results":[{"id":12,"name":"旅路","original_name":"旅路","original_language":"ja"}]}),
                    "/tmdb/tv/12" => json!({"seasons":[{"season_number":4,"air_date":"2026-04-08"},{"season_number":0,"air_date":"2026-08-12"}]}),
                    "/tmdb/tv/12/season/4" => json!({"episodes":[{"episode_number":1,"air_date":"2026-04-08"},{"episode_number":12,"air_date":"2026-08-12"}]}),
                    "/tmdb/tv/12/season/4/episode/19/translations" => json!({"translations":[
                        {"iso_639_1":"ja","iso_3166_1":"JP","data":{"name":"帰路"}},
                        {"iso_639_1":"zh","iso_3166_1":"TW","data":{"name":"歸途"}},
                        {"iso_639_1":"zh","iso_3166_1":"CN","data":{"name":"归途"}}]}),
                    path => panic!("unexpected lookup: {path}"),
                })
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let repo = Repository::connect(":memory:").await.unwrap();
        let until = Utc::now() + TimeDelta::days(7);
        repo.add_anime(NewAnime {
            title: "旅路".into(),
            aliases: vec![],
            next_episode: 19,
            expected_at: Some(Utc::now()),
            expected_weekday: None,
            expected_time: None,
            timezone: "Asia/Shanghai".into(),
            duration_min_sec: 1200,
            duration_max_sec: 2400,
            auto_schedule: Some(AutoScheduleMetadata {
                bangumi_subject_id: 100,
                anilist_media_id: None,
                anime_schedule_route: None,
                total_episodes: Some(8),
                broadcast_pattern: "R/2026-08-12T12:00:00Z/P7D".into(),
                schedule_source: "danime".into(),
                schedule_confidence: "estimated".into(),
                schedule_warning: None,
                next_sync_at: until,
                episode_mapping: Some(EpisodeNumberMapping {
                    local_origin: 12,
                    bangumi_origin: 78,
                }),
            }),
        })
        .await
        .unwrap();
        let key = ChapterKey {
            subject_id: 100,
            subject_index: 8,
            bangumi_number: 85,
        };
        repo.remember_chapter_title(key, None, Some("帰路"), until)
            .await
            .unwrap();
        let sync = ScheduleSynchronizer::new(
            repo.clone(),
            ScheduleConfig {
                bangumi_api_base_url: format!("http://{addr}"),
                ..ScheduleConfig::default()
            },
        )
        .unwrap();
        let base = format!("http://{addr}/tmdb");
        let mut tmdb = Tmdb {
            base: &base,
            client: &sync.provider.client,
            token: "fixture".into(),
            cooldown: Duration::ZERO,
        };
        sync.refresh_tmdb_title(&mut tmdb).await.unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 5);
        assert_eq!(
            repo.upcoming_releases(until).await.unwrap()[0]
                .chapter_title
                .as_deref(),
            Some("归途")
        );

        repo.remember_chapter_title(key, None, Some("別の章"), until)
            .await
            .unwrap();
        sync.refresh_tmdb_title(&mut tmdb).await.unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 6);
        assert_eq!(
            repo.upcoming_releases(until).await.unwrap()[0]
                .chapter_title
                .as_deref(),
            Some("別の章")
        );
        assert!(repo.next_tmdb_chapter().await.unwrap().is_none());

        repo.remember_chapter_title(key, Some("Bangumi 译名"), Some("帰路"), until)
            .await
            .unwrap();
        sync.refresh_tmdb_title(&mut tmdb).await.unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 6);
        assert_eq!(
            repo.upcoming_releases(until).await.unwrap()[0]
                .chapter_title
                .as_deref(),
            Some("Bangumi 译名")
        );
        task.abort();
    }
}
