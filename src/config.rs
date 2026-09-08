use std::{fs, path::Path};

use ipnet::IpNet;
use serde::Deserialize;

use crate::error::{AppError, Result};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub database: DatabaseConfig,
    pub bilibili: BilibiliConfig,
    pub polling: PollingConfig,
    pub confirmation: ConfirmationConfig,
    pub notification: NotificationConfig,
    pub schedule: ScheduleConfig,
    pub scheduler: SchedulerConfig,
    pub web: WebConfig,
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            let config = Self::default();
            config.validate()?;
            return Ok(config);
        }
        let raw = fs::read_to_string(path)
            .map_err(|e| AppError::Config(format!("cannot read {}: {e}", path.display())))?;
        let config: Self = toml::from_str(&raw)
            .map_err(|e| AppError::Config(format!("invalid {}: {e}", path.display())))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.bilibili.min_request_interval_secs == 0 {
            return Err(AppError::Config(
                "bilibili.min_request_interval_secs must be greater than zero".into(),
            ));
        }
        if self.confirmation.consensus_uploaders < 2 {
            return Err(AppError::Config(
                "confirmation.consensus_uploaders must be at least 2".into(),
            ));
        }
        if !(0..=1_000_000_000).contains(&self.confirmation.minimum_uploader_followers)
            || !(0..=1_000_000_000).contains(&self.confirmation.minimum_auto_confirm_video_views)
        {
            return Err(AppError::Config(
                "confirmation auto-confirm reputation thresholds must be between 0 and 1000000000"
                    .into(),
            ));
        }
        if self.polling.jitter_ratio < 0.0 || self.polling.jitter_ratio > 0.5 {
            return Err(AppError::Config(
                "polling.jitter_ratio must be between 0 and 0.5".into(),
            ));
        }
        if !(3_600..=31_536_000).contains(&self.schedule.sync_interval_secs) {
            return Err(AppError::Config(
                "schedule.sync_interval_secs must be between 3600 and 31536000".into(),
            ));
        }
        if self.schedule.failure_retry_secs < 60
            || self.schedule.failure_retry_secs > self.schedule.sync_interval_secs
        {
            return Err(AppError::Config(
                "schedule.failure_retry_secs must be between 60 and sync_interval_secs".into(),
            ));
        }
        if self.schedule.request_timeout_secs == 0 {
            return Err(AppError::Config(
                "schedule.request_timeout_secs must be greater than zero".into(),
            ));
        }
        if self.schedule.sync_batch_size <= 0 {
            return Err(AppError::Config(
                "schedule.sync_batch_size must be greater than zero".into(),
            ));
        }
        if !(0..=31).contains(&self.schedule.max_stream_offset_days) {
            return Err(AppError::Config(
                "schedule.max_stream_offset_days must be between 0 and 31".into(),
            ));
        }
        if !(0..=7).contains(&self.schedule.max_catalog_offset_days) {
            return Err(AppError::Config(
                "schedule.max_catalog_offset_days must be between 0 and 7".into(),
            ));
        }
        for site in std::iter::once(&self.schedule.preferred_site)
            .chain(self.schedule.stream_site_priority.iter())
            .chain(self.schedule.excluded_stream_sites.iter())
        {
            if site.trim().is_empty()
                || !site.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
                })
            {
                return Err(AppError::Config(
                    "schedule site names must contain only ASCII letters, digits, '_' or '-'"
                        .into(),
                ));
            }
        }
        if self
            .schedule
            .excluded_stream_sites
            .iter()
            .any(|site| site == &self.schedule.preferred_site)
        {
            return Err(AppError::Config(
                "schedule.preferred_site must not also appear in excluded_stream_sites".into(),
            ));
        }
        for (name, value) in [
            ("schedule.bangumi_data_url", &self.schedule.bangumi_data_url),
            (
                "schedule.bangumi_api_base_url",
                &self.schedule.bangumi_api_base_url,
            ),
            (
                "schedule.anime_schedule_api_url",
                &self.schedule.anime_schedule_api_url,
            ),
        ] {
            let url = url::Url::parse(value)
                .map_err(|_| AppError::Config(format!("{name} must be a valid URL")))?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(AppError::Config(format!("{name} must use HTTP or HTTPS")));
            }
        }
        let bind = self
            .web
            .bind
            .parse::<std::net::SocketAddr>()
            .map_err(|_| AppError::Config("web.bind must be an IP socket address".into()))?;
        if !bind.ip().is_loopback() && !self.web.dangerous_allow_public_bind {
            return Err(AppError::Config(
                "web.bind must be loopback unless web.dangerous_allow_public_bind=true".into(),
            ));
        }
        let public_url = url::Url::parse(&self.web.public_url)
            .map_err(|_| AppError::Config("web.public_url must be a valid URL".into()))?;
        if public_url.host_str().is_none()
            || public_url.path() != "/"
            || public_url.query().is_some()
            || public_url.fragment().is_some()
            || !public_url.username().is_empty()
            || public_url.password().is_some()
        {
            return Err(AppError::Config(
                "web.public_url must be an origin without credentials, path, query, or fragment"
                    .into(),
            ));
        }
        let local_http = public_url.scheme() == "http"
            && public_url.host_str().is_some_and(|host| {
                host == "localhost"
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            });
        if public_url.scheme() != "https" && !(self.web.development_mode && local_http) {
            return Err(AppError::Config(
                "web.public_url must use HTTPS (localhost HTTP requires web.development_mode=true)"
                    .into(),
            ));
        }
        if self.web.session_idle_secs < 300
            || self.web.session_absolute_secs < self.web.session_idle_secs
            || self.web.session_renewal_secs < 60
            || self.web.session_renewal_secs > self.web.session_idle_secs
        {
            return Err(AppError::Config(
                "web session timeouts are outside the safe range".into(),
            ));
        }
        if !(1..=20).contains(&self.web.login_max_failures)
            || !(60..=86_400).contains(&self.web.login_window_secs)
            || !(4_096..=1_048_576).contains(&self.web.max_body_bytes)
            || !(1..=120).contains(&self.web.request_timeout_secs)
        {
            return Err(AppError::Config(
                "web login limits or request body limit are outside the safe range".into(),
            ));
        }
        if self.web.cover_cache_dir.trim().is_empty() {
            return Err(AppError::Config(
                "web.cover_cache_dir must not be empty".into(),
            ));
        }
        self.web
            .timezone
            .parse::<chrono_tz::Tz>()
            .map_err(|_| AppError::Config("web.timezone must be a valid IANA timezone".into()))?;
        if self.scheduler.tick_secs == 0
            || self.scheduler.due_batch_size <= 0
            || self.scheduler.management_job_batch_size <= 0
        {
            return Err(AppError::Config(
                "scheduler intervals and batch sizes must be greater than zero".into(),
            ));
        }
        if !(1..=120).contains(&self.notification.request_timeout_secs)
            || !(0..=604_800).contains(&self.notification.review_grace_secs)
        {
            return Err(AppError::Config(
                "notification timeout or review grace period is outside the safe range".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    pub path: String,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: "anipulse.db".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BilibiliConfig {
    pub base_url: String,
    pub min_request_interval_secs: u64,
    pub max_requests_per_day: u32,
    pub max_search_requests_per_check: usize,
    pub max_detail_requests_per_check: usize,
    pub request_timeout_secs: u64,
    pub user_agent: String,
}

impl Default for BilibiliConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.bilibili.com".into(),
            min_request_interval_secs: 5,
            max_requests_per_day: 500,
            max_search_requests_per_check: 2,
            max_detail_requests_per_check: 3,
            request_timeout_secs: 15,
            user_agent: "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/131 Safari/537.36 AniPulse/0.1".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ConfirmationConfig {
    pub minimum_candidate_score: i32,
    pub detail_threshold_score: i32,
    pub consensus_uploaders: usize,
    pub max_duration_delta_secs: i64,
    pub max_publish_delta_secs: i64,
    pub candidate_expire_secs: i64,
    pub trusted_confirmed_count: i64,
    #[serde(alias = "minimum_auto_confirm_uploader_followers")]
    pub minimum_uploader_followers: i64,
    pub minimum_auto_confirm_video_views: i64,
}

impl Default for ConfirmationConfig {
    fn default() -> Self {
        Self {
            minimum_candidate_score: 60,
            detail_threshold_score: 35,
            consensus_uploaders: 2,
            max_duration_delta_secs: 180,
            max_publish_delta_secs: 7_200,
            candidate_expire_secs: 259_200,
            trusted_confirmed_count: 3,
            minimum_uploader_followers: 100,
            minimum_auto_confirm_video_views: 200,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PollingConfig {
    pub far_interval_secs: u64,
    pub two_day_interval_secs: u64,
    pub half_day_interval_secs: u64,
    pub release_interval_secs: u64,
    pub day_after_interval_secs: u64,
    pub three_day_interval_secs: u64,
    pub jitter_ratio: f64,
}

impl Default for PollingConfig {
    fn default() -> Self {
        Self {
            far_interval_secs: 21_600,
            two_day_interval_secs: 7_200,
            half_day_interval_secs: 3_600,
            release_interval_secs: 900,
            day_after_interval_secs: 1_800,
            three_day_interval_secs: 7_200,
            jitter_ratio: 0.1,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NotificationConfig {
    pub provider: String,
    pub channel: String,
    pub notify_pending: bool,
    pub request_timeout_secs: u64,
    pub review_grace_secs: i64,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            provider: "none".into(),
            channel: "default".into(),
            notify_pending: false,
            request_timeout_secs: 15,
            review_grace_secs: 3_600,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ScheduleConfig {
    pub bangumi_data_url: String,
    pub bangumi_api_base_url: String,
    pub anime_schedule_api_url: String,
    pub preferred_site: String,
    pub stream_site_priority: Vec<String>,
    pub excluded_stream_sites: Vec<String>,
    pub max_stream_offset_days: i64,
    pub max_catalog_offset_days: i64,
    pub sync_interval_secs: u64,
    pub failure_retry_secs: u64,
    pub request_timeout_secs: u64,
    pub sync_batch_size: i64,
    pub user_agent: String,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            bangumi_data_url: "https://unpkg.com/bangumi-data@0.3/dist/data.json".into(),
            bangumi_api_base_url: "https://api.bgm.tv".into(),
            anime_schedule_api_url: "https://animeschedule.net/api/v3".into(),
            preferred_site: "bilibili".into(),
            stream_site_priority: vec![
                "danime".into(),
                "abema".into(),
                "gamer".into(),
                "gamer_hk".into(),
            ],
            excluded_stream_sites: vec!["unext".into()],
            max_stream_offset_days: 14,
            max_catalog_offset_days: 1,
            sync_interval_secs: 86_400,
            failure_retry_secs: 900,
            request_timeout_secs: 30,
            sync_batch_size: 20,
            user_agent: "your-bangumi-id/AniPulse/0.1 (personal self-hosted)".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SchedulerConfig {
    pub tick_secs: u64,
    pub due_batch_size: i64,
    pub management_job_batch_size: i64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            tick_secs: 30,
            due_batch_size: 20,
            management_job_batch_size: 10,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    pub bind: String,
    pub public_url: String,
    pub cover_cache_dir: String,
    pub timezone: String,
    pub trusted_proxy_cidrs: Vec<IpNet>,
    pub session_idle_secs: i64,
    pub session_absolute_secs: i64,
    pub session_renewal_secs: i64,
    pub login_window_secs: i64,
    pub login_max_failures: i64,
    pub request_timeout_secs: u64,
    pub max_body_bytes: usize,
    pub development_mode: bool,
    pub dangerous_allow_public_bind: bool,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".into(),
            public_url: "https://localhost".into(),
            cover_cache_dir: "covers".into(),
            timezone: "Asia/Shanghai".into(),
            trusted_proxy_cidrs: vec![
                "127.0.0.1/32".parse().expect("valid loopback network"),
                "::1/128".parse().expect("valid loopback network"),
            ],
            session_idle_secs: 7_200,
            session_absolute_secs: 86_400,
            session_renewal_secs: 1_800,
            login_window_secs: 900,
            login_max_failures: 5,
            request_timeout_secs: 15,
            max_body_bytes: 65_536,
            development_mode: false,
            dangerous_allow_public_bind: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_web_defaults_are_valid_and_loopback_only() {
        let config = AppConfig::default();
        config.validate().unwrap();
        assert!(
            config
                .web
                .bind
                .parse::<std::net::SocketAddr>()
                .unwrap()
                .ip()
                .is_loopback()
        );
    }

    #[test]
    fn rejects_public_bind_and_non_origin_public_url() {
        let mut config = AppConfig::default();
        config.web.bind = "0.0.0.0:8080".into();
        assert!(config.validate().is_err());

        config.web.bind = "127.0.0.1:8080".into();
        config.web.public_url = "https://anime.example.com/panel".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_invalid_web_display_timezone() {
        let mut config = AppConfig::default();
        config.web.timezone = "Asia/Not-A-Real-City".into();

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("web.timezone"));
    }

    #[test]
    fn rejects_unsafe_schedule_alignment_policy() {
        let mut config = AppConfig::default();
        config.schedule.max_stream_offset_days = 32;
        assert!(config.validate().is_err());

        config.schedule.max_stream_offset_days = 14;
        config.schedule.stream_site_priority = vec!["not/a/site".into()];
        assert!(config.validate().is_err());

        config.schedule.stream_site_priority = vec!["danime".into()];
        config.schedule.preferred_site = "unext".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn old_schedule_config_excludes_unext_by_default() {
        let config: AppConfig = toml::from_str(
            r#"
                [schedule]
                anilist_api_url = "https://legacy.invalid/graphql"
                preferred_site = "bilibili"
                stream_site_priority = ["unext", "danime", "gamer"]
            "#,
        )
        .unwrap();

        assert_eq!(config.schedule.excluded_stream_sites, ["unext"]);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn old_follower_threshold_name_remains_compatible() {
        let config: AppConfig = toml::from_str(
            r#"
                [confirmation]
                minimum_auto_confirm_uploader_followers = 42
            "#,
        )
        .unwrap();

        assert_eq!(config.confirmation.minimum_uploader_followers, 42);
        assert!(config.validate().is_ok());
    }
}
