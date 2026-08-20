use std::{env, time::Duration};

use async_trait::async_trait;
use reqwest::{Client, Response};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::{
    Notifier,
    feishu::{release_card, test_card},
};
use crate::{
    domain::PendingNotification,
    error::{AppError, Result},
};

const FEISHU_API_BASE: &str = "https://open.feishu.cn";
const TOKEN_REFRESH_MARGIN_SECS: u64 = 300;

struct CachedAccessToken {
    value: String,
    refresh_at: tokio::time::Instant,
}

pub(super) struct FeishuAppNotifier {
    client: Client,
    api_base: String,
    app_id: String,
    app_secret: String,
    receive_id: String,
    receive_id_type: String,
    access_token: Mutex<Option<CachedAccessToken>>,
}

impl FeishuAppNotifier {
    pub(super) fn from_env(timeout_secs: u64) -> Result<Self> {
        let app_id = required_env("FEISHU_APP_ID", "feishu")?;
        let app_secret = required_env("FEISHU_APP_SECRET", "feishu")?;
        let receive_id = required_env("FEISHU_RECEIVE_ID", "feishu")?;
        let receive_id_type = env::var("FEISHU_RECEIVE_ID_TYPE")
            .unwrap_or_else(|_| "open_id".into())
            .trim()
            .to_ascii_lowercase();
        validate_receive_id_type(&receive_id_type)?;
        Self::new(
            FEISHU_API_BASE.into(),
            app_id,
            app_secret,
            receive_id,
            receive_id_type,
            timeout_secs,
        )
    }

    fn new(
        api_base: String,
        app_id: String,
        app_secret: String,
        receive_id: String,
        receive_id_type: String,
        timeout_secs: u64,
    ) -> Result<Self> {
        validate_receive_id_type(&receive_id_type)?;
        let client = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(|error| {
                AppError::Notification(format!("cannot build Feishu HTTP client: {error}"))
            })?;
        Ok(Self {
            client,
            api_base: api_base.trim_end_matches('/').into(),
            app_id,
            app_secret,
            receive_id,
            receive_id_type,
            access_token: Mutex::new(None),
        })
    }

    async fn access_token(&self) -> Result<String> {
        let mut cached = self.access_token.lock().await;
        if let Some(token) = cached.as_ref()
            && tokio::time::Instant::now() < token.refresh_at
        {
            return Ok(token.value.clone());
        }

        let response = self
            .client
            .post(format!(
                "{}/open-apis/auth/v3/tenant_access_token/internal",
                self.api_base
            ))
            .json(&json!({
                "app_id": self.app_id,
                "app_secret": self.app_secret
            }))
            .send()
            .await
            .map_err(|error| safe_request_error(error, "authentication"))?;
        let body = checked_response(response, "authentication").await?;
        let value = body
            .get("tenant_access_token")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                AppError::Notification(
                    "Feishu authentication response has no tenant_access_token".into(),
                )
            })?
            .to_owned();
        let expires_in = body.get("expire").and_then(Value::as_u64).unwrap_or(7_200);
        let refresh_after = expires_in.saturating_sub(TOKEN_REFRESH_MARGIN_SECS).max(1);
        *cached = Some(CachedAccessToken {
            value: value.clone(),
            refresh_at: tokio::time::Instant::now() + Duration::from_secs(refresh_after),
        });
        Ok(value)
    }

    async fn send_card(&self, card: Value) -> Result<()> {
        let token = self.access_token().await?;
        let content = serde_json::to_string(&card).map_err(|error| {
            AppError::Notification(format!("cannot serialize Feishu card: {error}"))
        })?;
        let response = self
            .client
            .post(format!("{}/open-apis/im/v1/messages", self.api_base))
            .query(&[("receive_id_type", self.receive_id_type.as_str())])
            .bearer_auth(token)
            .json(&json!({
                "receive_id": self.receive_id,
                "msg_type": "interactive",
                "content": content
            }))
            .send()
            .await
            .map_err(|error| safe_request_error(error, "message"))?;
        checked_response(response, "message").await?;
        Ok(())
    }
}

#[async_trait]
impl Notifier for FeishuAppNotifier {
    async fn notify_release(&self, event: &PendingNotification) -> Result<()> {
        self.send_card(release_card(event)).await
    }

    async fn test(&self) -> Result<()> {
        self.send_card(test_card(
            "自建应用鉴权、收件人配置和私聊消息卡片均工作正常。",
        ))
        .await
    }
}

fn required_env(name: &str, provider: &str) -> Result<String> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            AppError::Notification(format!(
                "{name} is required when notification.provider={provider}"
            ))
        })
}

fn validate_receive_id_type(value: &str) -> Result<()> {
    if matches!(value, "open_id" | "user_id" | "union_id" | "email") {
        return Ok(());
    }
    Err(AppError::Notification(
        "FEISHU_RECEIVE_ID_TYPE must be open_id, user_id, union_id, or email".into(),
    ))
}

async fn checked_response(response: Response, operation: &str) -> Result<Value> {
    let status = response.status();
    let body: Value = response.json().await.map_err(|_| {
        AppError::Notification(format!(
            "Feishu {operation} returned invalid JSON (HTTP {status})"
        ))
    })?;
    let code = body.get("code").and_then(Value::as_i64).ok_or_else(|| {
        AppError::Notification(format!(
            "Feishu {operation} response has no status code (HTTP {status})"
        ))
    })?;
    if status.is_success() && code == 0 {
        return Ok(body);
    }

    let message = body
        .get("msg")
        .and_then(Value::as_str)
        .unwrap_or("unknown error");
    let safe_message: String = message.chars().take(200).collect();
    Err(AppError::Notification(format!(
        "Feishu {operation} failed (HTTP {status}, code {code}): {safe_message}"
    )))
}

fn safe_request_error(error: reqwest::Error, operation: &str) -> AppError {
    let detail = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "could not connect"
    } else {
        "request failed"
    };
    AppError::Notification(format!("Feishu {operation} {detail}"))
}

#[cfg(test)]
mod tests {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

    #[test]
    fn rejects_group_receive_id_type() {
        assert!(validate_receive_id_type("chat_id").is_err());
    }

    #[tokio::test]
    async fn authenticates_then_sends_private_card() {
        let (api_base, requests) = mock_server().await;
        let notifier = FeishuAppNotifier::new(
            api_base,
            "cli_test".into(),
            "secret_test".into(),
            "me@example.com".into(),
            "email".into(),
            5,
        )
        .unwrap();

        notifier.test().await.unwrap();
        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 2);

        let auth_body: Value = serde_json::from_slice(request_body(&requests[0])).unwrap();
        assert_eq!(auth_body["app_id"], "cli_test");
        assert_eq!(auth_body["app_secret"], "secret_test");

        let message_headers = String::from_utf8_lossy(request_headers(&requests[1]));
        assert!(message_headers.contains("receive_id_type=email"));
        assert!(message_headers.contains("authorization: Bearer t-test"));
        let message_body: Value = serde_json::from_slice(request_body(&requests[1])).unwrap();
        assert_eq!(message_body["receive_id"], "me@example.com");
        assert_eq!(message_body["msg_type"], "interactive");
        let card: Value = serde_json::from_str(message_body["content"].as_str().unwrap()).unwrap();
        assert_eq!(card["header"]["template"], "blue");
    }

    async fn mock_server() -> (String, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response_body in [
                r#"{"code":0,"msg":"ok","tenant_access_token":"t-test","expire":7200}"#,
                r#"{"code":0,"msg":"success","data":{"message_id":"om_test"}}"#,
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                requests.push(request);
            }
            requests
        });
        (format!("http://{address}"), task)
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
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
        request
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

    fn request_headers(request: &[u8]) -> &[u8] {
        &request[..find_header_end(request).unwrap()]
    }

    fn request_body(request: &[u8]) -> &[u8] {
        &request[find_header_end(request).unwrap() + 4..]
    }
}
