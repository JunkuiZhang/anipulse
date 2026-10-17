use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

use super::*;
use crate::domain::{AutoScheduleMetadata, NewAnime};

const ROUTE: &str = "tensei-shita-daiseijo-wa-seijo-de-aru-koto-wo-hitakakusu";
const TITLE: &str = "転生した大聖女は、聖女であることをひた隠す";

fn time(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .unwrap()
        .with_timezone(&Utc)
}

struct Fixture {
    provider: ScheduleProvider,
    catalog: ScheduleCatalog,
    requests: Arc<Mutex<Vec<url::Url>>>,
    episode_outage: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Fixture {
    async fn new(entries: Vec<Value>, episode_override: Option<Value>, outage: bool) -> Self {
        Self::with_outage_week(entries, episode_override, outage, None).await
    }

    async fn with_outage_week(
        entries: Vec<Value>,
        episode_override: Option<Value>,
        outage: bool,
        outage_week: Option<u32>,
    ) -> Self {
        Self::with_faults(entries, episode_override, outage, outage_week, false).await
    }

    async fn with_faults(
        entries: Vec<Value>,
        episode_override: Option<Value>,
        outage: bool,
        outage_week: Option<u32>,
        chapter_outage: bool,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let episode_outage = Arc::new(AtomicBool::new(chapter_outage));
        let chapter_failure = episode_outage.clone();
        let media = json!({"title": TITLE, "route": ROUTE, "premier": "2026-10-03T13:00:00Z",
            "episodes":13, "status":"Ongoing", "episodeOverride":episode_override,
            "names":{"native":TITLE}, "mediaTypes":[{"route":"tv"}]});
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut buffer = [0; 2048];
                    let count = stream.read(&mut buffer).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                    if bytes.windows(4).any(|value| value == b"\r\n\r\n") {
                        break;
                    }
                    assert!(bytes.len() < 16_384);
                }
                let headers = String::from_utf8(bytes).unwrap();
                let path = headers.split_whitespace().nth(1).unwrap();
                let url = url::Url::parse(&format!("http://{address}{path}")).unwrap();
                recorded.lock().await.push(url.clone());
                let parameters = url.query_pairs().collect::<HashMap<_, _>>();
                let mut status = "200 OK";
                let body = match url.path() {
                    "/data.json" => json!({"items":[{"title":TITLE, "type":"tv", "begin":"2026-10-03T13:00:00Z",
                        "sites":[{"site":"bangumi","id":"542007"},
                            {"site":"gamer","begin":"2026-10-03T13:00:00Z"},
                            {"site":"danime","begin":"2026-10-03T13:30:00Z"},
                            {"site":"abema","id":"149-36","begin":"","broadcast":""}]}]}),
                    "/v0/subjects/542007" => json!({"id":542007,"name":TITLE,"date":"2026-10-03","platform":"TV","total_episodes":13}),
                    "/v0/episodes" => {
                        if chapter_failure.load(Ordering::Relaxed) {
                            status = "504 Gateway Timeout";
                            json!({"error":"upstream timeout"})
                        } else {
                            let offset: i64 = parameters["offset"].parse().unwrap();
                            let date = NaiveDate::from_ymd_opt(2026, 10, 3).unwrap() + Duration::days(offset * 7);
                            json!({"total":13,"data":[{"airdate":date.to_string(),"sort":78+offset,"ep":1+offset}]})
                        }
                    }
                    "/anime-schedule/anime" => json!({"anime":[media.clone()]}),
                    path if path == format!("/anime-schedule/anime/{ROUTE}") => media.clone(),
                    "/anime-schedule/timetables/all" => {
                        assert_eq!(parameters["tz"], "UTC");
                        if outage || outage_week.is_some_and(|week| parameters["week"].parse::<u32>().unwrap() == week) { status = "503 Service Unavailable"; json!({"error":"unavailable"}) }
                        else {
                            let year: i32 = parameters["year"].parse().unwrap();
                            let week: u32 = parameters["week"].parse().unwrap();
                            json!(entries.iter().filter(|entry| {
                                let date = time(entry["episodeDate"].as_str().unwrap()).iso_week();
                                date.year() == year && date.week() == week
                            }).collect::<Vec<_>>())
                        }
                    }
                    path => panic!("unexpected fixture path {path}"),
                }.to_string();
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let provider = ScheduleProvider::new(ScheduleConfig {
            bangumi_data_url: format!("http://{address}/data.json"),
            bangumi_api_base_url: format!("http://{address}"),
            anime_schedule_api_url: format!("http://{address}/anime-schedule"),
            request_timeout_secs: 5,
            ..ScheduleConfig::default()
        })
        .unwrap();
        let catalog = provider.load_catalog().await.unwrap();
        Self {
            provider,
            catalog,
            requests,
            episode_outage,
            task,
        }
    }

    async fn resolve(
        &self,
        local_episode: i64,
        mapping: Option<EpisodeNumberMapping>,
    ) -> ResolvedSchedule {
        self.provider
            .resolve_auto(
                &self.catalog,
                AutoScheduleRequest {
                    title: TITLE,
                    subject_id: Some(542_007),
                    next_episode: local_episode,
                    episode_mapping: mapping,
                    anilist_media_id: None,
                    anime_schedule_route: None,
                    timezone: "Asia/Shanghai",
                },
            )
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn exact_episode_overrides_catalog_clock_and_searches_previous_weeks_without_binding() {
    let fixture = Fixture::new(vec![
        json!({"route":ROUTE,"episodeNumber":3,"episodeDate":"2026-10-03T13:00:00Z","airType":"dub"}),
        json!({"route":ROUTE,"episodeNumber":3,"episodeDate":"2026-10-10T13:00:00Z","airType":"raw"}),
        json!({"route":ROUTE,"episodeNumber":3,"episodeDate":"2026-10-17T13:30:00Z","airType":"sub"}),
        json!({"route":ROUTE,"episodeNumber":4,"episodeDate":"2026-10-17T13:00:00Z","airType":"raw"}),
    ], None, false).await;
    let resolved = fixture.resolve(3, None).await;
    assert_eq!(resolved.schedule_source, "anime_schedule");
    assert_eq!(resolved.schedule_confidence, "calibrated");
    assert_eq!(resolved.expected_at, Some(time("2026-10-10T13:00:00Z")));
    assert_eq!(resolved.anime_schedule_route.as_deref(), Some(ROUTE));
    assert!(fixture.requests.lock().await.iter().any(|url| {
        url.query_pairs()
            .any(|(key, value)| key == "week" && value == "41")
    }));
    let before = fixture
        .requests
        .lock()
        .await
        .iter()
        .filter(|url| url.path().contains("timetables"))
        .count();
    fixture.resolve(3, None).await;
    let after = fixture
        .requests
        .lock()
        .await
        .iter()
        .filter(|url| url.path().contains("timetables"))
        .count();
    assert_eq!(
        before, after,
        "global timetables must be cached across episode checks"
    );
}

#[tokio::test]
async fn chapter_timeout_does_not_block_bound_or_discovered_exact_episode_five() {
    let fixture = Fixture::with_faults(
        vec![json!({"route":ROUTE,"episodeNumber":5,"episodeDate":"2026-10-24T13:00:00Z","airType":"raw"})],
        None, false, None, true,
    ).await;
    for binding in [None, Some(ROUTE)] {
        let resolved = fixture
            .provider
            .resolve_auto(
                &fixture.catalog,
                AutoScheduleRequest {
                    title: TITLE,
                    subject_id: Some(542_007),
                    next_episode: 5,
                    episode_mapping: None,
                    anilist_media_id: None,
                    anime_schedule_route: binding,
                    timezone: "Asia/Shanghai",
                },
            )
            .await
            .unwrap();
        assert_eq!(resolved.expected_at, Some(time("2026-10-24T13:00:00Z")));
        assert_eq!(resolved.schedule_source, "anime_schedule");
        assert_eq!(resolved.schedule_confidence, "calibrated");
        assert_eq!(resolved.total_episodes, Some(13));
        assert!(resolved.source_health_error.is_none());
        assert!(resolved.bangumi_health_error.unwrap().contains("504"));
        let warning = resolved.schedule_warning.unwrap();
        assert!(warning.contains("已确认本篇第 5 集"));
        assert!(warning.contains("Bangumi 章节日期请求失败"));
    }
    assert!(fixture.requests.lock().await.iter().any(|url| {
        url.path().contains("timetables")
            && url
                .query_pairs()
                .any(|(key, value)| key == "week" && value == "43")
    }));
}

#[tokio::test]
async fn missing_catalog_entry_can_use_exact_timetable_during_a_chapter_outage() {
    let mut fixture = Fixture::with_faults(
        vec![json!({"route":ROUTE,"episodeNumber":5,"episodeDate":"2026-10-24T13:00:00Z"})],
        None,
        false,
        None,
        true,
    )
    .await;
    fixture.catalog.items.clear();
    let resolved = fixture.resolve(5, None).await;
    assert_eq!(resolved.expected_at, Some(time("2026-10-24T13:00:00Z")));
    assert_eq!(resolved.schedule_confidence, "calibrated");
    assert!(resolved.bangumi_health_error.is_some());
}

#[tokio::test]
async fn simultaneous_chapter_and_timetable_outages_remain_estimated() {
    let fixture = Fixture::with_faults(vec![], None, true, None, true).await;
    let resolved = fixture.resolve(5, None).await;
    assert_eq!(resolved.schedule_source, "gamer");
    assert_eq!(resolved.schedule_confidence, "estimated");
    assert!(resolved.bangumi_health_error.unwrap().contains("504"));
    assert!(resolved.source_health_error.unwrap().contains("503"));
    assert!(
        fixture
            .requests
            .lock()
            .await
            .iter()
            .any(|url| { url.path().contains("timetables") })
    );
}

#[tokio::test]
async fn chapter_failure_health_and_retry_are_independent_of_a_usable_timetable() {
    let fixture = Fixture::with_faults(
        vec![json!({"route":ROUTE,"episodeNumber":5,"episodeDate":"2026-10-24T13:00:00Z"})],
        None,
        false,
        None,
        true,
    )
    .await;
    let repository = Repository::connect(":memory:").await.unwrap();
    let id = repository
        .add_anime(NewAnime {
            title: TITLE.into(),
            aliases: vec![],
            next_episode: 5,
            expected_at: Some(time("2026-10-31T13:00:00Z")),
            expected_weekday: Some(5),
            expected_time: Some("21:00".into()),
            timezone: "Asia/Shanghai".into(),
            duration_min_sec: 1200,
            duration_max_sec: 2400,
            auto_schedule: Some(AutoScheduleMetadata {
                bangumi_subject_id: 542_007,
                anilist_media_id: None,
                anime_schedule_route: Some(ROUTE.into()),
                total_episodes: Some(13),
                broadcast_pattern: "R/2026-10-03T13:00:00Z/P7D".into(),
                schedule_source: "gamer".into(),
                schedule_confidence: "estimated".into(),
                schedule_warning: None,
                next_sync_at: Utc::now(),
                episode_mapping: None,
            }),
        })
        .await
        .unwrap();
    let synchronizer =
        ScheduleSynchronizer::new(repository.clone(), fixture.provider.config.clone()).unwrap();
    let before = Utc::now();
    for _ in 0..2 {
        synchronizer.sync_now(id).await.unwrap();
        assert_eq!(
            repository.active_episode(id).await.unwrap().expected_at,
            Some(time("2026-10-24T13:00:00Z"))
        );
        let anime = repository.get_anime(id).await.unwrap().anime;
        assert_eq!(anime.schedule_source.as_deref(), Some("anime_schedule"));
        assert_eq!(anime.schedule_confidence.as_deref(), Some("calibrated"));
        assert!(
            anime.schedule_next_sync_at.unwrap()
                <= before
                    + Duration::seconds(fixture.provider.config.failure_retry_secs as i64 + 5)
        );
    }
    let failures = repository.pending_source_alerts().await.unwrap();
    assert!(
        failures
            .iter()
            .any(|failure| failure.source == "bangumi-schedule:542007")
    );
    assert!(
        !failures
            .iter()
            .any(|failure| failure.source == "anime-schedule:542007")
    );

    fixture.episode_outage.store(false, Ordering::Relaxed);
    synchronizer.sync_now(id).await.unwrap();
    let recovered = repository.get_anime(id).await.unwrap().anime;
    assert!(
        !recovered
            .schedule_warning
            .unwrap()
            .contains("Bangumi 章节日期请求失败")
    );
    assert!(
        recovered.schedule_next_sync_at.unwrap()
            >= before + Duration::seconds(fixture.provider.config.sync_interval_secs as i64)
    );
    assert!(repository.pending_source_alerts().await.unwrap().is_empty());
}

#[tokio::test]
async fn exact_episode_uses_subject_index_not_local_or_absolute_bangumi_number() {
    let fixture = Fixture::new(
        vec![json!({"route":ROUTE,"episodeNumber":3,"episodeDate":"2026-10-10T13:00:00Z"})],
        None,
        false,
    )
    .await;
    let resolved = fixture
        .resolve(
            14,
            Some(EpisodeNumberMapping {
                local_origin: 12,
                bangumi_origin: 78,
            }),
        )
        .await;
    assert_eq!(resolved.expected_at, Some(time("2026-10-10T13:00:00Z")));
    assert_eq!(resolved.schedule_confidence, "calibrated");
    assert!(fixture.requests.lock().await.iter().any(|url| {
        url.query_pairs()
            .any(|(key, value)| key == "offset" && value == "2")
    }));
}

#[tokio::test]
async fn a_partial_week_outage_does_not_discard_a_verified_episode_from_another_week() {
    let fixture = Fixture::with_outage_week(
        vec![json!({"route":ROUTE,"episodeNumber":3,"episodeDate":"2026-10-10T13:00:00Z"})],
        None,
        false,
        Some(42),
    )
    .await;
    let resolved = fixture.resolve(3, None).await;
    assert_eq!(resolved.expected_at, Some(time("2026-10-10T13:00:00Z")));
    assert_eq!(resolved.schedule_confidence, "calibrated");
    assert!(resolved.source_health_error.is_some());
    assert!(
        resolved
            .schedule_warning
            .unwrap()
            .contains("部分 AnimeSchedule 周排期查询失败")
    );
}

#[tokio::test]
async fn timetable_failures_are_cached_across_provider_clones() {
    let fixture = Fixture::new(vec![], None, true).await;
    let expected = time("2026-10-17T13:00:00Z");
    assert!(
        fixture
            .provider
            .anime_schedule_timetable(expected, ROUTE, 3)
            .await
            .is_err()
    );
    assert!(
        fixture
            .provider
            .clone()
            .anime_schedule_timetable(expected, ROUTE, 3)
            .await
            .is_err()
    );
    assert_eq!(
        fixture
            .requests
            .lock()
            .await
            .iter()
            .filter(|url| url.path().contains("timetables"))
            .count(),
        1
    );
}

#[tokio::test]
async fn premiere_batch_is_an_exact_range_and_carries_progress_forward_as_an_estimate() {
    let fixture = Fixture::new(
        vec![],
        Some(json!({"overrideEpisode":2,"episodesAired":1,"overrideDate":"2026-10-03T13:00:00Z"})),
        false,
    )
    .await;
    for episode in [1, 2] {
        let resolved = fixture.resolve(episode, None).await;
        assert_eq!(resolved.expected_at, Some(time("2026-10-03T13:00:00Z")));
        assert_eq!(resolved.schedule_confidence, "calibrated");
    }
    let third = fixture.resolve(3, None).await;
    assert_eq!(third.expected_at, Some(time("2026-10-10T13:00:00Z")));
    assert_eq!(third.schedule_confidence, "estimated");
    assert!(third.schedule_warning.unwrap().contains("连播/调档锚点"));
    let fourth = fixture.resolve(4, None).await;
    assert_eq!(fourth.expected_at, Some(time("2026-10-17T13:00:00Z")));
}

#[tokio::test]
async fn timetable_batch_range_applies_to_both_episodes() {
    let fixture = Fixture::new(vec![json!({"route":ROUTE,"episodeNumber":2,"subtractedEpisodeNumber":1,"episodeDate":"2026-10-03T13:00:00Z"})], None, false).await;
    for episode in [1, 2] {
        let resolved = fixture.resolve(episode, None).await;
        assert_eq!(resolved.expected_at, Some(time("2026-10-03T13:00:00Z")));
        assert_eq!(resolved.schedule_confidence, "calibrated");
    }
}

#[tokio::test]
async fn delay_without_a_new_timestamp_does_not_fabricate_a_release() {
    let fixture = Fixture::new(vec![json!({"route":ROUTE,"episodeNumber":3,"episodeDate":"2026-10-17T13:00:00Z","airingStatus":"delayed-air","delayedUntil":"0001-01-01T00:00:00Z"})], None, false).await;
    let resolved = fixture.resolve(3, None).await;
    assert_eq!(resolved.expected_at, None);
    assert_eq!(resolved.expected_time, None);
    assert_eq!(resolved.schedule_confidence, "unavailable");
    assert!(resolved.schedule_warning.unwrap().contains("延期"));
}

#[tokio::test]
async fn delay_uses_the_new_timestamp_instead_of_the_original_week() {
    let fixture = Fixture::new(vec![json!({"route":ROUTE,"episodeNumber":3,"episodeDate":"2026-10-17T13:00:00Z","status":"Delayed","delayedUntil":"2026-10-24T13:00:00Z"})], None, false).await;
    assert_eq!(
        fixture.resolve(3, None).await.expected_at,
        Some(time("2026-10-24T13:00:00Z"))
    );
}

#[tokio::test]
async fn repeated_outages_keep_the_same_episodes_last_verified_time() {
    let fixture = Fixture::new(vec![], None, true).await;
    let repository = Repository::connect(":memory:").await.unwrap();
    let expected = time("2026-10-10T13:00:00Z");
    let id = repository
        .add_anime(NewAnime {
            title: TITLE.into(),
            aliases: vec![],
            next_episode: 3,
            expected_at: Some(expected),
            expected_weekday: Some(5),
            expected_time: Some("21:00".into()),
            timezone: "Asia/Shanghai".into(),
            duration_min_sec: 1200,
            duration_max_sec: 2400,
            auto_schedule: Some(AutoScheduleMetadata {
                bangumi_subject_id: 542_007,
                anilist_media_id: None,
                anime_schedule_route: Some(ROUTE.into()),
                total_episodes: Some(13),
                broadcast_pattern: "R/2026-10-03T13:00:00Z/P7D".into(),
                schedule_source: "anime_schedule".into(),
                schedule_confidence: "calibrated".into(),
                schedule_warning: None,
                next_sync_at: Utc::now(),
                episode_mapping: None,
            }),
        })
        .await
        .unwrap();
    let synchronizer =
        ScheduleSynchronizer::new(repository.clone(), fixture.provider.config.clone()).unwrap();
    for _ in 0..2 {
        synchronizer.sync_now(id).await.unwrap();
        assert_eq!(
            repository.active_episode(id).await.unwrap().expected_at,
            Some(expected)
        );
        assert_eq!(
            repository
                .get_anime(id)
                .await
                .unwrap()
                .anime
                .schedule_confidence
                .as_deref(),
            Some("stale")
        );
    }
}

#[test]
fn week_search_handles_utc_iso_year_boundaries_and_stays_bounded() {
    let weeks = timetable_weeks(
        time("2027-01-02T13:00:00Z"),
        time("2026-12-31T23:00:00Z"),
        14,
    );
    assert!(weeks.contains(&(2026, 53)));
    assert!(weeks.contains(&(2027, 1)));
    assert_eq!(weeks.len(), 5);
    assert!(timetable_weeks(time("2027-01-02T13:00:00Z"), Utc::now(), i64::MAX).len() <= 12);
}
