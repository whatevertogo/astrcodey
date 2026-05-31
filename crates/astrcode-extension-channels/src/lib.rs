//! Bundled external channel extension.
//!
//! Channel-specific transport, config, and runtime state live in this crate.
//! The host only grants the extension explicit session-control capability.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use astrcode_extension_sdk::{
    extension::{
        Extension, ExtensionCapability, ExtensionConfig, ExtensionCtx, ExtensionError, Registrar,
        StopReason,
    },
    tool::{
        CreateRootSessionRequest, SessionAccess, SessionOperations, SubmitTurnRequest,
        SubmitTurnResult,
    },
};
use parking_lot::Mutex as ParkingMutex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const EXTENSION_ID: &str = "astrcode-channels";
const TELEGRAM_API_BASE: &str = "https://api.telegram.org";
const CONFIG_SLEEP_SECS: u64 = 5;

pub fn extension() -> Arc<dyn Extension> {
    Arc::new(TelegramChannelsExtension::new())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChannelsConfig {
    #[serde(default)]
    pub telegram: TelegramChannelConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TelegramChannelConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub bot_token: Option<String>,
    #[serde(default)]
    pub bot_token_env: Option<String>,
    #[serde(default)]
    pub allowed_chat_ids: Vec<String>,
    #[serde(default)]
    pub allow_all_chats: bool,
    #[serde(default)]
    pub register_commands: bool,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,
    #[serde(default = "default_poll_timeout_secs")]
    pub poll_timeout_secs: u64,
    #[serde(default = "default_max_reply_chars")]
    pub max_reply_chars: usize,
}

impl Default for TelegramChannelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token: None,
            bot_token_env: None,
            allowed_chat_ids: Vec::new(),
            allow_all_chats: false,
            register_commands: false,
            streaming: false,
            working_dir: None,
            request_timeout_secs: default_request_timeout_secs(),
            poll_timeout_secs: default_poll_timeout_secs(),
            max_reply_chars: default_max_reply_chars(),
        }
    }
}

const fn default_request_timeout_secs() -> u64 {
    30
}

const fn default_poll_timeout_secs() -> u64 {
    25
}

const fn default_max_reply_chars() -> usize {
    3500
}

struct TelegramChannelsExtension {
    runtime: ParkingMutex<Option<Arc<ChannelRuntime>>>,
}

impl TelegramChannelsExtension {
    fn new() -> Self {
        Self {
            runtime: ParkingMutex::new(None),
        }
    }

    fn load_config(config: &ExtensionConfig) -> Result<ChannelsConfig, ExtensionError> {
        config
            .deserialize::<ChannelsConfig>()
            .map_err(|e| ExtensionError::Internal(format!("invalid channels config: {e}")))
    }

    fn startup_working_dir(ctx: &ExtensionCtx) -> String {
        ctx.startup_working_dir()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| ".".into())
    }
}

#[async_trait::async_trait]
impl Extension for TelegramChannelsExtension {
    fn id(&self) -> &str {
        EXTENSION_ID
    }

    fn capabilities(&self) -> &[ExtensionCapability] {
        &[
            ExtensionCapability::SessionControl,
            ExtensionCapability::NetworkClient,
        ]
    }

    fn register(&self, _: &mut Registrar) {}

    async fn start(&self, ctx: ExtensionCtx) -> Result<(), ExtensionError> {
        let config = Self::load_config(&ctx.config)?;
        let startup_working_dir = Self::startup_working_dir(&ctx);
        let session_ops = ctx
            .host_services()
            .and_then(|services| services.session_ops.clone())
            .ok_or_else(|| {
                ExtensionError::Internal(
                    "telegram channel extension requires session_control host service".into(),
                )
            })?;

        let api = Arc::new(HttpTelegramApi::new());
        let runtime = Arc::new(ChannelRuntime::new(
            config,
            startup_working_dir,
            session_ops,
            api,
        ));
        ctx.tasks().spawn(
            "telegram-channel-poll",
            poll_telegram(Arc::clone(&runtime), ctx.shutdown()),
        );
        *self.runtime.lock() = Some(runtime);
        tracing::info!(
            extension_id = EXTENSION_ID,
            "telegram channel runtime started"
        );
        Ok(())
    }

    async fn stop(&self, _: StopReason) -> Result<(), ExtensionError> {
        self.runtime.lock().take();
        Ok(())
    }

    async fn on_config_changed(&self, config: ExtensionConfig) -> Result<(), ExtensionError> {
        let parsed = Self::load_config(&config)?;
        if let Some(runtime) = self.runtime.lock().as_ref() {
            runtime.update_config(parsed);
        }
        Ok(())
    }
}

struct ChannelRuntime {
    config: ParkingMutex<ChannelsConfig>,
    startup_working_dir: String,
    sessions_by_channel: Mutex<HashMap<String, String>>,
    session_ops: Arc<dyn SessionOperations>,
    telegram: Arc<dyn TelegramApi>,
}

impl ChannelRuntime {
    fn new(
        config: ChannelsConfig,
        startup_working_dir: String,
        session_ops: Arc<dyn SessionOperations>,
        telegram: Arc<dyn TelegramApi>,
    ) -> Self {
        Self {
            config: ParkingMutex::new(config),
            startup_working_dir,
            sessions_by_channel: Mutex::new(HashMap::new()),
            session_ops,
            telegram,
        }
    }

    fn update_config(&self, config: ChannelsConfig) {
        *self.config.lock() = config;
    }

    fn current_config(&self) -> ChannelsConfig {
        self.config.lock().clone()
    }

    fn is_allowed(&self, cfg: &TelegramChannelConfig, chat_id: &str) -> bool {
        cfg.allow_all_chats
            || cfg
                .allowed_chat_ids
                .iter()
                .any(|allowed| allowed == chat_id)
    }

    async fn handle_inbound(
        &self,
        cfg: &TelegramChannelConfig,
        inbound: InboundMessage,
    ) -> Result<(), ExtensionError> {
        if !self.is_allowed(cfg, &inbound.chat_id) {
            tracing::warn!(
                extension_id = EXTENSION_ID,
                chat_id = %inbound.chat_id,
                "ignored telegram message from unauthorized chat"
            );
            return Ok(());
        }
        if let Some(reply) = channel_command_reply(&inbound.text) {
            return self.send_reply(cfg, &inbound.chat_id, reply).await;
        }

        let session_id = self
            .session_for_channel("telegram", &inbound.chat_id, cfg)
            .await?;
        let result = self
            .session_ops
            .submit_turn(SubmitTurnRequest::for_session(session_id, inbound.text))
            .await;

        let reply = match result {
            Ok(SubmitTurnResult::Completed { content }) => content,
            Ok(SubmitTurnResult::Backgrounded { task_id, .. }) => {
                format!("AstrCode task started in background: {task_id}")
            },
            Err(error) => format!("AstrCode failed to handle the message: {error}"),
        };
        self.send_reply(cfg, &inbound.chat_id, &reply).await
    }

    async fn session_for_channel(
        &self,
        channel: &str,
        external_id: &str,
        cfg: &TelegramChannelConfig,
    ) -> Result<String, ExtensionError> {
        let key = session_key(channel, external_id);
        let cached_session_id = self
            .sessions_by_channel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned();
        if let Some(session_id) = cached_session_id {
            if self.cached_session_alive(&session_id).await {
                return Ok(session_id);
            }
            self.sessions_by_channel
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
        }
        let working_dir = cfg
            .working_dir
            .clone()
            .unwrap_or_else(|| self.startup_working_dir.clone());

        let handle = self
            .session_ops
            .create_root_session(CreateRootSessionRequest {
                working_dir,
                source_extension: Some(EXTENSION_ID.into()),
            })
            .await
            .map_err(|e| ExtensionError::Internal(format!("create telegram session: {e}")))?;

        self.sessions_by_channel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, handle.session_id.clone());
        Ok(handle.session_id)
    }

    async fn cached_session_alive(&self, session_id: &str) -> bool {
        matches!(
            self.session_ops
                .query_session(SessionAccess::same(session_id))
                .await,
            Ok(status) if status.alive
        )
    }

    async fn send_reply(
        &self,
        cfg: &TelegramChannelConfig,
        chat_id: &str,
        text: &str,
    ) -> Result<(), ExtensionError> {
        let bot_token = resolve_bot_token(cfg)?;
        for chunk in split_reply(text, cfg.max_reply_chars.max(1)) {
            self.telegram
                .send_message(&bot_token, chat_id, &chunk, cfg.request_timeout_secs)
                .await
                .map_err(|e| ExtensionError::Internal(format!("telegram sendMessage: {e}")))?;
        }
        Ok(())
    }
}

async fn poll_telegram(runtime: Arc<ChannelRuntime>, shutdown: CancellationToken) {
    let mut offset: Option<i64> = None;
    let mut registered_commands_for: Option<String> = None;
    loop {
        if shutdown.is_cancelled() {
            break;
        }

        let cfg = runtime.current_config().telegram;
        let bot_token = match resolve_bot_token(&cfg) {
            Ok(token) => token,
            Err(error) => {
                tracing::warn!(
                    extension_id = EXTENSION_ID,
                    error = %error,
                    "telegram bot token is not available"
                );
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep(Duration::from_secs(CONFIG_SLEEP_SECS)) => {},
                }
                continue;
            },
        };

        if !cfg.enabled || bot_token.is_empty() {
            registered_commands_for = None;
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(Duration::from_secs(CONFIG_SLEEP_SECS)) => {},
            }
            continue;
        }
        if cfg.streaming {
            tracing::warn!(
                extension_id = EXTENSION_ID,
                "telegram streaming=true is accepted but not active yet; replies are sent after \
                 the AstrCode turn completes"
            );
        }
        if cfg.register_commands && registered_commands_for.as_deref() != Some(&bot_token) {
            match runtime
                .telegram
                .set_commands(&bot_token, telegram_commands(), cfg.request_timeout_secs)
                .await
            {
                Ok(()) => registered_commands_for = Some(bot_token.clone()),
                Err(error) => tracing::warn!(
                    extension_id = EXTENSION_ID,
                    error = %error,
                    "telegram setMyCommands failed"
                ),
            }
        } else if !cfg.register_commands {
            registered_commands_for = None;
        }

        let updates = tokio::select! {
            () = shutdown.cancelled() => break,
            result = runtime.telegram.get_updates(
                &bot_token,
                offset,
                cfg.poll_timeout_secs,
                cfg.request_timeout_secs,
            ) => result,
        };

        match updates {
            Ok(updates) => {
                for update in updates {
                    offset = Some(offset.unwrap_or(update.update_id).max(update.update_id + 1));
                    if let Some(inbound) = inbound_message(update) {
                        if let Err(error) = runtime.handle_inbound(&cfg, inbound).await {
                            tracing::warn!(
                                extension_id = EXTENSION_ID,
                                error = %error,
                                "telegram inbound message failed"
                            );
                        }
                    }
                }
            },
            Err(error) => {
                tracing::warn!(
                    extension_id = EXTENSION_ID,
                    error = %error,
                    "telegram getUpdates failed"
                );
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep(Duration::from_secs(CONFIG_SLEEP_SECS)) => {},
                }
            },
        }
    }
}

#[async_trait::async_trait]
trait TelegramApi: Send + Sync {
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

struct HttpTelegramApi {
    client: reqwest::Client,
}

impl HttpTelegramApi {
    fn new() -> Self {
        let client = reqwest::Client::new();
        Self { client }
    }

    fn method_url(bot_token: &str, method: &str) -> String {
        format!("{TELEGRAM_API_BASE}/bot{bot_token}/{method}")
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
        let response = self
            .client
            .post(Self::method_url(bot_token, "getUpdates"))
            .timeout(Duration::from_secs(request_timeout_secs.max(1)))
            .json(&body)
            .send()
            .await?
            .json::<TelegramResponse<Vec<TelegramUpdate>>>()
            .await?;

        telegram_result(response)
    }

    async fn send_message(
        &self,
        bot_token: &str,
        chat_id: &str,
        text: &str,
        request_timeout_secs: u64,
    ) -> Result<(), TelegramError> {
        let response = self
            .client
            .post(Self::method_url(bot_token, "sendMessage"))
            .timeout(Duration::from_secs(request_timeout_secs.max(1)))
            .json(&json!({
                "chat_id": chat_id,
                "text": text,
            }))
            .send()
            .await?
            .json::<TelegramResponse<serde_json::Value>>()
            .await?;
        telegram_result(response).map(|_| ())
    }

    async fn set_commands(
        &self,
        bot_token: &str,
        commands: Vec<TelegramBotCommand>,
        request_timeout_secs: u64,
    ) -> Result<(), TelegramError> {
        let response = self
            .client
            .post(Self::method_url(bot_token, "setMyCommands"))
            .timeout(Duration::from_secs(request_timeout_secs.max(1)))
            .json(&json!({ "commands": commands }))
            .send()
            .await?
            .json::<TelegramResponse<bool>>()
            .await?;
        telegram_result(response).map(|_| ())
    }
}

#[derive(Debug, thiserror::Error)]
enum TelegramError {
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

fn telegram_result<T>(response: TelegramResponse<T>) -> Result<T, TelegramError> {
    if response.ok {
        response
            .result
            .ok_or_else(|| TelegramError::Api("telegram response missing result".into()))
    } else {
        Err(TelegramError::Api(
            response
                .description
                .unwrap_or_else(|| "telegram api error".into()),
        ))
    }
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
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
struct TelegramBotCommand {
    command: &'static str,
    description: &'static str,
}

struct InboundMessage {
    chat_id: String,
    text: String,
}

fn inbound_message(update: TelegramUpdate) -> Option<InboundMessage> {
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

fn split_reply(text: &str, max_chars: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;
    for ch in text.chars() {
        if current_len >= max_chars {
            chunks.push(std::mem::take(&mut current));
            current_len = 0;
        }
        current.push(ch);
        current_len += 1;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn session_key(channel: &str, external_id: &str) -> String {
    format!("{channel}:{external_id}")
}

fn telegram_commands() -> Vec<TelegramBotCommand> {
    vec![
        TelegramBotCommand {
            command: "start",
            description: "Start using AstrCode in this chat",
        },
        TelegramBotCommand {
            command: "help",
            description: "Show AstrCode Telegram usage",
        },
    ]
}

fn channel_command_reply(text: &str) -> Option<&'static str> {
    let command = text.split_whitespace().next()?;
    if matches!(command, "/start" | "/help")
        || command.starts_with("/start@")
        || command.starts_with("/help@")
    {
        Some(
            "AstrCode is ready. Send a coding request or question, and I will run it in the \
             configured workspace.",
        )
    } else {
        None
    }
}

fn resolve_bot_token(cfg: &TelegramChannelConfig) -> Result<String, ExtensionError> {
    if let Some(raw) = cfg
        .bot_token
        .as_deref()
        .filter(|token| !token.trim().is_empty())
    {
        return resolve_secret_ref(raw);
    }
    if let Some(env_name) = cfg
        .bot_token_env
        .as_deref()
        .filter(|name| !name.trim().is_empty())
    {
        return resolve_env_token(env_name);
    }
    Ok(String::new())
}

fn resolve_secret_ref(raw: &str) -> Result<String, ExtensionError> {
    let trimmed = raw.trim();
    if let Some(var_name) = trimmed.strip_prefix("env:") {
        return resolve_env_token(var_name);
    }
    Ok(trimmed.to_string())
}

fn resolve_env_token(raw_env_name: &str) -> Result<String, ExtensionError> {
    let env_name = raw_env_name
        .trim()
        .strip_prefix("env:")
        .unwrap_or(raw_env_name.trim());
    std::env::var(env_name).map_err(|_| {
        ExtensionError::Internal(format!(
            "missing telegram token environment variable: {env_name}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use astrcode_extension_sdk::tool::{SessionApiError, SessionHandle, SessionStatus};

    use super::*;

    #[derive(Default)]
    struct FakeTelegram {
        sent: Mutex<Vec<(String, String)>>,
        commands: Mutex<Vec<TelegramBotCommand>>,
    }

    #[async_trait::async_trait]
    impl TelegramApi for FakeTelegram {
        async fn get_updates(
            &self,
            _: &str,
            _: Option<i64>,
            _: u64,
            _: u64,
        ) -> Result<Vec<TelegramUpdate>, TelegramError> {
            Ok(Vec::new())
        }

        async fn send_message(
            &self,
            _: &str,
            chat_id: &str,
            text: &str,
            _: u64,
        ) -> Result<(), TelegramError> {
            self.sent
                .lock()
                .unwrap()
                .push((chat_id.to_string(), text.to_string()));
            Ok(())
        }

        async fn set_commands(
            &self,
            _: &str,
            commands: Vec<TelegramBotCommand>,
            _: u64,
        ) -> Result<(), TelegramError> {
            *self.commands.lock().unwrap() = commands;
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeSessionOps {
        root_creates: Mutex<Vec<CreateRootSessionRequest>>,
        submitted_prompts: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl SessionOperations for FakeSessionOps {
        async fn create_root_session(
            &self,
            request: CreateRootSessionRequest,
        ) -> Result<SessionHandle, SessionApiError> {
            let mut creates = self.root_creates.lock().unwrap();
            creates.push(request);
            Ok(SessionHandle {
                session_id: format!("session-{}", creates.len()),
            })
        }

        async fn submit_turn(
            &self,
            request: SubmitTurnRequest,
        ) -> Result<SubmitTurnResult, SessionApiError> {
            self.submitted_prompts
                .lock()
                .unwrap()
                .push(request.user_prompt.clone());
            Ok(SubmitTurnResult::Completed {
                content: format!("reply: {}", request.user_prompt),
            })
        }

        async fn query_session(
            &self,
            _access: SessionAccess<'_>,
        ) -> Result<SessionStatus, SessionApiError> {
            Ok(SessionStatus {
                alive: true,
                has_active_turn: false,
                last_finish_reason: None,
                message_count: 0,
            })
        }

        async fn create_session(
            &self,
            _parent_session_id: &str,
            _request: astrcode_extension_sdk::tool::CreateSessionRequest,
        ) -> Result<SessionHandle, SessionApiError> {
            Err(SessionApiError::internal_msg("unused in channels tests"))
        }

        async fn inject_message(
            &self,
            _access: SessionAccess<'_>,
            _content: String,
        ) -> Result<(), SessionApiError> {
            Ok(())
        }

        async fn recycle_session(&self, _access: SessionAccess<'_>) -> Result<(), SessionApiError> {
            Ok(())
        }

        async fn delete_session(&self, _access: SessionAccess<'_>) -> Result<(), SessionApiError> {
            Ok(())
        }

        async fn restore_session(&self, _access: SessionAccess<'_>) -> Result<(), SessionApiError> {
            Ok(())
        }

        async fn resolve_tool_approval(
            &self,
            _target_session_id: &str,
            _call_id: &str,
            _decision: astrcode_extension_sdk::permission::ApprovalDecision,
        ) -> Result<(), SessionApiError> {
            Ok(())
        }
    }

    #[test]
    fn nested_config_deserializes_with_defaults() {
        let cfg: ChannelsConfig = serde_json::from_value(json!({
            "telegram": {
                "enabled": true,
                "botToken": "env:TELEGRAM_BOT_TOKEN",
                "allowedChatIds": ["1"],
                "registerCommands": true,
                "streaming": true,
                "workingDir": "C:/tmp"
            }
        }))
        .unwrap();
        assert!(cfg.telegram.enabled);
        assert_eq!(
            cfg.telegram.bot_token.as_deref(),
            Some("env:TELEGRAM_BOT_TOKEN")
        );
        assert_eq!(cfg.telegram.allowed_chat_ids, vec!["1"]);
        assert!(cfg.telegram.register_commands);
        assert!(cfg.telegram.streaming);
        assert!(!cfg.telegram.allow_all_chats);
        assert_eq!(cfg.telegram.request_timeout_secs, 30);
        assert_eq!(cfg.telegram.poll_timeout_secs, 25);
    }

    #[test]
    fn flat_config_is_rejected() {
        let result = TelegramChannelsExtension::load_config(&ExtensionConfig(json!({
            "enabled": true,
            "botToken": "x",
            "allowedChatIds": ["1"]
        })));

        assert!(result.is_err());
    }

    #[test]
    fn inbound_message_extracts_text_message() {
        let update: TelegramUpdate = serde_json::from_value(json!({
            "update_id": 12,
            "message": {
                "chat": { "id": 42 },
                "text": " hello "
            }
        }))
        .unwrap();

        let inbound = inbound_message(update).unwrap();

        assert_eq!(inbound.chat_id, "42");
        assert_eq!(inbound.text, "hello");
    }

    #[test]
    fn split_reply_respects_character_limit() {
        assert_eq!(split_reply("abcdef", 2), vec!["ab", "cd", "ef"]);
        assert_eq!(split_reply("你好世界", 2), vec!["你好", "世界"]);
    }

    #[test]
    fn bot_token_supports_env_reference() {
        std::env::set_var("ASTRCODE_TEST_TELEGRAM_TOKEN", "token-from-env");
        assert_eq!(
            resolve_bot_token(&TelegramChannelConfig {
                bot_token: Some("env:ASTRCODE_TEST_TELEGRAM_TOKEN".into()),
                ..Default::default()
            })
            .unwrap(),
            "token-from-env"
        );
        assert_eq!(
            resolve_bot_token(&TelegramChannelConfig {
                bot_token: Some(" literal ".into()),
                ..Default::default()
            })
            .unwrap(),
            "literal"
        );
        assert_eq!(
            resolve_bot_token(&TelegramChannelConfig {
                bot_token_env: Some("ASTRCODE_TEST_TELEGRAM_TOKEN".into()),
                ..Default::default()
            })
            .unwrap(),
            "token-from-env"
        );
        assert_eq!(
            resolve_bot_token(&TelegramChannelConfig {
                bot_token_env: Some("env:ASTRCODE_TEST_TELEGRAM_TOKEN".into()),
                ..Default::default()
            })
            .unwrap(),
            "token-from-env"
        );
        std::env::remove_var("ASTRCODE_TEST_TELEGRAM_TOKEN");
    }

    #[test]
    fn telegram_commands_include_start_and_help() {
        let commands = telegram_commands();
        assert_eq!(
            commands,
            vec![
                TelegramBotCommand {
                    command: "start",
                    description: "Start using AstrCode in this chat"
                },
                TelegramBotCommand {
                    command: "help",
                    description: "Show AstrCode Telegram usage"
                }
            ]
        );
    }

    #[tokio::test]
    async fn inbound_messages_reuse_chat_session() {
        let session_ops = Arc::new(FakeSessionOps::default());
        let telegram = Arc::new(FakeTelegram::default());
        let runtime = ChannelRuntime::new(
            ChannelsConfig {
                telegram: TelegramChannelConfig {
                    enabled: true,
                    bot_token: Some("token".into()),
                    allowed_chat_ids: vec!["42".into()],
                    max_reply_chars: 100,
                    ..Default::default()
                },
            },
            "D:/workspace".into(),
            session_ops.clone(),
            telegram.clone(),
        );

        runtime
            .handle_inbound(
                &runtime.current_config().telegram,
                InboundMessage {
                    chat_id: "42".into(),
                    text: "first".into(),
                },
            )
            .await
            .unwrap();
        runtime
            .handle_inbound(
                &runtime.current_config().telegram,
                InboundMessage {
                    chat_id: "42".into(),
                    text: "second".into(),
                },
            )
            .await
            .unwrap();

        assert_eq!(session_ops.root_creates.lock().unwrap().len(), 1);
        assert_eq!(
            *session_ops.submitted_prompts.lock().unwrap(),
            vec!["first".to_string(), "second".to_string()]
        );
        assert_eq!(
            *telegram.sent.lock().unwrap(),
            vec![
                ("42".to_string(), "reply: first".to_string()),
                ("42".to_string(), "reply: second".to_string())
            ]
        );
    }

    #[tokio::test]
    async fn telegram_commands_reply_without_session_turn() {
        let session_ops = Arc::new(FakeSessionOps::default());
        let telegram = Arc::new(FakeTelegram::default());
        let runtime = ChannelRuntime::new(
            ChannelsConfig {
                telegram: TelegramChannelConfig {
                    enabled: true,
                    bot_token: Some("token".into()),
                    allowed_chat_ids: vec!["42".into()],
                    ..Default::default()
                },
            },
            "D:/workspace".into(),
            session_ops.clone(),
            telegram.clone(),
        );

        runtime
            .handle_inbound(
                &runtime.current_config().telegram,
                InboundMessage {
                    chat_id: "42".into(),
                    text: "/help@my_bot".into(),
                },
            )
            .await
            .unwrap();

        assert!(session_ops.root_creates.lock().unwrap().is_empty());
        assert!(session_ops.submitted_prompts.lock().unwrap().is_empty());
        assert_eq!(telegram.sent.lock().unwrap().len(), 1);
        assert!(
            telegram.sent.lock().unwrap()[0]
                .1
                .contains("AstrCode is ready")
        );
    }

    #[tokio::test]
    async fn unauthorized_chat_does_not_create_session() {
        let session_ops = Arc::new(FakeSessionOps::default());
        let telegram = Arc::new(FakeTelegram::default());
        let runtime = ChannelRuntime::new(
            ChannelsConfig {
                telegram: TelegramChannelConfig {
                    enabled: true,
                    bot_token: Some("token".into()),
                    allowed_chat_ids: vec!["42".into()],
                    ..Default::default()
                },
            },
            "D:/workspace".into(),
            session_ops.clone(),
            telegram.clone(),
        );

        runtime
            .handle_inbound(
                &runtime.current_config().telegram,
                InboundMessage {
                    chat_id: "99".into(),
                    text: "blocked".into(),
                },
            )
            .await
            .unwrap();

        assert!(session_ops.root_creates.lock().unwrap().is_empty());
        assert!(telegram.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn empty_allowlist_rejects_by_default() {
        let session_ops = Arc::new(FakeSessionOps::default());
        let telegram = Arc::new(FakeTelegram::default());
        let runtime = ChannelRuntime::new(
            ChannelsConfig {
                telegram: TelegramChannelConfig {
                    enabled: true,
                    bot_token: Some("token".into()),
                    allowed_chat_ids: Vec::new(),
                    allow_all_chats: false,
                    ..Default::default()
                },
            },
            "D:/workspace".into(),
            session_ops.clone(),
            telegram.clone(),
        );

        runtime
            .handle_inbound(
                &runtime.current_config().telegram,
                InboundMessage {
                    chat_id: "42".into(),
                    text: "blocked".into(),
                },
            )
            .await
            .unwrap();

        assert!(session_ops.root_creates.lock().unwrap().is_empty());
        assert!(telegram.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn allow_all_chats_is_explicit() {
        let session_ops = Arc::new(FakeSessionOps::default());
        let telegram = Arc::new(FakeTelegram::default());
        let runtime = ChannelRuntime::new(
            ChannelsConfig {
                telegram: TelegramChannelConfig {
                    enabled: true,
                    bot_token: Some("token".into()),
                    allow_all_chats: true,
                    ..Default::default()
                },
            },
            "D:/workspace".into(),
            session_ops.clone(),
            telegram.clone(),
        );

        runtime
            .handle_inbound(
                &runtime.current_config().telegram,
                InboundMessage {
                    chat_id: "42".into(),
                    text: "allowed".into(),
                },
            )
            .await
            .unwrap();

        assert_eq!(session_ops.root_creates.lock().unwrap().len(), 1);
        assert_eq!(
            *telegram.sent.lock().unwrap(),
            vec![("42".to_string(), "reply: allowed".to_string())]
        );
    }
}
