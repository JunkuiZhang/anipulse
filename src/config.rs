use std::{fs, path::Path};

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
    pub scheduler: SchedulerConfig,
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
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
        if self.polling.jitter_ratio < 0.0 || self.polling.jitter_ratio > 0.5 {
            return Err(AppError::Config(
                "polling.jitter_ratio must be between 0 and 0.5".into(),
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
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            provider: "none".into(),
            channel: "default".into(),
            notify_pending: false,
            request_timeout_secs: 15,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SchedulerConfig {
    pub tick_secs: u64,
    pub due_batch_size: i64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            tick_secs: 30,
            due_batch_size: 20,
        }
    }
}
