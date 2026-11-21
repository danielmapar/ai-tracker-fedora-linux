//! agent-usage: print one JSON snapshot of Claude, Codex and Gemini (Antigravity) account usage
//! plus the health of the remote-access workers on this machine, then exit. Run on demand by the
//! GNOME top-bar widget; it never runs in the background.
//!
//! Read-only: the collector itself never refreshes or rewrites login tokens and never starts, stops
//! or changes a service. (Fallbacks run other tools read-only: `claude auth status` for unusual
//! Claude login setups, `secret-tool lookup` for Gemini's keyring login, and a private short-lived
//! `codex app-server` when the Codex background service is not running.)
//!
//! It writes only to ~/.cache/agent-usage/ (directory 0700, files 0600):
//!   claude-tokens-v1.bin     incremental Claude transcript token counts
//!   gemini-tokens-v1.bin     incremental Gemini transcript token counts
//!   claude-usage-last.json   the last good Claude limits (shown when the API refuses)
//!   codex-usage-last.json    the last good Codex limits (shown when they fail to load)
//!   codex-tokens-last.json   the last good Codex token totals (idem)
//!   gemini-usage-last.json   the last good Gemini quota limits, plan & credits (idem)
//!   claude-drops-v2.json     journal cursor + today's Claude server reconnects
//!   codex-events-v1.json     journal cursor + today's Codex watchdog messages
//!   gemini-drops-v1.json     journal cursor + today's Gemini daemon reconnects
//!   last-snapshot.json       the last complete output, incl. account emails and session titles
//!
//! Live Claude plan limits are opt-in (see `claude_live_enabled`); live Gemini quota limits are on
//! by default and can be turned off with `"gemini_live_limits": false` in config.json.
//!
//! Speed: every source is read concurrently, so the wall time is that of the slowest live API.
//! Claude's account comes straight from ~/.claude.json and ~/.claude/.credentials.json; "tokens
//! today" is incremental (see tokens.rs); the Codex app-server calls are pipelined; Gemini's two
//! Cloud Code quota calls run concurrently; and the worker checks use one `systemctl --user show`
//! for all four units plus one `/proc` walk and parallel journal tails.
//!
//! Environment (for testing): AGENT_USAGE_TRACE=1 prints per-source timings to stderr;
//! AGENT_USAGE_OFFLINE=1 (or --offline) skips live usage APIs; AGENT_USAGE_NO_CACHE=1 does a full
//! transcript scan without the cache; AGENT_USAGE_CACHE_DIR overrides the cache directory;
//! AGENT_USAGE_GEMINI_URL overrides the Gemini Cloud Code endpoint (https:// or http://127.0.0.1:).

mod tokens;

use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TIMEOUT: Duration = Duration::from_secs(10);
/// Per-message read timeout of the Codex app-server, as before.
const RPC_READ: Duration = Duration::from_secs(8);

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn env_on(k: &str) -> bool {
    std::env::var_os(k).is_some_and(|v| !v.is_empty() && v != "0")
}

// ---------- tracing (AGENT_USAGE_TRACE=1; timings only, never values) ----------

static T0: OnceLock<Instant> = OnceLock::new();
static TRACE: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn trace(what: impl AsRef<str>) {
    if let Some(t0) = T0.get() {
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        if let Ok(mut v) = TRACE.lock() {
            v.push(format!("{ms:7.1} ms  {}", what.as_ref()));
        }
    }
}

/// Prefer ~/.local/bin/<name> (GNOME Shell's PATH may not include it), else rely on PATH.
fn tool(name: &str) -> String {
    let p = home().join(".local/bin").join(name);
    if p.exists() { p.to_string_lossy().into_owned() } else { name.to_string() }
}

/// Run a command with a timeout. Returns (exit code, stdout) or None if it could not run in time.
/// Returns as soon as the command exits (no polling interval on the critical path).
fn run(cmd: &[&str]) -> Option<(i32, String)> {
    let deadline = Instant::now() + TIMEOUT;
    let mut child = Command::new(cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut b = Vec::new();
        let _ = stdout.read_to_end(&mut b);
        let _ = tx.send(b);
    });
    let kill = |c: &mut Child| {
        let _ = c.kill();
        let _ = c.wait();
    };
    let Ok(out) = rx.recv_timeout(TIMEOUT) else {
        kill(&mut child);
        return None;
    };
    // stdout is closed: the command is exiting.
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return Some((st.code().unwrap_or(-1), String::from_utf8_lossy(&out).into_owned())),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_micros(500)),
            _ => {
                kill(&mut child);
                return None;
            }
        }
    }
}

fn run_out(cmd: &[&str]) -> String {
    run(cmd).map(|(_, o)| o.trim().to_string()).unwrap_or_default()
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    while let Some(n) = chars.next() {
                        if n.is_ascii_alphabetic() { break; }
                    }
                }
                Some(']') => {
                    // OSC (e.g. hyperlinks): ends with BEL or ESC \
                    chars.next();
                    while let Some(n) = chars.next() {
                        if n == '\u{7}' { break; }
                        if n == '\u{1b}' { chars.next(); break; }
                    }
                }
                _ => {}
            }
            continue;
        }
        out.push(c);
    }
    out
}

// ---------- time ----------

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// "YYYY-MM-DDTHH:MM:SS[.frac][Z|±HH:MM]" -> Unix seconds.
pub(crate) fn parse_iso(s: &str) -> Option<i64> {
    let num = |a: usize, n: usize| -> Option<i64> { s.get(a..a + n)?.parse().ok() };
    let (y, mo, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (h, mi, se) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    let mut t = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se;
    let tz = s.get(19..)?.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    if let Some(sign) = tz.chars().next() {
        if sign == '+' || sign == '-' {
            let hh: i64 = tz.get(1..3)?.parse().ok()?;
            let mm: i64 = tz.get(4..6).and_then(|x| x.parse().ok()).unwrap_or(0);
            let off = hh * 3600 + mm * 60;
            t -= if sign == '+' { off } else { -off };
        }
    }
    Some(t)
}

/// (local date "YYYY-MM-DD", local midnight as Unix seconds): what `date -d "today 00:00" "+%F %s"`
/// printed, from the C library's time zone rules instead of a process.
fn today() -> (String, i64) {
    extern "C" {
        fn tzset();
    }
    // SAFETY: plain libc time calls on locals.
    unsafe {
        tzset();
        let t: libc::time_t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if !libc::localtime_r(&t, &mut tm).is_null() {
            let date = format!("{:04}-{:02}-{:02}", tm.tm_year as i64 + 1900, tm.tm_mon + 1, tm.tm_mday);
            tm.tm_hour = 0;
            tm.tm_min = 0;
            tm.tm_sec = 0;
            tm.tm_isdst = -1;
            let m = libc::mktime(&mut tm);
            if m != -1 {
                return (date, m as i64);
            }
        }
    }
    let n = now();
    (String::new(), n - n % 86_400)
}

// ---------- shared worker checks ----------

fn journal_lines(unit: &str, n: &str) -> Vec<String> {
    let out = run_out(&["journalctl", "--user", "-u", unit, "-n", n, "-o", "cat", "-a", "--no-pager"]);
    out.lines().map(|l| strip_ansi(l).trim().to_string()).filter(|l| !l.is_empty()).collect()
}

const CLAUDE_UNIT: &str = "claude-remote-control.service";
const CODEX_UNIT: &str = "codex-remote-control.service";
const CODEX_TIMER: &str = "codex-remote-control.timer";
const GEMINI_UNIT: &str = "antigravity-cli-daemon.service";

/// One `systemctl --user show` for all user units: unit -> {property -> value}.
fn user_units() -> HashMap<&'static str, HashMap<String, String>> {
    let units = [CLAUDE_UNIT, CODEX_UNIT, CODEX_TIMER, GEMINI_UNIT];
    let out = run_out(&[
        "systemctl", "--user", "show", units[0], units[1], units[2], units[3],
        "-p", "ActiveState", "-p", "ActiveEnterTimestamp", "-p", "ExecMainExitTimestamp", "-p", "Result",
        "--timestamp=unix",
    ]);
    let mut res = HashMap::new();
    // One block per unit, in the order asked, separated by blank lines.
    for (unit, block) in units.iter().zip(out.split("\n\n")) {
        let props = block
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_string(), v.trim().to_string()))
            .collect();
        res.insert(*unit, props);
    }
    res
}

/// `systemctl is-active sshd` == "active". A running sshd.service has a populated cgroup, which is
/// a file read instead of a process; anything else asks systemctl.
fn sshd_active() -> bool {
    let ev = fs::read_to_string("/sys/fs/cgroup/system.slice/sshd.service/cgroup.events").unwrap_or_default();
    if ev.lines().any(|l| l.trim() == "populated 1") {
        return true;
    }
    run_out(&["systemctl", "is-active", "sshd"]) == "active"
}

// ---------- Claude ----------

fn read_json(p: &Path) -> Option<Value> {
    fs::read(p).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

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

/// What `claude auth status --json` reports for a claude.ai login, read from its files:
/// email/org from ~/.claude.json `oauthAccount`, plan from .credentials.json. None when the setup
/// is one the CLI resolves differently (API keys, other providers, profiles, logged out, ...).
fn claude_account_direct(h: &Path, creds: Option<&Value>, settings: Option<&Value>) -> Option<Value> {
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
    // ~/.claude.json is rewritten by running sessions; retry a torn read.
    let mut acct = None;
    for i in 0..4 {
        if let Some(c) = fs::read(h.join(".claude.json")).ok().and_then(|b| serde_json::from_slice::<GlobalConfig>(&b).ok()) {
            acct = Some(c.oauth_account.unwrap_or(Value::Null));
            break;
        }
        thread::sleep(Duration::from_millis(5 << i));
    }
    let acct = acct?;
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

/// The original: ask the CLI (slow: ~0.1-0.5 s, ~140 MB).
fn claude_account_cli(creds: Option<&Value>) -> Value {
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
/// offline, expired login) the saved numbers are returned under `stale` with their `fetched_at`,
/// so the widget can show them marked as old. Display-only: the cache never triggers a fetch.
fn claude_usage(creds: Option<&Value>, live: bool) -> Value {
    if !live {
        // Off by default: no token use, no request, and no saved numbers shown either.
        return json!({ "off": true });
    }
    let v = claude_usage_live(creds);
    let ok = v.get("windows").is_some();
    with_last_good("claude-usage-last.json", v, ok)
}

/// Last good value of a live part, for when a later fetch fails (rate limit, timeout, offline,
/// service down): a success is saved with its `fetched_at`; a failure gets the saved value attached
/// as `stale`, so the widget keeps showing the numbers, marked with their time. Display-only: the
/// saved value never triggers a fetch.
fn with_last_good(name: &str, mut v: Value, ok: bool) -> Value {
    let Some(dir) = tokens::cache_dir(&home()) else { return v };
    if ok {
        v["fetched_at"] = json!(now());
        write_private(&dir, name, v.to_string().as_bytes());
    } else if let Some(prev) = read_json(&dir.join(name)).filter(|p| p.is_object() && p.get("error").is_none()) {
        v["stale"] = prev;
    }
    v
}

/// The cache directory, private to the user (0700, also tightened if it already exists wider).
pub(crate) fn private_dir(dir: &Path) -> bool {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let _ = fs::DirBuilder::new().recursive(true).mode(0o700).create(dir);
    match fs::symlink_metadata(dir) {
        Ok(md) if md.is_dir() => {
            if md.permissions().mode() & 0o077 != 0 {
                let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
            }
            true
        }
        _ => false,
    }
}

/// A new temp file for an atomic write: created exclusively, never through a symlink, mode 0600.
/// A leftover one (e.g. from a run that was killed mid-write) is removed and created again.
pub(crate) fn create_temp(tmp: &Path) -> std::io::Result<fs::File> {
    let open = || {
        fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(tmp)
    };
    match open() {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            fs::remove_file(tmp)?;
            open()
        }
        r => r,
    }
}

/// Atomic write of a private (mode 600) file in the cache directory: temp file + rename.
fn write_private(dir: &Path, name: &str, data: &[u8]) {
    if !private_dir(dir) {
        return;
    }
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let written = create_temp(&tmp).and_then(|mut f| f.write_all(data));
    if written.is_ok() { let _ = fs::rename(&tmp, dir.join(name)); } else { let _ = fs::remove_file(&tmp); }
}

// ---------- today's drops and watchdog actions (journal) ----------

/// "1767225600.123456 host ident[pid]: message" (journalctl -o short-unix) -> Unix seconds.
fn journal_ts(line: &str) -> Option<i64> {
    let end = line.find(['.', ' '])?;
    line[..end].parse().ok()
}

/// (identifier, message) of a short-unix journal line.
fn journal_msg(line: &str) -> Option<(&str, &str)> {
    let rest = line.splitn(3, ' ').nth(2)?;
    let (ident, msg) = rest.split_once(": ")?;
    Some((ident.split('[').next().unwrap_or(ident), msg))
}

/// "14s", "1m 5s", "2h 3m", "800ms" -> whole seconds.
fn parse_dur(s: &str) -> Option<i64> {
    let (mut total, mut any) = (0.0, false);
    for tok in s.split_whitespace() {
        let tok = tok.trim_end_matches(['.', ',', ')']);
        let split = tok.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(tok.len());
        let (n, unit) = tok.split_at(split);
        let Ok(n) = n.parse::<f64>() else { break };
        let mul = match unit {
            "ms" => 0.001,
            "s" | "sec" => 1.0,
            "m" | "min" => 60.0,
            "h" => 3600.0,
            _ => break,
        };
        total += n * mul;
        any = true;
    }
    any.then(|| total.round() as i64)
}

/// New journal lines of a user unit since the last run: entries after `cursor`, or from local
/// midnight when there is no cursor (or it is no longer valid, e.g. after the journal rotated).
/// Updates `cursor`; returns (output, rescanned from midnight).
fn journal_incremental(unit: &str, midnight: i64, cursor: &mut Option<String>, all: bool) -> Option<(String, bool)> {
    let mut base = vec!["journalctl", "--user", "-u", unit, "--show-cursor", "-o", "short-unix", "--no-pager"];
    if all {
        base.push("-a");
    }
    let mut res = None;
    if let Some(c) = cursor.clone() {
        let mut cmd = base.clone();
        cmd.extend(["--after-cursor", c.as_str()]);
        res = run(&cmd).filter(|(code, _)| *code == 0);
    }
    let rescanned = res.is_none();
    if rescanned {
        *cursor = None;
        let since = format!("@{midnight}");
        let mut cmd = base;
        cmd.extend(["--since", since.as_str()]);
        res = run(&cmd).filter(|(code, _)| *code == 0);
    }
    let (_, out) = res?;
    if let Some(c) = out.lines().rev().find_map(|l| l.strip_prefix("-- cursor: ")) {
        *cursor = Some(c.trim().to_string());
    }
    Some((out, rescanned))
}

/// Today's cached state of one incremental journal reader, valid only for the same local day.
fn load_day_cache(name: &str, midnight: i64) -> (Option<String>, Value) {
    let p = tokens::cache_dir(&home()).and_then(|d| read_json(&d.join(name)));
    match p {
        Some(p) if p["day"].as_i64() == Some(midnight) => (p["cursor"].as_str().map(String::from), p["state"].clone()),
        _ => (None, Value::Null),
    }
}

fn save_day_cache(name: &str, midnight: i64, cursor: &Option<String>, state: Value) {
    if let Some(d) = tokens::cache_dir(&home()) {
        write_private(&d, name, json!({ "day": midnight, "cursor": cursor, "state": state }).to_string().as_bytes());
    }
}

const DROPS_FILE: &str = "claude-drops-v2.json";

/// Today's brief disconnects of the Claude Remote Control server: its "[hh:mm:ss] Reconnected after
/// 14s" journal lines. The server logs ~75k status lines a day (8+ MB), so the read is incremental:
/// a journal cursor and today's events are cached, and each run reads only the newer entries.
fn claude_drops(midnight: i64) -> Value {
    let (mut cursor, state) = load_day_cache(DROPS_FILE, midnight);
    let mut events: Vec<(i64, i64)> = state
        .as_array()
        .map(|a| a.iter().filter_map(|e| Some((e[0].as_i64()?, e[1].as_i64()?))).collect())
        .unwrap_or_default();
    let Some((out, rescanned)) = journal_incremental(CLAUDE_UNIT, midnight, &mut cursor, true) else { return Value::Null };
    trace(format!("claude drops: {} read", if rescanned { "full-day" } else { "incremental" }));
    if rescanned {
        events.clear();
    }
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
    save_day_cache(DROPS_FILE, midnight, &cursor, json!(events));
    json!(events.iter().map(|(at, secs)| json!({ "at": at, "secs": secs })).collect::<Vec<_>>())
}

const EVENTS_FILE: &str = "codex-events-v1.json";
/// Watchdog events kept for the day's timeline (the counts cover the whole day).
const EVENTS_KEPT: usize = 40;

/// Which other Codex app on this computer holds the relay link, as the watchdog last found it. The
/// relay takes one connection per server identity, and Codex Desktop enables remote control under
/// the background service's identity, so while it is open your phone reaches this computer through
/// it and the service is refused ("409 Remote app server already online"). The watchdog rewrites the
/// file every check while that lasts, so a file older than 3 minutes means nothing.
fn relay_held_by(runtime_dir: &Path, now_secs: i64) -> Option<String> {
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

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        // SAFETY: getuid has no preconditions and cannot fail.
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })))
}

/// Today's actions of the Codex watchdog. It logs only when something was wrong (relay
/// disconnected, service down, stray server) or was fixed, so every line of its own is an event.
/// Its unit also gets three systemd lines a minute, so this read is incremental too.
fn codex_events(midnight: i64) -> Value {
    let (mut cursor, state) = load_day_cache(EVENTS_FILE, midnight);
    let mut restarts = state["restarts"].as_u64().unwrap_or(0);
    let mut misses = state["relay_misses"].as_u64().unwrap_or(0);
    let mut recent: Vec<Value> = state["recent"].as_array().cloned().unwrap_or_default();
    let Some((out, rescanned)) = journal_incremental(CODEX_UNIT, midnight, &mut cursor, false) else { return Value::Null };
    trace(format!("codex events: {} read", if rescanned { "full-day" } else { "incremental" }));
    if rescanned {
        (restarts, misses) = (0, 0);
        recent.clear();
    }
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
    let recent = recent.split_off(recent.len().saturating_sub(EVENTS_KEPT));
    let v = json!({ "restarts": restarts, "relay_misses": misses, "recent": recent });
    save_day_cache(EVENTS_FILE, midnight, &cursor, v.clone());
    v
}

fn claude_usage_live(creds: Option<&Value>) -> Value {
    let Some(creds) = creds else {
        return json!({ "error": "Not signed in to Claude Code" });
    };
    let oauth = &creds["claudeAiOauth"];
    let Some(token) = oauth["accessToken"].as_str() else {
        return json!({ "error": "Not signed in to Claude Code" });
    };
    let expired = oauth["expiresAt"].as_i64().map(|ms| ms / 1000 < now()).unwrap_or(false);
    if expired {
        return json!({ "error": "Login token expired. Use any Claude Code session and it renews." });
    }
    if env_on("AGENT_USAGE_OFFLINE") || std::env::args().any(|a| a == "--offline") {
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

fn claude_server(active: bool, lines: &[String]) -> Value {
    let mut state = None;
    let mut capacity = None;
    let mut link = None;
    // Session titles are listed under the most recent "Capacity:" line.
    let mut sessions: Vec<String> = Vec::new();
    if let Some(ci) = lines.iter().rposition(|l| l.starts_with("Capacity: ")) {
        for l in &lines[ci + 1..] {
            if l.starts_with("Continue coding") || l.starts_with("Code anywhere") || l.starts_with("Or ask")
                || l.starts_with("space to") || l.contains(" · ") && (l.contains("Connected") || l.contains("Ready")) {
                break;
            }
            sessions.push(l.clone());
        }
    }
    for l in lines.iter().rev() {
        if state.is_none() {
            // After a network blip the server prints "[hh:mm:ss] Reconnected after 14s" and does not
            // repeat the "✔ Connected" status line, so that line means connected again.
            if l.contains("Reconnected after") {
                state = Some("Connected".to_string());
            }
        }
        if state.is_none() {
            for s in ["Connected", "Ready", "Reconnecting", "Connecting", "Disconnected", "Error"] {
                if l.contains(s) { state = Some(s.to_string()); break; }
            }
        }
        if capacity.is_none() {
            if let Some(i) = l.find("Capacity: ") {
                capacity = l[i + 10..].split_whitespace().next().map(|s| s.to_string());
            }
        }
        if link.is_none() {
            if let Some(i) = l.find("https://claude.ai/code?environment=") {
                link = l[i..].split_whitespace().next().map(|s| s.to_string());
            }
        }
    }
    json!({ "service_active": active, "state": state, "capacity": capacity, "link": link, "sessions": sessions })
}

/// Interactive terminal sessions and whether each is connected to Remote Control.
fn claude_sessions(h: &Path) -> Value {
    let (mut total, mut connected, mut offline) = (0, 0, Vec::new());
    if let Ok(rd) = fs::read_dir(h.join(".claude/sessions")) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x != "json").unwrap_or(true) { continue; }
            let Some(v) = read_json(&p) else { continue };
            if v["kind"] != "interactive" || v["entrypoint"] != "cli" { continue; }
            let Some(pid) = v["pid"].as_u64() else { continue };
            let alive = fs::read(format!("/proc/{pid}/cmdline"))
                .map(|c| String::from_utf8_lossy(&c).contains("claude"))
                .unwrap_or(false);
            if !alive { continue; }
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

// ---------- Gemini (Google Antigravity CLI) ----------

enum GeminiCreds {
    ApiKey,
    SignedOut,
    OAuth {
        email: Option<String>,
        auth_method: String,
        expiry: Option<i64>,
        /// Kept in memory ONLY when `gemini_live_enabled()` is true; never logged or written to disk.
        access_token: Option<String>,
    },
}

fn load_gemini_creds(settings: Option<&Value>, with_token: bool) -> GeminiCreds {
    if settings.and_then(|s| s["modelProvider"].as_str()) == Some("gemini") && env_on("GEMINI_API_KEY") {
        return GeminiCreds::ApiKey;
    }
    let Some((0, out)) = run(&["secret-tool", "lookup", "service", "gemini", "username", "antigravity"]) else {
        return GeminiCreds::SignedOut;
    };
    trace("gemini keyring: done");
    let Ok(d) = serde_json::from_str::<Value>(out.trim()) else {
        return GeminiCreds::SignedOut;
    };
    let tok = &d["token"];
    let has_access = tok["access_token"].as_str().is_some_and(|s| !s.is_empty());
    let has_refresh = tok["refresh_token"].as_str().is_some_and(|s| !s.is_empty());
    if !has_access && !has_refresh {
        return GeminiCreds::SignedOut;
    }
    let access_token = if with_token {
        tok["access_token"].as_str().filter(|s| !s.is_empty()).map(String::from)
    } else {
        None
    };
    let expiry = tok["expiry"].as_str().and_then(parse_iso);
    let email = d["id_token_email"].as_str().filter(|s| !s.is_empty()).map(String::from);
    let auth_method = match d["auth_method"].as_str().unwrap_or("") {
        "consumer" | "" => "google".into(),
        "business" | "enterprise" => "enterprise".into(),
        "adc" => "adc".into(),
        other => other.into(),
    };
    // `d` (including `refresh_token`) drops here immediately.
    GeminiCreds::OAuth { email, auth_method, expiry, access_token }
}

fn gemini_account(creds: &GeminiCreds, last_snap: Option<&Value>) -> Value {
    match creds {
        GeminiCreds::ApiKey => json!({
            "logged_in": true,
            "email": Value::Null,
            "plan": Value::Null,
            "auth_method": "apikey",
        }),
        GeminiCreds::SignedOut => json!({ "logged_in": false }),
        GeminiCreds::OAuth { email, auth_method, .. } => {
            let plan = email.as_deref().and_then(|em| {
                last_snap
                    .filter(|p| p["gemini"]["account"]["email"].as_str() == Some(em))
                    .and_then(|p| p["gemini"]["account"]["plan"].as_str())
                    .map(String::from)
            });
            json!({
                "logged_in": true,
                "email": email,
                "plan": plan,
                "auth_method": auth_method,
            })
        }
    }
}

/// Security invariant: never send Bearer token over non-loopback HTTP.
fn gemini_base_url() -> String {
    if let Ok(u) = std::env::var("AGENT_USAGE_GEMINI_URL") {
        let u = u.trim_end_matches('/');
        if u.starts_with("https://") || u.starts_with("http://127.0.0.1:") {
            return u.to_string();
        }
    }
    "https://daily-cloudcode-pa.googleapis.com".to_string()
}

fn gemini_tier_plan(tier: &Value) -> Option<&'static str> {
    let obj = tier.as_object()?;
    let id = obj.get("id").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    let name = obj.get("name").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    let words: Vec<&str> = id
        .split(|c: char| !c.is_ascii_alphanumeric())
        .chain(name.split(|c: char| !c.is_ascii_alphanumeric()))
        .filter(|w| !w.is_empty())
        .collect();
    for (needle, plan) in [
        ("ultra", "ultra"),
        ("enterprise", "enterprise"),
        ("standard", "standard"),
        ("free", "free"),
        ("pro", "pro"),
    ] {
        if words.contains(&needle) {
            return Some(plan);
        }
    }
    None
}

fn gemini_plan(lca: &Value) -> Option<String> {
    gemini_tier_plan(&lca["paidTier"])
        .or_else(|| gemini_tier_plan(&lca["currentTier"]))
        .map(String::from)
}

fn gemini_credits(lca: &Value) -> Value {
    let Some(arr) = lca["paidTier"]["availableCredits"].as_array() else {
        return Value::Null;
    };
    let parse_amt = |v: &Value| -> Option<Value> {
        if let Some(n) = v.as_i64() {
            return Some(json!(n));
        }
        if let Some(n) = v.as_u64() {
            return Some(json!(n));
        }
        if let Some(f) = v.as_f64() {
            return if f.is_finite() && f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
                Some(json!(f as i64))
            } else if f.is_finite() {
                Some(json!(f))
            } else {
                None
            };
        }
        if let Some(s) = v.as_str() {
            let s = s.trim();
            if let Ok(n) = s.parse::<i64>() {
                return Some(json!(n));
            }
            if let Ok(f) = s.parse::<f64>() {
                if f.is_finite() {
                    return Some(json!(f));
                }
            }
        }
        None
    };
    arr.iter()
        .find(|c| c["creditType"].as_str() == Some("GOOGLE_ONE_AI") && !c["creditAmount"].is_null())
        .or_else(|| arr.iter().find(|c| !c["creditAmount"].is_null()))
        .and_then(|c| parse_amt(&c["creditAmount"]))
        .unwrap_or(Value::Null)
}

#[derive(Clone, Copy)]
struct WinAgg {
    pct: f64,
    resets_at: Option<i64>,
}

impl WinAgg {
    fn add(slot: &mut Option<WinAgg>, pct: f64, resets_at: Option<i64>) {
        match slot {
            None => *slot = Some(WinAgg { pct, resets_at }),
            Some(w) => {
                if pct > w.pct {
                    w.pct = pct;
                }
                w.resets_at = match (w.resets_at, resets_at) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                };
            }
        }
    }
}

fn gemini_windows(ruq: &Value, now_secs: i64) -> Vec<Value> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Kind {
        GemPro,
        GemFlash,
        GemOther,
        ClaudeGpt,
    }
    struct Item {
        weekly: bool,
        kind: Kind,
        pct: f64,
        resets_at: Option<i64>,
    }

    let mut items = Vec::new();
    let mut has_non_gemini = false;
    for b in ruq["buckets"].as_array().into_iter().flatten() {
        let Some(model_id) = b["modelId"].as_str() else { continue };
        let mid = model_id.to_ascii_lowercase();
        if mid.starts_with("tab_") {
            continue;
        }
        let Some(rem) = b["remainingFraction"].as_f64().filter(|x| x.is_finite()) else { continue };
        let used_pct = ((1.0 - rem.clamp(0.0, 1.0)) * 1000.0).round() / 10.0;
        let resets_at = b["resetTime"].as_str().and_then(parse_iso);
        if resets_at.is_none() && used_pct == 0.0 {
            continue;
        }
        let kind = if mid.starts_with("gemini") {
            if ["pro", "carbon", "neon", "argon", "ultra"].iter().any(|k| mid.contains(k)) {
                Kind::GemPro
            } else if ["flash", "hydrogen"].iter().any(|k| mid.contains(k)) {
                Kind::GemFlash
            } else {
                Kind::GemOther
            }
        } else if mid.starts_with("claude") || mid.starts_with("gpt") {
            has_non_gemini = true;
            Kind::ClaudeGpt
        } else {
            continue;
        };
        let weekly = resets_at.is_some_and(|r| r - now_secs > 6 * 3600);
        items.push(Item { weekly, kind, pct: used_pct, resets_at });
    }

    let mut out = Vec::new();
    for (weekly, span) in [(false, "5-hour"), (true, "Weekly")] {
        let gem: Vec<&Item> = items
            .iter()
            .filter(|it| it.weekly == weekly && it.kind != Kind::ClaudeGpt)
            .collect();
        if !gem.is_empty() {
            let key0 = ((gem[0].pct * 10.0).round() as i64, gem[0].resets_at);
            let same = gem.iter().all(|it| ((it.pct * 10.0).round() as i64, it.resets_at) == key0);
            if same {
                let mut agg = None;
                for it in &gem {
                    WinAgg::add(&mut agg, it.pct, it.resets_at);
                }
                if let Some(w) = agg {
                    let label = if has_non_gemini { format!("{span} · Gemini") } else { span.to_string() };
                    out.push(json!({ "label": label, "pct": w.pct, "resets_at": w.resets_at }));
                }
            } else {
                let (mut pro, mut flash, mut other) = (None, None, None);
                for it in &gem {
                    match it.kind {
                        Kind::GemPro => WinAgg::add(&mut pro, it.pct, it.resets_at),
                        Kind::GemFlash => WinAgg::add(&mut flash, it.pct, it.resets_at),
                        Kind::GemOther => WinAgg::add(&mut other, it.pct, it.resets_at),
                        Kind::ClaudeGpt => {}
                    }
                }
                for (sub, agg) in [("Gemini Pro", pro), ("Gemini Flash", flash), ("Gemini", other)] {
                    if let Some(w) = agg {
                        out.push(json!({ "label": format!("{span} · {sub}"), "pct": w.pct, "resets_at": w.resets_at }));
                    }
                }
            }
        }
        let mut cg = None;
        for it in items.iter().filter(|it| it.weekly == weekly && it.kind == Kind::ClaudeGpt) {
            WinAgg::add(&mut cg, it.pct, it.resets_at);
        }
        if let Some(w) = cg {
            out.push(json!({ "label": format!("{span} · Claude & GPT"), "pct": w.pct, "resets_at": w.resets_at }));
        }
    }
    out
}

fn gemini_usage(creds: &GeminiCreds, live: bool) -> Value {
    if matches!(creds, GeminiCreds::ApiKey) {
        return json!({ "windows": [], "credits": Value::Null });
    }
    if !live {
        return json!({ "off": true });
    }
    let v = gemini_usage_live(creds);
    let ok = v.get("windows").is_some();
    with_last_good("gemini-usage-last.json", v, ok)
}

fn gemini_usage_live(creds: &GeminiCreds) -> Value {
    let GeminiCreds::OAuth { expiry, access_token, .. } = creds else {
        return json!({ "error": "Not signed in to Gemini (Antigravity)" });
    };
    if expiry.is_some_and(|exp| exp < now()) {
        return json!({ "error": "Login token expired. Use any Antigravity session and it renews." });
    }
    let Some(token) = access_token.as_deref() else {
        return json!({ "error": "Not signed in to Gemini (Antigravity)" });
    };
    if env_on("AGENT_USAGE_OFFLINE") || std::env::args().any(|a| a == "--offline") {
        return json!({ "error": "offline (AGENT_USAGE_OFFLINE)" });
    }
    let base = gemini_base_url();
    let auth = format!("Bearer {token}");
    let (lca_res, ruq_res) = thread::scope(|s| {
        let lca = s.spawn(|| {
            ureq::post(&format!("{base}/v1internal:loadCodeAssist"))
                .set("Authorization", &auth)
                .set("Content-Type", "application/json")
                .set("User-Agent", "antigravity/1.5")
                .timeout(Duration::from_secs(8))
                .send_json(json!({
                    "metadata": {
                        "ideType": "IDE_UNSPECIFIED",
                        "platform": "PLATFORM_UNSPECIFIED",
                        "pluginType": "GEMINI",
                    }
                }))
        });
        let ruq = ureq::post(&format!("{base}/v1internal:retrieveUserQuota"))
            .set("Authorization", &auth)
            .set("Content-Type", "application/json")
            .set("User-Agent", "antigravity/1.5")
            .timeout(Duration::from_secs(8))
            .send_json(json!({}));
        (lca.join().ok(), ruq)
    });
    trace("gemini usage: responses");

    let ruq: Value = match ruq_res {
        Ok(r) => match r.into_json() {
            Ok(v) => v,
            Err(_) => return json!({ "error": "Unreadable quota response" }),
        },
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            return json!({ "error": "Login token rejected. Use any Antigravity session and it renews." });
        }
        Err(ureq::Error::Status(429, _)) => {
            return json!({ "error": "Google is rate-limiting quota checks. Try Refresh again in a minute." });
        }
        Err(ureq::Error::Status(code, _)) => {
            return json!({ "error": format!("Quota endpoint returned HTTP {code}") });
        }
        Err(_) => return json!({ "error": "Could not reach Google Cloud Code (offline?)" }),
    };

    let lca: Option<Value> = lca_res.and_then(|r| r.ok()).and_then(|r| r.into_json().ok());
    let plan = lca.as_ref().and_then(gemini_plan);
    let credits = lca.as_ref().map(gemini_credits).unwrap_or(Value::Null);
    let windows = gemini_windows(&ruq, now());
    json!({
        "windows": windows,
        "credits": credits,
        "plan": plan,
    })
}

fn gemini_daemon(active: bool, since: Option<i64>, lines: &[String]) -> Value {
    let mut state = None;
    let mut instance_name = None;
    let mut link = None;
    for l in lines.iter().rev() {
        if state.is_none() {
            if l.contains("Reconnected") {
                state = Some("Connected".to_string());
            } else {
                for s in ["Connected", "Ready", "Reconnecting", "Connecting", "Disconnected", "Error"] {
                    if l.contains(s) {
                        state = Some(s.to_string());
                        break;
                    }
                }
            }
        }
        if instance_name.is_none() {
            for prefix in ["Instance name: ", "Hostname: "] {
                if let Some(i) = l.find(prefix) {
                    let raw = l[i + prefix.len()..].trim();
                    if !raw.is_empty() && !raw.to_ascii_lowercase().contains("not assigned yet") {
                        let cleaned: String = raw.chars().filter(|c| !c.is_control()).take(60).collect();
                        let cleaned = cleaned.trim().to_string();
                        if !cleaned.is_empty() {
                            instance_name = Some(cleaned);
                            break;
                        }
                    }
                }
            }
        }
        if link.is_none() {
            if let Some(i) = l.find("https://antigravity.google.com") {
                link = l[i..].split_whitespace().next().map(|s| s.to_string());
            }
        }
    }
    if active && state.is_none() {
        if lines.iter().any(|l| l.contains("Remote control enabled") || l.contains("Language server listening")) {
            state = Some("Connected".to_string());
        }
    }
    if active && link.is_none() {
        link = Some("https://antigravity.google.com".to_string());
    }
    json!({
        "service_active": active,
        "since": if active { json!(since) } else { Value::Null },
        "state": state,
        "instance_name": instance_name,
        "link": link,
        "drops_today": Value::Null,
    })
}

const GEMINI_DROPS_FILE: &str = "gemini-drops-v1.json";

/// Today's brief disconnects of `antigravity-cli-daemon.service`, read incrementally via journal cursor.
fn gemini_drops(midnight: i64) -> Value {
    let (mut cursor, state) = load_day_cache(GEMINI_DROPS_FILE, midnight);
    let mut events: Vec<(i64, i64)> = state["events"]
        .as_array()
        .or_else(|| state.as_array())
        .map(|a| a.iter().filter_map(|e| Some((e[0].as_i64()?, e[1].as_i64()?))).collect())
        .unwrap_or_default();
    let mut disc_at: Option<i64> = state["disc_at"].as_i64();
    let Some((out, rescanned)) = journal_incremental(GEMINI_UNIT, midnight, &mut cursor, true) else {
        return Value::Null;
    };
    trace(format!("gemini drops: {} read", if rescanned { "full-day" } else { "incremental" }));
    if rescanned {
        events.clear();
        disc_at = None;
    }
    for line in out.lines() {
        let clean = strip_ansi(line);
        let Some(at) = journal_ts(&clean) else { continue };
        if at < midnight {
            continue;
        }
        if let Some(i) = clean.find("Reconnected after ") {
            if let Some(secs) = parse_dur(&clean[i + 18..]) {
                events.push((at, secs));
                disc_at = None;
                continue;
            }
        }
        if clean.contains("Started ") || clean.contains("Starting ") {
            disc_at = None;
            continue;
        }
        if clean.contains("Reconnecting") || clean.contains("disconnected") || clean.contains("Disconnected") {
            disc_at.get_or_insert(at);
        } else if clean.contains("Connected") {
            if let Some(d0) = disc_at.take() {
                events.push((at, (at - d0).max(1)));
            }
        }
    }
    save_day_cache(GEMINI_DROPS_FILE, midnight, &cursor, json!({ "events": events, "disc_at": disc_at }));
    json!(events.iter().map(|(at, secs)| json!({ "at": at, "secs": secs })).collect::<Vec<_>>())
}

// ---------- Codex ----------

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
                if tx.send(line).is_err() { break; }
            }
        });
        Ok(Rpc::Stdio { child, stdin, rx })
    }

    fn send(&mut self, msg: &Value) -> Result<(), String> {
        let s = msg.to_string();
        match self {
            Rpc::Ws(ws) => ws.send(tungstenite::Message::Text(s)).map_err(|e| e.to_string()),
            Rpc::Stdio { stdin, .. } => {
                stdin.write_all(s.as_bytes()).and_then(|_| stdin.write_all(b"\n")).and_then(|_| stdin.flush()).map_err(|e| e.to_string())
            }
        }
    }

    /// Next message, waiting at most RPC_READ and never past `deadline`.
    fn recv(&mut self, deadline: Instant) -> Result<Value, String> {
        let left = deadline.saturating_duration_since(Instant::now()).min(RPC_READ);
        if left.is_zero() {
            return Err("timeout".into());
        }
        match self {
            Rpc::Ws(ws) => {
                ws.get_ref().set_read_timeout(Some(left)).ok();
                loop {
                    match ws.read().map_err(|e| e.to_string())? {
                        tungstenite::Message::Text(t) => return serde_json::from_str(&t).map_err(|e| e.to_string()),
                        tungstenite::Message::Close(_) => return Err("closed".into()),
                        // Ping/pong/binary: keep waiting, but never past the deadline.
                        _ if Instant::now() >= deadline => return Err("timeout".into()),
                        _ => continue,
                    }
                }
            }
            Rpc::Stdio { rx, .. } => {
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
                if let Some(err) = m.get("error") { return Err(err["message"].as_str().unwrap_or("error").to_string()); }
                return Ok(m["result"].clone());
            }
        }
    }

    /// Send all requests at once and collect the answers by id (the server works on them
    /// concurrently instead of one round trip after another).
    fn call_all(&mut self, calls: &[(u64, &str, Value)], deadline: Instant) -> HashMap<u64, Result<Value, String>> {
        let mut res = HashMap::new();
        for (id, method, params) in calls {
            if let Err(e) = self.send(&json!({ "id": id, "method": method, "params": params })) {
                for (id, ..) in calls { res.entry(*id).or_insert_with(|| Err(e.clone())); }
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
                    for (id, ..) in calls { res.entry(*id).or_insert_with(|| Err(e.clone())); }
                }
            }
        }
        res
    }

    fn init(&mut self, deadline: Instant) -> Result<(), String> {
        self.call(0, "initialize", json!({
            "clientInfo": { "name": "agent_usage", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": { "experimentalApi": true },
        }), deadline)?;
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

/// Start time (Unix seconds) of a process: /proc/<pid>/stat field 22 (clock ticks since boot).
fn process_start(pid: u32) -> Option<i64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let ticks: i64 = stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()?;
    let btime: i64 = fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok())?;
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    (hz > 0).then(|| btime + ticks / hz as i64)
}

/// Classify an `agy` process's arguments (after argv[0]):
/// `Some(connected)` for an interactive terminal session (`connected` = `--remote-control` present),
/// `None` for non-interactive flags (`-p`, `--print`, `--prompt`, `-h`, `--help`, `-v`, `--version`)
/// or CLI subcommands (`agy models`, `agy remote-control`, `agy mcp`, ...).
fn agy_interactive_session(args: &[&[u8]]) -> Option<bool> {
    const SUBCOMMANDS: &[&[u8]] = &[
        b"acp", b"agent", b"agents", b"changelog", b"help", b"install",
        b"mcp", b"mic-serve", b"models", b"plugin", b"plugins",
        b"remote-control", b"update",
    ];
    const VALUE_FLAGS: &[&[u8]] = &[
        b"--conversation", b"-r", b"--resume", b"-m", b"--model",
        b"-i", b"--prompt-interactive", b"--input-format", b"--output-format",
        b"--json-schema", b"--print-timeout",
        b"--effort", b"--mode", b"--agent", b"--project", b"--log-file",
        b"--settings", b"--add-dir", b"-C", b"--cwd", b"-w", b"--worktree",
    ];
    let mut connected = false;
    let mut saw_positional = false;
    let mut after_ddash = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if !after_ddash {
            if a == b"--" {
                after_ddash = true;
                i += 1;
                continue;
            }
            if matches!(a, b"-p" | b"--print" | b"--prompt" | b"-h" | b"--help" | b"-v" | b"--version")
                || a.starts_with(b"--prompt=")
                || a.starts_with(b"--print=")
            {
                return None;
            }
            if a == b"--remote-control" {
                connected = true;
                i += 1;
                continue;
            }
            if VALUE_FLAGS.contains(&a) {
                i += 2;
                continue;
            }
            if a.starts_with(b"-") {
                i += 1;
                continue;
            }
        }
        if !saw_positional {
            saw_positional = true;
            if SUBCOMMANDS.contains(&a) {
                return None;
            }
        }
        i += 1;
    }
    Some(connected)
}

#[derive(Default)]
struct ProcScan {
    codex_managed: bool,
    codex_since: Option<i64>,
    codex_stray: i32,
    agy_total: i32,
    agy_connected: i32,
}

/// Single-pass `/proc` scan for both Codex app-servers and interactive `agy` terminal sessions.
fn scan_processes() -> ProcScan {
    use std::os::unix::fs::MetadataExt;
    let managed_dir = home().join(".codex/packages/app-server-daemon").to_string_lossy().into_owned();
    let mut out = ProcScan::default();
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let Ok(rd) = fs::read_dir("/proc") else { return out };
    for e in rd.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
        let Ok(md) = e.metadata() else { continue };
        if md.uid() != uid {
            continue;
        }
        let Ok(raw) = fs::read(format!("/proc/{pid}/cmdline")) else { continue };
        let args: Vec<&[u8]> = raw.split(|b| *b == 0).filter(|a| !a.is_empty()).collect();
        if args.is_empty() {
            continue;
        }
        let Ok(exe_link) = fs::read_link(format!("/proc/{pid}/exe")) else { continue };
        let exe = exe_link.to_string_lossy();
        let exe_clean = exe.strip_suffix(" (deleted)").unwrap_or(&exe);
        if Path::new(exe_clean).file_name().and_then(|s| s.to_str()) == Some("agy") {
            if let Some(conn) = agy_interactive_session(&args[1..]) {
                out.agy_total += 1;
                if conn {
                    out.agy_connected += 1;
                }
            }
            continue;
        }
        if args.contains(&&b"app-server"[..]) && !args.iter().any(|a| *a == b"daemon" || *a == b"proxy") {
            if exe_clean.starts_with(&managed_dir) {
                if args.contains(&&b"--remote-control"[..]) {
                    out.codex_managed = true;
                    out.codex_since = process_start(pid);
                }
            } else if args.contains(&&b"--listen"[..]) && args.iter().any(|a| a.starts_with(b"unix://")) {
                out.codex_stray += 1;
            }
        }
    }
    out
}

/// Results flow to main() as (part, value) as soon as each one is known (see PARTS).
type Tx = mpsc::Sender<(&'static str, Value)>;

const ACCOUNT_READ: u64 = 1;
const LIMITS_READ: u64 = 2;
const USAGE_READ: u64 = 3;
const RELAY_READ: u64 = 4;

fn codex_unreachable() -> Value {
    let offline = env_on("AGENT_USAGE_OFFLINE") || std::env::args().any(|a| a == "--offline");
    json!({ "error": if offline { "offline (AGENT_USAGE_OFFLINE)" } else { "Codex is not reachable" } })
}

/// Turn one app-server answer into its part and send it.
fn codex_emit(tx: &Tx, id: u64, r: Option<Result<Value, String>>, date: &str) {
    let part = match id {
        ACCOUNT_READ => ("codex.account", match r {
            Some(Ok(a)) => {
                let acc = &a["account"];
                json!({ "logged_in": !acc.is_null(), "email": acc["email"], "plan": acc["planType"], "type": acc["type"] })
            }
            _ => json!({ "logged_in": false }),
        }),
        LIMITS_READ => ("codex.usage", last_good_part("codex-usage-last.json", match r {
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
        })),
        USAGE_READ => ("codex.tokens", last_good_part("codex-tokens-last.json", match r {
            Some(Ok(u)) => {
                let buckets = u["dailyUsageBuckets"].as_array().cloned().unwrap_or_default();
                let today = buckets.iter().find(|b| b["startDate"] == date).and_then(|b| b["tokens"].as_u64()).unwrap_or(0);
                let last7: u64 = buckets.iter().rev().take(7).filter_map(|b| b["tokens"].as_u64()).sum();
                json!({ "today": today, "last_7_days": last7, "lifetime": u["summary"]["lifetimeTokens"] })
            }
            // OpenAI's token history is the slowest call (seconds at times); past the read timeout
            // the saved totals are shown instead.
            Some(Err(e)) => json!({ "error": format!("Codex token totals did not load: {e}") }),
            None => json!({ "error": "Codex is not reachable" }),
        })),
        RELAY_READ => ("codex.relay", match r {
            Some(Ok(s)) => json!({ "status": s["status"], "name": s["serverName"] }),
            _ => json!({ "status": "unknown" }),
        }),
        _ => return,
    };
    let _ = tx.send(part);
}

/// A Codex part with the last-good fallback: anything without an `error` counts as a success.
fn last_good_part(name: &str, v: Value) -> Value {
    let ok = v.get("error").is_none();
    with_last_good(name, v, ok)
}

/// Codex account, limits, token totals and relay status from its app-server. Each result is sent
/// on `tx` the moment it is known, so a slow ChatGPT backend call holds back only its own part.
fn codex_api(date: &str, tx: &Tx) {
    let deadline = Instant::now() + TIMEOUT;
    let offline = env_on("AGENT_USAGE_OFFLINE") || std::env::args().any(|a| a == "--offline");
    let (mut rpc, via_daemon) = if offline {
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
            codex_emit(tx, id, None, date);
        }
        if via_daemon {
            codex_emit(tx, RELAY_READ, None, date);
        }
    };
    let Some(r) = rpc.as_mut() else { return fail_all() };
    trace(format!("codex rpc: connected ({})", if via_daemon { "daemon" } else { "private app-server" }));
    if r.init(deadline).is_err() {
        return fail_all();
    }
    trace("codex rpc: initialized");
    let account = (ACCOUNT_READ, "account/read", json!({ "refreshToken": false }));
    // These two go to the ChatGPT backend (~0.2-0.6 s each, sometimes 2 s+).
    let slow = [(LIMITS_READ, "account/rateLimits/read", Value::Null), (USAGE_READ, "account/usage/read", json!({}))];
    if via_daemon {
        // The background service answers one request at a time per connection, so each slow call
        // gets its own connection and the quick ones share the first: wall time is the slowest
        // call, not the sum. A call whose connection cannot be opened runs on the first one.
        let quick = [account, (RELAY_READ, "remoteControl/status/read", Value::Null)];
        thread::scope(|s| {
            let sides: Vec<_> = slow
                .into_iter()
                .map(|c| {
                    s.spawn(move || {
                        let opened = Rpc::daemon().and_then(|mut r| r.init(deadline).map(|_| r));
                        match opened {
                            Ok(mut r) => {
                                let mut m = r.call_all(std::slice::from_ref(&c), deadline);
                                codex_emit(tx, c.0, m.remove(&c.0), date);
                                None
                            }
                            Err(_) => Some(c),
                        }
                    })
                })
                .collect();
            let mut res = r.call_all(&quick, deadline);
            for (id, ..) in &quick {
                codex_emit(tx, *id, res.remove(id), date);
            }
            for h in sides {
                if let Ok(Some(c)) = h.join() {
                    let id = c.0;
                    let mut m = r.call_all(&[c], deadline);
                    codex_emit(tx, id, m.remove(&id), date);
                }
            }
        });
    } else {
        // A private `codex app-server`: one process, requests pipelined.
        let mut calls = vec![account];
        calls.extend(slow);
        let mut res = r.call_all(&calls, deadline);
        for id in all {
            codex_emit(tx, id, res.remove(&id), date);
        }
    }
}

// ---------- main ----------

/// Parts that arrive after the local checks, with where each one goes in the snapshot. "Soft"
/// parts are quick (local socket or journal) and normally make the first frame; the others need
/// the network.
const PARTS: &[(&str, &[&str], bool)] = &[
    ("codex.account", &["codex", "account"], true),
    ("codex.relay", &["codex", "workers", "service", "relay"], true),
    ("claude.drops", &["claude", "workers", "server", "drops_today"], true),
    ("codex.events", &["codex", "workers", "watchdog", "events_today"], true),
    ("gemini.drops", &["gemini", "workers", "daemon", "drops_today"], true),
    ("claude.usage", &["claude", "usage"], false),
    ("codex.usage", &["codex", "usage"], false),
    ("codex.tokens", &["codex", "tokens"], false),
    ("gemini.usage", &["gemini", "usage"], false),
];
/// How long (from start) the first frame waits for the soft parts before it goes out without them.
const SOFT_WAIT: Duration = Duration::from_millis(60);
/// The last complete snapshot, for the widget's instant first open after a login and as the
/// placeholder for parts still loading in --stream mode.
const LAST_FILE: &str = "last-snapshot.json";

fn part_path(key: &str) -> &'static [&'static str] {
    PARTS.iter().find(|p| p.0 == key).map(|p| p.1).unwrap_or(&[])
}

fn gemini_unreachable() -> Value {
    let offline = env_on("AGENT_USAGE_OFFLINE") || std::env::args().any(|a| a == "--offline");
    json!({ "error": if offline { "offline (AGENT_USAGE_OFFLINE)" } else { "Could not reach Google Cloud Code (offline?)" } })
}

/// What a part is when it never arrived (its sender ended without sending it): the same values as
/// before streaming.
fn part_default(key: &str) -> Value {
    match key {
        "codex.account" => json!({ "logged_in": false }),
        "codex.usage" => codex_unreachable(),
        "codex.relay" => json!({ "status": "unknown" }),
        "gemini.usage" => gemini_unreachable(),
        _ => Value::Null,
    }
}

fn get_path<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(v, |v, k| v.get(*k))
}

fn set_path(v: &mut Value, path: &[&str], x: Value) {
    let Some((last, parents)) = path.split_last() else { return };
    let mut cur = v;
    for k in parents {
        cur = &mut cur[*k];
    }
    cur[*last] = x;
}

fn write_line(v: &Value, pretty: bool) {
    let text = if pretty { serde_json::to_string_pretty(v) } else { serde_json::to_string(v) };
    // Not println!: a reader that went away must not turn into a panic (abort).
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", text.unwrap_or_else(|_| "{}".into()));
    let _ = out.flush();
}

fn tracker_config() -> Option<Value> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"));
    read_json(&dir.join("ai-tracker/config.json"))
}

/// Live Claude plan limits are opt-in (`{"claude_live_limits": true}`).
fn claude_live_enabled(cfg: Option<&Value>) -> bool {
    cfg.is_some_and(|c| c["claude_live_limits"].as_bool() == Some(true))
}

/// Live Gemini quota limits are on by default; turn off with `{"gemini_live_limits": false}`.
fn gemini_live_enabled(cfg: Option<&Value>) -> bool {
    cfg.and_then(|c| c["gemini_live_limits"].as_bool()) != Some(false)
}

/// Claude Code's credentials, reduced to what the collector uses: plan, tier, scopes, expiry and whether a
/// login exists. The access token itself is kept only when live limits are on; the refresh token and
/// everything else in the file are dropped right after parsing.
fn read_creds(p: &Path, with_token: bool) -> Option<Value> {
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

/// Usage: agent-usage [--stream] [--pretty] [--offline]
///   default   one JSON snapshot on stdout once everything is in.
///   --stream  newline-delimited snapshots: the first right after the local checks (~15 ms) with
///             "pending": [parts still loading] (shown with their last saved values), then one per
///             part as it lands, the last with "pending": []. The widget draws each one.
fn main() {
    // The process holds login tokens: no core dumps (and no ptrace by other processes).
    // SAFETY: prctl with integer arguments has no memory-safety preconditions.
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
    }
    let t0 = Instant::now();
    if env_on("AGENT_USAGE_TRACE") {
        let _ = T0.set(t0);
    }
    let pretty = std::env::args().any(|a| a == "--pretty");
    let stream = std::env::args().any(|a| a == "--stream");
    let offline = env_on("AGENT_USAGE_OFFLINE") || std::env::args().any(|a| a == "--offline");
    let h = home();
    let cache = tokens::cache_dir(&h);
    let (date, midnight) = today();
    trace(format!("today: {date} {midnight}"));
    let cfg = tracker_config();
    let live_claude = claude_live_enabled(cfg.as_ref());
    let live_gemini = gemini_live_enabled(cfg.as_ref());
    // Tiny files, read once and shared: access tokens never leave their live usage requests.
    let creds = read_creds(&h.join(".claude/.credentials.json"), live_claude);
    let settings = read_json(&h.join(".claude/settings.json"));
    let gemini_settings = read_json(&h.join(".gemini/antigravity-cli/settings.json"));
    let last_snap = cache.as_ref().and_then(|d| read_json(&d.join(LAST_FILE)));
    let prev = if stream { last_snap.as_ref() } else { None };
    let (creds_ref, date_ref, gset_ref) = (creds.as_ref(), date.as_str(), gemini_settings.as_ref());
    let gcreds_cell: OnceLock<GeminiCreds> = OnceLock::new();
    let get_gcreds = || gcreds_cell.get_or_init(|| load_gemini_creds(gset_ref, live_gemini));

    let (tx, rx) = mpsc::channel::<(&'static str, Value)>();
    let out = thread::scope(|s| {
        // Slowest first: the live APIs, then the journal reads.
        {
            let tx = tx.clone();
            s.spawn(move || {
                let v = claude_usage(creds_ref, live_claude);
                trace("claude usage: done");
                let _ = tx.send(("claude.usage", v));
            });
        }
        {
            let tx = tx.clone();
            s.spawn(move || {
                let v = gemini_usage(get_gcreds(), live_gemini);
                trace("gemini usage: done");
                let _ = tx.send(("gemini.usage", v));
            });
        }
        {
            let tx = tx.clone();
            s.spawn(move || {
                codex_api(date_ref, &tx);
                trace("codex rpc: done");
            });
        }
        {
            let tx = tx.clone();
            s.spawn(move || {
                let v = claude_drops(midnight);
                trace("claude drops: done");
                let _ = tx.send(("claude.drops", v));
            });
        }
        {
            let tx = tx.clone();
            s.spawn(move || {
                let v = codex_events(midnight);
                trace("codex events: done");
                let _ = tx.send(("codex.events", v));
            });
        }
        {
            let tx = tx.clone();
            s.spawn(move || {
                let v = gemini_drops(midnight);
                trace("gemini drops: done");
                let _ = tx.send(("gemini.drops", v));
            });
        }
        drop(tx);
        let claude_toks = s.spawn(|| {
            let r = tokens::claude_tokens_today(&h, midnight);
            trace(&r.trace);
            r.value
        });
        let gemini_toks = s.spawn(|| {
            let r = tokens::gemini_tokens_today(&h, midnight);
            trace(&r.trace);
            r.value
        });
        let units = s.spawn(|| {
            let u = user_units();
            trace("systemctl --user show: done");
            u
        });
        let jclaude = s.spawn(|| {
            let l = journal_lines(CLAUDE_UNIT, "80");
            trace("journalctl claude: done");
            l
        });
        let jcodex = s.spawn(|| {
            let l = journal_lines(CODEX_UNIT, "30");
            trace("journalctl codex: done");
            l
        });
        let ssh = s.spawn(sshd_active);
        let procs = s.spawn(scan_processes);

        let account = claude_account_direct(&h, creds.as_ref(), settings.as_ref()).unwrap_or_else(|| {
            trace("claude account: falling back to `claude auth status`");
            claude_account_cli(creds.as_ref())
        });
        trace("claude account: done");
        let sessions = claude_sessions(&h);
        let auto = settings.as_ref().and_then(|v| v["remoteControlAtStartup"].as_bool()).unwrap_or(false);
        trace("claude sessions: done");
        let gem_account = gemini_account(get_gcreds(), last_snap.as_ref());
        trace("gemini account: done");

        let units = units.join().unwrap_or_default();
        let prop = |u: &str, k: &str| units.get(u).and_then(|p| p.get(k)).cloned();
        let stamp = |u: &str, k: &str| prop(u, k).and_then(|v| v.trim_start_matches('@').parse::<i64>().ok());
        let active = |u: &str| prop(u, "ActiveState").as_deref() == Some("active");
        let gem_active = active(GEMINI_UNIT);
        // Only tail the Gemini daemon's journal when it is active: an unregistered unit with zero
        // journal entries makes `journalctl -n 60` scan the whole user journal backwards (~200 ms).
        let jgemini = gem_active.then(|| {
            s.spawn(|| {
                let l = journal_lines(GEMINI_UNIT, "60");
                trace("journalctl gemini: done");
                l
            })
        });
        let mut server = claude_server(active(CLAUDE_UNIT), &jclaude.join().unwrap_or_default());
        server["since"] = if active(CLAUDE_UNIT) { json!(stamp(CLAUDE_UNIT, "ActiveEnterTimestamp")) } else { Value::Null };
        let last = jcodex
            .join()
            .unwrap_or_default()
            .into_iter()
            .rev()
            .find(|l| !(l.starts_with("Starting ") || l.starts_with("Finished ") || l.contains("Consumed ") || l.starts_with("Stopped ")));
        let gem_since = if gem_active { stamp(GEMINI_UNIT, "ActiveEnterTimestamp") } else { None };
        let gem_lines = jgemini.and_then(|h| h.join().ok()).unwrap_or_default();
        let daemon = gemini_daemon(gem_active, gem_since, &gem_lines);
        let ps = procs.join().unwrap_or_default();
        let ssh = ssh.join().unwrap_or(false);
        let claude_toks = claude_toks.join().unwrap_or(Value::Null);
        let gemini_toks = gemini_toks.join().unwrap_or(Value::Null);
        let mut out = json!({
            "generated_at": now(),
            "claude": {
                "account": account,
                "usage": null,
                "tokens": claude_toks,
                "workers": {
                    "server": server,
                    "sessions": sessions,
                    "auto_connect": auto,
                },
            },
            "codex": {
                "account": null,
                "usage": null,
                "tokens": null,
                "workers": {
                    "service": { "running": ps.codex_managed, "since": ps.codex_since, "relay": null },
                    "stray_servers": ps.codex_stray,
                    "watchdog": {
                        "timer_active": active(CODEX_TIMER),
                        "last_run": stamp(CODEX_UNIT, "ExecMainExitTimestamp"),
                        "last_ok": prop(CODEX_UNIT, "Result").map(|v| v == "success"),
                        "last_message": last,
                        "relay_held_by": relay_held_by(&runtime_dir(), now()),
                    },
                    "ssh": { "active": ssh },
                },
            },
            "gemini": {
                "account": gem_account,
                "usage": null,
                "tokens": gemini_toks,
                "workers": {
                    "daemon": daemon,
                    "sessions": { "total": ps.agy_total, "connected": ps.agy_connected },
                },
            },
        });
        trace("local checks: done");

        let mut pending: Vec<&'static str> = PARTS.iter().map(|p| p.0).collect();
        let apply = |out: &mut Value, pending: &mut Vec<&'static str>, (k, v): (&'static str, Value)| {
            if k == "gemini.usage" {
                if let Some(p) = v
                    .get("plan")
                    .or_else(|| v.get("stale").and_then(|s| s.get("plan")))
                    .and_then(|x| x.as_str())
                {
                    if out["gemini"]["account"]["logged_in"] == json!(true)
                        && out["gemini"]["account"]["auth_method"] != json!("apikey")
                    {
                        out["gemini"]["account"]["plan"] = json!(p);
                    }
                }
            }
            set_path(out, part_path(k), v);
            pending.retain(|p| *p != k);
        };
        let soft_left = |pending: &[&str]| pending.iter().any(|k| PARTS.iter().any(|p| p.0 == *k && p.2));
        while soft_left(&pending) {
            match rx.recv_timeout((t0 + SOFT_WAIT).saturating_duration_since(Instant::now())) {
                Ok(m) => apply(&mut out, &mut pending, m),
                Err(_) => break,
            }
        }
        let frame = |out: &Value, pending: &[&str]| {
            let mut f = out.clone();
            for k in pending {
                let path = part_path(k);
                let placeholder = prev.and_then(|p| get_path(p, path)).cloned().unwrap_or(Value::Null);
                set_path(&mut f, path, placeholder);
            }
            f["generated_at"] = json!(now());
            f["pending"] = json!(pending);
            write_line(&f, pretty);
        };
        if stream && !pending.is_empty() {
            frame(&out, &pending);
            trace(format!("frame: first, {} pending", pending.len()));
        }
        while !pending.is_empty() {
            let Ok(m) = rx.recv() else { break };
            apply(&mut out, &mut pending, m);
            // Parts that landed together go out in one frame.
            while let Ok(m) = rx.try_recv() {
                apply(&mut out, &mut pending, m);
            }
            if stream && !pending.is_empty() {
                frame(&out, &pending);
                trace(format!("frame: {} pending", pending.len()));
            }
        }
        for k in std::mem::take(&mut pending) {
            set_path(&mut out, part_path(k), part_default(k));
        }
        out["generated_at"] = json!(now());
        out
    });

    if stream {
        let mut last = out.clone();
        last["pending"] = json!([]);
        write_line(&last, pretty);
    } else {
        write_line(&out, pretty);
    }
    if !offline {
        if let Some(d) = &cache {
            write_private(d, LAST_FILE, out.to_string().as_bytes());
        }
    }
    if T0.get().is_some() {
        trace("output written");
        if let Ok(v) = TRACE.lock() {
            eprintln!("{}", v.join("\n"));
        }
    }
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
    fn parse_iso_rfc3339_nanoseconds_and_offset() {
        let with_ns_off = parse_iso("2026-11-20T15:18:51.615636742-08:00").unwrap();
        let utc = parse_iso("2026-11-20T23:18:51Z").unwrap();
        let plus_off = parse_iso("2026-11-21T01:18:51.123456789+02:00").unwrap();
        assert_eq!(with_ns_off, utc);
        assert_eq!(plus_off, utc);
    }

    #[test]
    fn gemini_plan_tiers_and_credits() {
        let ultra = json!({
            "currentTier": { "id": "standard-tier", "name": "Gemini Code Assist" },
            "paidTier": {
                "id": "g1-ultra-tier",
                "name": "Google AI Ultra",
                "availableCredits": [{ "creditType": "GOOGLE_ONE_AI", "creditAmount": "500" }]
            }
        });
        assert_eq!(gemini_plan(&ultra).as_deref(), Some("ultra"));
        assert_eq!(gemini_credits(&ultra), json!(500));

        let pro = json!({
            "currentTier": { "id": "free-tier", "name": "Free" },
            "paidTier": {
                "id": "g1-pro-tier",
                "name": "Google AI Pro",
                "availableCredits": [{ "creditAmount": 250 }]
            }
        });
        assert_eq!(gemini_plan(&pro).as_deref(), Some("pro"));
        assert_eq!(gemini_credits(&pro), json!(250));

        let free = json!({ "currentTier": { "id": "free-tier", "name": "Free Tier" } });
        assert_eq!(gemini_plan(&free).as_deref(), Some("free"));
        assert_eq!(gemini_credits(&free), Value::Null);

        let standard = json!({ "currentTier": { "id": "standard-tier", "name": "Standard" } });
        assert_eq!(gemini_plan(&standard).as_deref(), Some("standard"));

        let ent = json!({ "paidTier": { "id": "enterprise-tier", "name": "Enterprise" } });
        assert_eq!(gemini_plan(&ent).as_deref(), Some("enterprise"));
    }

    #[test]
    fn gemini_windows_grouping_and_ordering() {
        let base = parse_iso("2026-11-20T12:00:00Z").unwrap();
        let t_3h = "2026-11-20T15:00:00Z";
        let t_2h = "2026-11-20T14:00:00Z";
        let t_5d = "2026-11-25T12:00:00Z";

        // 1) Unified 5-hour · Gemini + 5-hour · Claude & GPT, tab_* ignored
        let unified = json!({
            "buckets": [
                { "modelId": "tab_flash_lite_preview", "remainingFraction": 0.1, "resetTime": t_3h },
                { "modelId": "gemini-4-carbon-high", "remainingFraction": 0.75, "resetTime": t_3h },
                { "modelId": "gemini-3-flash", "remainingFraction": 0.75, "resetTime": t_3h },
                { "modelId": "claude-sonnet-4-5", "remainingFraction": 0.60, "resetTime": t_2h },
                { "modelId": "gpt-oss-120b", "remainingFraction": 0.90, "resetTime": t_3h },
            ]
        });
        let w = gemini_windows(&unified, base);
        let labels: Vec<_> = w.iter().map(|x| x["label"].as_str().unwrap()).collect();
        assert_eq!(labels, ["5-hour · Gemini", "5-hour · Claude & GPT"]);
        assert_eq!(w[0]["pct"], 25.0);
        assert_eq!(w[1]["pct"], 40.0);
        assert_eq!(w[1]["resets_at"], parse_iso(t_2h).unwrap());

        // 2) Split Gemini Pro vs Gemini Flash when fractions differ + Weekly (>6h) + Gemini-only label
        let split = json!({
            "buckets": [
                { "modelId": "gemini-3-pro", "remainingFraction": 0.55, "resetTime": t_3h },
                { "modelId": "gemini-4-argon", "remainingFraction": 0.70, "resetTime": t_2h },
                { "modelId": "gemini-3-flash", "remainingFraction": 0.90, "resetTime": t_3h },
                { "modelId": "gemini-4-ultra", "remainingFraction": 0.80, "resetTime": t_5d },
                { "modelId": "gemini-3-hydrogen", "remainingFraction": 0.80, "resetTime": t_5d },
            ]
        });
        let w2 = gemini_windows(&split, base);
        let labels2: Vec<_> = w2.iter().map(|x| x["label"].as_str().unwrap()).collect();
        assert_eq!(labels2, ["5-hour · Gemini Pro", "5-hour · Gemini Flash", "Weekly"]);
        assert_eq!(w2[0]["pct"], 45.0);
        assert_eq!(w2[0]["resets_at"], parse_iso(t_2h).unwrap()); // earliest reset in Pro group
        assert_eq!(w2[1]["pct"], 10.0);
        assert_eq!(w2[2]["pct"], 20.0);
    }

    #[test]
    fn agy_cli_session_classification() {
        let s = |v: &[&[u8]]| agy_interactive_session(v);
        assert_eq!(s(&[]), Some(false));
        assert_eq!(s(&[b"--remote-control"]), Some(true));
        assert_eq!(s(&[b"-c", b"--remote-control"]), Some(true));
        assert_eq!(s(&[b"-i", b"models"]), Some(false));
        assert_eq!(s(&[b"--model", b"models", b"--remote-control"]), Some(true));
        assert_eq!(s(&[b"--agent", b"agent"]), Some(false));
        assert_eq!(s(&[b"models"]), None);
        assert_eq!(s(&[b"remote-control"]), None);
        assert_eq!(s(&[b"mcp", b"list"]), None);
        assert_eq!(s(&[b"-p", b"hello"]), None);
        assert_eq!(s(&[b"--prompt=hello"]), None);
        assert_eq!(s(&[b"--help"]), None);
    }

    #[test]
    fn relay_holder_file() {
        let dir = std::env::temp_dir().join(format!("agent-usage-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("codex-remote-control-watchdog.held");
        assert_eq!(relay_held_by(&dir, now()), None);
        fs::write(&f, "Codex Desktop\n").unwrap();
        assert_eq!(relay_held_by(&dir, now()).as_deref(), Some("Codex Desktop"));
        assert_eq!(relay_held_by(&dir, now() + 181), None); // stale: the watchdog stopped checking
        fs::write(&f, "  \n").unwrap();
        assert_eq!(relay_held_by(&dir, now()), None);
        fs::write(&f, "a\u{1b}[31mb\n").unwrap();
        assert_eq!(relay_held_by(&dir, now()).as_deref(), Some("a[31mb"));
        fs::write(&f, "x".repeat(300)).unwrap();
        assert_eq!(relay_held_by(&dir, now()), None); // oversized
        fs::remove_file(&f).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", &f).unwrap();
        assert_eq!(relay_held_by(&dir, now()), None); // never follows a link
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn scoped_weekly_limits() {
        let d = json!({
            "five_hour": null,
            "seven_day": { "utilization": 32.0, "resets_at": "2026-11-05T21:59:59.738180+00:00" },
            "seven_day_opus": { "utilization": 5.0, "resets_at": null },
            "limits": [
                { "kind": "weekly_all", "percent": 32, "scope": null },
                { "kind": "weekly_scoped", "percent": 0, "resets_at": "2026-11-05T22:00:00+00:00",
                  "scope": { "model": { "id": null, "display_name": "Fable" }, "surface": null } },
                { "kind": "weekly_scoped", "percent": 7, "scope": { "model": { "display_name": "Opus" } } },
                { "kind": "weekly_scoped", "percent": 1, "scope": { "model": { "display_name": "  " } } },
                { "kind": "weekly_scoped", "scope": { "model": { "display_name": "NoPercent" } } }
            ]
        });
        let w = claude_windows(&d);
        let labels: Vec<_> = w.iter().map(|w| w["label"].as_str().unwrap()).collect();
        assert_eq!(labels, ["Weekly", "Weekly · Opus", "Weekly · Fable"]);
        assert_eq!(w[2]["pct"], 0.0);
        assert!(w[2]["resets_at"].as_i64().is_some());
        assert_eq!(w[1]["pct"], 5.0); // the fixed key wins over the list
        assert!(claude_windows(&json!({})).is_empty());
    }
}
