use super::*;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, Uri},
    response::{IntoResponse, Response},
};
use std::{
    collections::HashMap,
    sync::{
        Mutex as StdMutex,
        atomic::{AtomicU16, AtomicUsize, Ordering},
    },
};

const BVID: &str = "BV1Fenk6kE9V";

#[derive(Clone)]
struct Seen {
    path: String,
    query: HashMap<String, String>,
    cookie: String,
}

#[derive(Default)]
struct MockState {
    requests: StdMutex<Vec<Seen>>,
    detail_status: AtomicU16,
    search_status: AtomicU16,
    followers_status: AtomicU16,
    detail_mode: AtomicUsize,
}

struct Mock {
    base: String,
    state: Arc<MockState>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Mock {
    async fn start() -> Self {
        let state = Arc::new(MockState::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().fallback(handle).with_state(state.clone());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { base, state, task }
    }

    fn requests(&self) -> Vec<Seen> {
        self.state.requests.lock().unwrap().clone()
    }

    fn config(&self) -> BilibiliConfig {
        BilibiliConfig {
            base_url: self.base.clone(),
            min_request_interval_secs: 0,
            request_timeout_secs: 2,
            ..Default::default()
        }
    }
}

async fn handle(State(state): State<Arc<MockState>>, uri: Uri, headers: HeaderMap) -> Response {
    let query: HashMap<String, String> =
        url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
            .into_owned()
            .collect();
    let cookie = headers
        .get("cookie")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    state.requests.lock().unwrap().push(Seen {
        path: uri.path().into(),
        query: query.clone(),
        cookie,
    });
    let (status, value) = match uri.path() {
        "/x/frontend/finger/spi" => (
            0,
            serde_json::json!({"code": 0, "data": {"b_3": "public-device-3", "b_4": "public-device-4"}}),
        ),
        "/x/web-interface/nav" => (
            0,
            serde_json::json!({"code": -101, "data": {"wbi_img": {
                "img_url": "https://i.example/0123456789abcdef0123456789abcdef.png",
                "sub_url": "https://i.example/fedcba9876543210fedcba9876543210.png"
            }}}),
        ),
        "/x/web-interface/wbi/view/detail" => {
            assert!(query.contains_key("w_rid") && query.contains_key("wts"));
            let value = match state.detail_mode.load(Ordering::SeqCst) {
                1 => serde_json::json!({"code": -352, "message": "risk"}),
                2 => serde_json::json!({"code": 0, "data": {"v_voucher": "verification"}}),
                3 => serde_json::json!({"code": 0, "data": {"View": {"bvid": "BV1Vbao6aEHb"}}}),
                4 => serde_json::json!({"code": 0, "data": null}),
                _ => serde_json::json!({"code": 0, "data": {"View": {
                    "bvid": BVID, "title": "测试动画 第04集", "desc": "metadata only", "duration": 1421,
                    "pubdate": 1792656000_i64, "tname": "动画", "owner": {"mid": 123, "name": "测试UP"},
                    "stat": {"view": 1000, "reply": 20},
                    "pages": [{"page": 1, "cid": 88, "part": "第04集", "duration": 1421}]
                }}}),
            };
            (state.detail_status.load(Ordering::SeqCst), value)
        }
        "/x/relation/stat" => (
            state.followers_status.load(Ordering::SeqCst),
            serde_json::json!({"code": 0, "data": {"follower": 1000}}),
        ),
        "/x/web-interface/search/all/v2" => (
            state.search_status.load(Ordering::SeqCst),
            serde_json::json!({"code": 0, "data": {"numResults": 1, "result": [{"result_type": "video", "data": [{
                "bvid": BVID, "title": "测试动画 第04集", "duration": "23:41", "mid": 123, "author": "测试UP", "pubdate": 1792656000_i64
            }]}]}}),
        ),
        _ => panic!("unexpected API path: {}", uri.path()),
    };
    if status != 0 {
        (
            axum::http::StatusCode::from_u16(status).unwrap(),
            "<html>risk control</html>",
        )
            .into_response()
    } else {
        Json(value).into_response()
    }
}

#[test]
fn signing_has_stable_order_timestamp_and_sanitized_values() {
    let params = sign_params(
        vec![
            ("foo".into(), "1!1'4".into()),
            ("baz".into(), "1919810".into()),
            ("bar".into(), "514".into()),
        ],
        "ea1db124af3c7062474693fa704f4ff8",
        1702204169,
    );
    assert_eq!(
        params
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>(),
        ["bar", "baz", "foo", "wts", "w_rid"]
    );
    assert_eq!(params.last().unwrap().1, "6149fdadf571698ca7e6a567265cd0ee");
}

#[tokio::test]
async fn signed_detail_and_followers_are_cached_across_new_instances() {
    let mock = Mock::start().await;
    let repository = Repository::connect(":memory:").await.unwrap();
    let first = BilibiliProvider::new(mock.config(), repository.clone()).unwrap();
    let collection = first.collection(BVID).await.unwrap();
    assert_eq!(collection.video.duration_sec, 1421);
    assert_eq!(collection.parts[0].cid, 88);
    assert_eq!(collection.video.uploader_follower_count, Some(1000));
    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[0].path, "/x/frontend/finger/spi");
    assert_eq!(requests[1].path, "/x/web-interface/nav");
    assert_eq!(requests[2].path, "/x/web-interface/wbi/view/detail");
    assert!(requests[2].cookie.contains("buvid3=public-device-3"));
    assert!(requests[2].cookie.contains("buvid4=public-device-4"));
    assert_eq!(requests[2].query.get("bvid").unwrap(), BVID);
    assert_eq!(requests[2].query.get("platform").unwrap(), "web");
    let second = BilibiliProvider::new(mock.config(), repository).unwrap();
    let detailed = second.enrich(&collection.video).await.unwrap();
    assert!(detailed.enriched);
    assert_eq!(mock.requests().len(), 4);
}

#[tokio::test]
async fn detail_412_stops_retries_without_disabling_search() {
    let mock = Mock::start().await;
    mock.state.detail_status.store(412, Ordering::SeqCst);
    let repository = Repository::connect(":memory:").await.unwrap();
    let provider = BilibiliProvider::new(mock.config(), repository.clone()).unwrap();
    assert!(matches!(
        provider.collection(BVID).await,
        Err(ProviderError::RiskControl(_))
    ));
    let until = repository.provider_backoffs().await.unwrap()[0].backoff_until;
    let another = BilibiliProvider::new(mock.config(), repository.clone()).unwrap();
    assert!(matches!(
        another.collection(BVID).await,
        Err(ProviderError::ScopedBackoff { .. })
    ));
    assert_eq!(mock.requests().len(), 3);
    assert_eq!(
        another
            .search(&SearchQuery {
                keyword: "测试动画 04".into()
            })
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        repository.provider_backoffs().await.unwrap()[0].backoff_until,
        until
    );
    assert_eq!(mock.requests().len(), 4);
}

#[tokio::test]
async fn search_cooldown_does_not_disable_detail() {
    let mock = Mock::start().await;
    mock.state.search_status.store(412, Ordering::SeqCst);
    let repository = Repository::connect(":memory:").await.unwrap();
    let provider = BilibiliProvider::new(mock.config(), repository.clone()).unwrap();
    assert!(matches!(
        provider
            .search(&SearchQuery {
                keyword: "测试".into()
            })
            .await,
        Err(ProviderError::RiskControl(_))
    ));
    assert!(provider.collection(BVID).await.unwrap().video.enriched);
    assert_eq!(
        repository.provider_backoffs().await.unwrap()[0].provider,
        ProviderScope::Search.key()
    );
}

#[tokio::test]
async fn follower_failure_keeps_metadata_but_no_invented_follower_count() {
    let mock = Mock::start().await;
    mock.state.followers_status.store(412, Ordering::SeqCst);
    let repository = Repository::connect(":memory:").await.unwrap();
    let provider = BilibiliProvider::new(mock.config(), repository.clone()).unwrap();
    let collection = provider.collection(BVID).await.unwrap();
    assert!(collection.video.enriched);
    assert_eq!(collection.video.uploader_follower_count, None);
    assert_eq!(
        repository.provider_backoffs().await.unwrap()[0].provider,
        ProviderScope::Followers.key()
    );
    provider.enrich(&collection.video).await.unwrap();
    assert_eq!(mock.requests().len(), 4);
}

#[tokio::test]
async fn vouchers_and_json_risk_codes_are_not_cached_as_success() {
    for mode in [1, 2] {
        let mock = Mock::start().await;
        mock.state.detail_mode.store(mode, Ordering::SeqCst);
        let repository = Repository::connect(":memory:").await.unwrap();
        let provider = BilibiliProvider::new(mock.config(), repository.clone()).unwrap();
        assert!(matches!(
            provider.collection(BVID).await,
            Err(ProviderError::RiskControl(_))
        ));
        assert!(
            repository
                .cached_provider_value(&provider.cache_key(&format!("view:{BVID}")))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            repository.provider_backoffs().await.unwrap()[0].provider,
            ProviderScope::Detail.key()
        );
    }
}

#[tokio::test]
async fn missing_or_mismatched_payload_is_never_cached_or_accepted() {
    for mode in [3, 4] {
        let mock = Mock::start().await;
        mock.state.detail_mode.store(mode, Ordering::SeqCst);
        let repository = Repository::connect(":memory:").await.unwrap();
        let provider = BilibiliProvider::new(mock.config(), repository.clone()).unwrap();
        assert!(matches!(
            provider.collection(BVID).await,
            Err(ProviderError::InvalidResponse(_))
        ));
        assert!(
            repository
                .cached_provider_value(&provider.cache_key(&format!("view:{BVID}")))
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn existing_global_cooldown_is_preserved_without_network_calls() {
    let mock = Mock::start().await;
    let repository = Repository::connect(":memory:").await.unwrap();
    let until = repository
        .record_scoped_provider_failure(ProviderScope::Global, 3600, 3600)
        .await
        .unwrap();
    let provider = BilibiliProvider::new(mock.config(), repository.clone()).unwrap();
    assert!(matches!(
        provider.collection(BVID).await,
        Err(ProviderError::ScopedBackoff { .. })
    ));
    assert!(matches!(
        provider
            .search(&SearchQuery {
                keyword: "test".into()
            })
            .await,
        Err(ProviderError::ScopedBackoff { .. })
    ));
    assert!(mock.requests().is_empty());
    assert_eq!(
        repository.provider_backoffs().await.unwrap()[0].backoff_until,
        until
    );
}
