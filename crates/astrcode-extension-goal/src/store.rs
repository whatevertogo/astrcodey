//! Session-local goal state persistence.

use std::{
    io,
    path::{Path, PathBuf},
};

use astrcode_extension_sdk::hostpaths;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

const GOAL_STATE_FILE: &str = "goal-state.json";
const GOAL_SCHEMA_VERSION: u32 = 1;

/// Compute the goal storage root from the extension session data directory.
pub(crate) fn goal_dir_from_base(base: &Path) -> PathBuf {
    base.join("goal")
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GoalStatus {
    Active,
    Paused,
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
}

impl GoalStatus {
    pub(crate) fn allows_create_replacement(self) -> bool {
        self == Self::Complete
    }

    pub(crate) fn can_auto_continue(self) -> bool {
        self == Self::Active
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::UsageLimited => "usage_limited",
            Self::BudgetLimited => "budget_limited",
            Self::Complete => "complete",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GoalUpdateStatus {
    Paused,
    Complete,
    Blocked,
}

impl From<GoalUpdateStatus> for GoalStatus {
    fn from(status: GoalUpdateStatus) -> Self {
        match status {
            GoalUpdateStatus::Paused => GoalStatus::Paused,
            GoalUpdateStatus::Complete => GoalStatus::Complete,
            GoalUpdateStatus::Blocked => GoalStatus::Blocked,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GoalState {
    pub schema_version: u32,
    pub revision: u64,
    pub goal_id: String,
    pub objective: String,
    pub status: GoalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage_baseline: Option<u64>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub accumulated_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_used_at_stop: Option<u64>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub continuation_prompt_pending: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_turn_id: Option<String>,
    pub budget_limit_prompt_pending: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_limit_turn_id: Option<String>,
    #[serde(default)]
    pub continuation_count: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl GoalState {
    pub(crate) fn new(
        objective: String,
        token_budget: Option<u64>,
        token_usage_baseline: Option<u64>,
    ) -> Self {
        let now = Timestamp::now();
        Self {
            schema_version: GOAL_SCHEMA_VERSION,
            revision: 1,
            goal_id: uuid::Uuid::new_v4().to_string(),
            objective,
            status: GoalStatus::Active,
            token_budget,
            token_usage_baseline,
            accumulated_tokens: 0,
            tokens_used_at_stop: None,
            created_at: now,
            updated_at: now,
            continuation_prompt_pending: None,
            continuation_turn_id: None,
            budget_limit_prompt_pending: None,
            budget_limit_turn_id: None,
            continuation_count: 0,
        }
    }

    pub(crate) fn elapsed_seconds(&self) -> u64 {
        let end = if self.status == GoalStatus::Active {
            Timestamp::now()
        } else {
            self.updated_at
        };
        end.duration_since(self.created_at).as_secs().max(0) as u64
    }

    pub(crate) fn same_revision(&self, other: &Self) -> bool {
        self.goal_id == other.goal_id && self.revision == other.revision
    }

    pub(crate) fn same_lifecycle_state(&self, other: &Self) -> bool {
        self.goal_id == other.goal_id
            && self.status == other.status
            && self.token_budget == other.token_budget
            && self.tokens_used_at_stop == other.tokens_used_at_stop
    }

    fn limit_budget(&mut self, used: u64, turn_id: Option<&str>) {
        self.tokens_used_at_stop = Some(used);
        self.set_status(GoalStatus::BudgetLimited);
        self.budget_limit_prompt_pending = Some(uuid::Uuid::new_v4().to_string());
        self.budget_limit_turn_id = turn_id.map(str::to_owned);
    }

    pub(crate) fn tokens_used(&self, session_tokens: Option<u64>) -> Option<u64> {
        self.tokens_used_at_stop.or_else(|| {
            let baseline = self.token_usage_baseline?;
            let current = session_tokens?;
            Some(
                self.accumulated_tokens
                    .saturating_add(current.saturating_sub(baseline)),
            )
        })
    }

    fn apply(&mut self, action: GoalAction, session_tokens: Option<u64>) -> io::Result<bool> {
        let used = self.tokens_used(session_tokens);
        match action {
            GoalAction::Mark(status) => {
                let next = GoalStatus::from(status);
                if self.status == next {
                    return Ok(false);
                }
                if self.status != GoalStatus::Active && next != GoalStatus::Complete {
                    return Err(io::Error::other(format!(
                        "cannot mark goal {} while it is {}",
                        next.label(),
                        self.status.label()
                    )));
                }
                if used.is_none() {
                    // Without a stop reading, later session usage includes unrelated work.
                    self.token_usage_baseline = None;
                    self.accumulated_tokens = 0;
                }
                self.tokens_used_at_stop = used;
                self.set_status(next);
            },
            GoalAction::Resume => {
                match self.status {
                    GoalStatus::Paused | GoalStatus::Blocked | GoalStatus::UsageLimited => {},
                    GoalStatus::BudgetLimited => {
                        return Err(io::Error::other(
                            "raise the exhausted budget with /goal budget <new_total_budget>",
                        ));
                    },
                    other => {
                        return Err(io::Error::other(format!(
                            "cannot resume goal while it is {}",
                            other.label()
                        )));
                    },
                }
                if let Some(budget) = self.token_budget {
                    match used {
                        Some(used) if used >= budget => {
                            return Err(io::Error::other(format!(
                                "goal token budget is exhausted ({used}/{budget}); raise it with \
                                 /goal budget <new_total_budget> to resume"
                            )));
                        },
                        None => {
                            return Err(io::Error::other(
                                "goal token usage was unavailable when stopped; run /goal budget \
                                 <new_remaining_budget> to set a fresh budget or /goal clear",
                            ));
                        },
                        Some(_) => {},
                    }
                }
                self.reactivate(session_tokens)?;
            },
            GoalAction::AdjustBudget(budget) => {
                if budget == 0 {
                    return Err(io::Error::other("tokenBudget must be greater than zero"));
                }
                if self.status == GoalStatus::Complete {
                    return Err(io::Error::other("cannot adjust budget on a completed goal"));
                }
                if let Some(used) = used {
                    if budget <= used {
                        return Err(io::Error::other(format!(
                            "new total budget must exceed the {used} tokens already used"
                        )));
                    }
                    self.token_budget = Some(budget);
                    self.reactivate(session_tokens)?;
                } else {
                    let total = session_tokens
                        .ok_or_else(|| io::Error::other("goal token usage unavailable"))?;
                    self.accumulated_tokens = 0;
                    self.token_usage_baseline = Some(total);
                    self.tokens_used_at_stop = None;
                    self.token_budget = Some(budget);
                    self.set_status(GoalStatus::Active);
                }
            },
        }
        Ok(true)
    }

    fn reactivate(&mut self, session_tokens: Option<u64>) -> io::Result<()> {
        if let Some(used) = self.tokens_used_at_stop {
            let total = session_tokens.ok_or_else(|| {
                io::Error::other("cannot resume accounting without a current session token total")
            })?;
            self.accumulated_tokens = used;
            self.token_usage_baseline = Some(total);
        } else if self.token_usage_baseline.is_none()
            && let Some(total) = session_tokens
        {
            self.token_usage_baseline = Some(total);
        }
        self.tokens_used_at_stop = None;
        self.set_status(GoalStatus::Active);
        Ok(())
    }

    fn touch(&mut self) {
        self.revision = self.revision.saturating_add(1);
        self.updated_at = Timestamp::now();
    }

    fn mark_continuation_pending(&mut self, turn_id: Option<&str>) {
        self.continuation_turn_id = turn_id.map(str::to_owned);
        self.continuation_prompt_pending = Some(uuid::Uuid::new_v4().to_string());
        self.continuation_count = self.continuation_count.saturating_add(1);
        self.touch();
    }

    pub(crate) fn acknowledge_provider_contribution(&mut self, contribution_id: &str) -> bool {
        if self.continuation_prompt_pending.as_deref() == Some(contribution_id) {
            self.continuation_prompt_pending = None;
            self.continuation_turn_id = None;
            self.touch();
            return true;
        }
        if self.budget_limit_prompt_pending.as_deref() == Some(contribution_id) {
            self.budget_limit_prompt_pending = None;
            self.budget_limit_turn_id = None;
            self.touch();
            return true;
        }
        false
    }

    pub(crate) fn set_status(&mut self, status: GoalStatus) {
        self.status = status;
        self.continuation_prompt_pending = None;
        self.continuation_turn_id = None;
        if status != GoalStatus::BudgetLimited {
            self.budget_limit_prompt_pending = None;
            self.budget_limit_turn_id = None;
        }
        self.touch();
    }
}

pub(crate) enum GoalAction {
    Mark(GoalUpdateStatus),
    Resume,
    AdjustBudget(u64),
}

pub(crate) struct GoalStore {
    root: PathBuf,
}

impl GoalStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub(crate) fn load(&self) -> Result<Option<GoalState>, String> {
        let state = hostpaths::read_json_state(&self.state_path())
            .map_err(|error| format!("read goal state: {error}"))?;
        validate_schema(&state).map_err(|error| error.to_string())?;
        Ok(state)
    }

    fn update<R>(
        &self,
        change: impl FnOnce(Option<GoalState>) -> io::Result<(Option<GoalState>, R)>,
    ) -> Result<R, String> {
        hostpaths::update_json_state(&self.state_path(), |state| {
            validate_schema(&state)?;
            change(state)
        })
        .map_err(|error| error.to_string())
    }

    /// The host reads usage asynchronously. Only the revision that requested that
    /// snapshot may schedule work or consume its budget.
    pub(crate) fn schedule_prompt(
        &self,
        expected: &GoalState,
        exhausted_at: Option<u64>,
        turn_id: Option<&str>,
    ) -> Result<Option<GoalState>, String> {
        self.update(|state| {
            let Some(mut state) = state else {
                return Ok((None, None));
            };
            if !state.same_revision(expected) || state.status != GoalStatus::Active {
                return Ok((None, None));
            }
            if let Some(used) = exhausted_at {
                state.limit_budget(used, turn_id);
            } else if state.continuation_prompt_pending.is_none()
                || state.continuation_turn_id.as_deref() != turn_id
            {
                state.mark_continuation_pending(turn_id);
            } else {
                return Ok((None, Some(state)));
            }
            Ok((Some(state.clone()), Some(state)))
        })
    }

    pub(crate) fn acknowledge_provider_contribution(&self, id: &str) -> Result<(), String> {
        self.update(|state| {
            let changed = state
                .and_then(|mut state| state.acknowledge_provider_contribution(id).then_some(state));
            Ok((changed, ()))
        })
    }

    pub(crate) fn clear(&self) -> Result<(), String> {
        self.update(|state| {
            if state.is_some() {
                match std::fs::remove_file(self.state_path()) {
                    Ok(()) => {},
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {},
                    Err(error) => return Err(error),
                }
            }
            Ok((None, ()))
        })
    }

    pub(crate) fn create(
        &self,
        objective: String,
        token_budget: Option<u64>,
        token_usage_baseline: Option<u64>,
    ) -> Result<GoalState, String> {
        validate_objective(&objective)?;
        if token_budget == Some(0) {
            return Err("tokenBudget must be greater than zero".into());
        }
        self.update(|existing| {
            if let Some(goal) = existing
                && !goal.status.allows_create_replacement()
            {
                return Err(io::Error::other(format!(
                    "cannot create a new goal while current goal is {}",
                    goal.status.label()
                )));
            }
            let state = GoalState::new(
                objective.trim().to_string(),
                token_budget,
                token_usage_baseline,
            );
            Ok((Some(state.clone()), state))
        })
    }

    pub(crate) fn apply(
        &self,
        expected: &GoalState,
        action: GoalAction,
        session_tokens: Option<u64>,
    ) -> Result<GoalState, String> {
        self.update(|state| {
            let mut state =
                state.ok_or_else(|| io::Error::other("no goal exists for this session"))?;
            // Reject lifecycle/budget conflicts while allowing concurrent ack revision bumps.
            if !state.same_lifecycle_state(expected) {
                return Err(io::Error::other(
                    "goal changed; check its current status and retry",
                ));
            }
            if !state.apply(action, session_tokens)? {
                return Ok((None, state));
            }
            Ok((Some(state.clone()), state))
        })
    }

    fn state_path(&self) -> PathBuf {
        self.root.join(GOAL_STATE_FILE)
    }
}

fn validate_schema(state: &Option<GoalState>) -> io::Result<()> {
    if let Some(state) = state
        && state.schema_version != GOAL_SCHEMA_VERSION
    {
        return Err(io::Error::other(format!(
            "unsupported goal state schema version {}",
            state.schema_version
        )));
    }
    Ok(())
}

fn validate_objective(objective: &str) -> Result<(), String> {
    if objective.trim().is_empty() {
        return Err("objective must not be empty".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_persists_active_goal_and_validates_inputs() {
        let root = tempfile::tempdir().unwrap();
        let store = GoalStore::new(root.path().into());

        assert_eq!(
            store.create("   ".into(), None, None).unwrap_err(),
            "objective must not be empty"
        );
        assert_eq!(
            store.create("Ship".into(), Some(0), Some(0)).unwrap_err(),
            "tokenBudget must be greater than zero"
        );

        let state = store
            .create("  Ship the feature  ".into(), Some(1000), Some(50))
            .expect("create should succeed");

        assert_eq!(state.objective, "Ship the feature");
        assert_eq!(state.status, GoalStatus::Active);
        assert_eq!(state.token_budget, Some(1000));
        assert_eq!(store.load().unwrap().unwrap().goal_id, state.goal_id);
    }

    #[test]
    fn create_rejects_existing_unfinished_goal_and_replaces_completed() {
        let root = tempfile::tempdir().unwrap();
        let store = GoalStore::new(root.path().into());
        let first = store
            .create("First".into(), None, None)
            .expect("first create should succeed");

        let error = store
            .create("Second".into(), None, None)
            .expect_err("unfinished goal should block replacement");
        assert_eq!(
            error,
            "cannot create a new goal while current goal is active"
        );

        store
            .apply(&first, GoalAction::Mark(GoalUpdateStatus::Complete), None)
            .expect("complete should succeed");
        let second = store
            .create("Second".into(), None, None)
            .expect("completed goal can be replaced");
        assert_ne!(first.goal_id, second.goal_id);
    }

    #[test]
    fn stale_work_cannot_overwrite_a_pause_clear_or_replacement() {
        let root = tempfile::tempdir().unwrap();
        let store = GoalStore::new(root.path().into());
        let original = store
            .create("Original".into(), Some(100), Some(10))
            .unwrap();
        let pending = store
            .schedule_prompt(&original, None, Some("turn"))
            .unwrap()
            .unwrap();
        let old_id = pending.continuation_prompt_pending.as_deref().unwrap();
        let paused = store
            .apply(
                &pending,
                GoalAction::Mark(GoalUpdateStatus::Paused),
                Some(25),
            )
            .unwrap();
        assert!(
            store
                .schedule_prompt(&pending, Some(100), Some("turn"))
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .apply(&pending, GoalAction::AdjustBudget(200), Some(30))
                .is_err()
        );
        assert_eq!(paused.tokens_used(Some(500)), Some(15));

        let resumed = store.apply(&paused, GoalAction::Resume, Some(500)).unwrap();
        assert_eq!(resumed.tokens_used(Some(510)), Some(25));

        let blocked = store
            .apply(
                &resumed,
                GoalAction::Mark(GoalUpdateStatus::Blocked),
                Some(510),
            )
            .unwrap();
        let resumed = store
            .apply(&blocked, GoalAction::Resume, Some(600))
            .unwrap();
        let next = store
            .schedule_prompt(&resumed, None, Some("turn"))
            .unwrap()
            .unwrap();
        store.acknowledge_provider_contribution(old_id).unwrap();
        assert_eq!(store.load().unwrap(), Some(next.clone()));

        store.clear().unwrap();
        assert!(
            store
                .schedule_prompt(&next, None, Some("turn"))
                .unwrap()
                .is_none()
        );
        let replacement = store.create("Replacement".into(), None, Some(600)).unwrap();
        assert!(
            store
                .schedule_prompt(&original, Some(100), Some("turn"))
                .unwrap()
                .is_none()
        );
        assert_eq!(store.load().unwrap(), Some(replacement));
    }

    #[test]
    fn concurrent_acknowledge_does_not_block_user_pause_and_resume_survives_usage_reset() {
        let root = tempfile::tempdir().unwrap();
        let store = GoalStore::new(root.path().into());
        let original = store
            .create("Fork safe".into(), Some(100), Some(10))
            .unwrap();
        let pending = store
            .schedule_prompt(&original, None, Some("turn-1"))
            .unwrap()
            .unwrap();
        let contribution_id = pending.continuation_prompt_pending.as_deref().unwrap();

        // Acknowledge bumps `revision`, simulating a turn commit racing with user `/goal pause`.
        store
            .acknowledge_provider_contribution(contribution_id)
            .unwrap();

        let paused = store
            .apply(
                &pending,
                GoalAction::Mark(GoalUpdateStatus::Paused),
                Some(60),
            )
            .expect("ack revision bump should not block explicit pause");
        assert_eq!(paused.status, GoalStatus::Paused);
        assert_eq!(paused.tokens_used(Some(60)), Some(50));

        // Session usage resets below previously accumulated 50 tokens (e.g., fork or log reset).
        let resumed = store
            .apply(&paused, GoalAction::Resume, Some(12))
            .expect("resume should not underflow when session total < accumulated");
        assert_eq!(resumed.accumulated_tokens, 50);
        assert_eq!(resumed.token_usage_baseline, Some(12));
        assert_eq!(resumed.tokens_used(Some(12)), Some(50));
        assert_eq!(resumed.tokens_used(Some(22)), Some(60));
    }

    #[test]
    fn budgeted_goal_paused_without_usage_requires_adjust_budget_before_resume() {
        let root = tempfile::tempdir().unwrap();
        let store = GoalStore::new(root.path().into());
        let original = store
            .create("Recoverable".into(), Some(100), Some(10))
            .unwrap();
        let paused = store
            .apply(&original, GoalAction::Mark(GoalUpdateStatus::Paused), None)
            .unwrap();
        assert_eq!(paused.tokens_used(Some(500)), None);

        let resume_err = store
            .apply(&paused, GoalAction::Resume, Some(500))
            .expect_err("resume without stop usage should require budget reset");
        assert!(resume_err.contains("/goal budget"));

        let reanchored = store
            .apply(&paused, GoalAction::AdjustBudget(80), Some(500))
            .expect("adjust budget should re-anchor baseline");
        assert_eq!(reanchored.status, GoalStatus::Active);
        assert_eq!(reanchored.token_budget, Some(80));
        assert_eq!(reanchored.tokens_used(Some(515)), Some(15));
    }

    #[test]
    fn marking_paused_at_exhausted_budget_stays_paused_and_blocks_resume_until_raised() {
        let root = tempfile::tempdir().unwrap();
        let store = GoalStore::new(root.path().into());
        let original = store.create("Cap".into(), Some(50), Some(5)).unwrap();

        let paused = store
            .apply(
                &original,
                GoalAction::Mark(GoalUpdateStatus::Paused),
                Some(55),
            )
            .unwrap();
        assert_eq!(paused.status, GoalStatus::Paused);
        assert!(paused.budget_limit_prompt_pending.is_none());
        assert_eq!(paused.tokens_used(Some(80)), Some(50));

        let resume_err = store
            .apply(&paused, GoalAction::Resume, Some(80))
            .expect_err("paused goal at exhausted budget cannot resume without raising budget");
        assert!(resume_err.contains("/goal budget"));

        let resumed = store
            .apply(&paused, GoalAction::AdjustBudget(80), Some(80))
            .unwrap();
        assert_eq!(resumed.status, GoalStatus::Active);
        assert_eq!(resumed.tokens_used(Some(90)), Some(60));
    }

    #[test]
    fn previous_state_files_retain_pending_work_and_elapsed_freezes_when_stopped() {
        let root = tempfile::tempdir().unwrap();
        let store = GoalStore::new(root.path().into());
        let legacy = serde_json::json!({
            "schemaVersion": 1, "revision": 3, "goalId": "old", "objective": "Finish",
            "status": "active", "tokenBudget": 50, "tokenUsageBaseline": 5,
            "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:15Z",
            "continuationPromptPending": "old-delivery", "budgetLimitPromptPending": null
        });
        hostpaths::write_json_state(&store.state_path(), &legacy).unwrap();
        let goal = store.load().unwrap().unwrap();
        assert_eq!(goal.tokens_used(Some(25)), Some(20));

        store
            .acknowledge_provider_contribution("old-delivery")
            .unwrap();
        let settled = store.load().unwrap().unwrap();
        assert!(settled.continuation_prompt_pending.is_none());

        let mut stopped = goal;
        stopped.status = GoalStatus::Paused;
        assert_eq!(stopped.elapsed_seconds(), 15);
    }

    #[test]
    fn exhausted_budget_survives_retry_and_cannot_reactivate_without_headroom() {
        let root = tempfile::tempdir().unwrap();
        let store = GoalStore::new(root.path().into());
        let original = store.create("Finish".into(), Some(100), Some(10)).unwrap();
        let pending = store
            .schedule_prompt(&original, None, Some("turn"))
            .unwrap()
            .unwrap();
        let retry = store
            .schedule_prompt(&pending, None, Some("turn"))
            .unwrap()
            .unwrap();
        assert_eq!(pending, retry);

        let limited = store
            .schedule_prompt(&pending, Some(105), Some("turn"))
            .unwrap()
            .unwrap();
        let id = limited.budget_limit_prompt_pending.as_deref().unwrap();
        assert_eq!(limited.status, GoalStatus::BudgetLimited);
        assert_eq!(limited.budget_limit_turn_id.as_deref(), Some("turn"));
        assert!(limited.continuation_prompt_pending.is_none());
        assert!(limited.continuation_turn_id.is_none());

        store
            .acknowledge_provider_contribution(
                pending.continuation_prompt_pending.as_deref().unwrap(),
            )
            .unwrap();
        assert_eq!(store.load().unwrap(), Some(limited.clone()));

        store.acknowledge_provider_contribution(id).unwrap();
        store.acknowledge_provider_contribution(id).unwrap();
        let limited = store.load().unwrap().unwrap();
        assert!(limited.budget_limit_prompt_pending.is_none());
        assert!(limited.budget_limit_turn_id.is_none());

        assert!(
            store
                .apply(&limited, GoalAction::Resume, Some(130))
                .is_err()
        );
        assert!(
            store
                .apply(&limited, GoalAction::AdjustBudget(105), Some(130))
                .is_err()
        );
        let active = store
            .apply(&limited, GoalAction::AdjustBudget(200), Some(130))
            .unwrap();
        assert_eq!(active.tokens_used(Some(135)), Some(110));

        let done = store
            .apply(
                &active,
                GoalAction::Mark(GoalUpdateStatus::Complete),
                Some(135),
            )
            .unwrap();
        assert_eq!(done.tokens_used(Some(900)), Some(110));
        assert!(
            store
                .apply(&done, GoalAction::Mark(GoalUpdateStatus::Paused), Some(900))
                .is_err()
        );
        assert!(
            store
                .apply(&done, GoalAction::AdjustBudget(1000), Some(900))
                .is_err()
        );
        let next = store.create("Next".into(), None, Some(900)).unwrap();
        assert_ne!(done.goal_id, next.goal_id);
    }
}
