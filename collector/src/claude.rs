//! Claude Code account, live usage limits, remote-control server, drops and sessions.

use crate::system::{
    is_offline, journal_incremental, journal_ts, load_day_cache, now, parse_dur, parse_iso, read_json, run,
    save_day_cache, status_from_line, strip_ansi, tool, trace, with_last_good, CLAUDE_UNIT,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

#[derive(Deserialize)]
struct GlobalConfig {
    #[serde(rename = "oauthAccount")]
    oauth_account: Option<Value>,
}

/// Settings that can make the CLI pick another login than the claude.ai one in .credentials.json.
const AUTH_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_UNIX_SOCKET",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
    "CLAUDE_CODE_REMOTE",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_CODE_CUSTOM_OAUTH_URL",
];

fn auth_env_name(k: &str) -> bool {
    AUTH_ENV.contains(&k) || k.starts_with("CLAUDE_CODE_USE_")
}

/// Live Claude plan limits are opt-in (`{"claude_live_limits": true}`).
pub fn claude_live_enabled(cfg: Option<&Value>) -> bool {
    cfg.is_some_and(|c| c["claude_live_limits"].as_bool() == Some(true))
}

/// Claude Code's credentials, reduced to what the collector uses: plan, tier, scopes, expiry and whether a
/// login exists. The access token itself is kept only when live limits are on; the refresh token and
/// everything else in the file are dropped right after parsing.
pub fn read_creds(p: &Path, with_token: bool) -> Option<Value> {
    let full = read_json(p)?;
    let o = &full["claudeAiOauth"];
    let has = o["accessToken"].as_str().is_some_and(|t| !t.is_empty());
    Some(json!({ "claudeAiOauth": {
        "accessToken": if with_token { o["accessToken"].clone() } else { Value::Null },
        "hasAccessToken": has,
        "expiresAt": o["expiresAt"],
        "scopes": o["scopes"],
        "subscriptionType": o["subscriptionType"],
        "rateLimitTier": o["rateLimitTier"],
    }}))
}

/// Read `oauthAccount` from `~/.claude.json`, retrying torn writes from active Claude sessions.
pub fn read_oauth_account(h: &Path) -> Option<Value> {
    let path = h.join(".claude.json");
    for i in 0..4 {
        if let Some(c) = fs::read(&path).ok().and_then(|b| serde_json::from_slice::<GlobalConfig>(&b).ok()) {
            return Some(c.oauth_account.unwrap_or(Value::Null));
        }
        if i < 3 {
            thread::sleep(Duration::from_millis(5 << i));
        }
    }
    None
}

/// What `claude auth status --json` reports for a claude.ai login, read from its files:
/// email/org from ~/.claude.json `oauthAccount`, plan from .credentials.json. None when the setup
/// is one the CLI resolves differently (API keys, other providers, profiles, logged out, ...).
pub fn claude_account_direct<'a>(
    h: &Path,
    creds: Option<&Value>,
    settings: Option<&Value>,
    get_oauth_account: impl FnOnce() -> Option<&'a Value>,
) -> Option<Value> {
    if std::env::vars_os().any(|(k, v)| !v.is_empty() && k.to_str().is_some_and(auth_env_name)) {
        return None;
    }
    if let Some(s) = settings {
        if !s["apiKeyHelper"].is_null() {
            return None;
        }
        if s["env"].as_object().is_some_and(|e| e.keys().any(|k| auth_env_name(k))) {
            return None;
        }
    }
    for p in [h.join(".claude/.config.json"), h.join(".config/anthropic"), PathBuf::from("/etc/claude-code")] {
        if p.exists() {
            return None;
        }
    }
    let oauth = &creds?["claudeAiOauth"];
    if oauth["hasAccessToken"].as_bool() != Some(true) {
        return None;
    }
    let scopes = oauth["scopes"].as_array()?;
    if !scopes.iter().any(|s| s == "user:inference") {
        return None;
    }
    let plan = oauth["subscriptionType"].as_str()?;
    let acct = get_oauth_account()?;
    let tier = oauth["rateLimitTier"].as_str().or(acct["organizationRateLimitTier"].as_str());
    Some(json!({
        "logged_in": true,
        "email": acct["emailAddress"],
        "plan": claude_plan(plan, tier),
        "org": acct["organizationName"],
        "auth_method": "claude.ai",
    }))
}

/// The plan as claude.ai names it. Anthropic renamed Max 20x to Ultra on 2026-10-30, but the login
/// still says `max` and tells the two Max sizes apart only by the rate-limit tier (Claude Code does
/// the same check).
fn claude_plan(sub: &str, tier: Option<&str>) -> String {
    match (sub, tier) {
        ("max", Some("default_claude_max_20x")) => "ultra".into(),
        _ => sub.into(),
    }
}

/// Fallback when direct file detection does not apply: ask the CLI (slow: ~0.1-0.5 s, ~140 MB).
pub fn claude_account_cli(creds: Option<&Value>) -> Value {
    let bin = tool("claude");
    match run(&[&bin, "auth", "status", "--json"]) {
        Some((_, out)) => match serde_json::from_str::<Value>(&out) {
            Ok(d) => json!({
                "logged_in": d["loggedIn"].as_bool().unwrap_or(false),
                "email": d["email"],
                "plan": match d["subscriptionType"].as_str() {
                    // `auth status` has no tier: take it from the login file when that is the same plan.
                    Some(sub) => {
                        let o = creds.map(|c| &c["claudeAiOauth"]);
                        let tier = o.filter(|o| o["subscriptionType"] == sub).and_then(|o| o["rateLimitTier"].as_str());
                        json!(claude_plan(sub, tier))
                    }
                    None => d["subscriptionType"].clone(),
                },
                "org": d["orgName"],
                "auth_method": d["authMethod"],
            }),
            Err(_) => json!({ "logged_in": false, "error": "unreadable `claude auth status` output" }),
        },
        None => json!({ "logged_in": false, "error": "`claude auth status` did not answer" }),
    }
}

/// Claude plan usage with a last-good fallback: every successful fetch is saved to
/// `~/.cache/agent-usage/claude-usage-last.json`; when a later fetch fails (HTTP 429 rate limit,
/// offline, expired login) the saved numbers are returned under `stale` with their `fetched_at`
/// (never when signed out or switched to a different account).
pub fn claude_usage<'a>(
    creds: Option<&Value>,
    get_oauth_account: impl FnOnce() -> Option<&'a Value>,
    live: bool,
) -> Value {
    if !live {
        // Off by default: no token use, no request, and no saved numbers shown either.
        return json!({ "off": true });
    }
    let Some(creds) = creds else {
        // Signed out: never surface a previous account's stale limits.
        return json!({ "error": "Not signed in to Claude Code" });
    };
    let Some(token) = creds["claudeAiOauth"]["accessToken"].as_str().filter(|t| !t.is_empty()) else {
        return json!({ "error": "Not signed in to Claude Code" });
    };
    let account_email = get_oauth_account().and_then(|a| a["emailAddress"].as_str());
    let v = claude_usage_live(creds, token);
    let ok = v.get("windows").is_some();
    with_last_good("claude-usage-last.json", v, ok, account_email)
}

fn claude_usage_live(creds: &Value, token: &str) -> Value {
    let oauth = &creds["claudeAiOauth"];
    let expired = oauth["expiresAt"].as_i64().map(|ms| ms / 1000 < now()).unwrap_or(false);
    if expired {
        return json!({ "error": "Login token expired. Use any Claude Code session and it renews." });
    }
    if is_offline() {
        return json!({ "error": "offline (AGENT_USAGE_OFFLINE)" });
    }
    let resp = ureq::get("https://api.anthropic.com/api/oauth/usage")
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .set("User-Agent", "agent-usage/0.1")
        .timeout(Duration::from_secs(8))
        .call();
    trace("claude usage: response");
    let d: Value = match resp {
        Ok(r) => match r.into_json() {
            Ok(v) => v,
            Err(_) => return json!({ "error": "Unreadable usage response" }),
        },
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            return json!({ "error": "Login token rejected. Use any Claude Code session and it renews." })
        }
        Err(ureq::Error::Status(429, _)) => {
            return json!({ "error": "claude.ai is rate-limiting usage checks. Try Refresh again in a minute." })
        }
        Err(ureq::Error::Status(code, _)) => return json!({ "error": format!("Usage endpoint returned HTTP {code}") }),
        Err(_) => return json!({ "error": "Could not reach claude.ai (offline?)" }),
    };
    let windows = claude_windows(&d);
    let extra = &d["extra_usage"];
    json!({
        "windows": windows,
        "extra_usage": if extra["is_enabled"].as_bool() == Some(true) {
            json!({ "pct": extra["utilization"], "used": extra["used_credits"], "limit": extra["monthly_limit"], "currency": extra["currency"] })
        } else { Value::Null },
    })
}

/// Limit windows from the usage response: the fixed keys, plus the model- or surface-scoped weekly
/// limits that only the `limits` list carries (Ultra's separate weekly Fable limit).
fn claude_windows(d: &Value) -> Vec<Value> {
    let named = [
        ("five_hour", "5-hour"),
        ("seven_day", "Weekly"),
        ("seven_day_opus", "Weekly · Opus"),
        ("seven_day_sonnet", "Weekly · Sonnet"),
        ("seven_day_oauth_apps", "Weekly · apps"),
        ("seven_day_cowork", "Weekly · Cowork"),
    ];
    let mut windows = Vec::new();
    for (key, label) in named {
        let w = &d[key];
        if let Some(pct) = w["utilization"].as_f64() {
            windows.push(json!({
                "label": label,
                "pct": pct,
                "resets_at": w["resets_at"].as_str().and_then(parse_iso),
            }));
        }
    }
    for l in d["limits"].as_array().into_iter().flatten() {
        if l["kind"] != "weekly_scoped" {
            continue;
        }
        let (scope, pct) = (&l["scope"], l["percent"].as_f64());
        let name = [&scope["model"]["display_name"], &scope["surface"]["display_name"], &scope["surface"]]
            .into_iter()
            .find_map(|v| v.as_str())
            .map(|n| n.chars().filter(|c| !c.is_control()).take(24).collect::<String>());
        let (Some(name), Some(pct)) = (name.filter(|n| !n.trim().is_empty()), pct) else { continue };
        let label = format!("Weekly · {}", name.trim());
        if windows.iter().any(|w| w["label"] == label.as_str()) {
            continue;
        }
        windows.push(json!({
            "label": label,
            "pct": pct,
            "resets_at": l["resets_at"].as_str().and_then(parse_iso),
        }));
    }
    windows
}

const DROPS_FILE: &str = "claude-drops-v2.json";

/// Today's brief disconnects of the Claude Remote Control server: its "[hh:mm:ss] Reconnected after
/// 14s" journal lines. The server logs ~75k status lines a day (8+ MB), so the read is incremental.
pub fn claude_drops(midnight: i64) -> Value {
    let (cached, mut cursor, state) = load_day_cache(DROPS_FILE, midnight);
    let prev_cursor = cursor.clone();
    let mut events: Vec<(i64, i64)> = state
        .as_array()
        .map(|a| a.iter().filter_map(|e| Some((e[0].as_i64()?, e[1].as_i64()?))).collect())
        .unwrap_or_default();
    let Some((out, rescanned)) = journal_incremental(CLAUDE_UNIT, midnight, &mut cursor, true) else {
        return Value::Null;
    };
    trace(format!("claude drops: {} read", if rescanned { "full-day" } else { "incremental" }));
    if rescanned {
        events.clear();
    }
    let before = events.len();
    for line in out.lines() {
        if !line.contains("Reconnected after ") {
            continue;
        }
        let clean = strip_ansi(line);
        let Some(i) = clean.find("Reconnected after ") else { continue };
        if let (Some(at), Some(secs)) = (journal_ts(&clean), parse_dur(&clean[i + 18..])) {
            if at >= midnight {
                events.push((at, secs));
            }
        }
    }
    if !cached || rescanned || cursor != prev_cursor || events.len() != before {
        save_day_cache(DROPS_FILE, midnight, &cursor, json!(events));
    }
    json!(events.iter().map(|(at, secs)| json!({ "at": at, "secs": secs })).collect::<Vec<_>>())
}

pub fn claude_server(active: bool, lines: &[String]) -> Value {
    let mut state = None;
    let mut capacity = None;
    let mut link = None;
    // Session titles are listed under the most recent "Capacity:" line.
    let mut sessions: Vec<String> = Vec::new();
    if let Some(ci) = lines.iter().rposition(|l| l.starts_with("Capacity: ")) {
        for l in &lines[ci + 1..] {
            if l.starts_with("Continue coding")
                || l.starts_with("Code anywhere")
                || l.starts_with("Or ask")
                || l.starts_with("space to")
                || l.contains(" · ") && (l.contains("Connected") || l.contains("Ready"))
            {
                break;
            }
            sessions.push(l.clone());
        }
    }
    for l in lines.iter().rev() {
        if state.is_none() {
            // After a network blip the server prints "[hh:mm:ss] Reconnected after 14s" and does not
            // repeat the "✔ Connected" status line, so that line means connected again.
            state = status_from_line(l, "Reconnected after").map(String::from);
        }
        if capacity.is_none() {
            if let Some(i) = l.find("Capacity: ") {
                capacity = l[i + 10..].split_whitespace().next().map(String::from);
            }
        }
        if link.is_none() {
            if let Some(i) = l.find("https://claude.ai/code?environment=") {
                link = l[i..].split_whitespace().next().map(String::from);
            }
        }
    }
    json!({ "service_active": active, "state": state, "capacity": capacity, "link": link, "sessions": sessions })
}

/// Interactive terminal sessions and whether each is connected to Remote Control.
pub fn claude_sessions(h: &Path) -> Value {
    let (mut total, mut connected, mut offline) = (0, 0, Vec::new());
    if let Ok(rd) = fs::read_dir(h.join(".claude/sessions")) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let Some(v) = read_json(&p) else { continue };
            if v["kind"] != "interactive" || v["entrypoint"] != "cli" {
                continue;
            }
            let Some(pid) = v["pid"].as_u64() else { continue };
            let alive = fs::read(format!("/proc/{pid}/cmdline"))
                .map(|c| String::from_utf8_lossy(&c).contains("claude"))
                .unwrap_or(false);
            if !alive {
                continue;
            }
            total += 1;
            if v["bridgeSessionId"].is_string() {
                connected += 1;
            } else {
                offline.push(v["name"].as_str().unwrap_or("?").to_string());
            }
        }
    }
    json!({ "total": total, "connected": connected, "not_connected": offline })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_20x_is_ultra() {
        assert_eq!(claude_plan("max", Some("default_claude_max_20x")), "ultra");
        assert_eq!(claude_plan("max", Some("default_claude_max_5x")), "max");
        assert_eq!(claude_plan("max", None), "max");
        assert_eq!(claude_plan("pro", Some("default_claude_max_20x")), "pro");
        assert_eq!(claude_plan("ultra", None), "ultra");
    }

    #[test]
    fn scoped_weekly_limits() {
        let r = json!({
            "five_hour": null,
            "seven_day": { "utilization": 32.0, "resets_at": "2026-11-05T14:00:00Z" },
            "seven_day_sonnet": { "utilization": 0.0, "resets_at": null },
            "limits": [
                { "kind": "weekly_scoped", "percent": 0.0, "resets_at": "2026-11-05T14:00:00Z",
                  "scope": { "model": { "display_name": "Fable" } } },
                { "kind": "weekly_scoped", "percent": 5.0, "resets_at": null,
                  "scope": { "model": { "display_name": "Sonnet" } } },
                { "kind": "other", "percent": 9.0, "scope": { "model": { "display_name": "Ignored" } } },
            ],
        });
        let w = claude_windows(&r);
        let labels: Vec<_> = w.iter().map(|x| x["label"].as_str().unwrap()).collect();
        assert_eq!(labels, ["Weekly", "Weekly · Sonnet", "Weekly · Fable"]);
        assert_eq!(w[2]["pct"], 0.0);
        assert!(w[2]["resets_at"].is_i64());
    }
}
