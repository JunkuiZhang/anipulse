use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::FromRow;
use std::time::Duration;

use super::Repository;
use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderScope {
    Global,
    Search,
    Detail,
    Device,
    Session,
    Followers,
}

impl ProviderScope {
    pub fn key(self) -> &'static str {
        match self {
            Self::Global => "bilibili",
            Self::Search => "bilibili:search",
            Self::Detail => "bilibili:detail",
            Self::Device => "bilibili:device",
            Self::Session => "bilibili:session",
            Self::Followers => "bilibili:followers",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Global => "全局保护",
            Self::Search => "搜索",
            Self::Detail => "视频详情",
            Self::Session => "WBI 签名",
            Self::Device => "设备初始化",
            Self::Followers => "UP 粉丝数",
        }
    }
}

#[derive(Debug)]
pub enum ProviderPermit {
    Ready,
    Wait(Duration),
    Paused {
        scope: ProviderScope,
        until: DateTime<Utc>,
    },
    BudgetExhausted,
}

#[derive(Debug, Clone, FromRow)]
pub struct ProviderBackoff {
    pub provider: String,
    pub backoff_until: DateTime<Utc>,
}

impl Repository {
    /// Atomically gate all instances/processes sharing the database. Never hold
    /// a transaction while sleeping or making an HTTP request.
    pub async fn reserve_scoped_provider_request(
        &self,
        scope: ProviderScope,
        max_per_day: u32,
        minimum_interval: Duration,
        probe_timeout: Duration,
    ) -> Result<ProviderPermit> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = Utc::now();
        for gate in [ProviderScope::Global, scope] {
            let until: Option<DateTime<Utc>> =
                sqlx::query_scalar("SELECT backoff_until FROM provider_state WHERE provider = ?")
                    .bind(gate.key())
                    .fetch_one(&mut *tx)
                    .await?;
            if let Some(until) = until.filter(|until| *until > now) {
                return Ok(ProviderPermit::Paused { scope: gate, until });
            }
        }
        let (date, count, next): (Option<chrono::NaiveDate>, i64, Option<DateTime<Utc>>) =
            sqlx::query_as("SELECT daily_date, daily_count, next_request_at FROM provider_state WHERE provider = 'bilibili'")
                .fetch_one(&mut *tx).await?;
        let count = if date == Some(now.date_naive()) {
            count
        } else {
            0
        };
        if count >= i64::from(max_per_day) {
            return Ok(ProviderPermit::BudgetExhausted);
        }
        if let Some(next) = next.filter(|next| *next > now) {
            return Ok(ProviderPermit::Wait(
                (next - now).to_std().unwrap_or_default(),
            ));
        }
        let (failures, probe_until): (i64, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT consecutive_failures, probe_until FROM provider_state WHERE provider = ?",
        )
        .bind(scope.key())
        .fetch_one(&mut *tx)
        .await?;
        if failures > 0 {
            if let Some(until) = probe_until.filter(|until| *until > now) {
                return Ok(ProviderPermit::Wait(
                    (until - now).to_std().unwrap_or_default(),
                ));
            }
            let lease =
                chrono::Duration::from_std(probe_timeout).unwrap_or(chrono::Duration::minutes(5));
            sqlx::query("UPDATE provider_state SET probe_until = ? WHERE provider = ?")
                .bind(now + lease)
                .bind(scope.key())
                .execute(&mut *tx)
                .await?;
        }
        let interval = chrono::Duration::from_std(minimum_interval)
            .unwrap_or(chrono::Duration::seconds(86_400));
        sqlx::query("UPDATE provider_state SET daily_date = ?, daily_count = ?, next_request_at = ?, updated_at = ? WHERE provider = 'bilibili'")
            .bind(now.date_naive()).bind(count + 1).bind(now + interval).bind(now)
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(ProviderPermit::Ready)
    }

    pub async fn record_scoped_provider_failure(
        &self,
        scope: ProviderScope,
        base_seconds: i64,
        max_seconds: i64,
    ) -> Result<DateTime<Utc>> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = Utc::now();
        let (previous, previous_until): (i64, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT consecutive_failures, backoff_until FROM provider_state WHERE provider = ?",
        )
        .bind(scope.key())
        .fetch_one(&mut *tx)
        .await?;
        // In-flight calls must not keep extending an already active cooldown.
        if let Some(until) = previous_until.filter(|until| *until > now) {
            return Ok(until);
        }
        let failures = previous.saturating_add(1);
        let delay = base_seconds
            .saturating_mul(2_i64.pow((failures - 1).clamp(0, 8) as u32))
            .min(max_seconds);
        let until = now + chrono::Duration::seconds(delay);
        sqlx::query("UPDATE provider_state SET backoff_until = ?, consecutive_failures = ?, probe_until = NULL, updated_at = ? WHERE provider = ?")
            .bind(until).bind(failures).bind(now).bind(scope.key()).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(until)
    }

    pub async fn clear_scoped_provider_failure(&self, scope: ProviderScope) -> Result<()> {
        // A successful in-flight request cannot reopen a gate another request
        // just closed. Only a successful post-cooldown probe resets its scope.
        sqlx::query("UPDATE provider_state SET backoff_until = NULL, consecutive_failures = 0, probe_until = NULL, updated_at = ? WHERE provider = ? AND (backoff_until IS NULL OR julianday(backoff_until) <= julianday(?))")
            .bind(Utc::now()).bind(scope.key()).bind(Utc::now()).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn provider_backoffs(&self) -> Result<Vec<ProviderBackoff>> {
        Ok(sqlx::query_as("SELECT provider, backoff_until FROM provider_state WHERE (provider = 'bilibili' OR provider LIKE 'bilibili:%') AND julianday(backoff_until) > julianday(?) ORDER BY provider")
            .bind(Utc::now()).fetch_all(&self.pool).await?)
    }

    pub async fn cached_provider_value(&self, key: &str) -> Result<Option<Value>> {
        let value: Option<String> = sqlx::query_scalar("SELECT payload_json FROM provider_cache WHERE cache_key = ? AND julianday(expires_at) > julianday(?)")
            .bind(key).bind(Utc::now()).fetch_optional(&self.pool).await?;
        Ok(value.and_then(|value| serde_json::from_str(&value).ok()))
    }

    pub async fn cache_provider_value(
        &self,
        key: &str,
        value: &Value,
        ttl_seconds: i64,
    ) -> Result<()> {
        let now = Utc::now();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("DELETE FROM provider_cache WHERE julianday(expires_at) <= julianday(?)")
            .bind(now)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO provider_cache(cache_key, payload_json, expires_at, updated_at) VALUES (?, ?, ?, ?) ON CONFLICT(cache_key) DO UPDATE SET payload_json = excluded.payload_json, expires_at = excluded.expires_at, updated_at = excluded.updated_at")
            .bind(key).bind(value.to_string()).bind(now + chrono::Duration::seconds(ttl_seconds)).bind(now)
            .execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
}
