use serde::Serialize;

/// A single rate-limit gauge (e.g. the rolling 5h window or the weekly window).
#[derive(Serialize, Clone, Default)]
pub struct Gauge {
    pub used_percent: f64,
    pub window_minutes: i64,
    /// Unix seconds when this window resets, if known.
    pub resets_at: Option<i64>,
    /// Human string like "36m" or "2d 18h".
    pub resets_in: Option<String>,
}

/// A model-scoped weekly window (e.g. the Fable-only weekly limit) reported
/// alongside the all-models weekly gauge.
#[derive(Serialize, Clone, Default)]
pub struct ModelGauge {
    /// Model display name straight from the API, e.g. "Fable".
    pub model: String,
    pub gauge: Gauge,
}

/// One day of aggregated usage, used for the little bar chart.
#[derive(Serialize, Clone)]
pub struct DayBucket {
    pub date: String, // YYYY-MM-DD
    pub tokens: u64,
    pub cost: f64,
}

/// One Codex "free rate-limit reset" credit, as exposed by the
/// wham/rate-limit-reset-credits endpoint. Date fields are flattened from the
/// backend's RFC3339 strings to unix seconds; `expires_in` is a human
/// countdown derived from `expires_at`.
#[derive(Serialize, Clone, Default)]
pub struct ResetCredit {
    pub id: String,
    pub status: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub granted_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub expires_in: Option<String>,
    pub redeem_started_at: Option<i64>,
    pub redeemed_at: Option<i64>,
}

/// Snapshot of Codex reset credits. `available` is true only when the live
/// fetch succeeded, so the UI and the alert pipeline can distinguish "no
/// credits" from "couldn't reach the endpoint".
#[derive(Serialize, Clone, Default)]
pub struct CodexResets {
    pub available: bool,
    pub available_count: u64,
    pub credits: Vec<ResetCredit>,
}

#[derive(Serialize, Clone, Default)]
pub struct CodexUsage {
    pub available: bool,
    /// True when the live wham/usage call succeeded.
    pub live: bool,
    pub plan_type: Option<String>,
    pub primary: Option<Gauge>,   // 5h session window
    pub secondary: Option<Gauge>, // weekly window
    pub credits: Option<f64>,
    pub updated_at: Option<i64>,
    pub cost_today: f64,
    pub tokens_today: u64,
    pub cost_30d: f64,
    pub tokens_30d: u64,
    pub daily: Vec<DayBucket>,
    /// Reset credits, when the live endpoint was reachable.
    pub resets: Option<CodexResets>,
}

/// Live rate-limit windows for one Claude account. Cost/token history is not
/// represented here: the local logs carry no account identifier, so that data
/// stays machine-wide on `ClaudeUsage`.
#[derive(Serialize, Clone, Default)]
pub struct ClaudeAccountUsage {
    /// Stable account identity and key: its config directory path.
    pub id: String,
    /// Display name (user-set, or derived from plan + the folder name).
    pub label: String,
    pub subscription_type: Option<String>,
    /// True for the built-in `~/.claude` account.
    pub active: bool,
    /// True for user-added directories (which can be removed); false for the
    /// built-in `~/.claude`.
    pub removable: bool,
    /// True when this account's live gauges were fetched successfully.
    pub live: bool,
    pub five_hour: Option<Gauge>,
    pub seven_day: Option<Gauge>,
    /// Model-scoped weekly window (e.g. Fable-only), when the plan has one.
    pub seven_day_model: Option<ModelGauge>,
}

#[derive(Serialize, Clone, Default)]
pub struct ClaudeUsage {
    pub available: bool,
    /// True when at least one account's live /api/oauth/usage call succeeded.
    pub live: bool,
    pub five_hour: Option<Gauge>, // active account's live 5h window
    pub seven_day: Option<Gauge>, // active account's live weekly window
    /// Active account's model-scoped weekly window (e.g. Fable-only).
    pub seven_day_model: Option<ModelGauge>,
    /// Per-account live gauges. One entry per known account; empty for a
    /// single-account, logs-only state.
    pub accounts: Vec<ClaudeAccountUsage>,
    pub cost_today: f64,
    pub tokens_today: u64,
    pub cost_30d: f64,
    pub tokens_30d: u64,
    pub daily: Vec<DayBucket>,
}

#[derive(Serialize, Clone, Default)]
pub struct Usage {
    pub codex: CodexUsage,
    pub claude: ClaudeUsage,
    pub generated_at: i64,
    pub error: Option<String>,
}
