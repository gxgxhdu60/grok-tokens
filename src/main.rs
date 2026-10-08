//! grok-tokens — real Grok Build usage tokens from local session logs.
//!
//! Native Rust CLI. The Python script is deprecated and will be removed.

use anyhow::{Context, Result};
use chrono::{NaiveDate, TimeZone, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use walkdir::WalkDir;

const VERSION: &str = env!("CARGO_PKG_VERSION");

// ── Pricing (USD per 1M tokens) — https://docs.x.ai/developers/pricing ────
//
// `costUsdTicks` is the CLI's own charge: one tick = 1e-10 USD.
// A turn_completed row sums every API request in that turn, so recomputing
// list price from those totals cannot place xAI's 200k long-context cliff
// (it applies per request, then 2× the whole request). Use ticks when
// present; the rate table is only a fallback for rows with no ticks.

const COST_USD_TICKS_PER_USD: f64 = 10_000_000_000.0;

type Rates = (f64, f64, f64); // input, cached, output

fn rates_for_model(model: &str) -> (Rates, Rates, u64) {
    let m = model.to_ascii_lowercase();
    if m.contains("grok-build-0.1") {
        return ((1.0, 0.2, 2.0), (2.0, 0.4, 4.0), 200_000);
    }
    if m.contains("grok-4.3") {
        return ((1.25, 0.2, 2.5), (2.5, 0.4, 5.0), 200_000);
    }
    if m.contains("grok-4.6") {
        return ((2.0, 0.5, 6.0), (4.0, 1.0, 12.0), 200_000);
    }
    // grok-4.5 / grok-4.5-build / default
    ((2.0, 0.3, 6.0), (4.0, 0.6, 12.0), 200_000)
}

fn model_from_usage(u: &Value) -> String {
    if let Some(mu) = u.get("modelUsage").and_then(|v| v.as_object()) {
        if let Some(k) = mu.keys().next() {
            return k.clone();
        }
    }
    "grok-4.5".into()
}

fn cost_from_ticks(ticks: u64) -> f64 {
    ticks as f64 / COST_USD_TICKS_PER_USD
}

/// Long-context 2× is only safe when this row is a single API request.
fn apply_long_context_tier(u: &Value, input_tokens: u64, threshold: u64) -> bool {
    json_u64(u, "modelCalls") <= 1 && input_tokens >= threshold
}

fn rate_table_cost(u: &Value, cache_discount: bool) -> f64 {
    let inp = json_u64(u, "inputTokens");
    let out = json_u64(u, "outputTokens");
    let cache = json_u64(u, "cachedReadTokens").min(inp);
    let fresh = inp.saturating_sub(cache);
    let model = model_from_usage(u);
    let (short, long, thr) = rates_for_model(&model);
    let (rin, rcache, rout) = if apply_long_context_tier(u, inp, thr) {
        long
    } else {
        short
    };
    let billed_input = if cache_discount {
        fresh as f64 * rin + cache as f64 * rcache
    } else {
        inp as f64 * rin
    };
    (billed_input + out as f64 * rout) / 1_000_000.0
}

fn event_cost_usd(u: &Value) -> f64 {
    let ticks = json_u64(u, "costUsdTicks");
    if ticks > 0 {
        cost_from_ticks(ticks)
    } else {
        rate_table_cost(u, true)
    }
}

fn json_u64(v: &Value, key: &str) -> u64 {
    v.get(key)
        .and_then(|x| x.as_u64().or_else(|| x.as_i64().map(|i| i.max(0) as u64)))
        .unwrap_or(0)
}

// ── Data ──────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone, Serialize)]
struct TokenBucket {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    cached_read_tokens: u64,
    reasoning_tokens: u64,
    cost_usd_ticks: u64,
    cost_api_usd: f64,
    /// List-price estimate on this row's token totals (fallback / Saved$).
    cost_rate_usd: f64,
    cost_api_no_cache_usd: f64,
    events: u64,
    model_calls: u64,
    models: BTreeSet<String>,
}

impl TokenBucket {
    fn add_usage(&mut self, u: &Value) {
        self.input_tokens += json_u64(u, "inputTokens");
        self.output_tokens += json_u64(u, "outputTokens");
        self.total_tokens += json_u64(u, "totalTokens");
        self.cached_read_tokens += json_u64(u, "cachedReadTokens");
        self.reasoning_tokens += json_u64(u, "reasoningTokens");
        self.cost_usd_ticks += json_u64(u, "costUsdTicks");
        self.cost_rate_usd += rate_table_cost(u, true);
        self.cost_api_usd += event_cost_usd(u);
        self.cost_api_no_cache_usd += rate_table_cost(u, false);
        self.events += 1;
        self.model_calls += json_u64(u, "modelCalls");
        if let Some(mu) = u.get("modelUsage").and_then(|v| v.as_object()) {
            for k in mu.keys() {
                self.models.insert(k.clone());
            }
        } else {
            self.models.insert("unknown".into());
        }
    }

    fn merge(&mut self, o: &TokenBucket) {
        self.input_tokens += o.input_tokens;
        self.output_tokens += o.output_tokens;
        self.total_tokens += o.total_tokens;
        self.cached_read_tokens += o.cached_read_tokens;
        self.reasoning_tokens += o.reasoning_tokens;
        self.cost_usd_ticks += o.cost_usd_ticks;
        self.cost_api_usd += o.cost_api_usd;
        self.cost_rate_usd += o.cost_rate_usd;
        self.cost_api_no_cache_usd += o.cost_api_no_cache_usd;
        self.events += o.events;
        self.model_calls += o.model_calls;
        self.models.extend(o.models.iter().cloned());
    }

    fn fresh_input_tokens(&self) -> u64 {
        self.input_tokens.saturating_sub(self.cached_read_tokens)
    }
    fn total_without_cache(&self) -> u64 {
        self.fresh_input_tokens() + self.output_tokens
    }
    fn cost_cli_usd(&self) -> f64 {
        cost_from_ticks(self.cost_usd_ticks)
    }
    fn cost_billing_usd(&self) -> f64 {
        self.cost_cli_usd()
    }
    fn cost_public_usd(&self) -> f64 {
        self.cost_rate_usd
    }
    fn cache_savings_usd(&self) -> f64 {
        (self.cost_api_no_cache_usd - self.cost_rate_usd).max(0.0)
    }
    fn round4(x: f64) -> f64 {
        (x * 10000.0).round() / 10000.0
    }
    fn hit_pct(&self) -> f64 {
        if self.input_tokens == 0 {
            0.0
        } else {
            100.0 * self.cached_read_tokens as f64 / self.input_tokens as f64
        }
    }

    fn as_public(&self) -> serde_json::Value {
        serde_json::json!({
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "total_tokens": self.total_tokens,
            "cached_read_tokens": self.cached_read_tokens,
            "fresh_input_tokens": self.fresh_input_tokens(),
            "total_without_cache": self.total_without_cache(),
            "reasoning_tokens": self.reasoning_tokens,
            "cost_usd_ticks": self.cost_usd_ticks,
            "cost_billing_usd": Self::round4(self.cost_billing_usd()),
            "cost_public_usd": Self::round4(self.cost_public_usd()),
            "cost_cli_usd": Self::round4(self.cost_billing_usd()),
            "cost_api_usd": Self::round4(self.cost_api_usd),
            "cost_rate_usd": Self::round4(self.cost_public_usd()),
            "cost_api_no_cache_usd": (self.cost_api_no_cache_usd * 10000.0).round() / 10000.0,
            "cache_savings_usd": (self.cache_savings_usd() * 10000.0).round() / 10000.0,
            "events": self.events,
            "model_calls": self.model_calls,
            "models": self.models.iter().cloned().collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
struct SessionStat {
    session_id: String,
    path: String,
    cwd: String,
    tokens: TokenBucket,
    last_activity: String,
    last_activity_day: String,
    peak_input_tokens: u64,
    meta_peak_context: u64,
    meta_final_context: u64,
    meta_first_context: u64,
    has_usage: bool,
}

impl SessionStat {
    fn peak_context(&self) -> u64 {
        self.peak_input_tokens.max(self.meta_peak_context)
    }
    fn as_public(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        m.insert("session_id".into(), self.session_id.clone().into());
        m.insert("path".into(), self.path.clone().into());
        m.insert("cwd".into(), self.cwd.clone().into());
        m.insert("last_activity".into(), self.last_activity.clone().into());
        m.insert(
            "last_activity_day".into(),
            self.last_activity_day.clone().into(),
        );
        m.insert("peak_context".into(), self.peak_context().into());
        m.insert("peak_input_tokens".into(), self.peak_input_tokens.into());
        m.insert("meta_peak_context".into(), self.meta_peak_context.into());
        m.insert("meta_final_context".into(), self.meta_final_context.into());
        m.insert("meta_first_context".into(), self.meta_first_context.into());
        m.insert("has_usage".into(), self.has_usage.into());
        if let Value::Object(t) = self.tokens.as_public() {
            m.extend(t);
        }
        Value::Object(m)
    }
}

#[derive(Debug, Clone)]
struct DailyStat {
    date: String,
    tokens: TokenBucket,
    sessions: BTreeSet<String>,
}

impl DailyStat {
    fn as_public(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        m.insert("date".into(), self.date.clone().into());
        m.insert("sessions".into(), self.sessions.len().into());
        if let Value::Object(t) = self.tokens.as_public() {
            m.extend(t);
        }
        Value::Object(m)
    }
}

// ── Paths / discovery ─────────────────────────────────────────────────────

fn dirs_home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn grok_home() -> PathBuf {
    if let Ok(h) = env::var("GROK_HOME") {
        if !h.trim().is_empty() {
            return PathBuf::from(h);
        }
    }
    dirs_home().join(".grok")
}

fn sessions_root() -> PathBuf {
    if let Ok(d) = env::var("GROK_DATA_DIR") {
        if !d.trim().is_empty() {
            return PathBuf::from(d);
        }
    }
    let p = grok_home().join("sessions");
    if p.is_dir() {
        return p;
    }
    dirs_home().join(".grok").join("sessions")
}

fn auth_json_path() -> PathBuf {
    if let Ok(p) = env::var("GROK_AUTH_PATH") {
        let t = p.trim();
        if !t.is_empty() {
            return PathBuf::from(t);
        }
    }
    grok_home().join("auth.json")
}

fn profiles_dir() -> PathBuf {
    if let Ok(d) = env::var("GROK_TOKENS_PROFILES") {
        let t = d.trim();
        if !t.is_empty() {
            return PathBuf::from(t);
        }
    }
    if let Ok(xdg) = env::var("XDG_DATA_HOME") {
        let t = xdg.trim();
        if !t.is_empty() {
            return PathBuf::from(t).join("grok-tokens").join("accounts");
        }
    }
    #[cfg(windows)]
    if let Ok(la) = env::var("LOCALAPPDATA") {
        let t = la.trim();
        if !t.is_empty() {
            return PathBuf::from(t).join("grok-tokens").join("accounts");
        }
    }
    dirs_home()
        .join(".local")
        .join("share")
        .join("grok-tokens")
        .join("accounts")
}

// ── Account limit (Weekly/Monthly — same as Grok /usage) ─────────────────

#[derive(Debug, Clone, Serialize)]
struct AccountLimit {
    percent: Option<f64>,
    period_label: String,
    period_type: String,
    period_start: Option<String>,
    period_end: Option<String>,
    subscription: Option<String>,
    fetched_at: Option<String>,
    source: String,
    email: Option<String>,
    user_id: Option<String>,
    profile: Option<String>,
}

impl AccountLimit {
    fn headline(&self) -> String {
        match self.percent {
            Some(p) if (p - p.round()).abs() < 1e-9 => {
                format!("{} limit: {:.0}%", self.period_label, p)
            }
            Some(p) => format!("{} limit: {}%", self.period_label, p),
            None => format!("{} limit: n/a", self.period_label),
        }
    }

    fn as_public(&self) -> Value {
        serde_json::json!({
            "percent": self.percent,
            "period_label": self.period_label,
            "period_type": self.period_type,
            "period_start": self.period_start,
            "period_end": self.period_end,
            "subscription": self.subscription,
            "fetched_at": self.fetched_at,
            "source": self.source,
            "headline": self.headline(),
            "email": self.email,
            "user_id": self.user_id,
            "profile": self.profile,
        })
    }
}

fn load_account_limit(max_bytes: u64) -> Option<AccountLimit> {
    let log = grok_home().join("logs").join("unified.jsonl");
    if !log.is_file() {
        // Weak fallback: last TUI clipboard
        let lc = grok_home().join("last-copy.txt");
        if let Ok(text) = fs::read_to_string(lc) {
            let re = Regex::new(r"(?i)(Weekly|Monthly)\s+limit:\s*([0-9]+(?:\.[0-9]+)?)\s*%")
                .ok()?;
            if let Some(c) = re.captures(text.trim()) {
                return Some(AccountLimit {
                    percent: c.get(2).and_then(|m| m.as_str().parse().ok()),
                    period_label: c
                        .get(1)
                        .map(|m| {
                            let s = m.as_str();
                            format!(
                                "{}{}",
                                s.chars().next().unwrap().to_uppercase(),
                                s[1..].to_lowercase()
                            )
                        })
                        .unwrap_or_else(|| "Period".into()),
                    period_type: String::new(),
                    period_start: None,
                    period_end: None,
                    subscription: None,
                    fetched_at: None,
                    source: "last-copy".into(),
                    email: None,
                    user_id: None,
                    profile: None,
                });
            }
        }
        return None;
    }

    let meta = fs::metadata(&log).ok()?;
    let size = meta.len();
    let file = File::open(&log).ok()?;
    let mut reader = BufReader::new(file);
    if size > max_bytes {
        use std::io::{Seek, SeekFrom};
        let _ = reader.seek(SeekFrom::Start(size - max_bytes));
        // skip partial first line
        let mut skip = String::new();
        let _ = reader.read_line(&mut skip);
    }

    let mut last: Option<Value> = None;
    for line in reader.lines().map_while(Result::ok) {
        if !line.contains("billing: fetched credits config") {
            continue;
        }
        if let Ok(obj) = serde_json::from_str::<Value>(&line) {
            if obj.get("msg").and_then(|m| m.as_str()) == Some("billing: fetched credits config")
            {
                last = Some(obj);
            }
        }
    }
    let last = last?;
    let ctx = last.get("ctx")?;
    let cfg = ctx.get("config")?;
    let period = cfg.get("currentPeriod");
    let ptype = period
        .and_then(|p| p.get("type"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let label = if ptype.to_uppercase().contains("WEEKLY") {
        "Weekly"
    } else if ptype.to_uppercase().contains("MONTHLY") {
        "Monthly"
    } else {
        "Period"
    };
    let percent = cfg
        .get("creditUsagePercent")
        .and_then(|v| v.as_f64().or_else(|| v.as_i64().map(|i| i as f64)));
    let period_start = period
        .and_then(|p| p.get("start"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            cfg.get("billingPeriodStart")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });
    let period_end = period
        .and_then(|p| p.get("end"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            cfg.get("billingPeriodEnd")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });
    let sub = ctx
        .get("subscriptionTiers")
        .or_else(|| ctx.get("subscription_tier"))
        .or_else(|| cfg.get("subscription_tier"))
        .map(|v| match v {
            Value::String(s) => s.clone(),
            Value::Array(a) => a
                .iter()
                .filter_map(|x| x.as_str())
                .collect::<Vec<_>>()
                .join(","),
            other => other.to_string(),
        })
        .filter(|s| !s.is_empty() && s != "null");

    Some(AccountLimit {
        percent,
        period_label: label.into(),
        period_type: ptype,
        period_start,
        period_end,
        subscription: sub,
        fetched_at: last
            .get("ts")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string()),
        source: "unified.jsonl".into(),
        email: None,
        user_id: None,
        profile: None,
    })
}

fn print_account_limit_banner(limit: &AccountLimit, color: bool) {
    let head = limit.headline();
    let head_s = match limit.percent {
        Some(p) if p >= 95.0 => c(color, "1;31", &head),
        Some(p) if p >= 80.0 => c(color, "1;33", &head),
        _ => c(color, "1;32", &head),
    };
    let mut bits = Vec::new();
    if let Some(ref email) = limit.email {
        bits.push(c(color, "1", email));
    }
    if let Some(ref profile) = limit.profile {
        bits.push(c(color, "32", &format!("profile {profile}")));
    }
    bits.push(head_s);
    if let Some(ref end) = limit.period_end {
        bits.push(c(color, "2", &format!("reset {}", &end[..end.len().min(10)])));
    }
    if let Some(ref sub) = limit.subscription {
        bits.push(c(color, "36", sub));
    }
    if let Some(ref ts) = limit.fetched_at {
        let shown = ts.replace('T', " ");
        bits.push(c(
            color,
            "2",
            &format!("as of {} UTC", &shown[..shown.len().min(19)]),
        ));
    }
    println!("{}", bits.join("  ·  "));
    println!();
}

fn discover_sessions(root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    if !root.is_dir() {
        return found.into_iter().map(|(_, p)| p).collect();
    }
    for e in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        if !e.file_type().is_dir() {
            continue;
        }
        let updates = e.path().join("updates.jsonl");
        if !updates.is_file() {
            continue;
        }
        let mtime = fs::metadata(&updates)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        found.push((mtime, e.path().to_path_buf()));
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().take(limit).map(|(_, p)| p).collect()
}

fn read_cwd(session_dir: &Path) -> String {
    let summary = session_dir.join("summary.json");
    if summary.is_file() {
        if let Ok(txt) = fs::read_to_string(&summary) {
            if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                if let Some(cwd) = v
                    .pointer("/info/cwd")
                    .or_else(|| v.get("cwd"))
                    .and_then(|x| x.as_str())
                {
                    if !cwd.is_empty() {
                        return cwd.to_string();
                    }
                }
            }
        }
    }
    if let Some(parent) = session_dir.parent().and_then(|p| p.file_name()) {
        let name = parent.to_string_lossy();
        if name.starts_with('%') {
            return urlencoding_decode(&name);
        }
    }
    String::new()
}

fn urlencoding_decode(s: &str) -> String {
    // minimal: %2F → / etc.
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h = || {
                let a = bytes[i + 1];
                let b = bytes[i + 2];
                let n = |c: u8| match c {
                    b'0'..=b'9' => Some(c - b'0'),
                    b'a'..=b'f' => Some(c - b'a' + 10),
                    b'A'..=b'F' => Some(c - b'A' + 10),
                    _ => None,
                };
                Some(n(a)? * 16 + n(b)?)
            };
            if let Some(v) = h() {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ── JSON walk ─────────────────────────────────────────────────────────────

fn walk_usage_dicts(v: &Value, out: &mut Vec<Value>) {
    match v {
        Value::Object(map) => {
            if map.contains_key("inputTokens")
                && map.contains_key("outputTokens")
                && map.contains_key("totalTokens")
            {
                out.push(v.clone());
                for (k, child) in map {
                    if k == "modelUsage" {
                        continue;
                    }
                    walk_usage_dicts(child, out);
                }
                return;
            }
            for child in map.values() {
                walk_usage_dicts(child, out);
            }
        }
        Value::Array(arr) => {
            for child in arr {
                walk_usage_dicts(child, out);
            }
        }
        _ => {}
    }
}

fn agent_ts_ms(obj: &Value) -> Option<i64> {
    obj.pointer("/params/_meta/agentTimestampMs")
        .and_then(|v| v.as_i64().or_else(|| v.as_u64().map(|u| u as i64)))
        .or_else(|| {
            obj.pointer("/params/update/_meta/agentTimestampMs")
                .and_then(|v| v.as_i64().or_else(|| v.as_u64().map(|u| u as i64)))
        })
}

fn ms_to_day(ts: Option<i64>) -> String {
    match ts {
        Some(ms) if ms > 0 => Utc
            .timestamp_millis_opt(ms)
            .single()
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "unknown".into()),
        _ => "unknown".into(),
    }
}

fn ms_to_rfc3339(ts: Option<i64>) -> String {
    match ts {
        Some(ms) if ms > 0 => Utc
            .timestamp_millis_opt(ms)
            .single()
            .map(|d| d.to_rfc3339())
            .unwrap_or_else(|| "unknown".into()),
        _ => "unknown".into(),
    }
}

fn usage_key(u: &Value) -> (u64, u64, u64, u64, u64, u64) {
    (
        json_u64(u, "inputTokens"),
        json_u64(u, "outputTokens"),
        json_u64(u, "totalTokens"),
        json_u64(u, "cachedReadTokens"),
        json_u64(u, "costUsdTicks"),
        json_u64(u, "modelCalls"),
    )
}

fn prefer_usage(found: Vec<Value>) -> Vec<Value> {
    let top: Vec<Value> = found
        .iter()
        .filter(|u| u.get("costUsdTicks").is_some() || u.get("modelUsage").is_some())
        .cloned()
        .collect();
    if top.is_empty() {
        found
    } else {
        top
    }
}

fn load_session(path: &Path) -> Option<SessionStat> {
    let updates = path.join("updates.jsonl");
    if !updates.is_file() {
        return None;
    }
    let file = File::open(&updates).ok()?;
    let reader = BufReader::new(file);

    let mut bucket = TokenBucket::default();
    let mut last_ts: Option<i64> = None;
    let mut meta_vals: Vec<u64> = Vec::new();
    let mut peak_input: u64 = 0;
    let mut seen_keys: HashSet<(u64, u64, u64, u64, u64, u64)> = HashSet::new();

    for line in reader.lines().map_while(Result::ok) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let obj: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !obj.is_object() {
            continue;
        }

        if let Some(ts) = agent_ts_ms(&obj) {
            last_ts = Some(last_ts.map_or(ts, |p| p.max(ts)));
        }

        if let Some(tot) = obj
            .pointer("/params/_meta/totalTokens")
            .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i.max(0) as u64)))
        {
            meta_vals.push(tot);
        }

        if !line.contains("inputTokens") && !line.contains("usage") {
            continue;
        }

        let mut found = Vec::new();
        walk_usage_dicts(&obj, &mut found);
        let top = prefer_usage(found);
        for u in top {
            let key = usage_key(&u);
            if !seen_keys.insert(key) {
                continue;
            }
            peak_input = peak_input.max(json_u64(&u, "inputTokens"));
            bucket.add_usage(&u);
        }
    }

    let meta_peak = meta_vals.iter().copied().max().unwrap_or(0);
    let meta_first = meta_vals.first().copied().unwrap_or(0);
    let meta_last = meta_vals.last().copied().unwrap_or(0);
    let sid = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    Some(SessionStat {
        session_id: sid,
        path: path.display().to_string(),
        cwd: read_cwd(path),
        has_usage: bucket.events > 0,
        tokens: bucket,
        last_activity: ms_to_rfc3339(last_ts),
        last_activity_day: ms_to_day(last_ts),
        peak_input_tokens: peak_input,
        meta_peak_context: meta_peak,
        meta_final_context: meta_last,
        meta_first_context: meta_first,
    })
}

fn daily_from_raw(session_dirs: &[PathBuf], since: Option<&str>) -> Vec<DailyStat> {
    let mut by_day: BTreeMap<String, DailyStat> = BTreeMap::new();

    for path in session_dirs {
        let updates = path.join("updates.jsonl");
        if !updates.is_file() {
            continue;
        }
        let sid = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Ok(file) = File::open(&updates) else {
            continue;
        };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if !line.contains("inputTokens") && !line.contains("usage") {
                continue;
            }
            let obj: Value = match serde_json::from_str(line.trim()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let mut found = Vec::new();
            walk_usage_dicts(&obj, &mut found);
            let top = prefer_usage(found);
            if top.is_empty() {
                continue;
            }
            let day = ms_to_day(agent_ts_ms(&obj));
            if let Some(s) = since {
                if day != "unknown" && day.as_str() < s {
                    continue;
                }
            }
            let entry = by_day.entry(day.clone()).or_insert_with(|| DailyStat {
                date: day,
                tokens: TokenBucket::default(),
                sessions: BTreeSet::new(),
            });
            entry.sessions.insert(sid.clone());
            let mut seen = HashSet::new();
            for u in top {
                let key = usage_key(&u);
                if !seen.insert(key) {
                    continue;
                }
                entry.tokens.add_usage(&u);
            }
        }
    }

    by_day.into_values().collect()
}

// ── Formatting / table ────────────────────────────────────────────────────

static ANSI_RE: OnceLock<Regex> = OnceLock::new();

fn ansi_re() -> &'static Regex {
    ANSI_RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;]*m").unwrap())
}

fn use_color(no_color_flag: bool) -> bool {
    if no_color_flag {
        return false;
    }
    if env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if matches!(
        env::var("FORCE_COLOR").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    ) {
        return true;
    }
    atty_stdout()
}

fn atty_stdout() -> bool {
    std::io::stdout().is_terminal()
}

fn c(enabled: bool, code: &str, text: &str) -> String {
    if !enabled || text.is_empty() {
        return text.to_string();
    }
    format!("\x1b[{code}m{text}\x1b[0m")
}

fn visible_len(s: &str) -> usize {
    ansi_re().replace_all(s, "").chars().count()
}

fn fmt_tokens(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    if n < 1_000_000 {
        let v = n as f64 / 1_000.0;
        let s = format!("{v:.1}");
        return format!("{}K", s.trim_end_matches('0').trim_end_matches('.'));
    }
    if n < 1_000_000_000 {
        let v = n as f64 / 1_000_000.0;
        let s = format!("{v:.2}");
        return format!("{}M", s.trim_end_matches('0').trim_end_matches('.'));
    }
    let v = n as f64 / 1_000_000_000.0;
    let s = format!("{v:.2}");
    format!("{}B", s.trim_end_matches('0').trim_end_matches('.'))
}

fn fmt_money(x: f64) -> String {
    format!("${x:.2}")
}

fn pad(s: &str, width: usize, right: bool) -> String {
    let vis = visible_len(s);
    let padn = width.saturating_sub(vis);
    if right {
        format!("{}{}", " ".repeat(padn), s)
    } else {
        format!("{}{}", s, " ".repeat(padn))
    }
}

fn print_table(headers: &[String], rows: &[Vec<String>], color: bool, total_row: bool) {
    if headers.is_empty() {
        return;
    }
    let n = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| visible_len(h)).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(n) {
            widths[i] = widths[i].max(visible_len(cell));
        }
    }

    let dim = |s: &str| c(color, "2", s);
    let bold = |s: &str| c(color, "1", s);
    let hcyan = |s: &str| c(color, "1;36", s);

    let hline = |left: &str, mid: &str, right: &str| {
        let parts: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        dim(&format!("{left}{}{right}", parts.join(mid)))
    };

    let row_line = |cells: &[String], emphasize: bool| {
        let mut out = Vec::new();
        for i in 0..n {
            let cell = cells.get(i).map(|s| s.as_str()).unwrap_or("");
            let right = i > 0;
            let mut padded = pad(cell, widths[i], right);
            if emphasize {
                padded = bold(&padded);
            }
            out.push(format!(" {padded} "));
        }
        format!(
            "{}{}{}",
            dim("│"),
            out.join(&dim("│")),
            dim("│")
        )
    };

    println!("{}", hline("╭", "┬", "╮"));
    let hcells: Vec<String> = headers.iter().map(|h| hcyan(h)).collect();
    println!("{}", row_line(&hcells, false));
    println!("{}", hline("├", "┼", "┤"));

    for row in rows {
        let plain0 = ansi_re().replace_all(row.first().map(|s| s.as_str()).unwrap_or(""), "");
        let is_total = total_row && plain0.trim().eq_ignore_ascii_case("TOTAL");
        if is_total {
            println!("{}", hline("├", "┼", "┤"));
        }
        let mut cells = row.clone();
        while cells.len() < n {
            cells.push(String::new());
        }
        println!("{}", row_line(&cells[..n], is_total));
    }
    println!("{}", hline("╰", "┴", "╯"));
}

#[derive(Serialize)]
struct AccountRow {
    name: String,
    email: Option<String>,
    user_id: Option<String>,
    active: bool,
    unsaved: bool,
    limit: Option<AccountLimit>,
}

fn collect_account_rows(live_limit: Option<&AccountLimit>) -> Vec<AccountRow> {
    let live = live_identity();
    let live_uid = live.as_ref().and_then(|i| i.user_id.clone());
    let names = list_profile_names().unwrap_or_default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut rows = Vec::new();

    for name in names {
        let stored = read_json(&profile_auth_path(&name)).ok();
        let id = stored.as_ref().and_then(identity_from_auth);
        let uid = id.as_ref().and_then(|i| i.user_id.clone());
        let active = match (live_uid.as_deref(), uid.as_deref()) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        };
        if let Some(u) = &uid {
            seen.insert(u.clone());
        }
        let limit = if active {
            live_limit.cloned()
        } else {
            let mut stored_limit = read_profile_limit(&name);
            if let Some(ref mut l) = stored_limit {
                if l.email.is_none() {
                    l.email = id.as_ref().and_then(|i| i.email.clone());
                }
                if l.profile.is_none() {
                    l.profile = Some(name.clone());
                }
            }
            stored_limit
        };
        rows.push(AccountRow {
            name,
            email: id.as_ref().and_then(|i| i.email.clone()),
            user_id: uid,
            active,
            unsaved: false,
            limit,
        });
    }

    if let Some(id) = live {
        let uid = id.user_id.clone();
        let already = uid.as_ref().is_some_and(|u| seen.contains(u));
        if !already {
            rows.insert(
                0,
                AccountRow {
                    name: id
                        .email
                        .as_deref()
                        .map(default_profile_name_from_email)
                        .unwrap_or_else(|| "(current)".into()),
                    email: id.email.clone(),
                    user_id: uid,
                    active: true,
                    unsaved: true,
                    limit: live_limit.cloned(),
                },
            );
        }
    }
    rows
}

fn default_profile_name_from_email(email: &str) -> String {
    default_profile_name(&AuthIdentity {
        email: Some(email.to_string()),
        user_id: None,
        first_name: None,
        last_name: None,
        expires_at: None,
        auth_mode: None,
    })
}

fn print_account_overview(live_limit: Option<&AccountLimit>, color: bool) {
    let rows = collect_account_rows(live_limit);
    if rows.is_empty() {
        if let Some(l) = live_limit {
            print_account_limit_banner(l, color);
        }
        return;
    }
    if rows.len() == 1 && !rows[0].unsaved {
        if let Some(l) = live_limit.or(rows[0].limit.as_ref()) {
            print_account_limit_banner(l, color);
        } else {
            println!(
                "{}{}",
                c(
                    color,
                    "1",
                    rows[0].email.as_deref().unwrap_or(&rows[0].name)
                ),
                c(color, "2", "  ·  no quota in logs yet")
            );
            println!();
        }
        return;
    }
    if rows.len() == 1 && rows[0].unsaved {
        if let Some(l) = live_limit {
            print_account_limit_banner(l, color);
        } else {
            println!(
                "{}{}",
                c(
                    color,
                    "1",
                    rows[0].email.as_deref().unwrap_or("signed in")
                ),
                c(color, "2", "  ·  no quota in logs yet")
            );
            println!();
        }
        return;
    }

    let headers = vec![
        "".into(),
        "Profile".into(),
        "Account".into(),
        "Limit".into(),
        "Reset".into(),
    ];
    let mut table = Vec::new();
    for r in &rows {
        let mark = if r.active {
            c(color, "1;32", "*")
        } else {
            " ".into()
        };
        let pname = if r.unsaved {
            format!("{} (unsaved)", r.name)
        } else {
            r.name.clone()
        };
        let email = r.email.as_deref().unwrap_or("—");
        let (limit_s, reset_s) = match &r.limit {
            Some(l) => {
                let reset = l
                    .period_end
                    .as_deref()
                    .map(|e| e[..e.len().min(10)].to_string())
                    .unwrap_or_else(|| "—".into());
                (l.headline(), reset)
            }
            None => ("—".into(), "—".into()),
        };
        table.push(vec![
            mark,
            if r.active {
                c(color, "1;32", &pname)
            } else {
                pname
            },
            email.into(),
            if r.active {
                match r.limit.as_ref().and_then(|l| l.percent) {
                    Some(p) if p >= 95.0 => c(color, "1;31", &limit_s),
                    Some(p) if p >= 80.0 => c(color, "1;33", &limit_s),
                    Some(_) => c(color, "1;32", &limit_s),
                    None => limit_s,
                }
            } else {
                c(color, "2", &limit_s)
            },
            c(color, "2", &reset_s),
        ]);
    }
    print_table(&headers, &table, color, false);
    println!();
    println!(
        "{}",
        c(
            color,
            "2",
            "Limit = current (or last saved) Grok /usage quota. Token table below is local logs, not split by account."
        )
    );
    println!();
}

fn print_daily(
    dailies: &[DailyStat],
    json: bool,
    verbose: bool,
    color: bool,
    account_limit: Option<&AccountLimit>,
) {
    if json {
        let payload = serde_json::json!({
            "account_limit": account_limit.map(|l| l.as_public()),
            "accounts": collect_account_rows(account_limit),
            "daily": dailies.iter().map(|d| d.as_public()).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
        return;
    }

    println!(
        "{}{}",
        c(color, "1;36", "Grok Tokens"),
        c(color, "2", "  ·  daily (UTC)")
    );
    println!();
    print_account_overview(account_limit, color);
    if dailies.is_empty() {
        println!("{}", c(color, "33", "No usage events found."));
        return;
    }

    let mut headers = vec![
        "Date", "Reqs", "Sess", "Input", "Cache", "Hit%", "Fresh", "Output", "Total", "NoCache",
        "Billing", "API$",
    ]
    .into_iter()
    .map(String::from)
    .collect::<Vec<_>>();
    if verbose {
        headers.push("Saved".into());
    }

    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut tot = TokenBucket::default();
    let mut all_sess: BTreeSet<String> = BTreeSet::new();

    for d in dailies {
        let t = &d.tokens;
        tot.merge(t);
        all_sess.extend(d.sessions.iter().cloned());
        let mut row = vec![
            c(color, "36", &d.date),
            t.events.to_string(),
            d.sessions.len().to_string(),
            c(color, "32", &fmt_tokens(t.input_tokens)),
            c(color, "2", &fmt_tokens(t.cached_read_tokens)),
            c(color, "2", &format!("{:.0}%", t.hit_pct())),
            c(color, "33", &fmt_tokens(t.fresh_input_tokens())),
            c(color, "35", &fmt_tokens(t.output_tokens)),
            c(color, "1", &fmt_tokens(t.total_tokens)),
            c(color, "33", &fmt_tokens(t.total_without_cache())),
            c(color, "32", &fmt_money(t.cost_billing_usd())),
            c(color, "33", &fmt_money(t.cost_public_usd())),
        ];
        if verbose {
            row.push(c(color, "36", &fmt_money(t.cache_savings_usd())));
        }
        rows.push(row);
    }

    let mut total_row = vec![
        "TOTAL".into(),
        tot.events.to_string(),
        all_sess.len().to_string(),
        fmt_tokens(tot.input_tokens),
        fmt_tokens(tot.cached_read_tokens),
        format!("{:.0}%", tot.hit_pct()),
        fmt_tokens(tot.fresh_input_tokens()),
        fmt_tokens(tot.output_tokens),
        fmt_tokens(tot.total_tokens),
        fmt_tokens(tot.total_without_cache()),
        fmt_money(tot.cost_billing_usd()),
        fmt_money(tot.cost_public_usd()),
    ];
    if verbose {
        total_row.push(fmt_money(tot.cache_savings_usd()));
    }
    rows.push(total_row);

    print_table(&headers, &rows, color, true);
    println!();
    println!(
        "{}",
        c(
            color,
            "2",
            "Billing = costUsdTicks / 1e10 (CLI / SuperGrok receipt). API$ = public list price on turn totals."
        )
    );
    println!(
        "{}",
        c(
            color,
            "2",
            "Total = Input+Output · NoCache = Fresh+Output (excludes cached input)."
        )
    );
    println!(
        "{}",
        c(
            color,
            "2",
            "Cache ⊂ Input · Fresh = Input − Cache · 200k 2× on API$ only if modelCalls ≤ 1."
        )
    );
    if !verbose {
        println!(
            "{}",
            c(color, "2", "Tip: -v shows cache Saved$ vs billing all input at full rate.")
        );
    }
}

fn print_sessions(
    stats: &[SessionStat],
    json: bool,
    sort: SortMode,
    verbose: bool,
    color: bool,
    account_limit: Option<&AccountLimit>,
) {
    let mut ordered = stats.to_vec();
    match sort {
        SortMode::Recent => ordered.sort_by(|a, b| b.last_activity.cmp(&a.last_activity)),
        SortMode::Total => ordered.sort_by(|a, b| b.tokens.total_tokens.cmp(&a.tokens.total_tokens)),
    }

    if json {
        let payload = serde_json::json!({
            "account_limit": account_limit.map(|l| l.as_public()),
            "accounts": collect_account_rows(account_limit),
            "sessions": ordered.iter().map(|s| s.as_public()).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
        return;
    }

    println!(
        "{}{}",
        c(color, "1;36", "Grok Tokens"),
        c(color, "2", "  ·  session (UTC)")
    );
    println!();
    print_account_overview(account_limit, color);
    if ordered.is_empty() {
        println!("{}", c(color, "33", "No sessions found."));
        return;
    }

    let mut headers = vec![
        "Session", "Reqs", "Input", "Cache", "Hit%", "Fresh", "Output", "Total", "NoCache",
        "Billing", "API$", "Peak", "Last",
    ]
    .into_iter()
    .map(String::from)
    .collect::<Vec<_>>();
    if verbose {
        headers.push("Cwd".into());
    }

    let mut rows = Vec::new();
    for s in &ordered {
        let t = &s.tokens;
        let last = if s.last_activity == "unknown" {
            "-".into()
        } else {
            s.last_activity
                .chars()
                .take(16)
                .collect::<String>()
                .replace('T', " ")
        };
        let sid: String = s.session_id.chars().take(10).collect();
        let mut row = if t.events > 0 {
            vec![
                c(color, "36", &sid),
                t.events.to_string(),
                c(color, "32", &fmt_tokens(t.input_tokens)),
                c(color, "2", &fmt_tokens(t.cached_read_tokens)),
                c(color, "2", &format!("{:.0}%", t.hit_pct())),
                c(color, "33", &fmt_tokens(t.fresh_input_tokens())),
                c(color, "35", &fmt_tokens(t.output_tokens)),
                c(color, "1", &fmt_tokens(t.total_tokens)),
                c(color, "33", &fmt_tokens(t.total_without_cache())),
                c(color, "32", &fmt_money(t.cost_billing_usd())),
                c(color, "33", &fmt_money(t.cost_public_usd())),
                c(color, "34", &fmt_tokens(s.peak_context())),
                c(color, "2", &last),
            ]
        } else {
            vec![
                c(color, "36", &sid),
                c(color, "2", "0"),
                c(color, "2", "-"),
                c(color, "2", "-"),
                c(color, "2", "-"),
                c(color, "2", "-"),
                c(color, "2", "-"),
                c(color, "2", "-"),
                c(color, "2", "-"),
                c(color, "2", "-"),
                c(color, "2", "-"),
                if s.peak_context() > 0 {
                    c(color, "34", &fmt_tokens(s.peak_context()))
                } else {
                    c(color, "2", "-")
                },
                c(color, "2", &last),
            ]
        };
        if verbose {
            let mut cwd = if s.cwd.is_empty() {
                "-".into()
            } else {
                s.cwd.clone()
            };
            if cwd.chars().count() > 28 {
                let tail: String = cwd
                    .chars()
                    .rev()
                    .take(27)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                cwd = format!("…{tail}");
            }
            row.push(c(color, "2", &cwd));
        }
        rows.push(row);
    }

    print_table(&headers, &rows, color, false);
    let n_usage = ordered.iter().filter(|s| s.has_usage).count();
    println!();
    println!(
        "{}",
        c(
            color,
            "2",
            &format!(
                "{} sessions · {} with usage · Billing=CLI receipt · API$=public list · Peak=max input",
                ordered.len(),
                n_usage
            )
        )
    );
    if !verbose {
        println!(
            "{}",
            c(color, "2", "Tip: -v shows project path.")
        );
    }
}

// ── Account profiles (local auth.json snapshots) ──────────────────────────

#[derive(Debug, Clone, Serialize)]
struct AuthIdentity {
    email: Option<String>,
    user_id: Option<String>,
    first_name: Option<String>,
    last_name: Option<String>,
    expires_at: Option<String>,
    auth_mode: Option<String>,
}

impl AuthIdentity {
    fn display_name(&self) -> Option<String> {
        let first = self.first_name.as_deref().unwrap_or("").trim();
        let last = self.last_name.as_deref().unwrap_or("").trim();
        let s = format!("{first} {last}").trim().to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    fn as_public(&self) -> Value {
        serde_json::json!({
            "email": self.email,
            "user_id": self.user_id,
            "name": self.display_name(),
            "expires_at": self.expires_at,
            "auth_mode": self.auth_mode,
        })
    }
}

fn json_opt_str(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn identity_from_auth(v: &Value) -> Option<AuthIdentity> {
    let obj = v.as_object()?;
    for (_scope, entry) in obj {
        let Some(e) = entry.as_object() else {
            continue;
        };
        if e.get("email").is_none() && e.get("user_id").is_none() && e.get("key").is_none() {
            continue;
        }
        return Some(AuthIdentity {
            email: json_opt_str(e, "email"),
            user_id: json_opt_str(e, "user_id").or_else(|| json_opt_str(e, "principal_id")),
            first_name: json_opt_str(e, "first_name"),
            last_name: json_opt_str(e, "last_name"),
            expires_at: json_opt_str(e, "expires_at"),
            auth_mode: json_opt_str(e, "auth_mode"),
        });
    }
    None
}

fn sanitize_stem(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
        } else if matches!(c, '+' | ' ') {
            out.push('-');
        }
    }
    out.trim_matches(|c| c == '.' || c == '-').to_string()
}

fn default_profile_name(id: &AuthIdentity) -> String {
    if let Some(email) = &id.email {
        let local = email.split('@').next().unwrap_or(email);
        let s = sanitize_stem(local);
        if !s.is_empty() {
            return s;
        }
    }
    if let Some(uid) = &id.user_id {
        let s: String = uid
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(8)
            .collect();
        if !s.is_empty() {
            return format!("user-{s}");
        }
    }
    "default".into()
}

fn validate_profile_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        anyhow::bail!("profile name must be 1–64 characters");
    }
    if name == "." || name == ".." || name == "current" {
        anyhow::bail!("invalid profile name: {name}");
    }
    if name.starts_with('.') {
        anyhow::bail!("profile name must not start with '.'");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        anyhow::bail!("profile name may only contain A-Z a-z 0-9 . _ -");
    }
    Ok(())
}

fn profile_auth_path(name: &str) -> PathBuf {
    profiles_dir().join(name).join("auth.json")
}

fn profile_limit_path(name: &str) -> PathBuf {
    profiles_dir().join(name).join("limit.json")
}

fn account_limit_from_stored(v: &Value) -> Option<AccountLimit> {
    Some(AccountLimit {
        percent: v.get("percent").and_then(|x| x.as_f64()),
        period_label: v
            .get("period_label")
            .and_then(|x| x.as_str())
            .unwrap_or("Period")
            .to_string(),
        period_type: v
            .get("period_type")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        period_start: v
            .get("period_start")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        period_end: v
            .get("period_end")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        subscription: v
            .get("subscription")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        fetched_at: v
            .get("fetched_at")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        source: v
            .get("source")
            .and_then(|x| x.as_str())
            .unwrap_or("profile")
            .to_string(),
        email: v
            .get("email")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        user_id: v
            .get("user_id")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
        profile: v
            .get("profile")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
    })
}

fn read_profile_limit(name: &str) -> Option<AccountLimit> {
    account_limit_from_stored(&read_json(&profile_limit_path(name)).ok()?)
}

fn write_profile_limit(name: &str, limit: &AccountLimit) -> Result<()> {
    write_secret_json(&profile_limit_path(name), &limit.as_public())
}

fn live_identity() -> Option<AuthIdentity> {
    identity_from_auth(&read_auth_file().ok()??)
}

fn enrich_limit(mut limit: AccountLimit) -> AccountLimit {
    if let Some(id) = live_identity() {
        limit.email = id.email.clone();
        limit.user_id = id.user_id.clone();
        limit.profile = matching_profile(&id).ok().flatten();
        if let Some(ref name) = limit.profile {
            let _ = write_profile_limit(name, &limit);
        }
    }
    limit
}

fn read_json(path: &Path) -> Result<Value> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("invalid JSON in {}", path.display()))
}

fn read_auth_file() -> Result<Option<Value>> {
    let path = auth_json_path();
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(read_json(&path)?))
}

fn set_secret_perms(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    let _ = path;
    Ok(())
}

fn write_secret_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = File::create(&tmp)
            .with_context(|| format!("failed to create {}", tmp.display()))?;
        serde_json::to_writer_pretty(&mut f, value)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }
    set_secret_perms(&tmp)?;
    fs::rename(&tmp, path).with_context(|| format!("failed to write {}", path.display()))?;
    set_secret_perms(path)?;
    Ok(())
}

fn read_current_profile() -> Result<Option<String>> {
    let p = profiles_dir().join("current");
    if !p.is_file() {
        return Ok(None);
    }
    let s = fs::read_to_string(p)?;
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    Ok(Some(s.to_string()))
}

fn write_current_profile(name: &str) -> Result<()> {
    let dir = profiles_dir();
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("current"), format!("{name}\n"))?;
    Ok(())
}

fn clear_current_profile() -> Result<()> {
    let p = profiles_dir().join("current");
    if p.is_file() {
        fs::remove_file(p)?;
    }
    Ok(())
}

fn list_profile_names() -> Result<Vec<String>> {
    let dir = profiles_dir();
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if validate_profile_name(&name).is_err() {
            continue;
        }
        if entry.path().join("auth.json").is_file() {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

fn persist_live_auth() -> Result<Option<String>> {
    let live = match read_auth_file()? {
        Some(v) => v,
        None => return Ok(None),
    };
    let live_uid = identity_from_auth(&live).and_then(|i| i.user_id);

    let mut candidates = Vec::new();
    if let Some(cur) = read_current_profile()? {
        candidates.push(cur);
    }
    for name in list_profile_names()? {
        if !candidates.contains(&name) {
            candidates.push(name);
        }
    }

    for name in candidates {
        let path = profile_auth_path(&name);
        if !path.is_file() {
            continue;
        }
        let stored = read_json(&path)?;
        let stored_uid = identity_from_auth(&stored).and_then(|i| i.user_id);
        match (live_uid.as_deref(), stored_uid.as_deref()) {
            (Some(a), Some(b)) if a == b => {
                write_secret_json(&path, &live)?;
                return Ok(Some(name));
            }
            _ => {}
        }
    }
    Ok(None)
}

fn require_live_auth() -> Result<(Value, AuthIdentity)> {
    let value = read_auth_file()?.ok_or_else(|| {
        anyhow::anyhow!(
            "not signed in (no {})\nRun: grok login",
            auth_json_path().display()
        )
    })?;
    let id = identity_from_auth(&value).ok_or_else(|| {
        anyhow::anyhow!(
            "could not read account identity from {}",
            auth_json_path().display()
        )
    })?;
    Ok((value, id))
}

fn matching_profile(id: &AuthIdentity) -> Result<Option<String>> {
    let uid = match &id.user_id {
        Some(u) => u.clone(),
        None => return Ok(read_current_profile()?),
    };
    if let Some(cur) = read_current_profile()? {
        if let Ok(stored) = read_json(&profile_auth_path(&cur)) {
            if identity_from_auth(&stored).and_then(|i| i.user_id) == Some(uid.clone()) {
                return Ok(Some(cur));
            }
        }
    }
    for name in list_profile_names()? {
        if let Ok(stored) = read_json(&profile_auth_path(&name)) {
            if identity_from_auth(&stored).and_then(|i| i.user_id) == Some(uid.clone()) {
                return Ok(Some(name));
            }
        }
    }
    Ok(None)
}

fn run_account(cmd: &AccountCmd, json: bool, color: bool) -> Result<()> {
    match cmd {
        AccountCmd::Whoami => account_whoami(json, color),
        AccountCmd::Save { name } => account_save(name.as_deref(), json, color),
        AccountCmd::List => account_list(json, color),
        AccountCmd::Switch { name } => account_switch(name, json, color),
        AccountCmd::Remove { name } => account_remove(name, json, color),
        AccountCmd::Export { name, out } => account_export(name.as_deref(), out.as_deref(), json, color),
        AccountCmd::Import {
            file,
            name,
            no_switch,
        } => account_import(file, name.as_deref(), *no_switch, json, color),
    }
}

fn account_whoami(json: bool, color: bool) -> Result<()> {
    let (_value, id) = require_live_auth()?;
    let profile = matching_profile(&id)?;
    if json {
        let mut out = id.as_public();
        if let Some(obj) = out.as_object_mut() {
            obj.insert("logged_in".into(), Value::Bool(true));
            obj.insert(
                "profile".into(),
                profile
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            obj.insert(
                "auth_path".into(),
                Value::String(auth_json_path().display().to_string()),
            );
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    println!(
        "{}{}",
        c(color, "1;36", "Grok account"),
        profile
            .as_deref()
            .map(|p| format!("  ·  {}", c(color, "32", &format!("profile {p}"))))
            .unwrap_or_default()
    );
    println!();
    println!("  Email        {}", id.email.as_deref().unwrap_or("—"));
    println!("  Name         {}", id.display_name().as_deref().unwrap_or("—"));
    println!("  User ID      {}", id.user_id.as_deref().unwrap_or("—"));
    println!("  Auth         {}", id.auth_mode.as_deref().unwrap_or("—"));
    println!("  Expires      {}", id.expires_at.as_deref().unwrap_or("—"));
    println!("  Auth file    {}", auth_json_path().display());
    Ok(())
}

fn account_save(name: Option<&str>, json: bool, color: bool) -> Result<()> {
    let (value, id) = require_live_auth()?;
    let name = match name {
        Some(n) => {
            validate_profile_name(n)?;
            n.to_string()
        }
        None => {
            let n = default_profile_name(&id);
            validate_profile_name(&n)?;
            n
        }
    };
    let dest = profile_auth_path(&name);
    if dest.is_file() {
        if let Ok(stored) = read_json(&dest) {
            let stored_id = identity_from_auth(&stored);
            let stored_uid = stored_id.as_ref().and_then(|s| s.user_id.clone());
            if let (Some(a), Some(b)) = (id.user_id.as_deref(), stored_uid.as_deref()) {
                if a != b {
                    let other = stored_id
                        .and_then(|s| s.email)
                        .unwrap_or_else(|| b.to_string());
                    anyhow::bail!(
                        "profile '{name}' already belongs to {other}\nPick another name or: grok-tokens account remove {name}"
                    );
                }
            }
        }
    }
    write_secret_json(&dest, &value)?;
    write_current_profile(&name)?;
    if let Some(mut limit) = load_account_limit(4_000_000) {
        limit.email = id.email.clone();
        limit.user_id = id.user_id.clone();
        limit.profile = Some(name.clone());
        let _ = write_profile_limit(&name, &limit);
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "action": "save",
                "name": name,
                "email": id.email,
                "user_id": id.user_id,
                "path": dest.display().to_string(),
            }))?
        );
        return Ok(());
    }
    println!(
        "Saved profile {} ({})",
        c(color, "1;32", &name),
        id.email.as_deref().unwrap_or("unknown")
    );
    println!("{}", c(color, "2", &format!("  {}", dest.display())));
    Ok(())
}

fn account_list(json: bool, color: bool) -> Result<()> {
    let names = list_profile_names()?;
    let live = read_auth_file().ok().flatten();
    let live_id = live.as_ref().and_then(identity_from_auth);
    let live_uid = live_id.as_ref().and_then(|i| i.user_id.clone());
    let current = read_current_profile()?;

    let mut rows_json = Vec::new();
    for name in &names {
        let stored = read_json(&profile_auth_path(name)).ok();
        let id = stored.as_ref().and_then(identity_from_auth);
        let active = match (live_uid.as_deref(), id.as_ref().and_then(|i| i.user_id.as_deref())) {
            (Some(a), Some(b)) => a == b,
            _ => current.as_deref() == Some(name.as_str()),
        };
        rows_json.push(serde_json::json!({
            "name": name,
            "email": id.as_ref().and_then(|i| i.email.clone()),
            "user_id": id.as_ref().and_then(|i| i.user_id.clone()),
            "name_display": id.as_ref().and_then(|i| i.display_name()),
            "expires_at": id.as_ref().and_then(|i| i.expires_at.clone()),
            "active": active,
        }));
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "current": current,
                "live": live_id.as_ref().map(|i| i.as_public()),
                "profiles": rows_json,
            }))?
        );
        return Ok(());
    }

    if names.is_empty() {
        println!("{}", c(color, "33", "No saved profiles."));
        println!(
            "{}",
            c(
                color,
                "2",
                "Save the current login:  grok-tokens account save [name]"
            )
        );
        return Ok(());
    }

    let headers = vec![
        "".into(),
        "Profile".into(),
        "Email".into(),
        "Expires".into(),
    ];
    let mut rows = Vec::new();
    for item in &rows_json {
        let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let email = item
            .get("email")
            .and_then(|v| v.as_str())
            .unwrap_or("—");
        let exp = item
            .get("expires_at")
            .and_then(|v| v.as_str())
            .unwrap_or("—");
        let exp = if exp.len() >= 10 { &exp[..10] } else { exp };
        let active = item.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
        let mark = if active {
            c(color, "1;32", "*")
        } else {
            " ".into()
        };
        rows.push(vec![
            mark,
            if active {
                c(color, "1;32", name)
            } else {
                name.into()
            },
            email.into(),
            c(color, "2", exp),
        ]);
    }
    print_table(&headers, &rows, color, false);
    println!();
    println!(
        "{}",
        c(
            color,
            "2",
            &format!(
                "{} profile{}  ·  * = matches current ~/.grok/auth.json  ·  {}",
                names.len(),
                if names.len() == 1 { "" } else { "s" },
                profiles_dir().display()
            )
        )
    );
    Ok(())
}

fn account_switch(name: &str, json: bool, color: bool) -> Result<()> {
    validate_profile_name(name)?;
    let src = profile_auth_path(name);
    if !src.is_file() {
        anyhow::bail!(
            "no profile '{name}'\nSaved: {}\nList:  grok-tokens account list",
            list_profile_names()?.join(", ")
        );
    }
    let incoming = read_json(&src)?;
    let id = identity_from_auth(&incoming).ok_or_else(|| {
        anyhow::anyhow!("profile '{name}' has no account identity")
    })?;

    let persisted = persist_live_auth()?;
    if let Some(ref prev) = persisted {
        if let Some(mut limit) = load_account_limit(4_000_000) {
            if let Some(cur) = live_identity() {
                limit.email = cur.email;
                limit.user_id = cur.user_id;
            }
            limit.profile = Some(prev.clone());
            let _ = write_profile_limit(prev, &limit);
        }
    }
    write_secret_json(&auth_json_path(), &incoming)?;
    write_current_profile(name)?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "action": "switch",
                "name": name,
                "email": id.email,
                "user_id": id.user_id,
                "persisted": persisted,
                "auth_path": auth_json_path().display().to_string(),
            }))?
        );
        return Ok(());
    }
    if let Some(prev) = persisted.as_deref() {
        if prev != name {
            println!(
                "{}",
                c(color, "2", &format!("Updated snapshot for profile {prev}"))
            );
        }
    }
    println!(
        "Switched to {} ({})",
        c(color, "1;32", name),
        id.email.as_deref().unwrap_or("unknown")
    );
    println!(
        "{}",
        c(
            color,
            "2",
            "Grok picks this up on the next API call. Restart a running session if it does not."
        )
    );
    Ok(())
}

fn account_remove(name: &str, json: bool, color: bool) -> Result<()> {
    validate_profile_name(name)?;
    let dir = profiles_dir().join(name);
    let auth = dir.join("auth.json");
    if !auth.is_file() {
        anyhow::bail!("no profile '{name}'");
    }
    fs::remove_file(&auth)?;
    if dir.is_dir() {
        let _ = fs::remove_dir(&dir);
    }
    if read_current_profile()?.as_deref() == Some(name) {
        clear_current_profile()?;
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "action": "remove",
                "name": name,
            }))?
        );
        return Ok(());
    }
    println!("Removed profile {}", c(color, "1;33", name));
    println!(
        "{}",
        c(
            color,
            "2",
            "Grok stays signed in; this only deletes the saved snapshot."
        )
    );
    Ok(())
}

const ACCOUNT_BUNDLE_FORMAT: &str = "grok-tokens-account";

fn parse_account_bundle(v: &Value) -> Result<(Value, Option<String>)> {
    if v.get("format").and_then(|x| x.as_str()) == Some(ACCOUNT_BUNDLE_FORMAT) {
        let auth = v
            .get("auth")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("export file is missing auth"))?;
        if identity_from_auth(&auth).is_none() {
            anyhow::bail!("export file auth has no account identity");
        }
        let name = v
            .get("name")
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        return Ok((auth, name));
    }
    if identity_from_auth(v).is_some() {
        return Ok((v.clone(), None));
    }
    anyhow::bail!("file is not a grok-tokens export or Grok auth.json")
}

fn account_export(
    name: Option<&str>,
    out: Option<&Path>,
    json: bool,
    color: bool,
) -> Result<()> {
    let _ = persist_live_auth();
    let (auth, id, profile_name) = if let Some(n) = name {
        validate_profile_name(n)?;
        let path = profile_auth_path(n);
        if !path.is_file() {
            anyhow::bail!("no profile '{n}' — save it first: grok-tokens account save {n}");
        }
        let auth = read_json(&path)?;
        let id = identity_from_auth(&auth)
            .ok_or_else(|| anyhow::anyhow!("profile '{n}' has no account identity"))?;
        (auth, id, n.to_string())
    } else {
        let (auth, id) = require_live_auth()?;
        let n = matching_profile(&id)?.unwrap_or_else(|| default_profile_name(&id));
        (auth, id, n)
    };

    let out = match out {
        Some(p) if p.as_os_str() == "-" => {
            anyhow::bail!("refusing to write credentials to stdout; pass --out FILE")
        }
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(format!("grok-account-{profile_name}.json")),
    };

    let bundle = serde_json::json!({
        "format": ACCOUNT_BUNDLE_FORMAT,
        "version": 1,
        "exported_at": Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "name": profile_name,
        "email": id.email,
        "user_id": id.user_id,
        "auth": auth,
        "limit": read_profile_limit(&profile_name).map(|l| l.as_public()),
    });
    write_secret_json(&out, &bundle)?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "action": "export",
                "name": profile_name,
                "email": id.email,
                "user_id": id.user_id,
                "path": out.display().to_string(),
            }))?
        );
        return Ok(());
    }
    println!(
        "Exported {} ({}) → {}",
        c(color, "1;32", &profile_name),
        id.email.as_deref().unwrap_or("unknown"),
        out.display()
    );
    println!(
        "{}",
        c(
            color,
            "33",
            "Contains a refresh token. Do not git-commit, chat, or email this file."
        )
    );
    println!(
        "{}",
        c(
            color,
            "2",
            "On the other side:  grok-tokens account import <file>"
        )
    );
    Ok(())
}

fn account_import(
    file: &Path,
    name: Option<&str>,
    no_switch: bool,
    json: bool,
    color: bool,
) -> Result<()> {
    let raw = read_json(file)?;
    let (auth, bundle_name) = parse_account_bundle(&raw)?;
    let id = identity_from_auth(&auth)
        .ok_or_else(|| anyhow::anyhow!("import file has no account identity"))?;

    let name = match name {
        Some(n) => {
            validate_profile_name(n)?;
            n.to_string()
        }
        None => {
            let n = bundle_name.unwrap_or_else(|| default_profile_name(&id));
            validate_profile_name(&n)?;
            n
        }
    };

    let dest = profile_auth_path(&name);
    if dest.is_file() {
        if let Ok(stored) = read_json(&dest) {
            let stored_id = identity_from_auth(&stored);
            let stored_uid = stored_id.as_ref().and_then(|s| s.user_id.clone());
            if let (Some(a), Some(b)) = (id.user_id.as_deref(), stored_uid.as_deref()) {
                if a != b {
                    let other = stored_id
                        .and_then(|s| s.email)
                        .unwrap_or_else(|| b.to_string());
                    anyhow::bail!(
                        "profile '{name}' already belongs to {other}\nPick --name or: grok-tokens account remove {name}"
                    );
                }
            }
        }
    }

    if !no_switch {
        let _ = persist_live_auth();
    }
    write_secret_json(&dest, &auth)?;
    if let Some(limit_v) = raw.get("limit") {
        if let Some(mut limit) = account_limit_from_stored(limit_v) {
            limit.email = id.email.clone();
            limit.user_id = id.user_id.clone();
            limit.profile = Some(name.clone());
            let _ = write_profile_limit(&name, &limit);
        }
    }
    write_current_profile(&name)?;
    if !no_switch {
        write_secret_json(&auth_json_path(), &auth)?;
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "action": "import",
                "name": name,
                "email": id.email,
                "user_id": id.user_id,
                "switched": !no_switch,
                "profile_path": dest.display().to_string(),
                "auth_path": auth_json_path().display().to_string(),
            }))?
        );
        return Ok(());
    }
    println!(
        "Imported {} ({})",
        c(color, "1;32", &name),
        id.email.as_deref().unwrap_or("unknown")
    );
    if no_switch {
        println!(
            "{}",
            c(
                color,
                "2",
                &format!("Saved profile only. Switch later:  grok-tokens account switch {name}")
            )
        );
    } else {
        println!(
            "{}",
            c(
                color,
                "2",
                &format!("Wrote live login:  {}", auth_json_path().display())
            )
        );
        println!(
            "{}",
            c(
                color,
                "2",
                "Grok picks this up on the next API call. Restart a running session if it does not."
            )
        );
    }
    Ok(())
}

/// `--since` → canonical `YYYY-MM-DD`. Accepts `YYYY-MM-DD` and `YYYYMMDD`.
fn normalize_since_date(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    let (ys, ms, ds) = if s.len() == 8 && s.bytes().all(|b| b.is_ascii_digit()) {
        (&s[0..4], &s[4..6], &s[6..8])
    } else if s.len() == 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-' {
        (&s[0..4], &s[5..7], &s[8..10])
    } else {
        return Err(since_date_hint());
    };
    let y: i32 = ys.parse().map_err(|_| since_date_hint())?;
    let m: u32 = ms.parse().map_err(|_| since_date_hint())?;
    let d: u32 = ds.parse().map_err(|_| since_date_hint())?;
    if NaiveDate::from_ymd_opt(y, m, d).is_none() {
        return Err("not a valid calendar date".into());
    }
    Ok(format!("{y:04}-{m:02}-{d:02}"))
}

fn since_date_hint() -> String {
    "expected YYYY-MM-DD or YYYYMMDD".into()
}


// ── CLI ───────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "grok-tokens",
    version = VERSION,
    about = "Count real Grok Build usage tokens from local session logs"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Sessions root (default: $GROK_DATA_DIR or ~/.grok/sessions)
    #[arg(long, global = true)]
    root: Option<PathBuf>,

    /// Only include data on/after this UTC date (YYYY-MM-DD or YYYYMMDD)
    #[arg(long, global = true, value_name = "DATE", value_parser = normalize_since_date)]
    since: Option<String>,

    /// Max session directories to scan (newest first)
    #[arg(long, global = true, default_value_t = 200)]
    limit: usize,

    /// Only sessions whose project cwd matches this path
    #[arg(long, global = true)]
    cwd: Option<String>,

    /// JSON output
    #[arg(long, global = true)]
    json: bool,

    /// session: hide sessions with zero usage events
    #[arg(long, global = true)]
    usage_only: bool,

    /// Show cache Saved$ (daily) and project path (session)
    #[arg(short = 'v', long, global = true)]
    verbose: bool,

    /// Disable ANSI colors
    #[arg(long, global = true)]
    no_color: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Token totals aggregated by UTC day
    Daily,
    /// Per-session token totals
    Session {
        /// Sort by total tokens (default) or last activity
        #[arg(long, value_enum, default_value_t = SortMode::Total)]
        sort: SortMode,
    },
    /// Account Weekly/Monthly limit (same source as Grok /usage)
    Limit,
    /// Save / switch Grok login profiles (local auth.json snapshots)
    Account {
        #[command(subcommand)]
        command: AccountCmd,
    },
}

#[derive(Subcommand, Debug)]
enum AccountCmd {
    /// Show the signed-in Grok account
    Whoami,
    /// Snapshot ~/.grok/auth.json as a named profile
    Save {
        /// Profile name (default: email local-part)
        name: Option<String>,
    },
    /// List saved profiles
    List,
    /// Activate a saved profile (replaces auth.json)
    Switch {
        name: String,
    },
    /// Delete a saved profile (does not log out of Grok)
    Remove {
        name: String,
    },
    /// Write a portable account file (for WSL ↔ Windows)
    Export {
        /// Profile to export (default: current login)
        name: Option<String>,
        /// Output file (default: grok-account-<name>.json)
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Load a file from `account export` (or raw auth.json)
    Import {
        /// Exported bundle or ~/.grok/auth.json
        file: PathBuf,
        /// Profile name (default: name in file, else email local-part)
        #[arg(long)]
        name: Option<String>,
        /// Save profile only; do not replace live auth.json
        #[arg(long)]
        no_switch: bool,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, Default)]
enum SortMode {
    #[default]
    Total,
    Recent,
}

fn path_norm(p: &str) -> PathBuf {
    // lightweight normalize
    PathBuf::from(p)
}

fn print_limit_detail(limit: Option<&AccountLimit>, json: bool, color: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&limit.map(|l| l.as_public())).unwrap()
        );
        return;
    }
    match limit {
        None => {
            println!("{}", c(color, "33", "No account limit data found."));
            println!(
                "{}",
                c(
                    color,
                    "2",
                    "Open Grok CLI and run /usage once, or keep a session running so billing is logged."
                )
            );
            println!(
                "{}",
                c(
                    color,
                    "2",
                    &format!(
                        "Expected log: {}",
                        grok_home().join("logs").join("unified.jsonl").display()
                    )
                )
            );
        }
        Some(l) => {
            println!(
                "{}{}",
                c(color, "1;36", "Grok Account Limit"),
                c(color, "2", "  ·  same source as /usage")
            );
            println!();
            print_account_limit_banner(l, color);
            println!("  Account      {}", l.email.as_deref().unwrap_or("—"));
            println!("  Profile      {}", l.profile.as_deref().unwrap_or("—"));
            println!("  User ID      {}", l.user_id.as_deref().unwrap_or("—"));
            println!("  Limit        {}", l.headline());
            println!(
                "  Period       {} ({})",
                l.period_label,
                if l.period_type.is_empty() {
                    "—"
                } else {
                    &l.period_type
                }
            );
            println!(
                "  Start        {}",
                l.period_start.as_deref().unwrap_or("—")
            );
            println!(
                "  End / reset  {}",
                l.period_end.as_deref().unwrap_or("—")
            );
            println!(
                "  Plan         {}",
                l.subscription.as_deref().unwrap_or("—")
            );
            println!(
                "  Fetched      {}",
                l.fetched_at.as_deref().unwrap_or("—")
            );
            println!("  Source       {}", l.source);
            println!();
            println!(
                "{}",
                c(
                    color,
                    "2",
                    "Note: This is account quota %, not local session token totals."
                )
            );
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let color = use_color(cli.no_color);

    if let Commands::Account { command } = &cli.command {
        return run_account(command, cli.json, color);
    }

    let account_limit = load_account_limit(4_000_000).map(enrich_limit);

    if matches!(cli.command, Commands::Limit) {
        print_limit_detail(account_limit.as_ref(), cli.json, color);
        if account_limit.is_none() {
            std::process::exit(1);
        }
        return Ok(());
    }

    let root = cli.root.clone().unwrap_or_else(sessions_root);
    if !root.is_dir() {
        eprintln!("No sessions root at {}", root.display());
        eprintln!("Set GROK_DATA_DIR or GROK_HOME, or pass --root.");
        std::process::exit(1);
    }

    let mut session_dirs = discover_sessions(&root, cli.limit);
    if session_dirs.is_empty() {
        eprintln!(
            "No session directories with updates.jsonl under {}",
            root.display()
        );
        std::process::exit(1);
    }

    if let Some(ref cwd) = cli.cwd {
        let want = path_norm(cwd);
        session_dirs.retain(|p| {
            let c = read_cwd(p);
            !c.is_empty() && path_norm(&c) == want
        });
        if session_dirs.is_empty() {
            eprintln!("No sessions for cwd={cwd} under {}", root.display());
            std::process::exit(1);
        }
    }

    match cli.command {
        Commands::Daily => {
            let dailies = daily_from_raw(&session_dirs, cli.since.as_deref());
            print_daily(
                &dailies,
                cli.json,
                cli.verbose,
                color,
                account_limit.as_ref(),
            );
        }
        Commands::Session { sort } => {
            let mut stats: Vec<SessionStat> = session_dirs
                .iter()
                .filter_map(|p| load_session(p))
                .collect();
            if let Some(ref since) = cli.since {
                stats.retain(|s| {
                    s.last_activity_day == "unknown"
                        || s.last_activity_day.as_str() >= since.as_str()
                });
            }
            if cli.usage_only {
                stats.retain(|s| s.has_usage);
            }
            print_sessions(
                &stats,
                cli.json,
                sort,
                cli.verbose,
                color,
                account_limit.as_ref(),
            );
        }
        Commands::Limit => unreachable!(),
        Commands::Account { .. } => unreachable!(),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn profile_name_rules() {
        assert!(validate_profile_name("work").is_ok());
        assert!(validate_profile_name("huhan.ken").is_ok());
        assert!(validate_profile_name("user-4b8c12c2").is_ok());
        assert!(validate_profile_name("..").is_err());
        assert!(validate_profile_name("current").is_err());
        assert!(validate_profile_name("foo/bar").is_err());
        assert!(validate_profile_name(".hidden").is_err());
    }

    #[test]
    fn identity_parses_oidc_blob() {
        let v = json!({
            "https://auth.x.ai::abc": {
                "email": "a@b.com",
                "user_id": "uid-1",
                "first_name": "Ada",
                "last_name": "Lovelace",
                "expires_at": "2026-01-01T00:00:00Z",
                "auth_mode": "oidc",
                "key": "secret-must-not-be-required-for-identity"
            }
        });
        let id = identity_from_auth(&v).unwrap();
        assert_eq!(id.email.as_deref(), Some("a@b.com"));
        assert_eq!(id.user_id.as_deref(), Some("uid-1"));
        assert_eq!(id.display_name().as_deref(), Some("Ada Lovelace"));
        assert_eq!(default_profile_name(&id), "a");
    }

    #[test]
    fn parse_bundle_and_raw_auth() {
        let auth = json!({
            "https://auth.x.ai::abc": {
                "email": "a@b.com",
                "user_id": "uid-1"
            }
        });
        let bundle = json!({
            "format": "grok-tokens-account",
            "version": 1,
            "name": "work",
            "auth": auth
        });
        let (got, name) = parse_account_bundle(&bundle).unwrap();
        assert_eq!(name.as_deref(), Some("work"));
        assert_eq!(identity_from_auth(&got).unwrap().email.as_deref(), Some("a@b.com"));

        let (raw, raw_name) = parse_account_bundle(&auth).unwrap();
        assert!(raw_name.is_none());
        assert_eq!(identity_from_auth(&raw).unwrap().user_id.as_deref(), Some("uid-1"));

        assert!(parse_account_bundle(&json!({"hello": 1})).is_err());
    }

    fn usage(
        model: &str,
        input: u64,
        cache: u64,
        output: u64,
        calls: u64,
        ticks: u64,
    ) -> Value {
        json!({
            "inputTokens": input,
            "outputTokens": output,
            "totalTokens": input + output,
            "cachedReadTokens": cache,
            "modelCalls": calls,
            "costUsdTicks": ticks,
            "modelUsage": { model: {
                "inputTokens": input,
                "outputTokens": output,
                "totalTokens": input + output,
                "cachedReadTokens": cache,
            }}
        })
    }

    #[test]
    fn ticks_are_1e_10_usd() {
        // 08-14 single-call grok-4.6-build: ticks match official short list price.
        let u = usage("grok-4.6-build", 13_867, 256, 451, 1, 300_560_000);
        assert!((event_cost_usd(&u) - 0.030056).abs() < 1e-9);
        assert!((cost_from_ticks(300_560_000) - 0.030056).abs() < 1e-9);
    }

    #[test]
    fn cost_prefers_ticks_over_rate_table() {
        // Same tokens as a 200k+ multi-call turn: ticks win, even though the
        // old logic would have applied the 2× cliff to the whole sum.
        let u = usage("grok-4.6-build", 4_256_897, 4_185_984, 21_905, 24, 4_403_000_000);
        assert!((event_cost_usd(&u) - 0.4403).abs() < 1e-9);
        assert!(rate_table_cost(&u, true) > event_cost_usd(&u) * 4.0);
    }

    #[test]
    fn rate_table_skips_200k_cliff_on_aggregated_turns() {
        let multi = usage("grok-4.6-build", 250_000, 0, 0, 5, 0);
        let single = usage("grok-4.6-build", 250_000, 0, 0, 1, 0);
        // Short $2/M vs long $4/M.
        assert!((rate_table_cost(&multi, true) - 0.50).abs() < 1e-9);
        assert!((rate_table_cost(&single, true) - 1.00).abs() < 1e-9);
    }

    #[test]
    fn grok_46_cache_rate_differs_from_45() {
        let u46 = usage("grok-4.6-build", 1_000_000, 1_000_000, 0, 2, 0);
        let u45 = usage("grok-4.5-build", 1_000_000, 1_000_000, 0, 2, 0);
        assert!((rate_table_cost(&u46, true) - 0.50).abs() < 1e-9);
        assert!((rate_table_cost(&u45, true) - 0.30).abs() < 1e-9);
    }

    #[test]
    fn since_accepts_iso_and_compact_dates() {
        assert_eq!(normalize_since_date("2026-10-01").unwrap(), "2026-10-01");
        assert_eq!(normalize_since_date("20261001").unwrap(), "2026-10-01");
        assert_eq!(normalize_since_date("2024-02-29").unwrap(), "2024-02-29");
        assert_eq!(normalize_since_date("20240229").unwrap(), "2024-02-29");
    }

    #[test]
    fn since_rejects_other_shapes_and_impossible_days() {
        for raw in [
            "",
            "2026/10/01",
            "2026-10-1",
            "2026100",
            "202610011",
            "10-01-2026",
            "not-a-date",
        ] {
            assert!(
                normalize_since_date(raw).unwrap_err().contains("YYYY-MM-DD"),
                "{raw}"
            );
        }
        for raw in ["2026-02-30", "2026-02-29", "20260230", "20261301"] {
            assert!(
                normalize_since_date(raw).unwrap_err().contains("calendar"),
                "{raw}"
            );
        }
    }

    #[test]
    fn since_flag_normalizes_before_and_after_subcommand() {
        use clap::Parser;

        let before = Cli::try_parse_from(["grok-tokens", "--since", "20261001", "daily"]).unwrap();
        assert_eq!(before.since.as_deref(), Some("2026-10-01"));

        let after = Cli::try_parse_from(["grok-tokens", "daily", "--since", "2026-10-01"]).unwrap();
        assert_eq!(after.since.as_deref(), Some("2026-10-01"));

        assert!(Cli::try_parse_from(["grok-tokens", "--since", "2026-02-30", "daily"]).is_err());
    }
}
