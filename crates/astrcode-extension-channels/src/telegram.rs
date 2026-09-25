//! Telegram HTTP API, wire responses, and inbound-message decoding.

use std::time::Duration;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;

const TELEGRAM_API_BASE: &str = "https://api.telegram.org";

#[async_trait::async_trait]
pub(super) trait TelegramApi: Send + Sync {
    async fn get_updates(
        &self,
        bot_token: &str,
        offset: Option<i64>,
        timeout_secs: u64,
        request_timeout_secs: u64,
    ) -> Result<Vec<TelegramUpdate>, TelegramError>;

    async fn send_message(
        &self,
        bot_token: &str,
        chat_id: &str,
        text: &str,
        request_timeout_secs: u64,
    ) -> Result<(), TelegramError>;

    async fn set_commands(
        &self,
        bot_token: &str,
        commands: Vec<TelegramBotCommand>,
        request_timeout_secs: u64,
    ) -> Result<(), TelegramError>;
}

pub(super) struct HttpTelegramApi {
    client: reqwest::Client,
}

impl HttpTelegramApi {
    pub(super) fn new() -> Self {
        let client = reqwest::Client::new();
        Self { client }
    }

    fn method_url(bot_token: &str, method: &str) -> String {
        format!("{TELEGRAM_API_BASE}/bot{bot_token}/{method}")
    }

    async fn post<T: DeserializeOwned>(
        &self,
        bot_token: &str,
        method: &str,
        body: &impl Serialize,
        request_timeout_secs: u64,
    ) -> Result<T, TelegramError> {
        self.client
            .post(Self::method_url(bot_token, method))
            .timeout(Duration::from_secs(request_timeout_secs.max(1)))
            .json(body)
            .send()
            .await?
            .json::<TelegramResponse<T>>()
            .await?
            .into_result()
    }
}

#[async_trait::async_trait]
impl TelegramApi for HttpTelegramApi {
    async fn get_updates(
        &self,
        bot_token: &str,
        offset: Option<i64>,
        timeout_secs: u64,
        request_timeout_secs: u64,
    ) -> Result<Vec<TelegramUpdate>, TelegramError> {
        let mut body = json!({
            "timeout": timeout_secs,
            "allowed_updates": ["message"],
        });
        if let Some(offset) = offset {
            body["offset"] = json!(offset);
        }
        self.post(bot_token, "getUpdates", &body, request_timeout_secs)
            .await
    }

    async fn send_message(
        &self,
        bot_token: &str,
        chat_id: &str,
        text: &str,
        request_timeout_secs: u64,
    ) -> Result<(), TelegramError> {
        let _: serde_json::Value = self
            .post(
                bot_token,
                "sendMessage",
                &json!({
                "chat_id": chat_id,
                "text": text,
                }),
                request_timeout_secs,
            )
            .await?;
        Ok(())
    }

    async fn set_commands(
        &self,
        bot_token: &str,
        commands: Vec<TelegramBotCommand>,
        request_timeout_secs: u64,
    ) -> Result<(), TelegramError> {
        let _: bool = self
            .post(
                bot_token,
                "setMyCommands",
                &json!({ "commands": commands }),
                request_timeout_secs,
            )
            .await?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum TelegramError {
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("{0}")]
    Api(String),
}

#[derive(Debug, Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

impl<T> TelegramResponse<T> {
    fn into_result(self) -> Result<T, TelegramError> {
        if self.ok {
            self.result
                .ok_or_else(|| TelegramError::Api("telegram response missing result".into()))
        } else {
            Err(TelegramError::Api(
                self.description
                    .unwrap_or_else(|| "telegram api error".into()),
            ))
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct TelegramUpdate {
    pub(super) update_id: i64,
    message: Option<TelegramMessage>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramMessage {
    chat: TelegramChat,
    text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramChat {
    id: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct TelegramBotCommand {
    pub(super) command: &'static str,
    pub(super) description: &'static str,
}

pub(super) struct InboundMessage {
    pub(super) chat_id: String,
    pub(super) text: String,
}

pub(super) fn inbound_message(update: TelegramUpdate) -> Option<InboundMessage> {
    let message = update.message?;
    let text = message.text?.trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(InboundMessage {
        chat_id: message.chat.id.to_string(),
        text,
    })
}
