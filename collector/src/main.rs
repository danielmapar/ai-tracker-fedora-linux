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
//! Environment (for testing): AGENT_USAGE_TRACE=1 prints per-source timings to stderr;
//! AGENT_USAGE_OFFLINE=1 (or --offline) skips live usage APIs; AGENT_USAGE_NO_CACHE=1 does a full
//! transcript scan without the cache; AGENT_USAGE_CACHE_DIR overrides the cache directory;
//! AGENT_USAGE_GEMINI_URL overrides the Gemini Cloud Code endpoint (https:// or http://127.0.0.1:).

mod claude;
mod codex;
mod gemini;
mod system;
mod tokens;

use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{mpsc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use claude::{
    claude_account_cli, claude_account_direct, claude_drops, claude_live_enabled, claude_server, claude_sessions,
    claude_usage, read_creds, read_oauth_account,
};
use codex::{codex_api, codex_events, codex_unreachable, relay_held_by};
use gemini::{
    gemini_account, gemini_daemon, gemini_drops, gemini_live_enabled, gemini_unreachable, gemini_usage,
    load_gemini_creds, GeminiCreds,
};
use system::{
    env_on, flush_trace, home, init_trace, is_offline, journal_lines, now, read_json, runtime_dir, scan_processes,
    sshd_active, today, trace, user_units, write_private, CLAUDE_UNIT, CODEX_TIMER, CODEX_UNIT, GEMINI_UNIT,
};

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

/// What a part is when it never arrived (its sender ended without sending it).
fn part_default(key: &str) -> Value {
    match key {
        "codex.account" => json!({ "logged_in": false }),
        "codex.usage" | "codex.tokens" => codex_unreachable(),
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
        init_trace(t0);
    }
    let pretty = std::env::args().any(|a| a == "--pretty");
    let stream = std::env::args().any(|a| a == "--stream");
    let offline = is_offline();
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
    let (h_ref, creds_ref, date_ref, gset_ref) = (h.as_path(), creds.as_ref(), date.as_str(), gemini_settings.as_ref());
    let claude_acct_cell: OnceLock<Option<Value>> = OnceLock::new();
    let get_claude_acct = || claude_acct_cell.get_or_init(|| read_oauth_account(h_ref)).as_ref();
    let gcreds_cell: OnceLock<GeminiCreds> = OnceLock::new();
    let get_gcreds = || gcreds_cell.get_or_init(|| load_gemini_creds(gset_ref, live_gemini));

    let (tx, rx) = mpsc::channel::<(&'static str, Value)>();
    let out = thread::scope(|s| {
        // Slowest first: the live APIs, then the journal reads.
        {
            let tx = tx.clone();
            s.spawn(move || {
                let v = claude_usage(creds_ref, get_claude_acct, live_claude);
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

        let account = claude_account_direct(&h, creds_ref, settings.as_ref(), get_claude_acct)
            .unwrap_or_else(|| {
                trace("claude account: falling back to `claude auth status`");
                claude_account_cli(creds_ref)
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
        if stream {
            let soft_left = |pending: &[&str]| pending.iter().any(|k| PARTS.iter().any(|p| p.0 == *k && p.2));
            while soft_left(&pending) {
                match rx.recv_timeout((t0 + SOFT_WAIT).saturating_duration_since(Instant::now())) {
                    Ok(m) => apply(&mut out, &mut pending, m),
                    Err(_) => break,
                }
            }
            if !pending.is_empty() {
                frame(&out, &pending);
                trace(format!("frame: first, {} pending", pending.len()));
            }
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
    flush_trace();
}
