use std::{env, time::Duration};

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::Utc;
use hmac::{Hmac, KeyInit, Mac};
use reqwest::Client;
use serde_json::{Value, json};
use sha2::Sha256;
use url::Url;

use super::Notifier;
use crate::{
    domain::PendingNotification,
    error::{AppError, Result},
};

type HmacSha256 = Hmac<Sha256>;

pub(super) struct FeishuWebhookNotifier {
    client: Client,
    endpoint: String,
    signing_secret: Option<String>,
}

impl FeishuWebhookNotifier {
    pub(super) fn from_env(timeout_secs: u64) -> Result<Self> {
        let endpoint = env::var("FEISHU_WEBHOOK_URL").map_err(|_| {
            AppError::Notification(
                "FEISHU_WEBHOOK_URL is required when notification.provider=feishu_webhook".into(),
            )
        })?;
        validate_official_webhook(&endpoint)?;
        let signing_secret = env::var("FEISHU_BOT_SECRET")
            .ok()
            .filter(|secret| !secret.trim().is_empty());
        Self::new(endpoint, signing_secret, timeout_secs)
    }

    fn new(endpoint: String, signing_secret: Option<String>, timeout_secs: u64) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(|error| {
                AppError::Notification(format!("cannot build Feishu HTTP client: {error}"))
            })?;
        Ok(Self {
            client,
            endpoint,
            signing_secret,
        })
    }

    async fn send_payload(&self, mut payload: Value) -> Result<()> {
        if let Some(secret) = &self.signing_secret {
            let timestamp = Utc::now().timestamp();
            payload["timestamp"] = Value::String(timestamp.to_string());
            payload["sign"] = Value::String(generate_signature(secret, timestamp)?);
        }

        let response = self
            .client
            .post(&self.endpoint)
            .json(&payload)
            .send()
            .await
            .map_err(safe_request_error)?;
        if !response.status().is_success() {
            return Err(AppError::Notification(format!(
                "Feishu webhook returned HTTP {}",
                response.status()
            )));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|_| AppError::Notification("Feishu webhook returned invalid JSON".into()))?;
        let code = body
            .get("code")
            .or_else(|| body.get("StatusCode"))
            .and_then(Value::as_i64)
            .ok_or_else(|| {
                AppError::Notification("Feishu webhook response has no status code".into())
            })?;
        if code != 0 {
            let message = body
                .get("msg")
                .or_else(|| body.get("StatusMessage"))
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            let safe_message: String = message.chars().take(200).collect();
            return Err(AppError::Notification(format!(
                "Feishu webhook rejected message (code {code}): {safe_message}"
            )));
        }
        Ok(())
    }

    fn release_payload(event: &PendingNotification) -> Value {
        json!({
            "msg_type": "interactive",
            "card": release_card(event)
        })
    }

    fn test_payload() -> Value {
        json!({
            "msg_type": "interactive",
            "card": test_card("Webhook、消息卡片和签名配置均工作正常。")
        })
    }
}

pub(super) fn release_card(event: &PendingNotification) -> Value {
    let title = truncate_chars(
        &format!("📺 {} EP{} 已更新", event.anime_title, event.episode_no),
        80,
    );
    let reason = readable_confirmation_reason(event.confirmation_reason.as_deref());
    let mut lines = vec![format!("**确认依据：** {}", escape_lark_markdown(&reason))];
    if let Some(uploader) = &event.uploader_name {
        lines.push(format!("**UP 主：** {}", escape_lark_markdown(uploader)));
    }
    if let Some(duration) = event.duration_sec {
        lines.push(format!(
            "**视频时长：** {}:{:02}",
            duration / 60,
            duration % 60
        ));
    }
    if let Some(bvid) = &event.bvid {
        lines.push(format!("**BV 号：** {}", escape_lark_markdown(bvid)));
    }

    let mut elements = vec![json!({
        "tag": "div",
        "text": {
            "tag": "lark_md",
            "content": lines.join("\n")
        }
    })];
    if let Some(url) = event.url.as_deref().and_then(safe_http_url) {
        elements.push(json!({
            "tag": "action",
            "actions": [{
                "tag": "button",
                "text": {
                    "tag": "plain_text",
                    "content": "立即观看"
                },
                "type": "primary",
                "url": url
            }]
        }));
    }
    elements.push(json!({
        "tag": "note",
        "elements": [{
            "tag": "plain_text",
            "content": "由 AniPulse 自动确认；搜索结果本身不会触发通知。"
        }]
    }));

    json!({
        "config": {
            "wide_screen_mode": true
        },
        "header": {
            "template": "green",
            "title": {
                "tag": "plain_text",
                "content": title
            }
        },
        "elements": elements
    })
}

pub(super) fn test_card(message: &str) -> Value {
    json!({
        "config": {
            "wide_screen_mode": true
        },
        "header": {
            "template": "blue",
            "title": {
                "tag": "plain_text",
                "content": "🧪 AniPulse 飞书通知测试"
            }
        },
        "elements": [{
            "tag": "div",
            "text": {
                "tag": "lark_md",
                "content": message
            }
        }]
    })
}

#[async_trait]
impl Notifier for FeishuWebhookNotifier {
    async fn notify_release(&self, event: &PendingNotification) -> Result<()> {
        self.send_payload(Self::release_payload(event)).await
    }

    async fn test(&self) -> Result<()> {
        self.send_payload(Self::test_payload()).await
    }
}

fn validate_official_webhook(endpoint: &str) -> Result<()> {
    let url = Url::parse(endpoint)
        .map_err(|_| AppError::Notification("FEISHU_WEBHOOK_URL is not a valid URL".into()))?;
    let official_host = matches!(
        url.host_str(),
        Some("open.feishu.cn" | "open.larksuite.com")
    );
    if url.scheme() != "https"
        || !official_host
        || !url.path().starts_with("/open-apis/bot/v2/hook/")
    {
        return Err(AppError::Notification(
            "FEISHU_WEBHOOK_URL must be an official HTTPS custom-bot webhook".into(),
        ));
    }
    Ok(())
}

fn generate_signature(secret: &str, timestamp: i64) -> Result<String> {
    let string_to_sign = format!("{timestamp}\n{secret}");
    let mut mac = HmacSha256::new_from_slice(string_to_sign.as_bytes())
        .map_err(|_| AppError::Notification("invalid Feishu signing secret".into()))?;
    mac.update(&[]);
    Ok(STANDARD.encode(mac.finalize().into_bytes()))
}

fn safe_request_error(error: reqwest::Error) -> AppError {
    let message = if error.is_timeout() {
        "Feishu webhook request timed out"
    } else if error.is_connect() {
        "cannot connect to Feishu webhook"
    } else {
        "Feishu webhook request failed"
    };
    AppError::Notification(message.into())
}

fn safe_http_url(value: &str) -> Option<String> {
    let url = Url::parse(value).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
}

fn readable_confirmation_reason(reason: Option<&str>) -> String {
    match reason.unwrap_or("manual_confirmation") {
        "trusted_uploader" => "可信 UP 主".into(),
        "manual_confirmation" => "人工确认".into(),
        value if value.starts_with("consensus:") => {
            let votes = value
                .trim_start_matches("consensus:")
                .trim_end_matches("_uploaders");
            format!("{votes} 个独立 UP 主形成共识")
        }
        value => value.to_string(),
    }
}

fn escape_lark_markdown(value: &str) -> String {
    value.chars().fold(String::new(), |mut escaped, character| {
        if matches!(character, '\\' | '*' | '_' | '[' | ']' | '(' | ')' | '`') {
            escaped.push('\\');
        }
        escaped.push(character);
        escaped
    })
}

fn truncate_chars(value: &str, maximum: usize) -> String {
    value.chars().take(maximum).collect()
}

#[cfg(test)]
mod tests {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

    #[test]
    fn signature_matches_documented_algorithm() {
        assert_eq!(
            generate_signature("demo", 1_599_360_473).unwrap(),
            "l1N0gAcBjdwBvGm1xMjOF0XSyaLRpR7tuO5dHfhAYc8="
        );
    }

    #[test]
    fn release_card_contains_explanation_and_safe_button() {
        let event = event();
        let payload = FeishuWebhookNotifier::release_payload(&event);
        assert_eq!(payload["msg_type"], "interactive");
        assert_eq!(payload["card"]["header"]["template"], "green");
        let serialized = payload.to_string();
        assert!(serialized.contains("2 个独立 UP 主形成共识"));
        assert!(serialized.contains("立即观看"));
        assert!(serialized.contains("BVmock00001"));
    }

    #[tokio::test]
    async fn posts_signed_card_to_webhook() {
        let (endpoint, request) = mock_server().await;
        let notifier = FeishuWebhookNotifier::new(endpoint, Some("demo".into()), 5).unwrap();
        notifier.notify_release(&event()).await.unwrap();
        let body = request.await.unwrap();
        let payload: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["msg_type"], "interactive");
        assert!(payload["timestamp"].as_str().is_some());
        assert!(
            payload["sign"]
                .as_str()
                .is_some_and(|sign| !sign.is_empty())
        );
    }

    fn event() -> PendingNotification {
        PendingNotification {
            id: 1,
            episode_id: 1,
            channel: "feishu-anime".into(),
            attempts: 0,
            anime_title: "Silent Witch".into(),
            episode_no: 8,
            bvid: Some("BVmock00001".into()),
            uploader_name: Some("测试 UP".into()),
            duration_sec: Some(1_420),
            url: Some("https://www.bilibili.com/video/BVmock00001".into()),
            confirmation_reason: Some("consensus:2_uploaders".into()),
        }
    }

    async fn mock_server() -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4_096];
            loop {
                let read = stream.read(&mut chunk).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if complete_http_request(&request) {
                    break;
                }
            }
            let response_body = r#"{"code":0,"msg":"success"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            let body_start = find_header_end(&request).unwrap() + 4;
            request[body_start..].to_vec()
        });
        (format!("http://{address}/hook"), task)
    }

    fn complete_http_request(request: &[u8]) -> bool {
        let Some(header_end) = find_header_end(request) else {
            return false;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or_default();
        request.len() >= header_end + 4 + content_length
    }

    fn find_header_end(request: &[u8]) -> Option<usize> {
        request.windows(4).position(|window| window == b"\r\n\r\n")
    }
}
