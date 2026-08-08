//! Session-local goal state persistence.

use std::path::{Path, PathBuf};

use astrcode_extension_sdk::hostpaths;
use chrono::{DateTime, Utc};
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
    Complete,
    Blocked,
}

impl From<GoalUpdateStatus> for GoalStatus {
    fn from(status: GoalUpdateStatus) -> Self {
        match status {
            GoalUpdateStatus::Complete => GoalStatus::Complete,
            GoalUpdateStatus::Blocked => GoalStatus::Blocked,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GoalState {
    pub schema_version: u32,
    pub goal_id: String,
    pub objective: String,
    pub status: GoalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage_baseline: Option<u64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub continuation_prompt_pending: bool,
    #[serde(default)]
    pub budget_limit_prompt_pending: bool,
    #[serde(default)]
    pub continuation_count: u64,
}

impl GoalState {
    pub(crate) fn new(
        objective: String,
        token_budget: Option<u64>,
        token_usage_baseline: Option<u64>,
    ) -> Self {
        let now = Utc::now();
        Self {
            schema_version: GOAL_SCHEMA_VERSION,
            goal_id: uuid::Uuid::new_v4().to_string(),
            objective,
            status: GoalStatus::Active,
            token_budget,
            token_usage_baseline,
            created_at: now,
            updated_at: now,
            continuation_prompt_pending: false,
            budget_limit_prompt_pending: false,
            continuation_count: 0,
        }
    }

    pub(crate) fn elapsed_seconds(&self) -> u64 {
        Utc::now()
            .signed_duration_since(self.created_at)
            .num_seconds()
            .max(0) as u64
    }

    pub(crate) fn touch(&mut self) {
        self.updated_at = Utc::now();
    }

    pub(crate) fn mark_continuation_pending(&mut self) {
        self.continuation_prompt_pending = true;
        self.continuation_count = self.continuation_count.saturating_add(1);
        self.touch();
    }

    pub(crate) fn take_continuation_prompt_pending(&mut self) -> bool {
        let pending = self.continuation_prompt_pending;
        self.continuation_prompt_pending = false;
        if pending {
            self.touch();
        }
        pending
    }

    pub(crate) fn mark_budget_limit_prompt_pending(&mut self) {
        self.budget_limit_prompt_pending = true;
        self.touch();
    }

    pub(crate) fn take_budget_limit_prompt_pending(&mut self) -> bool {
        let pending = self.budget_limit_prompt_pending;
        self.budget_limit_prompt_pending = false;
        if pending {
            self.touch();
        }
        pending
    }

    pub(crate) fn set_status(&mut self, status: GoalStatus) {
        self.status = status;
        if status != GoalStatus::Active {
            self.continuation_prompt_pending = false;
        }
        if status != GoalStatus::BudgetLimited {
            self.budget_limit_prompt_pending = false;
        }
        self.touch();
    }
}

pub(crate) struct GoalStore {
    root: PathBuf,
}

impl GoalStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub(crate) fn load(&self) -> Result<Option<GoalState>, String> {
        let state = hostpaths::read_json_state::<GoalState>(&self.state_path())
            .map_err(|error| format!("read goal state: {error}"))?;
        if let Some(state) = &state {
            if state.schema_version != GOAL_SCHEMA_VERSION {
                return Err(format!(
                    "unsupported goal state schema version {}",
                    state.schema_version
                ));
            }
        }
        Ok(state)
    }

    pub(crate) fn save(&self, state: &GoalState) -> Result<(), String> {
        hostpaths::write_json_state(&self.state_path(), state)
            .map_err(|error| format!("save goal state: {error}"))
    }

    pub(crate) fn clear(&self) -> Result<(), String> {
        let path = self.state_path();
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("clear goal state: {error}")),
        }
    }

    pub(crate) fn create(
        &self,
        objective: String,
        token_budget: Option<u64>,
        token_usage_baseline: Option<u64>,
    ) -> Result<GoalState, String> {
        validate_objective(&objective)?;
        if let Some(0) = token_budget {
            return Err("tokenBudget must be greater than zero".to_string());
        }
        if let Some(existing) = self.load()? {
            if !existing.status.allows_create_replacement() {
                return Err(format!(
                    "cannot create a new goal while current goal is {}",
                    existing.status.label()
                ));
            }
        }

        let state = GoalState::new(
            objective.trim().to_string(),
            token_budget,
            token_usage_baseline,
        );
        self.save(&state)?;
        Ok(state)
    }

    pub(crate) fn update_status(&self, status: GoalUpdateStatus) -> Result<GoalState, String> {
        let mut state = self
            .load()?
            .ok_or_else(|| "no goal exists for this session".to_string())?;
        state.set_status(status.into());
        self.save(&state)?;
        Ok(state)
    }

    pub(crate) fn pause(&self) -> Result<GoalState, String> {
        let mut state = self
            .load()?
            .ok_or_else(|| "no goal exists for this session".to_string())?;
        state.set_status(GoalStatus::Paused);
        self.save(&state)?;
        Ok(state)
    }

    pub(crate) fn resume(&self) -> Result<GoalState, String> {
        let mut state = self
            .load()?
            .ok_or_else(|| "no goal exists for this session".to_string())?;
        if !matches!(state.status, GoalStatus::Paused | GoalStatus::BudgetLimited) {
            return Err(format!(
                "only paused or budget_limited goals can be resumed; current status is {}",
                state.status.label()
            ));
        }
        state.set_status(GoalStatus::Active);
        self.save(&state)?;
        Ok(state)
    }

    /// 调整 goal 的 token 预算并恢复到 active。
    ///
    /// 用于 BudgetLimited（预算耗尽）后"追加额度继续"的场景：保留原有
    /// `token_usage_baseline` 和 `continuation_count`，只替换预算上限并解冻，
    /// 避免用户只能 `/goal clear` 重建而丢失进度。
    ///
    /// `new_budget` 为新总预算（不是增量）；已完成的 goal 不可调整。
    pub(crate) fn adjust_budget(&self, new_budget: u64) -> Result<GoalState, String> {
        if new_budget == 0 {
            return Err("tokenBudget must be greater than zero".to_string());
        }
        let mut state = self
            .load()?
            .ok_or_else(|| "no goal exists for this session".to_string())?;
        if state.status == GoalStatus::Complete {
            return Err("cannot adjust budget on a completed goal".to_string());
        }
        state.token_budget = Some(new_budget);
        state.set_status(GoalStatus::Active);
        self.save(&state)?;
        Ok(state)
    }

    fn state_path(&self) -> PathBuf {
        self.root.join(GOAL_STATE_FILE)
    }
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

    fn test_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir()
            .join("astrcode-goal-store-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn create_persists_active_goal() {
        let store = GoalStore::new(test_root("create"));
        let state = store
            .create("Ship the feature".into(), Some(1000), Some(50))
            .expect("create should succeed");

        assert_eq!(state.objective, "Ship the feature");
        assert_eq!(state.status, GoalStatus::Active);
        assert_eq!(state.token_budget, Some(1000));
        assert_eq!(store.load().unwrap().unwrap().goal_id, state.goal_id);
    }

    #[test]
    fn create_rejects_existing_unfinished_goal() {
        let store = GoalStore::new(test_root("reject-unfinished"));
        store
            .create("First".into(), None, None)
            .expect("first create should succeed");

        let error = store
            .create("Second".into(), None, None)
            .expect_err("unfinished goal should block replacement");

        assert_eq!(
            error,
            "cannot create a new goal while current goal is active"
        );
    }

    #[test]
    fn create_replaces_completed_goal() {
        let store = GoalStore::new(test_root("replace-complete"));
        let first = store
            .create("First".into(), None, None)
            .expect("first create should succeed");
        store
            .update_status(GoalUpdateStatus::Complete)
            .expect("complete should succeed");

        let second = store
            .create("Second".into(), None, None)
            .expect("completed goal can be replaced");

        assert_ne!(first.goal_id, second.goal_id);
        assert_eq!(second.objective, "Second");
    }

    #[test]
    fn update_status_clears_pending_continuation() {
        let store = GoalStore::new(test_root("update-clears-continuation"));
        let mut state = store
            .create("Finish work".into(), None, None)
            .expect("create should succeed");
        state.mark_continuation_pending();
        state.mark_budget_limit_prompt_pending();
        store.save(&state).expect("save should succeed");

        let updated = store
            .update_status(GoalUpdateStatus::Blocked)
            .expect("update should succeed");

        assert_eq!(updated.status, GoalStatus::Blocked);
        assert!(!updated.continuation_prompt_pending);
        assert!(!updated.budget_limit_prompt_pending);
    }

    #[test]
    fn resume_allows_paused_or_budget_limited() {
        let store = GoalStore::new(test_root("resume"));
        store
            .create("Finish work".into(), None, None)
            .expect("create should succeed");

        let error = store
            .resume()
            .expect_err("active goal should not resume again");
        assert_eq!(
            error,
            "only paused or budget_limited goals can be resumed; current status is active"
        );

        store.pause().expect("pause should succeed");
        let resumed = store.resume().expect("resume should succeed");
        assert_eq!(resumed.status, GoalStatus::Active);
    }

    #[test]
    fn resume_allows_budget_limited_goal() {
        let store = GoalStore::new(test_root("resume-budget-limited"));
        let mut state = store
            .create("Finish work".into(), Some(100), Some(0))
            .expect("create should succeed");
        state.set_status(GoalStatus::BudgetLimited);
        store.save(&state).expect("save should succeed");

        let resumed = store.resume().expect("budget_limited can resume");
        assert_eq!(resumed.status, GoalStatus::Active);
    }

    #[test]
    fn adjust_budget_raises_ceiling_and_reactivates() {
        let store = GoalStore::new(test_root("adjust-budget"));
        let mut state = store
            .create("Finish work".into(), Some(100), Some(0))
            .expect("create should succeed");
        let original_baseline = state.token_usage_baseline;
        // 累积一次续跑，记录此时的 baseline 和 count，验证 adjust_budget 不重置它们。
        state.mark_continuation_pending();
        state.take_continuation_prompt_pending();
        store.save(&state).expect("save before budget limit");
        let state_before_limit = store.load().unwrap().unwrap();
        let expected_count = state_before_limit.continuation_count;
        let mut state = state_before_limit;
        state.set_status(GoalStatus::BudgetLimited);
        store.save(&state).expect("save budget limited");

        let adjusted = store.adjust_budget(500).expect("adjust should succeed");

        assert_eq!(adjusted.status, GoalStatus::Active);
        assert_eq!(adjusted.token_budget, Some(500));
        // baseline 与 continuation_count 应保留，体现"追加额度"而非重建。
        assert_eq!(adjusted.token_usage_baseline, original_baseline);
        assert_eq!(adjusted.continuation_count, expected_count);
    }

    #[test]
    fn adjust_budget_rejects_zero_and_completed() {
        let store = GoalStore::new(test_root("adjust-budget-reject"));

        let error = store
            .adjust_budget(0)
            .expect_err("zero budget rejected even without a goal");
        assert_eq!(error, "tokenBudget must be greater than zero");

        store
            .create("Finish work".into(), Some(100), None)
            .expect("create should succeed");
        store
            .update_status(GoalUpdateStatus::Complete)
            .expect("complete should succeed");

        let error = store
            .adjust_budget(500)
            .expect_err("completed goal cannot adjust budget");
        assert_eq!(error, "cannot adjust budget on a completed goal");
    }
}
