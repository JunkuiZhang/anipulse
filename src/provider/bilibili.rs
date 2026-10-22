use std::{sync::Arc, time::Duration};

use crate::{
    config::BilibiliConfig,
    domain::VideoCandidate,
    provider::{ProviderError, ProviderResult, SearchQuery, VideoSearchProvider},
    repository::{ProviderPermit, ProviderScope, Repository},
};
use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{debug, warn};

const MIXIN_KEY_ENC_TAB: [usize; 64] = [
    46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19, 29,
    28, 14, 39, 12, 38, 41, 13, 37, 48, 7, 16, 24, 55, 40, 61, 26, 17, 0, 1, 60, 51, 30, 4, 22, 25,
    54, 21, 56, 59, 6, 63, 57, 62, 11, 36, 20, 34, 44, 52,
];

#[cfg(test)]
mod http_tests;

#[derive(Clone)]
pub struct BilibiliProvider {
    config: BilibiliConfig,
    client: Client,
    repository: Repository,
    request_lock: Arc<Mutex<()>>,
    public_cookie: Arc<Mutex<Option<String>>>,
}

impl BilibiliProvider {
    pub fn new(config: BilibiliConfig, repository: Repository) -> ProviderResult<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .user_agent(&config.user_agent)
            .referer(false)
            .build()
            .map_err(|e| ProviderError::Permanent(format!("cannot build HTTP client: {e}")))?;
        Ok(Self {
            config,
            client,
            repository,
            request_lock: Arc::new(Mutex::new(())),
            public_cookie: Arc::new(Mutex::new(None)),
        })
    }

    async fn uploader_follower_count(&self, mid: i64) -> Option<i64> {
        if mid <= 0 {
            return None;
        }
        let key = self.cache_key(&format!("followers:{mid}"));
        if let Ok(Some(value)) = self.repository.cached_provider_value(&key).await {
            return value.as_i64().filter(|count| *count >= 0);
        }
        let params = vec![("vmid".to_string(), mid.to_string())];
        let value = match self
            .request_value(ProviderScope::Followers, "/x/relation/stat", &params)
            .await
        {
            Ok(value) => value,
            Err(error) => {
                if matches!(error, ProviderError::ScopedBackoff { .. }) {
                    debug!(mid, %error, "uploader follower lookup paused");
                } else {
                    warn!(mid, %error, "uploader follower lookup failed");
                }
                return None;
            }
        };
        if let Err(error) = Self::ensure_success(&value, "uploader relation stat") {
            self.record_temporary_failure(ProviderScope::Followers)
                .await;
            warn!(mid, %error, "uploader follower response unusable");
            return None;
        }
        let followers = value
            .pointer("/data/follower")
            .and_then(value_as_i64)
            .filter(|count| *count >= 0);
        let Some(followers) = followers else {
            self.record_temporary_failure(ProviderScope::Followers)
                .await;
            return None;
        };
        self.succeeded(ProviderScope::Followers).await.ok()?;
        if let Err(error) = self
            .repository
            .cache_provider_value(&key, &Value::from(followers), 21_600)
            .await
        {
            warn!(%error, "cannot cache uploader followers");
        }
        Some(followers)
    }

    fn cache_key(&self, suffix: &str) -> String {
        format!("{}:{suffix}", self.config.base_url.trim_end_matches('/'))
    }

    async fn guard(&self, scope: ProviderScope) -> ProviderResult<()> {
        let backoffs = self
            .repository
            .provider_backoffs()
            .await
            .map_err(|error| ProviderError::Temporary(error.to_string()))?;
        if let Some(backoff) = backoffs.iter().find(|state| {
            state.provider == ProviderScope::Global.key() || state.provider == scope.key()
        }) {
            let gate = if backoff.provider == ProviderScope::Global.key() {
                ProviderScope::Global
            } else {
                scope
            };
            return Err(Self::paused(gate, backoff.backoff_until));
        }
        Ok(())
    }

    fn paused(scope: ProviderScope, until: chrono::DateTime<Utc>) -> ProviderError {
        ProviderError::ScopedBackoff {
            scope: scope.label(),
            until: until.with_timezone(&chrono_tz::Asia::Shanghai),
        }
    }

    async fn succeeded(&self, scope: ProviderScope) -> ProviderResult<()> {
        self.repository
            .clear_scoped_provider_failure(scope)
            .await
            .map_err(|error| ProviderError::Temporary(error.to_string()))
    }

    async fn request_value(
        &self,
        scope: ProviderScope,
        path: &str,
        params: &[(String, String)],
    ) -> ProviderResult<Value> {
        let _request_lock = self.request_lock.lock().await;
        loop {
            match self
                .repository
                .reserve_scoped_provider_request(
                    scope,
                    self.config.max_requests_per_day,
                    Duration::from_secs(self.config.min_request_interval_secs),
                    Duration::from_secs(self.config.request_timeout_secs.saturating_add(10)),
                )
                .await
                .map_err(|error| ProviderError::Temporary(error.to_string()))?
            {
                ProviderPermit::Ready => break,
                ProviderPermit::Wait(duration) => tokio::time::sleep(duration).await,
                ProviderPermit::Paused { scope, until } => return Err(Self::paused(scope, until)),
                ProviderPermit::BudgetExhausted => {
                    return Err(ProviderError::Backoff(format!(
                        "Bilibili daily safety budget exhausted ({})",
                        self.config.max_requests_per_day
                    )));
                }
            }
        }

        let url = format!("{}{}", self.config.base_url.trim_end_matches('/'), path);
        let mut request = self
            .client
            .get(url)
            .header("Referer", "https://www.bilibili.com/")
            .query(params);
        if let Some(cookie) = self.public_cookie.lock().await.clone() {
            request = request.header(reqwest::header::COOKIE, cookie);
        }
        let response = request.send().await;

        let response = match response {
            Ok(response) => response,
            Err(error) => {
                let until = self.record_temporary_failure(scope).await;
                return Err(ProviderError::Temporary(format!(
                    "{} {path}: {error}; backoff until {until}",
                    scope.label()
                )));
            }
        };
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            let until = self.record_failure(scope, 1_800, 14_400).await;
            return Err(ProviderError::RateLimited(format!(
                "{} {path}: HTTP 429; backoff until {until}",
                scope.label()
            )));
        }
        if status == StatusCode::PRECONDITION_FAILED {
            let until = self.record_failure(scope, 21_600, 86_400).await;
            return Err(ProviderError::RiskControl(format!(
                "{} {path}: HTTP 412; backoff until {until}",
                scope.label()
            )));
        }
        if status.is_server_error() {
            let until = self.record_temporary_failure(scope).await;
            return Err(ProviderError::Temporary(format!(
                "{} {path}: HTTP {status}; backoff until {until}",
                scope.label()
            )));
        }
        if !status.is_success() {
            return Err(ProviderError::Permanent(format!("HTTP {status}")));
        }

        let value: Value = match response.json().await {
            Ok(value) => value,
            Err(error) => {
                let until = self.record_temporary_failure(scope).await;
                return Err(ProviderError::InvalidResponse(format!(
                    "{} {path}: {error}; backoff until {until}",
                    scope.label()
                )));
            }
        };
        match value.get("code").and_then(Value::as_i64) {
            Some(-412 | -352) => {
                let until = self.record_failure(scope, 21_600, 86_400).await;
                Err(ProviderError::RiskControl(format!(
                    "{} {path}: Bilibili code {}; backoff until {until}",
                    scope.label(),
                    value["code"]
                )))
            }
            Some(-509 | -799) => {
                let until = self.record_failure(scope, 1_800, 14_400).await;
                Err(ProviderError::RateLimited(format!(
                    "{} {path}: Bilibili code {}; backoff until {until}",
                    scope.label(),
                    value["code"]
                )))
            }
            _ if value
                .pointer("/data/v_voucher")
                .and_then(Value::as_str)
                .is_some() =>
            {
                let until = self.record_failure(scope, 21_600, 86_400).await;
                Err(ProviderError::RiskControl(format!(
                    "{} {path}: verification voucher; backoff until {until}",
                    scope.label()
                )))
            }
            _ => Ok(value),
        }
    }

    async fn record_failure(&self, scope: ProviderScope, base: i64, maximum: i64) -> String {
        match self
            .repository
            .record_scoped_provider_failure(scope, base, maximum)
            .await
        {
            Ok(until) => until.to_rfc3339(),
            Err(error) => format!("unknown ({error})"),
        }
    }

    async fn record_temporary_failure(&self, scope: ProviderScope) -> String {
        self.record_failure(scope, 300, 3_600).await
    }

    async fn wbi_mixin_key(&self) -> ProviderResult<String> {
        let key = self.cache_key("wbi-key");
        if let Some(value) = self
            .repository
            .cached_provider_value(&key)
            .await
            .map_err(|error| ProviderError::Temporary(error.to_string()))?
            && let Some(mixin) = value
                .as_str()
                .filter(|value| value.len() == 32 && value.is_ascii())
        {
            return Ok(mixin.to_owned());
        }
        self.ensure_public_cookie().await?;
        let nav = self
            .request_value(ProviderScope::Session, "/x/web-interface/nav", &[])
            .await?;
        let image_url = nav
            .pointer("/data/wbi_img/img_url")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::InvalidResponse("nav missing wbi img_url".into()))?;
        let sub_url = nav
            .pointer("/data/wbi_img/sub_url")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::InvalidResponse("nav missing wbi sub_url".into()))?;
        let raw_key = format!("{}{}", file_stem(image_url)?, file_stem(sub_url)?);
        let mixin: String = MIXIN_KEY_ENC_TAB
            .iter()
            .filter_map(|index| raw_key.as_bytes().get(*index).copied())
            .take(32)
            .map(char::from)
            .collect();
        if mixin.len() != 32 {
            return Err(ProviderError::InvalidResponse(
                "cannot derive 32-byte WBI mixin key".into(),
            ));
        }
        self.succeeded(ProviderScope::Session).await?;
        self.repository
            .cache_provider_value(&key, &Value::String(mixin.clone()), 3_600)
            .await
            .map_err(|error| ProviderError::Temporary(error.to_string()))?;
        Ok(mixin)
    }

    async fn ensure_public_cookie(&self) -> ProviderResult<()> {
        let key = self.cache_key("public-device");
        let cached = self
            .repository
            .cached_provider_value(&key)
            .await
            .map_err(|error| ProviderError::Temporary(error.to_string()))?;
        let fetched = cached.is_none();
        let value = if let Some(value) = cached {
            value
        } else {
            self.request_value(ProviderScope::Device, "/x/frontend/finger/spi", &[])
                .await?
        };
        Self::ensure_success(&value, "public device cookie")?;
        let buvid3 = value
            .pointer("/data/b_3")
            .and_then(Value::as_str)
            .filter(|value| valid_cookie_value(value))
            .ok_or_else(|| ProviderError::InvalidResponse("spi missing valid b_3".into()))?;
        let buvid4 = value
            .pointer("/data/b_4")
            .and_then(Value::as_str)
            .filter(|value| valid_cookie_value(value))
            .ok_or_else(|| ProviderError::InvalidResponse("spi missing valid b_4".into()))?;
        *self.public_cookie.lock().await = Some(format!("buvid3={buvid3}; buvid4={buvid4}"));
        if fetched {
            self.succeeded(ProviderScope::Device).await?;
            self.repository
                .cache_provider_value(&key, &value, 21_600)
                .await
                .map_err(|error| ProviderError::Temporary(error.to_string()))?;
        }
        Ok(())
    }

    async fn signed_search_params(&self, keyword: &str) -> ProviderResult<Vec<(String, String)>> {
        self.signed_params(vec![
            ("keyword".to_string(), sanitize_wbi_value(keyword)),
            ("order".to_string(), "pubdate".to_string()),
            ("page".to_string(), "1".to_string()),
            ("page_size".to_string(), "20".to_string()),
            ("search_type".to_string(), "video".to_string()),
        ])
        .await
    }

    async fn signed_params(
        &self,
        params: Vec<(String, String)>,
    ) -> ProviderResult<Vec<(String, String)>> {
        let mixin = self.wbi_mixin_key().await?;
        Ok(sign_params(params, &mixin, Utc::now().timestamp()))
    }

    /// Both single-video imports and multipart imports use the same validated,
    /// signed metadata endpoint and persistent cache. Never retry the old view
    /// endpoint after an explicit risk-control response.
    async fn view_value(&self, bvid: &str) -> ProviderResult<Value> {
        let key = self.cache_key(&format!("view:{bvid}"));
        if let Some(value) = self
            .repository
            .cached_provider_value(&key)
            .await
            .map_err(|error| ProviderError::Temporary(error.to_string()))?
        {
            return Ok(value);
        }
        self.guard(ProviderScope::Detail).await?;
        self.ensure_public_cookie().await?;
        let params = self
            .signed_params(vec![
                ("bvid".into(), bvid.into()),
                ("platform".into(), "web".into()),
            ])
            .await?;
        let response = self
            .request_value(
                ProviderScope::Detail,
                "/x/web-interface/wbi/view/detail",
                &params,
            )
            .await?;
        if let Err(error) = Self::ensure_success(&response, "signed video detail") {
            if !matches!(error, ProviderError::Permanent(_)) {
                self.record_temporary_failure(ProviderScope::Detail).await;
            }
            return Err(error);
        }
        let data = response.pointer("/data/View").filter(|data| {
            data.get("bvid").and_then(Value::as_str) == Some(bvid)
                && data
                    .get("title")
                    .and_then(Value::as_str)
                    .is_some_and(|title| !title.trim().is_empty())
                && int_field(data, "duration").is_some_and(|duration| duration > 0)
                && data
                    .pointer("/owner/mid")
                    .and_then(value_as_i64)
                    .is_some_and(|mid| mid > 0)
                && data
                    .get("pages")
                    .and_then(Value::as_array)
                    .is_some_and(|pages| !pages.is_empty())
        });
        let Some(data) = data else {
            self.record_temporary_failure(ProviderScope::Detail).await;
            return Err(ProviderError::InvalidResponse(
                "signed video detail has missing or mismatched video metadata".into(),
            ));
        };
        let value = serde_json::json!({"code": 0, "data": data});
        self.succeeded(ProviderScope::Detail).await?;
        self.repository
            .cache_provider_value(&key, &value, 1_800)
            .await
            .map_err(|error| ProviderError::Temporary(error.to_string()))?;
        Ok(value)
    }

    fn ensure_success(value: &Value, context: &str) -> ProviderResult<()> {
        let code = value
            .get("code")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MIN);
        if code == 0 {
            return Ok(());
        }
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        if code == -101 {
            Err(ProviderError::Unauthorized(format!("{context}: {message}")))
        } else if matches!(code, -404 | 62002 | 62004) {
            Err(ProviderError::Permanent(format!(
                "{context}: code {code}: {message}"
            )))
        } else {
            Err(ProviderError::Temporary(format!(
                "{context}: code {code}: {message}"
            )))
        }
    }
}

#[async_trait]
impl VideoSearchProvider for BilibiliProvider {
    async fn search(&self, query: &SearchQuery) -> ProviderResult<Vec<VideoCandidate>> {
        let params = vec![
            ("keyword".to_string(), sanitize_wbi_value(&query.keyword)),
            ("order".to_string(), "pubdate".to_string()),
            ("page".to_string(), "1".to_string()),
        ];
        let value = self
            .request_value(
                ProviderScope::Search,
                "/x/web-interface/search/all/v2",
                &params,
            )
            .await?;
        Self::ensure_success(&value, "search")?;
        let results = match search_result_items(&value) {
            Ok(results) => results,
            Err(ProviderError::RiskControl(message)) => {
                let until = self
                    .record_failure(ProviderScope::Search, 21_600, 86_400)
                    .await;
                return Err(ProviderError::RiskControl(format!(
                    "{message}; backoff until {until}"
                )));
            }
            Err(primary_error) => {
                warn!(%primary_error, "all-search response unusable; trying signed video search");
                let params = self.signed_search_params(&query.keyword).await?;
                let value = self
                    .request_value(
                        ProviderScope::Search,
                        "/x/web-interface/wbi/search/type",
                        &params,
                    )
                    .await?;
                Self::ensure_success(&value, "signed search")?;
                match search_result_items(&value) {
                    Ok(results) => results,
                    Err(ProviderError::RiskControl(message)) => {
                        let until = self
                            .record_failure(ProviderScope::Search, 21_600, 86_400)
                            .await;
                        return Err(ProviderError::RiskControl(format!(
                            "{message}; backoff until {until}"
                        )));
                    }
                    Err(error) => return Err(error),
                }
            }
        };
        self.succeeded(ProviderScope::Search).await?;
        let now = Utc::now();
        let mut candidates = Vec::with_capacity(results.len());
        for item in &results {
            let Some(bvid) = string_field(item, "bvid") else {
                continue;
            };
            let Some(title) = string_field(item, "title") else {
                continue;
            };
            let published_at = timestamp_field(item, &["pubdate", "senddate"])
                .and_then(|timestamp| Utc.timestamp_opt(timestamp, 0).single())
                .unwrap_or(now);
            let duration_sec = item
                .get("duration")
                .and_then(|value| {
                    value
                        .as_str()
                        .and_then(parse_duration)
                        .or_else(|| value.as_i64())
                })
                .unwrap_or_default();
            let uploader_mid = int_field(item, "mid").unwrap_or_default();
            let uploader_name = string_field(item, "author").unwrap_or_else(|| "unknown".into());
            let tags = string_field(item, "tag")
                .map(|value| {
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|tag| !tag.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            candidates.push(VideoCandidate {
                url: string_field(item, "arcurl")
                    .unwrap_or_else(|| format!("https://www.bilibili.com/video/{bvid}")),
                description: string_field(item, "description"),
                bvid,
                title,
                uploader_mid,
                uploader_name,
                duration_sec,
                published_at,
                tags,
                page_count: None,
                view_count: int_field(item, "play"),
                reply_count: int_field(item, "review"),
                uploader_follower_count: None,
                discovered_at: now,
                enriched: false,
            });
        }
        Ok(candidates)
    }

    async fn enrich(&self, candidate: &VideoCandidate) -> ProviderResult<VideoCandidate> {
        let value = self.view_value(&candidate.bvid).await?;
        Self::ensure_success(&value, "view")?;
        let data = value
            .get("data")
            .ok_or_else(|| ProviderError::InvalidResponse("view missing data".into()))?;
        let mut detailed = candidate.clone();
        detailed.title = string_field(data, "title").unwrap_or_else(|| candidate.title.clone());
        detailed.description = string_field(data, "desc").or_else(|| candidate.description.clone());
        detailed.duration_sec = int_field(data, "duration").unwrap_or(candidate.duration_sec);
        detailed.published_at = timestamp_field(data, &["pubdate", "ctime"])
            .and_then(|timestamp| Utc.timestamp_opt(timestamp, 0).single())
            .unwrap_or(candidate.published_at);
        detailed.uploader_mid = data
            .pointer("/owner/mid")
            .and_then(value_as_i64)
            .unwrap_or(candidate.uploader_mid);
        detailed.uploader_name = data
            .pointer("/owner/name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| candidate.uploader_name.clone());
        detailed.page_count = data
            .get("pages")
            .and_then(Value::as_array)
            .map(|pages| pages.len() as i64);
        detailed.view_count = data.pointer("/stat/view").and_then(value_as_i64);
        detailed.reply_count = data.pointer("/stat/reply").and_then(value_as_i64);
        detailed.uploader_follower_count =
            self.uploader_follower_count(detailed.uploader_mid).await;
        if let Some(category) = string_field(data, "tname")
            && !detailed.tags.iter().any(|tag| tag == &category)
        {
            detailed.tags.push(category);
        }
        detailed.enriched = true;
        Ok(detailed)
    }
}

impl BilibiliProvider {
    /// Read a multipart video's metadata once for the entire import, not once
    /// per episode. No video or audio media is downloaded.
    pub async fn collection(&self, bvid: &str) -> ProviderResult<crate::domain::VideoCollection> {
        let value = self.view_value(bvid).await?;
        Self::ensure_success(&value, "view")?;
        let data = value
            .get("data")
            .ok_or_else(|| ProviderError::InvalidResponse("view missing data".into()))?;
        let now = Utc::now();
        let mid = data
            .pointer("/owner/mid")
            .and_then(value_as_i64)
            .unwrap_or(0);
        let parts = data
            .get("pages")
            .and_then(Value::as_array)
            .ok_or_else(|| ProviderError::InvalidResponse("video has no pages".into()))?
            .iter()
            .map(|p| crate::domain::VideoPart {
                page: int_field(p, "page").unwrap_or(0),
                cid: int_field(p, "cid").unwrap_or(0),
                title: string_field(p, "part").unwrap_or_default(),
                duration_sec: int_field(p, "duration").unwrap_or(0),
            })
            .collect::<Vec<_>>();
        if parts.is_empty()
            || parts.len() > 1000
            || parts
                .iter()
                .any(|p| p.page <= 0 || p.cid <= 0 || p.duration_sec <= 0)
        {
            return Err(ProviderError::InvalidResponse(
                "invalid multipart metadata".into(),
            ));
        }
        Ok(crate::domain::VideoCollection {
            video: VideoCandidate {
                bvid: bvid.into(),
                title: string_field(data, "title").unwrap_or_default(),
                description: string_field(data, "desc"),
                uploader_mid: mid,
                uploader_name: data
                    .pointer("/owner/name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .into(),
                duration_sec: int_field(data, "duration").unwrap_or(0),
                published_at: timestamp_field(data, &["pubdate", "ctime"])
                    .and_then(|t| Utc.timestamp_opt(t, 0).single())
                    .unwrap_or(now),
                url: format!("https://www.bilibili.com/video/{bvid}"),
                tags: vec![],
                page_count: Some(parts.len() as i64),
                view_count: data.pointer("/stat/view").and_then(value_as_i64),
                reply_count: data.pointer("/stat/reply").and_then(value_as_i64),
                uploader_follower_count: self.uploader_follower_count(mid).await,
                discovered_at: now,
                enriched: true,
            },
            parts,
        })
    }
}

fn search_result_items(value: &Value) -> ProviderResult<Vec<Value>> {
    if value
        .pointer("/data/v_voucher")
        .and_then(Value::as_str)
        .is_some()
    {
        return Err(ProviderError::RiskControl(
            "Bilibili search requested a verification voucher".into(),
        ));
    }
    let results = value
        .pointer("/data/result")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::InvalidResponse("search missing data.result array".into()))?;
    if results
        .iter()
        .any(|group| group.get("result_type").is_some())
    {
        if let Some(videos) = results
            .iter()
            .find(|group| group.get("result_type").and_then(Value::as_str) == Some("video"))
            .and_then(|group| group.get("data"))
            .and_then(Value::as_array)
        {
            return Ok(videos.clone());
        }
        if value.pointer("/data/numResults").and_then(value_as_i64) == Some(0) {
            return Ok(Vec::new());
        }
        return Err(ProviderError::InvalidResponse(
            "all-search missing video result group".into(),
        ));
    }
    Ok(results.clone())
}

fn file_stem(url: &str) -> ProviderResult<&str> {
    url.rsplit('/')
        .next()
        .and_then(|file| file.split('.').next())
        .filter(|stem| !stem.is_empty())
        .ok_or_else(|| ProviderError::InvalidResponse(format!("invalid WBI image URL: {url}")))
}

fn sanitize_wbi_value(value: &str) -> String {
    value
        .chars()
        .filter(|character| !matches!(character, '!' | '\'' | '(' | ')' | '*'))
        .collect()
}

fn sign_params(
    mut params: Vec<(String, String)>,
    mixin: &str,
    timestamp: i64,
) -> Vec<(String, String)> {
    params.retain(|(key, _)| key != "wts" && key != "w_rid");
    for (_, value) in &mut params {
        *value = sanitize_wbi_value(value);
    }
    params.push(("wts".into(), timestamp.to_string()));
    params.sort_by(|left, right| left.0.cmp(&right.0));
    let encoded = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(
            params
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )
        .finish();
    let signature = format!("{:x}", md5::compute(format!("{encoded}{mixin}")));
    params.push(("w_rid".into(), signature));
    params
}

fn valid_cookie_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.bytes().all(|byte| {
            matches!(
                byte,
                0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e
            )
        })
}

fn string_field(value: &Value, field: &str) -> Option<String> {
    value.get(field)?.as_str().map(str::to_string)
}

fn int_field(value: &Value, field: &str) -> Option<i64> {
    value.get(field).and_then(value_as_i64)
}

fn value_as_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
        .or_else(|| value.as_str().and_then(|number| number.parse().ok()))
}

fn timestamp_field(value: &Value, fields: &[&str]) -> Option<i64> {
    fields.iter().find_map(|field| int_field(value, field))
}

fn parse_duration(value: &str) -> Option<i64> {
    let parts: Vec<_> = value.split(':').collect();
    match parts.as_slice() {
        [minutes, seconds] => {
            Some(minutes.parse::<i64>().ok()? * 60 + seconds.parse::<i64>().ok()?)
        }
        [hours, minutes, seconds] => Some(
            hours.parse::<i64>().ok()? * 3_600
                + minutes.parse::<i64>().ok()? * 60
                + seconds.parse::<i64>().ok()?,
        ),
        _ => value.parse().ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_search_duration() {
        assert_eq!(parse_duration("23:40"), Some(1_420));
        assert_eq!(parse_duration("1:02:03"), Some(3_723));
    }

    #[test]
    fn wbi_value_removes_forbidden_characters() {
        assert_eq!(sanitize_wbi_value("a!b'c(d)*"), "abcd");
    }

    #[test]
    fn rejects_cookie_header_injection() {
        assert!(valid_cookie_value("1234-abc_infoc-xyz=="));
        assert!(!valid_cookie_value("abc; SESSDATA=bad"));
        assert!(!valid_cookie_value("abc\r\nInjected: yes"));
    }

    #[test]
    fn extracts_video_group_from_all_search() {
        let value = serde_json::json!({
            "code": 0,
            "data": {
                "result": [
                    {"result_type": "media_bangumi", "data": []},
                    {"result_type": "video", "data": [
                        {"bvid": "BV1Es8A6UEnr", "title": "尼古喵喵 08"}
                    ]}
                ]
            }
        });

        let results = search_result_items(&value).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["bvid"], "BV1Es8A6UEnr");
    }

    #[test]
    fn recognizes_soft_risk_control_voucher() {
        let value = serde_json::json!({
            "code": 0,
            "data": {"v_voucher": "voucher-example"}
        });

        assert!(matches!(
            search_result_items(&value),
            Err(ProviderError::RiskControl(_))
        ));
    }

    #[test]
    fn accepts_empty_all_search_results() {
        let value = serde_json::json!({
            "code": 0,
            "data": {
                "numResults": 0,
                "result": [{"result_type": "tips", "data": []}]
            }
        });

        assert!(search_result_items(&value).unwrap().is_empty());
    }
}
