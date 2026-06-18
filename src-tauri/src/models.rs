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

/// One day of aggregated usage, used for the little bar chart.
#[derive(Serialize, Clone)]
pub struct DayBucket {
    pub date: String, // YYYY-MM-DD
    pub tokens: u64,
    pub cost: f64,
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
}

/// Live rate-limit windows for one Claude account. Cost/token history is not
/// represented here: the local logs carry no account identifier, so that data
/// stays machine-wide on `ClaudeUsage`.
#[derive(Serialize, Clone, Default)]
pub struct ClaudeAccountUsage {
    /// Stable organization UUID; the key the store and alerts dedupe on.
    pub org_uuid: String,
    /// Display name (user-set, or derived from plan + a UUID fragment).
    pub label: String,
    pub subscription_type: Option<String>,
    /// True for the account currently in `~/.claude/.credentials.json`.
    pub active: bool,
    /// True when this account's live gauges were fetched successfully.
    pub live: bool,
    pub five_hour: Option<Gauge>,
    pub seven_day: Option<Gauge>,
}

#[derive(Serialize, Clone, Default)]
pub struct ClaudeUsage {
    pub available: bool,
    /// True when at least one account's live /api/oauth/usage call succeeded.
    pub live: bool,
    pub five_hour: Option<Gauge>, // active account's live 5h window
    pub seven_day: Option<Gauge>, // active account's live weekly window
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
