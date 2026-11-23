//! Codex app-server JSON-RPC, watchdog journal events and relay-holder note.

use crate::system::{
    home, is_offline, journal_incremental, journal_msg, journal_ts, load_day_cache, save_day_cache, tool, trace,
    with_last_good, CODEX_UNIT, RPC_READ, TIMEOUT,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Instant, UNIX_EPOCH};

/// Results flow to main() as (part, value) as soon as each one is known.
pub type Tx = mpsc::Sender<(&'static str, Value)>;

const EVENTS_FILE: &str = "codex-events-v1.json";
/// Watchdog events kept for the day's timeline (the counts cover the whole day).
const EVENTS_KEPT: usize = 40;

const ACCOUNT_READ: u64 = 1;
const LIMITS_READ: u64 = 2;
const USAGE_READ: u64 = 3;
const RELAY_READ: u64 = 4;

/// Which other Codex app on this computer holds the relay link, as the watchdog last found it.
pub fn relay_held_by(runtime_dir: &Path, now_secs: i64) -> Option<String> {
    let p = runtime_dir.join("codex-remote-control-watchdog.held");
    let md = fs::symlink_metadata(&p).ok().filter(|m| m.is_file() && m.len() <= 256)?;
    let at = md.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    if now_secs - at > 180 {
        return None;
    }
    let text = fs::read_to_string(&p).ok()?;
    let name: String = text.lines().next()?.chars().filter(|c| !c.is_control()).take(60).collect();
    Some(name.trim().to_string()).filter(|n| !n.is_empty())
}

/// Today's actions of the Codex watchdog, read incrementally via journal cursor.
pub fn codex_events(midnight: i64) -> Value {
    let (cached, mut cursor, state) = load_day_cache(EVENTS_FILE, midnight);
    let prev_cursor = cursor.clone();
    let mut restarts = state["restarts"].as_u64().unwrap_or(0);
    let mut misses = state["relay_misses"].as_u64().unwrap_or(0);
    let mut recent: Vec<Value> = state["recent"].as_array().cloned().unwrap_or_default();
    let Some((out, rescanned)) = journal_incremental(CODEX_UNIT, midnight, &mut cursor, false) else {
        return Value::Null;
    };
    trace(format!("codex events: {} read", if rescanned { "full-day" } else { "incremental" }));
    if rescanned {
        (restarts, misses) = (0, 0);
        recent.clear();
    }
    let (prev_restarts, prev_misses, prev_len) = (restarts, misses, recent.len());
    for line in out.lines() {
        let Some((ident, msg)) = journal_msg(line) else { continue };
        if ident != "codex-remote-control-watchdog" {
            continue;
        }
        let Some(at) = journal_ts(line) else { continue };
        if at < midnight {
            continue;
        }
        let msg = msg.trim();
        if msg.contains("(miss ") {
            misses += 1;
        }
        if msg.starts_with("restarting") || msg.contains("starting it") || msg.contains("replacing it") {
            restarts += 1;
        }
        recent.push(json!({ "at": at, "msg": msg }));
    }
    let changed = restarts != prev_restarts || misses != prev_misses || recent.len() != prev_len;
    let recent = recent.split_off(recent.len().saturating_sub(EVENTS_KEPT));
    let v = json!({ "restarts": restarts, "relay_misses": misses, "recent": recent });
    if !cached || rescanned || cursor != prev_cursor || changed {
        save_day_cache(EVENTS_FILE, midnight, &cursor, v.clone());
    }
    v
}

/// JSON-RPC to a Codex app-server: the running background service over its control socket
/// (WebSocket), or a private short-lived `codex app-server` over stdio as a fallback.
enum Rpc {
    Ws(tungstenite::WebSocket<UnixStream>),
    Stdio { child: Child, stdin: ChildStdin, rx: mpsc::Receiver<String> },
}

impl Rpc {
    fn daemon() -> Result<Rpc, String> {
        let path = home().join(".codex/app-server-control/app-server-control.sock");
        let stream = UnixStream::connect(&path).map_err(|e| e.to_string())?;
        stream.set_read_timeout(Some(RPC_READ)).ok();
        stream.set_write_timeout(Some(RPC_READ)).ok();
        let (ws, _) = tungstenite::client("ws://localhost/rpc", stream).map_err(|e| e.to_string())?;
        Ok(Rpc::Ws(ws))
    }

    fn private() -> Result<Rpc, String> {
        let mut child = Command::new(tool("codex"))
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Rpc::Stdio { child, stdin, rx })
    }

    fn send(&mut self, msg: &Value) -> Result<(), String> {
        let s = msg.to_string();
        match self {
            Rpc::Ws(ws) => ws.send(tungstenite::Message::Text(s)).map_err(|e| e.to_string()),
            Rpc::Stdio { stdin, .. } => stdin
                .write_all(s.as_bytes())
                .and_then(|_| stdin.write_all(b"\n"))
                .and_then(|_| stdin.flush())
                .map_err(|e| e.to_string()),
        }
    }

    /// Next message, waiting at most RPC_READ and never past `deadline`.
    fn recv(&mut self, deadline: Instant) -> Result<Value, String> {
        match self {
            Rpc::Ws(ws) => loop {
                let left = deadline.saturating_duration_since(Instant::now()).min(RPC_READ);
                if left.is_zero() {
                    return Err("timeout".into());
                }
                ws.get_ref().set_read_timeout(Some(left)).ok();
                match ws.read().map_err(|e| e.to_string())? {
                    tungstenite::Message::Text(t) => return serde_json::from_str(&t).map_err(|e| e.to_string()),
                    tungstenite::Message::Close(_) => return Err("closed".into()),
                    // Ping/pong/binary: keep waiting, refreshing timeout against `deadline`.
                    _ => continue,
                }
            },
            Rpc::Stdio { rx, .. } => {
                let left = deadline.saturating_duration_since(Instant::now()).min(RPC_READ);
                if left.is_zero() {
                    return Err("timeout".into());
                }
                let line = rx.recv_timeout(left).map_err(|_| "timeout".to_string())?;
                serde_json::from_str(&line).map_err(|e| e.to_string())
            }
        }
    }

    fn call(&mut self, id: u64, method: &str, params: Value, deadline: Instant) -> Result<Value, String> {
        self.send(&json!({ "id": id, "method": method, "params": params }))?;
        loop {
            let m = self.recv(deadline)?;
            if m["id"].as_u64() == Some(id) {
                if let Some(err) = m.get("error") {
                    return Err(err["message"].as_str().unwrap_or("error").to_string());
                }
                return Ok(m["result"].clone());
            }
        }
    }

    /// Send all requests at once and collect the answers by id.
    fn call_all(&mut self, calls: &[(u64, &str, Value)], deadline: Instant) -> HashMap<u64, Result<Value, String>> {
        let mut res = HashMap::new();
        for (id, method, params) in calls {
            if let Err(e) = self.send(&json!({ "id": id, "method": method, "params": params })) {
                for (id, ..) in calls {
                    res.entry(*id).or_insert_with(|| Err(e.clone()));
                }
                return res;
            }
        }
        while res.len() < calls.len() {
            match self.recv(deadline) {
                Ok(m) => {
                    let Some(id) = m["id"].as_u64() else { continue };
                    let Some((_, method, _)) = calls.iter().find(|c| c.0 == id) else { continue };
                    trace(format!("codex rpc: {method} answered"));
                    let r = match m.get("error") {
                        Some(err) => Err(err["message"].as_str().unwrap_or("error").to_string()),
                        None => Ok(m["result"].clone()),
                    };
                    res.insert(id, r);
                }
                Err(e) => {
                    for (id, ..) in calls {
                        res.entry(*id).or_insert_with(|| Err(e.clone()));
                    }
                }
            }
        }
        res
    }

    fn init(&mut self, deadline: Instant) -> Result<(), String> {
        self.call(
            0,
            "initialize",
            json!({
                "clientInfo": { "name": "agent_usage", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": { "experimentalApi": true },
            }),
            deadline,
        )?;
        self.send(&json!({ "method": "initialized" }))
    }
}

impl Drop for Rpc {
    fn drop(&mut self) {
        if let Rpc::Stdio { child, .. } = self {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn window_label(mins: i64) -> String {
    match mins {
        300 => "5-hour".into(),
        10_080 => "Weekly".into(),
        m if m > 0 && m % 1440 == 0 => format!("{}-day", m / 1440),
        m if m > 0 && m % 60 == 0 => format!("{}-hour", m / 60),
        m => format!("{m}-minute"),
    }
}

fn codex_windows(rl: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let limits: Vec<&Value> = match rl["rateLimitsByLimitId"].as_object() {
        Some(m) if !m.is_empty() => m.values().collect(),
        _ => vec![&rl["rateLimits"]],
    };
    let multi = limits.len() > 1;
    for l in limits {
        for slot in ["primary", "secondary"] {
            let w = &l[slot];
            let Some(pct) = w["usedPercent"].as_f64() else { continue };
            let mut label = window_label(w["windowDurationMins"].as_i64().unwrap_or(0));
            if multi {
                if let Some(name) = l["limitName"].as_str().or(l["limitId"].as_str()).filter(|n| *n != "codex") {
                    label = format!("{label} · {name}");
                }
            }
            out.push(json!({ "label": label, "pct": pct, "resets_at": w["resetsAt"] }));
        }
    }
    out
}

pub fn codex_unreachable() -> Value {
    json!({ "error": if is_offline() { "offline (AGENT_USAGE_OFFLINE)" } else { "Codex is not reachable" } })
}

fn last_good_part(name: &str, v: Value, logged_in: bool, account: Option<&str>) -> Value {
    if !logged_in {
        return v;
    }
    let ok = v.get("error").is_none();
    with_last_good(name, v, ok, account)
}

/// Turn one app-server answer into its part and send it.
fn codex_emit(tx: &Tx, id: u64, r: Option<Result<Value, String>>, date: &str, logged_in: bool, account: Option<&str>) {
    let part = match id {
        ACCOUNT_READ => (
            "codex.account",
            match r {
                Some(Ok(a)) => {
                    let acc = &a["account"];
                    json!({ "logged_in": !acc.is_null(), "email": acc["email"], "plan": acc["planType"], "type": acc["type"] })
                }
                _ => json!({ "logged_in": false }),
            },
        ),
        LIMITS_READ => (
            "codex.usage",
            last_good_part(
                "codex-usage-last.json",
                match r {
                    Some(Ok(rl)) => {
                        let main = &rl["rateLimits"];
                        let credits = &main["credits"];
                        json!({
                            "windows": codex_windows(&rl),
                            "credits": if credits["unlimited"].as_bool() == Some(true) { json!("unlimited") }
                                       else if credits["hasCredits"].as_bool() == Some(true) { credits["balance"].clone() }
                                       else { Value::Null },
                            "limit_reached": main["rateLimitReachedType"],
                        })
                    }
                    Some(Err(e)) => json!({ "error": format!("Could not read limits: {e}") }),
                    None => codex_unreachable(),
                },
                logged_in,
                account,
            ),
        ),
        USAGE_READ => (
            "codex.tokens",
            last_good_part(
                "codex-tokens-last.json",
                match r {
                    Some(Ok(u)) => {
                        let buckets = u["dailyUsageBuckets"].as_array().map(Vec::as_slice).unwrap_or(&[]);
                        let today = buckets
                            .iter()
                            .find(|b| b["startDate"] == date)
                            .and_then(|b| b["tokens"].as_u64())
                            .unwrap_or(0);
                        let last7: u64 = buckets.iter().rev().take(7).filter_map(|b| b["tokens"].as_u64()).sum();
                        json!({ "today": today, "last_7_days": last7, "lifetime": u["summary"]["lifetimeTokens"] })
                    }
                    Some(Err(e)) => json!({ "error": format!("Codex token totals did not load: {e}") }),
                    None => codex_unreachable(),
                },
                logged_in,
                account,
            ),
        ),
        RELAY_READ => (
            "codex.relay",
            match r {
                Some(Ok(s)) => json!({ "status": s["status"], "name": s["serverName"] }),
                _ => json!({ "status": "unknown" }),
            },
        ),
        _ => return,
    };
    let _ = tx.send(part);
}

fn account_identity(res: Option<&Result<Value, String>>) -> (bool, Option<String>) {
    match res {
        Some(Ok(a)) if !a["account"].is_null() => {
            (true, a["account"]["email"].as_str().filter(|s| !s.is_empty()).map(String::from))
        }
        _ => (false, None),
    }
}

/// Codex account, limits, token totals and relay status from its app-server. Each result is sent
/// on `tx` the moment it is known, so a slow ChatGPT backend call holds back only its own part.
pub fn codex_api(date: &str, tx: &Tx) {
    let deadline = Instant::now() + TIMEOUT;
    let (mut rpc, via_daemon) = if is_offline() {
        (None, true)
    } else {
        match Rpc::daemon() {
            Ok(r) => (Some(r), true),
            Err(_) => (Rpc::private().ok(), false),
        }
    };
    if !via_daemon {
        let _ = tx.send(("codex.relay", json!({ "status": "service not running" })));
    }
    let all = [ACCOUNT_READ, LIMITS_READ, USAGE_READ];
    let fail_all = || {
        for id in all {
            codex_emit(tx, id, None, date, false, None);
        }
        if via_daemon {
            codex_emit(tx, RELAY_READ, None, date, false, None);
        }
    };
    let Some(r) = rpc.as_mut() else { return fail_all() };
    trace(format!("codex rpc: connected ({})", if via_daemon { "daemon" } else { "private app-server" }));
    if r.init(deadline).is_err() {
        return fail_all();
    }
    trace("codex rpc: initialized");
    let account = (ACCOUNT_READ, "account/read", json!({ "refreshToken": false }));
    let slow = [
        (LIMITS_READ, "account/rateLimits/read", Value::Null),
        (USAGE_READ, "account/usage/read", json!({})),
    ];
    if via_daemon {
        // `account/read` is a local in-memory read (~0.5 ms); resolve it first so `limits` and
        // `usage` know whether the user is signed in and which account's stale cache applies.
        let acct_res = r.call(account.0, account.1, account.2, deadline);
        let (logged_in, email) = account_identity(Some(&acct_res));
        let email_ref = email.as_deref();
        codex_emit(tx, ACCOUNT_READ, Some(acct_res), date, logged_in, email_ref);

        let relay = (RELAY_READ, "remoteControl/status/read", Value::Null);
        thread::scope(|s| {
            let sides: Vec<_> = slow
                .into_iter()
                .map(|c| {
                    s.spawn(move || {
                        let opened = Rpc::daemon().and_then(|mut r| r.init(deadline).map(|_| r));
                        match opened {
                            Ok(mut r) => {
                                let mut m = r.call_all(std::slice::from_ref(&c), deadline);
                                codex_emit(tx, c.0, m.remove(&c.0), date, logged_in, email_ref);
                                None
                            }
                            Err(_) => Some(c),
                        }
                    })
                })
                .collect();
            let mut res = r.call_all(&[relay], deadline);
            codex_emit(tx, RELAY_READ, res.remove(&RELAY_READ), date, logged_in, email_ref);
            for h in sides {
                if let Ok(Some(c)) = h.join() {
                    let id = c.0;
                    let mut m = r.call_all(&[c], deadline);
                    codex_emit(tx, id, m.remove(&id), date, logged_in, email_ref);
                }
            }
        });
    } else {
        // A private `codex app-server`: one process, requests pipelined.
        let mut calls = vec![account];
        calls.extend(slow);
        let mut res = r.call_all(&calls, deadline);
        let (logged_in, email) = account_identity(res.get(&ACCOUNT_READ));
        let email_ref = email.as_deref();
        for id in all {
            codex_emit(tx, id, res.remove(&id), date, logged_in, email_ref);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_holder_file() {
        let dir = std::env::temp_dir().join(format!("agent-usage-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let p = dir.join("codex-remote-control-watchdog.held");
        fs::write(&p, "Codex Desktop\nignored second line\n").unwrap();
        let mtime = fs::metadata(&p).unwrap().modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        assert_eq!(relay_held_by(&dir, mtime + 30).as_deref(), Some("Codex Desktop"));
        assert_eq!(relay_held_by(&dir, mtime + 600), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
