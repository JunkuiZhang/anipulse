mod feishu;
mod feishu_app;

use std::{env, sync::Arc, time::Duration};

use async_trait::async_trait;
use reqwest::Client;
use tracing::{info, warn};

use crate::{
    config::NotificationConfig,
    domain::{PendingNotification, PendingReviewNotification},
    error::{AppError, Result},
    repository::Repository,
};

use feishu::FeishuWebhookNotifier;
use feishu_app::FeishuAppNotifier;

#[async_trait]
trait Notifier: Send + Sync {
    async fn notify_release(&self, event: &PendingNotification) -> Result<()>;
    async fn notify_review(&self, event: &PendingReviewNotification) -> Result<()>;
    async fn test(&self) -> Result<()>;
}

struct NoopNotifier;

#[async_trait]
impl Notifier for NoopNotifier {
    async fn notify_release(&self, event: &PendingNotification) -> Result<()> {
        info!(anime = %event.anime_title, episode = event.episode_no, "notification disabled; marking delivered");
        Ok(())
    }

    async fn test(&self) -> Result<()> {
        info!("notification provider is none; test succeeded without an external message");
        Ok(())
    }

    async fn notify_review(&self, event: &PendingReviewNotification) -> Result<()> {
        info!(anime = %event.anime_title, episode = event.episode_no, "review notification disabled; marking delivered");
        Ok(())
    }
}

struct ServerChanNotifier {
    client: Client,
    endpoint: String,
}

impl ServerChanNotifier {
    fn from_env(timeout_secs: u64) -> Result<Self> {
        let send_key = env::var("SERVERCHAN_SEND_KEY")
            .or_else(|_| env::var("SERVERCHAN_SENDKEY"))
            .map_err(|_| {
                AppError::Notification(
                    "SERVERCHAN_SEND_KEY is required when notification.provider=serverchan".into(),
                )
            })?;
        if send_key.trim().is_empty() {
            return Err(AppError::Notification(
                "SERVERCHAN_SEND_KEY cannot be empty".into(),
            ));
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(|e| AppError::Notification(format!("cannot build HTTP client: {e}")))?;
        Ok(Self {
            client,
            endpoint: format!("https://sctapi.ftqq.com/{send_key}.send"),
        })
    }

    async fn send(&self, title: &str, body: &str) -> Result<()> {
        let response = self
            .client
            .post(&self.endpoint)
            .form(&[("title", title), ("desp", body)])
            .send()
            .await
            .map_err(|error| {
                let message = if error.is_timeout() {
                    "ServerChan request timed out"
                } else if error.is_connect() {
                    "cannot connect to ServerChan"
                } else {
                    "ServerChan request failed"
                };
                AppError::Notification(message.into())
            })?;
        if !response.status().is_success() {
            return Err(AppError::Notification(format!(
                "ServerChan returned HTTP {}",
                response.status()
            )));
        }
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| AppError::Notification(format!("invalid ServerChan response: {e}")))?;
        if body
            .get("code")
            .and_then(serde_json::Value::as_i64)
            .is_some_and(|code| code != 0)
        {
            return Err(AppError::Notification(format!(
                "ServerChan rejected message: {}",
                body.get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown error")
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl Notifier for ServerChanNotifier {
    async fn notify_release(&self, event: &PendingNotification) -> Result<()> {
        let full_title = format!("📺 {} EP{} 已更新", event.anime_title, event.episode_no);
        let title: String = full_title.chars().take(32).collect();
        let mut body = format!(
            "确认依据：{}\n\n",
            event.confirmation_reason.as_deref().unwrap_or("manual")
        );
        if let Some(uploader) = &event.uploader_name {
            body.push_str(&format!("UP：{uploader}\n\n"));
        }
        if let Some(duration) = event.duration_sec {
            body.push_str(&format!("时长：{}:{:02}\n\n", duration / 60, duration % 60));
        }
        if let Some(url) = &event.url {
            body.push_str(&format!("观看：{url}"));
        }
        self.send(&title, &body).await
    }

    async fn test(&self) -> Result<()> {
        self.send(
            "AniPulse 通知测试",
            "如果你看到这条消息，通知配置工作正常。",
        )
        .await
    }

    async fn notify_review(&self, event: &PendingReviewNotification) -> Result<()> {
        let full_title = format!("🔎 {} EP{} 需要确认", event.anime_title, event.episode_no);
        let title: String = full_title.chars().take(32).collect();
        let body = if event.candidates.is_empty() {
            format!("没有找到可用候选。\n\n处理：{}", event.review_url)
        } else {
            format!(
                "找到 {} 个待确认候选。\n\n处理：{}",
                event.candidates.len(),
                event.review_url
            )
        };
        self.send(&title, &body).await
    }
}

#[derive(Clone)]
pub struct NotificationDispatcher {
    repository: Repository,
    notifier: Arc<dyn Notifier>,
    review_public_url: String,
}

impl NotificationDispatcher {
    pub fn new(
        repository: Repository,
        config: &NotificationConfig,
        review_public_url: &str,
    ) -> Result<Self> {
        let notifier: Arc<dyn Notifier> = match config.provider.to_ascii_lowercase().as_str() {
            "none" => Arc::new(NoopNotifier),
            "serverchan" => Arc::new(ServerChanNotifier::from_env(config.request_timeout_secs)?),
            "feishu" | "feishu_app" => {
                Arc::new(FeishuAppNotifier::from_env(config.request_timeout_secs)?)
            }
            "feishu_webhook" => Arc::new(FeishuWebhookNotifier::from_env(
                config.request_timeout_secs,
            )?),
            provider => {
                return Err(AppError::Config(format!(
                    "unsupported notification provider: {provider}"
                )));
            }
        };
        Ok(Self {
            repository,
            notifier,
            review_public_url: review_public_url.trim_end_matches('/').into(),
        })
    }

    pub async fn dispatch_pending(&self) -> Result<()> {
        for event in self.repository.pending_notifications().await? {
            match self.notifier.notify_release(&event).await {
                Ok(()) => {
                    self.repository.mark_notification_sent(event.id).await?;
                    info!(notification_id = event.id, "notification sent");
                }
                Err(error) => {
                    self.repository
                        .mark_notification_failed(event.id, &error.to_string())
                        .await?;
                    warn!(notification_id = event.id, %error, "notification remains pending");
                }
            }
        }
        for event in self
            .repository
            .pending_review_notifications(&self.review_public_url)
            .await?
        {
            match self.notifier.notify_review(&event).await {
                Ok(()) => {
                    self.repository
                        .mark_review_notification_sent(event.id)
                        .await?;
                    info!(
                        review_notification_id = event.id,
                        "review notification sent"
                    );
                }
                Err(error) => {
                    self.repository
                        .mark_review_notification_failed(event.id, &error.to_string())
                        .await?;
                    warn!(review_notification_id = event.id, %error, "review notification remains pending");
                }
            }
        }
        Ok(())
    }

    pub async fn test(&self) -> Result<()> {
        self.notifier.test().await?;
        info!("notification test sent successfully");
        Ok(())
    }
}
