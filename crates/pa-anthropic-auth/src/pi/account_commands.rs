//! The plugins' account commands over the shared store's logins:
//! `/claude-routing` (pi `commands.ts`, core `routing.ts`), `/claude-killswitch`
//! (the opencode plugin's handler, core `killswitch.ts`) and `/claude-quota`
//! (pi `commands.ts`, core `quotas.ts`). Same arguments, same text; the
//! settings they change are written to the plugins' sidecar as the plugins'
//! setters write it (`PluginSettings::update`, under its write lock).
//!
//! Where prime-agent's pool differs from the plugins' (the store's logins,
//! not the sidecar's fallback `accounts`), the commands list the store's
//! logins: the killswitch table's rows and per-login thresholds are by store
//! id, and the quota summary shows every OAuth login of the store (the one
//! the store serves first as `main`).

use anthropic::quota::{QuotaMoney, QuotaSnapshot, QuotaWindow, ScopedQuotaWindow};
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};

use super::commands::merge_section;
use super::settings::{PluginSettings, SettingsError};

/// `/claude-routing`.
pub(crate) const ROUTING_COMMAND: &str = "claude-routing";
pub(crate) const ROUTING_DESCRIPTION: &str = "Show or change Claude account routing mode";
pub(crate) const ROUTING_HINT: &str = "[main-first|fallback-first|sticky-balanced|reset]";
/// `/claude-killswitch`.
pub(crate) const KILLSWITCH_COMMAND: &str = "claude-killswitch";
pub(crate) const KILLSWITCH_DESCRIPTION: &str =
    "Manage killswitch — hard-block requests when quota drops below per-account thresholds.";
pub(crate) const KILLSWITCH_HINT: &str = "[on|off|set <login>:<5h>,<1w>[,<scoped>] ...]";
/// `/claude-quota`.
pub(crate) const QUOTA_COMMAND: &str = "claude-quota";
pub(crate) const QUOTA_DESCRIPTION: &str = "Show Claude quota state for the shared store's logins";

// ---------------------------------------------------------------------------
// JavaScript's number text
// ---------------------------------------------------------------------------

/// `String(value)` for the numbers these commands print: integers without a
/// fraction, others as the shortest round-trip text (as JavaScript prints
/// them in this range).
pub(crate) fn js_number(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e21 {
        #[allow(clippy::cast_possible_truncation)]
        // an integral value below 1e21 (JavaScript's own cut-over)
        let integral = value as i128;
        return integral.to_string();
    }
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    value.to_string()
}

/// `Math.round(value)`: halves round up.
fn js_round(value: f64) -> f64 {
    (value + 0.5).floor()
}

// ---------------------------------------------------------------------------
// /claude-routing
// ---------------------------------------------------------------------------

/// The routing modes, in the plugins' order (`ROUTING_MODES`).
const ROUTING_MODES: [&str; 3] = ["main-first", "fallback-first", "sticky-balanced"];
const DEFAULT_ROUTING_MODE: &str = "main-first";
const ROUTING_USAGE: &str = "Usage: `/claude-routing`, `/claude-routing main-first`, `/claude-routing fallback-first`, `/claude-routing sticky-balanced`, or `/claude-routing reset`.";

/// What `/claude-routing`'s arguments ask for (`parseRoutingCommandAction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoutingAction {
    Status,
    Mode(&'static str),
    Reset,
    Usage,
}

fn routing_mode(word: &str) -> Option<&'static str> {
    ROUTING_MODES.into_iter().find(|mode| *mode == word)
}

/// `parseRoutingCommandAction`: the words lowercased.
pub(crate) fn parse_routing(args: &str) -> RoutingAction {
    let words: Vec<String> = args.split_whitespace().map(str::to_lowercase).collect();
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    match words.as_slice() {
        [] => RoutingAction::Status,
        ["reset"] => RoutingAction::Reset,
        [mode] | ["mode", mode] => {
            routing_mode(mode).map_or(RoutingAction::Usage, RoutingAction::Mode)
        }
        _ => RoutingAction::Usage,
    }
}

/// `getRoutingMode`: the sidecar's mode, else `main-first`.
pub(crate) fn current_routing_mode(config: &Map<String, Value>) -> &'static str {
    config
        .get("routing")
        .filter(|routing| routing.is_object())
        .and_then(|routing| routing.get("mode"))
        .and_then(Value::as_str)
        .and_then(routing_mode)
        .unwrap_or(DEFAULT_ROUTING_MODE)
}

fn routing_behavior(mode: &str) -> &'static str {
    match mode {
        "fallback-first" => "Try usable fallback accounts before the main account. If no fallback succeeds, try the main account.",
        "sticky-balanced" => "Assign each session to a quota-weighted OAuth account, keep it sticky across transient failures, and migrate only for confirmed long-lived exhaustion or permanent account failure.",
        _ => "Try the main account first. Use fallback accounts only when quota policy or fallback errors require it.",
    }
}

/// `buildRoutingStatusSummary` without its title (`.split('\n').slice(2)`).
fn routing_status_body(mode: &str) -> String {
    format!(
        "- Mode: `{mode}`\n- Behavior: {}\n\n{ROUTING_USAGE}",
        routing_behavior(mode)
    )
}

/// `executeRoutingCommand`.
pub(crate) fn routing_text(action: RoutingAction, mode: &str) -> String {
    match action {
        RoutingAction::Status => {
            format!("## Claude Routing Status\n\n{}", routing_status_body(mode))
        }
        RoutingAction::Mode(next) => format!(
            "## Claude Routing Updated\n\nMode updated to `{next}`.\n\n{}",
            routing_status_body(next)
        ),
        RoutingAction::Reset => format!(
            "## Claude Routing Assignment Reset\n\nThe current session will be assigned again on its next request.\n\n{}",
            routing_status_body(mode)
        ),
        RoutingAction::Usage => format!(
            "## Claude Routing Usage\n\n{ROUTING_USAGE}\n\n{}",
            routing_status_body(mode)
        ),
    }
}

/// `/claude-routing <args>`: the mode written (when asked), the session's
/// sticky assignment cleared (for `reset`, through `clear_session`), and the
/// text.
pub(crate) fn run_routing(
    settings: &PluginSettings,
    args: &str,
    clear_session: impl FnOnce() -> Result<(), String>,
) -> Result<String, String> {
    let action = parse_routing(args);
    let mut mode = current_routing_mode(&settings.read());
    match action {
        // `setRoutingMode`.
        RoutingAction::Mode(next) => {
            settings
                .update(|config| merge_section(config, "routing", [("mode", json!(next))]))
                .map_err(|error| error.to_string())?;
            mode = next;
        }
        // `clearPiStickyRoutingSession`.
        RoutingAction::Reset => clear_session()?,
        RoutingAction::Status | RoutingAction::Usage => {}
    }
    Ok(routing_text(action, mode))
}

// ---------------------------------------------------------------------------
// /claude-killswitch
// ---------------------------------------------------------------------------

/// One `set` entry: `<login>:<5h>,<1w>[,<scoped>]`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct KillswitchEntry {
    pub(crate) account: String,
    pub(crate) five_hour: f64,
    pub(crate) seven_day: f64,
    pub(crate) scoped: Option<f64>,
}

/// What `/claude-killswitch`'s arguments ask for
/// (`parseKillswitchCommandAction`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum KillswitchAction {
    Status,
    On,
    Off,
    Set(Vec<KillswitchEntry>),
    Usage,
}

/// `^([^:]+):(\d+),(\d+)(?:,(\d+))?$`.
fn killswitch_entry(part: &str) -> Option<KillswitchEntry> {
    let (account, numbers) = part.split_once(':')?;
    if account.is_empty() {
        return None;
    }
    let numbers: Vec<&str> = numbers.split(',').collect();
    if !(2..=3).contains(&numbers.len())
        || numbers
            .iter()
            .any(|number| number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }
    // `Number.parseInt` of ASCII digits.
    let number = |text: &str| text.parse::<f64>().ok();
    Some(KillswitchEntry {
        account: account.to_string(),
        five_hour: number(numbers[0])?,
        seven_day: number(numbers[1])?,
        scoped: match numbers.get(2) {
            Some(scoped) => Some(number(scoped)?),
            None => None,
        },
    })
}

/// `parseKillswitchCommandAction`.
pub(crate) fn parse_killswitch(args: &str) -> KillswitchAction {
    let parts: Vec<&str> = args.split_whitespace().collect();
    match parts.as_slice() {
        [] => KillswitchAction::Status,
        ["on"] => KillswitchAction::On,
        ["off"] => KillswitchAction::Off,
        ["set", entries @ ..] => {
            let parsed: Option<Vec<KillswitchEntry>> =
                entries.iter().map(|part| killswitch_entry(part)).collect();
            match parsed {
                Some(entries) if !entries.is_empty() => KillswitchAction::Set(entries),
                _ => KillswitchAction::Usage,
            }
        }
        _ => KillswitchAction::Usage,
    }
}

/// The plugins' default thresholds (`DEFAULT_KILLSWITCH_THRESHOLDS`).
const DEFAULT_FIVE_HOUR: f64 = 5.0;
const DEFAULT_SEVEN_DAY: f64 = 10.0;
const DEFAULT_SCOPED: f64 = 0.0;

/// `normalizeKillswitchThresholds`: `five_hour ?? 5h`, `seven_day ?? 1w`,
/// `scoped`; a value that is not a finite number is its default.
fn thresholds(value: Option<&Value>) -> (f64, f64, f64) {
    let field = |primary: &str, alias: Option<&str>, default: f64| {
        let Some(value) = value.filter(|value| value.is_object()) else {
            return default;
        };
        let chosen = value
            .get(primary)
            .filter(|value| !value.is_null())
            .or_else(|| alias.and_then(|alias| value.get(alias)));
        chosen
            .and_then(Value::as_f64)
            .filter(|number| number.is_finite())
            .unwrap_or(default)
    };
    (
        field("five_hour", Some("5h"), DEFAULT_FIVE_HOUR),
        field("seven_day", Some("1w"), DEFAULT_SEVEN_DAY),
        field("scoped", None, DEFAULT_SCOPED),
    )
}

fn threshold_row(name: &str, value: Option<&Value>) -> String {
    let (five_hour, seven_day, scoped) = thresholds(value);
    format!(
        "| {name} | \u{2265} {}% | \u{2265} {}% | \u{2264} {}% |",
        js_number(five_hour),
        js_number(seven_day),
        js_number(scoped)
    )
}

/// `buildStatusTable`.
fn killswitch_table(config: &Map<String, Value>, account_ids: &[String]) -> String {
    let enabled = config.get("enabled") == Some(&Value::Bool(true));
    let mut lines = vec![
        "## Killswitch".to_string(),
        String::new(),
        format!("Status: **{}**", if enabled { "ON" } else { "OFF" }),
    ];
    if enabled {
        lines.push(String::new());
        lines.push("| Account | 5h threshold | 1w threshold | Scoped |".to_string());
        lines.push("| ------- | ------------ | ------------ | ------ |".to_string());
        let main = config.get("main").filter(|main| !main.is_null());
        lines.push(threshold_row("main", main));
        let accounts = config.get("accounts").and_then(Value::as_object);
        for id in account_ids {
            // `config.accounts?.[id] ?? config.main`.
            let own = accounts
                .and_then(|accounts| accounts.get(id))
                .filter(|value| !value.is_null());
            lines.push(threshold_row(id, own.or(main)));
        }
    }
    lines.join("\n")
}

const KILLSWITCH_USAGE: &str = "## Killswitch Commands\n\n```\n/claude-killswitch              — show status\n/claude-killswitch on           — enable with current or default thresholds\n/claude-killswitch off          — disable\n/claude-killswitch set all:5,10 — set all accounts to 5h≥5%, 1w≥10%\n/claude-killswitch set main:3,8,0 — set 5h≥3%, 1w≥8%, scoped≤0%\n/claude-killswitch set main:3,8 work-alt:5,10 — per-account\n```";

/// A threshold set as the command writes it.
fn threshold_value(entry: &KillswitchEntry) -> Value {
    let mut thresholds = Map::new();
    thresholds.insert("five_hour".to_string(), number_value(entry.five_hour));
    thresholds.insert("seven_day".to_string(), number_value(entry.seven_day));
    if let Some(scoped) = entry.scoped {
        thresholds.insert("scoped".to_string(), number_value(scoped));
    }
    Value::Object(thresholds)
}

fn number_value(number: f64) -> Value {
    if number.fract() == 0.0 && number.abs() < 9_007_199_254_740_992.0 {
        #[allow(clippy::cast_possible_truncation)]
        // an integral value within f64's exact integer range
        let integral = number as i64;
        return Value::from(integral);
    }
    serde_json::Number::from_f64(number).map_or(Value::Null, Value::Number)
}

/// `executeKillswitchCommand`: the text, and the configuration to write.
pub(crate) fn killswitch_text(
    action: &KillswitchAction,
    config: &Map<String, Value>,
    account_ids: &[String],
) -> (String, Option<Map<String, Value>>) {
    match action {
        KillswitchAction::Status | KillswitchAction::Usage => (
            format!(
                "{}\n\n{KILLSWITCH_USAGE}",
                killswitch_table(config, account_ids)
            ),
            None,
        ),
        KillswitchAction::On => {
            // `{...config, enabled: true, main: config.main ?? defaults}`.
            let mut updated = config.clone();
            updated.insert("enabled".to_string(), Value::Bool(true));
            let main = config
                .get("main")
                .filter(|main| !main.is_null())
                .cloned()
                .unwrap_or_else(|| json!({ "five_hour": 5, "seven_day": 10 }));
            updated.insert("main".to_string(), main);
            let text = format!(
                "## Killswitch Enabled\n\n{}",
                killswitch_table(&updated, account_ids)
            );
            (text, Some(updated))
        }
        KillswitchAction::Off => {
            let mut updated = config.clone();
            updated.insert("enabled".to_string(), Value::Bool(false));
            ("## Killswitch Disabled".to_string(), Some(updated))
        }
        KillswitchAction::Set(entries) => {
            // `{...config, enabled: true, accounts: {...config.accounts}}`.
            let mut accounts = config
                .get("accounts")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let mut main = None;
            for entry in entries {
                let value = threshold_value(entry);
                match entry.account.as_str() {
                    "main" => main = Some(value),
                    "all" => {
                        main = Some(value.clone());
                        for id in account_ids {
                            accounts.insert(id.clone(), value.clone());
                        }
                    }
                    account => {
                        accounts.insert(account.to_string(), value);
                    }
                }
            }
            let mut updated = config.clone();
            updated.insert("enabled".to_string(), Value::Bool(true));
            updated.insert("accounts".to_string(), Value::Object(accounts));
            if let Some(main) = main {
                updated.insert("main".to_string(), main);
            }
            let text = format!(
                "## Killswitch Updated\n\n{}",
                killswitch_table(&updated, account_ids)
            );
            (text, Some(updated))
        }
    }
}

/// `getKillswitchConfig`: the sidecar's `killswitch` object, else
/// `{enabled: false}`.
pub(crate) fn current_killswitch(config: &Map<String, Value>) -> Map<String, Value> {
    config
        .get("killswitch")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_else(|| {
            let mut off = Map::new();
            off.insert("enabled".to_string(), Value::Bool(false));
            off
        })
}

/// `/claude-killswitch <args>` over the logins `account_ids`: the
/// configuration written (`setKillswitchPersistent`) when it changed, and
/// the text.
pub(crate) fn run_killswitch(
    settings: &PluginSettings,
    args: &str,
    account_ids: &[String],
) -> Result<String, SettingsError> {
    let config = current_killswitch(&settings.read());
    let (text, updated) = killswitch_text(&parse_killswitch(args), &config, account_ids);
    if let Some(updated) = updated {
        settings.update(|config| {
            config.insert("killswitch".to_string(), Value::Object(updated));
        })?;
    }
    Ok(text)
}

// ---------------------------------------------------------------------------
// /claude-quota
// ---------------------------------------------------------------------------

/// One login in the quota summary (`QuotaAccountSummary`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct QuotaAccountSummary {
    pub(crate) name: String,
    /// `true`: the login the store serves first (the plugins' main).
    pub(crate) main: bool,
    /// `None`: not stated (printed as enabled).
    pub(crate) enabled: Option<bool>,
    pub(crate) quota: Option<QuotaSnapshot>,
    /// Epoch ms.
    pub(crate) last_refreshed_at: Option<i64>,
    pub(crate) error: Option<String>,
    pub(crate) tier_label: Option<String>,
}

/// `formatPercent`.
fn percent(value: f64) -> String {
    format!("{}%", js_number(js_round(value * 10.0) / 10.0))
}

/// `formatAge`.
fn age(checked_at: i64, now: i64) -> String {
    let minutes = (now - checked_at).max(0) / 60_000;
    if minutes < 1 {
        return "just now".to_string();
    }
    if minutes == 1 {
        return "1m ago".to_string();
    }
    if minutes < 60 {
        return format!("{minutes}m ago");
    }
    let (hours, remainder) = (minutes / 60, minutes % 60);
    if remainder == 0 {
        format!("{hours}h ago")
    } else {
        format!("{hours}h {remainder}m ago")
    }
}

/// `formatResetDuration`.
fn reset_duration(resets_at: &str, now: i64) -> String {
    let Ok(reset) = DateTime::parse_from_rfc3339(resets_at) else {
        return resets_at.to_string();
    };
    let remaining = reset.timestamp_millis() - now;
    if remaining <= 0 {
        return "now".to_string();
    }
    // `Math.max(1, Math.ceil(remainingMs / 60_000))`.
    let total = ((remaining + 59_999) / 60_000).max(1);
    if total < 60 {
        return format!("in {total}m");
    }
    let (hours, minutes) = (total / 60, total % 60);
    if minutes == 0 {
        format!("in {hours}h")
    } else {
        format!("in {hours}h {minutes}m")
    }
}

fn reset(resets_at: Option<&str>, now: i64) -> String {
    resets_at.map_or_else(String::new, |resets_at| {
        format!(", resets {}", reset_duration(resets_at, now))
    })
}

/// `formatWindow`.
fn window_line(
    label: &str,
    key: &str,
    window: Option<&QuotaWindow>,
    now: i64,
    binding: Option<&str>,
) -> String {
    let Some(window) = window else {
        return format!("  - {label}: unknown");
    };
    let line = format!(
        "  - {label}: {} remaining ({} used{}, checked {})",
        percent(window.remaining_percent),
        percent(window.used_percent),
        reset(window.resets_at.as_deref(), now),
        age(window.checked_at, now)
    );
    if binding == Some(key) {
        format!("{line} •")
    } else {
        line
    }
}

/// `formatScopedWindow`.
fn scoped_line(window: &ScopedQuotaWindow, now: i64) -> String {
    format!(
        "  - {}: {} remaining ({} used{}, checked {})",
        window.title,
        percent(window.remaining_percent),
        percent(window.used_percent),
        reset(window.resets_at.as_deref(), now),
        age(window.checked_at, now)
    )
}

/// The `en-US` symbol of the currencies `Intl` writes with one.
fn currency_symbol(code: &str) -> Option<&'static str> {
    Some(match code {
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" => "¥",
        "CAD" => "CA$",
        "AUD" => "A$",
        "NZD" => "NZ$",
        "HKD" => "HK$",
        "MXN" => "MX$",
        "BRL" => "R$",
        "TWD" => "NT$",
        "CNY" => "CN¥",
        "INR" => "₹",
        "KRW" => "₩",
        "ILS" => "₪",
        "VND" => "₫",
        _ => return None,
    })
}

/// Digits grouped by three (`1,234,567`).
fn grouped(digits: &str) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// `formatQuotaMoney`: `Intl.NumberFormat('en-US', {style: 'currency'})`
/// with the money's exponent as its fraction digits; a currency `Intl`
/// refuses prints as `<amountMinor> <currency>`.
pub(crate) fn money(money: &QuotaMoney) -> String {
    let code = money.currency.to_ascii_uppercase();
    let valid = code.len() == 3 && code.bytes().all(|byte| byte.is_ascii_uppercase());
    if !valid || money.exponent > 20 || !money.amount_minor.is_finite() {
        return format!("{} {}", js_number(money.amount_minor), money.currency);
    }
    let exponent = money.exponent as usize;
    #[allow(clippy::cast_possible_wrap)]
    // at most 20, checked above
    let scale = 10f64.powi(money.exponent as i32);
    let amount = money.amount_minor / scale;
    let text = format!("{:.*}", exponent, amount.abs());
    let (whole, fraction) = text
        .split_once('.')
        .map_or((text.as_str(), None), |(whole, fraction)| {
            (whole, Some(fraction))
        });
    let mut number = grouped(whole);
    if let Some(fraction) = fraction {
        number.push('.');
        number.push_str(fraction);
    }
    let sign = if amount < 0.0
        && number
            .bytes()
            .any(|byte| byte.is_ascii_digit() && byte != b'0')
    {
        "-"
    } else {
        ""
    };
    match currency_symbol(&code) {
        Some(symbol) => format!("{sign}{symbol}{number}"),
        None => format!("{sign}{code}\u{a0}{number}"),
    }
}

/// `buildClaudeQuotaSummary` (no `refreshedAt`, as pi calls it).
pub(crate) fn quota_text(accounts: &[QuotaAccountSummary], now: i64) -> String {
    let mut lines = vec!["## Claude Quotas".to_string(), String::new()];
    if accounts.is_empty() {
        lines.push("No Claude OAuth accounts found yet.".to_string());
        return lines.join("\n");
    }
    for account in accounts {
        let role = if account.main { "main" } else { "fallback" };
        let disabled = if account.enabled == Some(false) {
            " disabled"
        } else {
            ""
        };
        lines.push(format!("### {} ({role}{disabled})", account.name));
        if let Some(tier) = &account.tier_label {
            lines.push(format!("  - Tier: {tier}"));
        }
        if let Some(refreshed) = account.last_refreshed_at.filter(|at| *at != 0) {
            lines.push(format!("  - Last token refresh: {}", age(refreshed, now)));
        }
        if let Some(error) = account.error.as_deref().filter(|error| !error.is_empty()) {
            lines.push(format!("  - Error: {error}"));
        }
        let quota = account.quota.as_ref();
        let binding = quota.and_then(|quota| quota.binding_window.as_deref());
        lines.push(window_line(
            "5h",
            "five_hour",
            quota.and_then(|quota| quota.five_hour.as_ref()),
            now,
            binding,
        ));
        lines.push(window_line(
            "1w",
            "seven_day",
            quota.and_then(|quota| quota.seven_day.as_ref()),
            now,
            binding,
        ));
        for window in quota
            .and_then(|quota| quota.scoped.as_deref())
            .unwrap_or_default()
        {
            let line = scoped_line(window, now);
            lines.push(if binding == Some(window.id.as_str()) {
                format!("{line} •")
            } else {
                line
            });
        }
        if let Some(extra) = quota.and_then(|quota| quota.extra_usage.as_ref()) {
            lines.push(format!(
                "  - credits {}/{}{}",
                money(&extra.used),
                money(&extra.limit),
                if extra.exhausted { " · exhausted" } else { "" }
            ));
        }
        if quota.and_then(|quota| quota.fallback_advised) == Some(true) {
            lines.push("  - → fallback advised".to_string());
        }
        lines.push(String::new());
    }
    lines.join("\n").trim_end().to_string()
}

/// The store's logins as the quota summary lists them: the login the store
/// serves first as `main`, then the others in store order; each one's
/// quota is this process's readings (headers and polls), else what the
/// store recorded for it.
pub(crate) fn store_quota_summaries(
    store: &anthropic::AccountStore,
    readings: impl Fn(&str) -> Option<QuotaSnapshot>,
    now: DateTime<Utc>,
) -> Vec<QuotaAccountSummary> {
    let main = crate::source::served_login(store, now).map(|account| account.id.clone());
    let mut logins: Vec<&anthropic::Account> = store
        .accounts
        .iter()
        .filter(|account| {
            account
                .oauth()
                .is_some_and(|tokens| tokens.scopes.is_empty() || tokens.grants_inference())
        })
        .collect();
    logins.sort_by_key(|account| Some(&account.id) != main.as_ref());
    logins
        .into_iter()
        .map(|account| QuotaAccountSummary {
            name: account
                .label
                .as_deref()
                .map(str::trim)
                .filter(|label| !label.is_empty())
                .unwrap_or(&account.id)
                .to_string(),
            main: Some(&account.id) == main.as_ref(),
            enabled: Some(account.enabled),
            quota: readings(&account.id).or_else(|| {
                account
                    .quota
                    .as_ref()
                    .and_then(crate::quota::recorded_snapshot)
            }),
            last_refreshed_at: account.last_refreshed_at.map(|at| at.timestamp_millis()),
            error: account.current_error().map(str::to_string),
            tier_label: None,
        })
        .collect()
}

impl crate::SharedStoreSource {
    /// Run the feature's command `name` (one of the plugins' commands the
    /// feature lists) for `session` (the sticky routing key): its text.
    /// Blocking: reads and writes the settings file (under the plugins'
    /// write lock), the store and the sticky routing state.
    pub(crate) fn run_command(
        &self,
        name: &str,
        args: &str,
        session: &str,
    ) -> Result<String, String> {
        let settings = &self.pi.settings;
        match name {
            super::commands::FAST_COMMAND => {
                super::commands::run_fast(settings, args).map_err(|error| error.to_string())
            }
            super::commands::CACHE_COMMAND => {
                super::commands::run_cache(settings, args).map_err(|error| error.to_string())
            }
            crate::cachekeep::COMMAND => crate::cachekeep::run_command(
                settings,
                args,
                || self.cachekeep_sessions(),
                *chrono::Local::now().offset(),
            )
            .map_err(|error| error.to_string()),
            ROUTING_COMMAND => run_routing(settings, args, || {
                let Some(state) = &self.config.routing_state_path else {
                    return Ok(());
                };
                anthropic::sticky_routing::StickySessionRouter::new(state)
                    .clear(session, Utc::now().timestamp_millis())
                    .map_err(|error| error.to_string())
            }),
            KILLSWITCH_COMMAND => {
                let logins: Vec<String> = anthropic::AccountStore::load(&self.config.store_path)
                    .map(|store| {
                        crate::source::logins(&store)
                            .map(|account| account.id.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                run_killswitch(settings, args, &logins).map_err(|error| error.to_string())
            }
            QUOTA_COMMAND => {
                let now = Utc::now();
                let store = anthropic::AccountStore::load(&self.config.store_path)
                    .map_err(|error| error.to_string())?;
                let summaries = store_quota_summaries(
                    &store,
                    |account_id| self.quota.snapshot(account_id),
                    now,
                );
                Ok(quota_text(&summaries, now.timestamp_millis()))
            }
            other => Err(format!("unknown command /{other}")),
        }
    }
}

#[cfg(test)]
mod tests;
