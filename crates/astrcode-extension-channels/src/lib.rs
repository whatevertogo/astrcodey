//! Bundled external channel extension.
//!
//! Channel-specific transport, config, and runtime state live in this crate.
//! The host only grants the extension explicit session-control capability.

use std::{
    collections::{HashMap, hash_map::RandomState},
    hash::BuildHasher,
    sync::Arc,
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
use serde::{Deserialize, Serialize, de::DeserializeOwned};
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

impl TelegramChannelConfig {
    fn active_bot_token(&self) -> Result<Option<String>, ExtensionError> {
        if !self.enabled {
            return Ok(None);
        }
        resolve_bot_token(self).map(|token| (!token.is_empty()).then_some(token))
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
    runtime: ParkingMutex<Option<Arc<TelegramRuntime>>>,
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
        let runtime = Arc::new(TelegramRuntime::new(
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

struct TelegramRuntime {
    config: ParkingMutex<ChannelsConfig>,
    startup_working_dir: String,
    sessions_by_chat: ParkingMutex<HashMap<String, String>>,
    session_ops: Arc<dyn SessionOperations>,
    telegram: Arc<dyn TelegramApi>,
}

impl TelegramRuntime {
    fn new(
        config: ChannelsConfig,
        startup_working_dir: String,
        session_ops: Arc<dyn SessionOperations>,
        telegram: Arc<dyn TelegramApi>,
    ) -> Self {
        Self {
            config: ParkingMutex::new(config),
            startup_working_dir,
            sessions_by_chat: ParkingMutex::new(HashMap::new()),
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

        let session_id = self.session_for_chat(&inbound.chat_id, cfg).await?;
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

    async fn session_for_chat(
        &self,
        chat_id: &str,
        cfg: &TelegramChannelConfig,
    ) -> Result<String, ExtensionError> {
        let cached_session_id = self.sessions_by_chat.lock().get(chat_id).cloned();
        if let Some(session_id) = cached_session_id {
            if self.cached_session_alive(&session_id).await {
                return Ok(session_id);
            }
            self.sessions_by_chat.lock().remove(chat_id);
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

        self.sessions_by_chat
            .lock()
            .insert(chat_id.to_owned(), handle.session_id.clone());
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

#[derive(Default)]
struct TelegramPollState {
    token_hasher: RandomState,
    token_fingerprint: Option<u64>,
    offset: Option<i64>,
    commands_registered: bool,
}

impl TelegramPollState {
    fn activate(&mut self, bot_token: &str) {
        let fingerprint = self.token_hasher.hash_one(bot_token);
        if self.token_fingerprint != Some(fingerprint) {
            self.token_fingerprint = Some(fingerprint);
            self.offset = None;
            self.commands_registered = false;
        }
    }

    fn observe(&mut self, update_id: i64) {
        self.offset = Some(self.offset.unwrap_or(update_id).max(update_id + 1));
    }
}

async fn wait_to_retry(shutdown: &CancellationToken) -> bool {
    tokio::select! {
        () = shutdown.cancelled() => false,
        () = tokio::time::sleep(Duration::from_secs(CONFIG_SLEEP_SECS)) => true,
    }
}

async fn poll_telegram(runtime: Arc<TelegramRuntime>, shutdown: CancellationToken) {
    let mut state = TelegramPollState::default();
    loop {
        if shutdown.is_cancelled() {
            break;
        }

        let cfg = runtime.current_config().telegram;
        let bot_token = match cfg.active_bot_token() {
            Ok(Some(token)) => token,
            inactive => {
                state.commands_registered = false;
                if let Err(error) = inactive {
                    tracing::warn!(
                        extension_id = EXTENSION_ID,
                        error = %error,
                        "telegram bot token is not available"
                    );
                }
                if !wait_to_retry(&shutdown).await {
                    break;
                }
                continue;
            },
        };

        state.activate(&bot_token);
        if cfg.streaming {
            tracing::warn!(
                extension_id = EXTENSION_ID,
                "telegram streaming=true is accepted but not active yet; replies are sent after \
                 the AstrCode turn completes"
            );
        }
        if cfg.register_commands && !state.commands_registered {
            match runtime
                .telegram
                .set_commands(&bot_token, telegram_commands(), cfg.request_timeout_secs)
                .await
            {
                Ok(()) => state.commands_registered = true,
                Err(error) => tracing::warn!(
                    extension_id = EXTENSION_ID,
                    error = %error,
                    "telegram setMyCommands failed"
                ),
            }
        } else if !cfg.register_commands {
            state.commands_registered = false;
        }

        let updates = tokio::select! {
            () = shutdown.cancelled() => break,
            result = runtime.telegram.get_updates(
                &bot_token,
                state.offset,
                cfg.poll_timeout_secs,
                cfg.request_timeout_secs,
            ) => result,
        };

        match updates {
            Ok(updates) => {
                for update in updates {
                    state.observe(update.update_id);
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
                if !wait_to_retry(&shutdown).await {
                    break;
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
    use std::sync::Mutex;

    use astrcode_extension_sdk::tool::{
        SessionApiError, SessionDeliveryOutcome, SessionHandle, SessionStatus,
    };

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
        ) -> Result<SessionDeliveryOutcome, SessionApiError> {
            Ok(SessionDeliveryOutcome::Started {
                turn_id: "test-turn".into(),
            })
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

        async fn resolve_tool_ui_response(
            &self,
            _target_session_id: &str,
            _call_id: &str,
            _answers: std::collections::BTreeMap<String, String>,
        ) -> Result<(), SessionApiError> {
            Ok(())
        }
    }

    struct TestHarness {
        runtime: TelegramRuntime,
        session_ops: Arc<FakeSessionOps>,
        telegram: Arc<FakeTelegram>,
    }

    impl TestHarness {
        fn new(allowed_chat_ids: &[&str], allow_all_chats: bool) -> Self {
            let session_ops = Arc::new(FakeSessionOps::default());
            let telegram = Arc::new(FakeTelegram::default());
            let runtime = TelegramRuntime::new(
                ChannelsConfig {
                    telegram: TelegramChannelConfig {
                        enabled: true,
                        bot_token: Some("token".into()),
                        allowed_chat_ids: allowed_chat_ids
                            .iter()
                            .map(|chat_id| (*chat_id).into())
                            .collect(),
                        allow_all_chats,
                        max_reply_chars: 100,
                        ..Default::default()
                    },
                },
                "D:/workspace".into(),
                session_ops.clone(),
                telegram.clone(),
            );
            Self {
                runtime,
                session_ops,
                telegram,
            }
        }

        async fn handle(&self, chat_id: &str, text: &str) {
            let cfg = self.runtime.current_config().telegram;
            self.runtime
                .handle_inbound(
                    &cfg,
                    InboundMessage {
                        chat_id: chat_id.into(),
                        text: text.into(),
                    },
                )
                .await
                .unwrap();
        }

        fn root_create_count(&self) -> usize {
            self.session_ops.root_creates.lock().unwrap().len()
        }

        fn submitted_prompts(&self) -> Vec<String> {
            self.session_ops.submitted_prompts.lock().unwrap().clone()
        }

        fn sent_messages(&self) -> Vec<(String, String)> {
            self.telegram.sent.lock().unwrap().clone()
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
        assert_eq!(
            TelegramChannelConfig {
                enabled: false,
                bot_token_env: Some("ASTRCODE_TEST_MISSING_TELEGRAM_TOKEN".into()),
                ..Default::default()
            }
            .active_bot_token()
            .unwrap(),
            None
        );
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
    fn poll_state_resets_cursor_only_when_bot_changes() {
        let mut state = TelegramPollState::default();

        state.activate("first");
        let first_fingerprint = state.token_fingerprint;
        state.observe(4);
        state.observe(2);
        state.commands_registered = true;
        state.activate("first");
        assert_eq!(state.token_fingerprint, first_fingerprint);
        assert_eq!(state.offset, Some(5));
        assert!(state.commands_registered);

        state.activate("second");
        assert_ne!(state.token_fingerprint, first_fingerprint);
        assert_eq!(state.offset, None);
        assert!(!state.commands_registered);
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
        let test = TestHarness::new(&["42"], false);

        test.handle("42", "first").await;
        test.handle("42", "second").await;

        assert_eq!(test.root_create_count(), 1);
        assert_eq!(
            test.submitted_prompts(),
            vec!["first".to_string(), "second".to_string()]
        );
        assert_eq!(
            test.sent_messages(),
            vec![
                ("42".to_string(), "reply: first".to_string()),
                ("42".to_string(), "reply: second".to_string())
            ]
        );
    }

    #[tokio::test]
    async fn telegram_commands_reply_without_session_turn() {
        let test = TestHarness::new(&["42"], false);

        test.handle("42", "/help@my_bot").await;

        assert_eq!(test.root_create_count(), 0);
        assert!(test.submitted_prompts().is_empty());
        let sent = test.sent_messages();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].1.contains("AstrCode is ready"));
    }

    #[tokio::test]
    async fn chat_access_policy_is_explicit() {
        let cases: [(&str, &[&str], bool, &str, bool); 3] = [
            ("unlisted chat", &["42"], false, "99", false),
            ("empty allowlist", &[], false, "42", false),
            ("allow all", &[], true, "42", true),
        ];

        for (name, allowed_chat_ids, allow_all_chats, chat_id, allowed) in cases {
            let test = TestHarness::new(allowed_chat_ids, allow_all_chats);

            test.handle(chat_id, "request").await;

            assert_eq!(test.root_create_count(), usize::from(allowed), "{name}");
            assert_eq!(
                test.submitted_prompts().len(),
                usize::from(allowed),
                "{name}"
            );
            assert_eq!(test.sent_messages().len(), usize::from(allowed), "{name}");
        }
    }
}
