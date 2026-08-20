use std::{sync::Arc, time::Duration};

use crate::{
    config::BilibiliConfig,
    domain::VideoCandidate,
    provider::{ProviderError, ProviderResult, SearchQuery, VideoSearchProvider},
    repository::Repository,
};
use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use tokio::{sync::Mutex, time::Instant};

const MIXIN_KEY_ENC_TAB: [usize; 64] = [
    46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19, 29,
    28, 14, 39, 12, 38, 41, 13, 37, 48, 7, 16, 24, 55, 40, 61, 26, 17, 0, 1, 60, 51, 30, 4, 22, 25,
    54, 21, 56, 59, 6, 63, 57, 62, 11, 36, 20, 34, 44, 52,
];

#[derive(Clone)]
pub struct BilibiliProvider {
    config: BilibiliConfig,
    client: Client,
    repository: Repository,
    request_lock: Arc<Mutex<Option<Instant>>>,
    wbi_cache: Arc<Mutex<Option<(String, Instant)>>>,
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
            request_lock: Arc::new(Mutex::new(None)),
            wbi_cache: Arc::new(Mutex::new(None)),
            public_cookie: Arc::new(Mutex::new(None)),
        })
    }

    async fn request_value(
        &self,
        path: &str,
        params: &[(String, String)],
    ) -> ProviderResult<Value> {
        let mut last_request = self.request_lock.lock().await;
        if let Some(last) = *last_request {
            let minimum = Duration::from_secs(self.config.min_request_interval_secs);
            if let Some(remaining) = minimum.checked_sub(last.elapsed()) {
                tokio::time::sleep(remaining).await;
            }
        }
        self.repository
            .reserve_provider_request(self.config.max_requests_per_day)
            .await
            .map_err(|e| ProviderError::Backoff(e.to_string()))?;

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
        *last_request = Some(Instant::now());

        let response = match response {
            Ok(response) => response,
            Err(error) => {
                let until = self.record_temporary_failure().await;
                return Err(ProviderError::Temporary(format!(
                    "{error}; backoff until {until}"
                )));
            }
        };
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            let until = self.record_failure(1_800, 14_400).await;
            return Err(ProviderError::RateLimited(format!(
                "HTTP 429; backoff until {until}"
            )));
        }
        if status == StatusCode::PRECONDITION_FAILED {
            let until = self.record_failure(21_600, 86_400).await;
            return Err(ProviderError::RiskControl(format!(
                "HTTP 412; backoff until {until}"
            )));
        }
        if status.is_server_error() {
            let until = self.record_temporary_failure().await;
            return Err(ProviderError::Temporary(format!(
                "HTTP {status}; backoff until {until}"
            )));
        }
        if !status.is_success() {
            return Err(ProviderError::Permanent(format!("HTTP {status}")));
        }

        let value: Value = response
            .json()
            .await
            .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;
        match value.get("code").and_then(Value::as_i64) {
            Some(-412 | -352) => {
                let until = self.record_failure(21_600, 86_400).await;
                Err(ProviderError::RiskControl(format!(
                    "Bilibili code {}; backoff until {until}",
                    value["code"]
                )))
            }
            _ => {
                self.repository
                    .clear_provider_failures()
                    .await
                    .map_err(|e| ProviderError::Temporary(e.to_string()))?;
                Ok(value)
            }
        }
    }

    async fn record_failure(&self, base: i64, maximum: i64) -> String {
        match self.repository.record_provider_failure(base, maximum).await {
            Ok(until) => until.to_rfc3339(),
            Err(error) => format!("unknown ({error})"),
        }
    }

    async fn record_temporary_failure(&self) -> String {
        self.record_failure(300, 3_600).await
    }

    async fn wbi_mixin_key(&self) -> ProviderResult<String> {
        let mut cache = self.wbi_cache.lock().await;
        if let Some((key, fetched_at)) = cache.as_ref()
            && fetched_at.elapsed() < Duration::from_secs(43_200)
        {
            return Ok(key.clone());
        }
        self.ensure_public_cookie().await?;
        let nav = self.request_value("/x/web-interface/nav", &[]).await?;
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
        *cache = Some((mixin.clone(), Instant::now()));
        Ok(mixin)
    }

    async fn ensure_public_cookie(&self) -> ProviderResult<()> {
        if self.public_cookie.lock().await.is_some() {
            return Ok(());
        }
        let value = self.request_value("/x/frontend/finger/spi", &[]).await?;
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
        Ok(())
    }

    async fn signed_search_params(&self, keyword: &str) -> ProviderResult<Vec<(String, String)>> {
        let mixin = self.wbi_mixin_key().await?;
        let mut params = vec![
            ("keyword".to_string(), sanitize_wbi_value(keyword)),
            ("page".to_string(), "1".to_string()),
            ("page_size".to_string(), "20".to_string()),
            ("search_type".to_string(), "video".to_string()),
            ("wts".to_string(), Utc::now().timestamp().to_string()),
        ];
        params.sort_by(|left, right| left.0.cmp(&right.0));
        let encoded = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(
                params
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            )
            .finish();
        let signature = format!("{:x}", md5::compute(format!("{encoded}{mixin}")));
        params.push(("w_rid".to_string(), signature));
        Ok(params)
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
        let params = self.signed_search_params(&query.keyword).await?;
        let value = self
            .request_value("/x/web-interface/wbi/search/type", &params)
            .await?;
        Self::ensure_success(&value, "search")?;
        let Some(results) = value.pointer("/data/result").and_then(Value::as_array) else {
            return Ok(Vec::new());
        };
        let now = Utc::now();
        let mut candidates = Vec::with_capacity(results.len());
        for item in results {
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
                discovered_at: now,
                enriched: false,
            });
        }
        Ok(candidates)
    }

    async fn enrich(&self, candidate: &VideoCandidate) -> ProviderResult<VideoCandidate> {
        let params = vec![("bvid".to_string(), candidate.bvid.clone())];
        let value = self.request_value("/x/web-interface/view", &params).await?;
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
        if let Some(category) = string_field(data, "tname")
            && !detailed.tags.iter().any(|tag| tag == &category)
        {
            detailed.tags.push(category);
        }
        detailed.enriched = true;
        Ok(detailed)
    }
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
}
