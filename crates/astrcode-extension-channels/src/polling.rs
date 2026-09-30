//! Telegram polling cursor and cancellation-aware retry loop.

use std::{collections::hash_map::RandomState, hash::BuildHasher, sync::Arc, time::Duration};

use tokio_util::sync::CancellationToken;

use super::{EXTENSION_ID, TelegramRuntime, telegram::inbound_message, telegram_commands};

const CONFIG_SLEEP_SECS: u64 = 5;

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

pub(super) async fn poll_telegram(runtime: Arc<TelegramRuntime>, shutdown: CancellationToken) {
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
                    if let Some(inbound) = inbound_message(update)
                        && let Err(error) = runtime.handle_inbound(&cfg, inbound).await
                    {
                        tracing::warn!(
                            extension_id = EXTENSION_ID,
                            error = %error,
                            "telegram inbound message failed"
                        );
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


#[cfg(test)]
mod tests {
    use super::*;

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
}
