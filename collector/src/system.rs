//! Shared OS, subprocess, time, private-cache and journal helpers for `agent-usage`.

use serde_json::{json, Value};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const TIMEOUT: Duration = Duration::from_secs(10);
/// Per-message read timeout of the Codex app-server.
pub const RPC_READ: Duration = Duration::from_secs(8);

pub const CLAUDE_UNIT: &str = "claude-remote-control.service";
pub const CODEX_UNIT: &str = "codex-remote-control.service";
pub const CODEX_TIMER: &str = "codex-remote-control.timer";
pub const GEMINI_UNIT: &str = "antigravity-cli-daemon.service";

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn env_on(k: &str) -> bool {
    std::env::var_os(k).is_some_and(|v| !v.is_empty() && v != "0")
}

pub fn is_offline() -> bool {
    env_on("AGENT_USAGE_OFFLINE") || std::env::args().any(|a| a == "--offline")
}

// ---------- tracing (AGENT_USAGE_TRACE=1; timings only, never values) ----------

static T0: OnceLock<Instant> = OnceLock::new();
static TRACE: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn init_trace(t0: Instant) {
    let _ = T0.set(t0);
}

pub fn trace(what: impl AsRef<str>) {
    if let Some(t0) = T0.get() {
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        if let Ok(mut v) = TRACE.lock() {
            v.push(format!("{ms:7.1} ms  {}", what.as_ref()));
        }
    }
}

pub fn flush_trace() {
    if T0.get().is_some() {
        trace("output written");
        if let Ok(v) = TRACE.lock() {
            eprintln!("{}", v.join("\n"));
        }
    }
}

// ---------- subprocesses & text ----------

/// Prefer ~/.local/bin/<name> (GNOME Shell's PATH may not include it), else rely on PATH.
pub fn tool(name: &str) -> String {
    let p = home().join(".local/bin").join(name);
    if p.exists() { p.to_string_lossy().into_owned() } else { name.to_string() }
}

/// Run a command with a timeout. Returns (exit code, stdout) or None if it could not run in time.
/// Returns as soon as the command exits (no polling interval on the critical path).
pub fn run(cmd: &[&str]) -> Option<(i32, String)> {
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
            Ok(Some(st)) => {
                let text = String::from_utf8(out)
                    .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
                return Some((st.code().unwrap_or(-1), text));
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_micros(500)),
            _ => {
                kill(&mut child);
                return None;
            }
        }
    }
}

pub fn run_out(cmd: &[&str]) -> String {
    run(cmd).map(|(_, o)| o.trim().to_string()).unwrap_or_default()
}

/// Remove ANSI CSI and OSC escape sequences; borrows `s` without allocating when no ESC is present.
pub fn strip_ansi(s: &str) -> Cow<'_, str> {
    if !s.as_bytes().contains(&0x1b) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for n in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC (e.g. hyperlinks): ends with BEL or ESC \
                    chars.next();
                    while let Some(n) = chars.next() {
                        if n == '\u{7}' {
                            break;
                        }
                        if n == '\u{1b}' {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        out.push(c);
    }
    Cow::Owned(out)
}

/// Match a daemon/server status keyword from a log line (preferring reconnect messages as Connected).
pub fn status_from_line(line: &str, reconnect_needle: &str) -> Option<&'static str> {
    if line.contains(reconnect_needle) {
        return Some("Connected");
    }
    ["Connected", "Ready", "Reconnecting", "Connecting", "Disconnected", "Error"]
        .into_iter()
        .find(|s| line.contains(s))
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

/// "YYYY-MM-DDTHH:MM:SS[.frac][Z|±HH:MM|±HHMM]" -> Unix seconds.
pub fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't' | b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let digits = |part: &str| -> Option<i64> {
        if part.is_empty() || !part.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        part.parse::<u32>().ok().map(i64::from)
    };
    let num = |a: usize, n: usize| -> Option<i64> { digits(s.get(a..a + n)?) };
    let (y, mo, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (h, mi, se) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut t = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se;
    let tz = s.get(19..)?.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    if let Some(&sign) = tz.as_bytes().first() {
        if sign == b'+' || sign == b'-' {
            let hh = digits(tz.get(1..3)?)?;
            let rest = tz.get(3..).unwrap_or("");
            let mm = if rest.is_empty() {
                0
            } else {
                let mm_str = rest.strip_prefix(':').unwrap_or(rest);
                if mm_str.len() != 2 {
                    return None;
                }
                digits(mm_str)?
            };
            if hh > 23 || mm > 59 {
                return None;
            }
            let off = hh * 3600 + mm * 60;
            t -= if sign == b'+' { off } else { -off };
        }
    }
    Some(t)
}

/// (local date "YYYY-MM-DD", local midnight as Unix seconds): what `date -d "today 00:00" "+%F %s"`
/// printed, from the C library's time zone rules instead of a process.
pub fn today() -> (String, i64) {
    // `libc` does not expose `tzset` on Linux targets.
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

// ---------- private files & last-good cache ----------

pub fn read_json(p: &Path) -> Option<Value> {
    fs::read(p).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

/// The cache directory, private to the user (0700, also tightened if it already exists wider).
fn private_dir(dir: &Path) -> bool {
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
fn create_temp(tmp: &Path) -> std::io::Result<fs::File> {
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
pub fn write_private(dir: &Path, name: &str, data: &[u8]) {
    if !private_dir(dir) {
        return;
    }
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let ok = create_temp(&tmp).and_then(|mut f| f.write_all(data)).is_ok();
    if !ok || fs::rename(&tmp, dir.join(name)).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

/// Last good value of a live part, for when a later fetch fails (rate limit, timeout, offline,
/// service down): a success is saved with its `fetched_at` (tagged with `account` when known);
/// a failure gets the saved value attached as `stale` only if it belongs to the same account.
pub fn with_last_good(name: &str, mut v: Value, ok: bool, account: Option<&str>) -> Value {
    let Some(dir) = crate::tokens::cache_dir(&home()) else { return v };
    if ok {
        v["fetched_at"] = json!(now());
        let mut to_save = v.clone();
        if let Some(acct) = account {
            to_save["_account"] = json!(acct);
        }
        write_private(&dir, name, to_save.to_string().as_bytes());
    } else if let Some(mut prev) = read_json(&dir.join(name)).filter(|p| p.is_object() && p.get("error").is_none()) {
        let saved_acct = prev.as_object_mut().and_then(|o| o.remove("_account"));
        let same_account = match (saved_acct.as_ref().and_then(Value::as_str), account) {
            (Some(saved), Some(cur)) => saved == cur,
            (Some(_), None) => false,
            (None, _) => true, // legacy cache file without an account tag
        };
        if same_account {
            v["stale"] = prev;
        }
    }
    v
}

// ---------- systemd & journal helpers ----------

pub fn journal_lines(unit: &str, n: &str) -> Vec<String> {
    let out = run_out(&["journalctl", "--user", "-u", unit, "-n", n, "-o", "cat", "-a", "--no-pager"]);
    out.lines().map(|l| strip_ansi(l).trim().to_string()).filter(|l| !l.is_empty()).collect()
}

/// One `systemctl --user show` for all user units: unit -> {property -> value}.
pub fn user_units() -> HashMap<&'static str, HashMap<String, String>> {
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
pub fn sshd_active() -> bool {
    let ev = fs::read_to_string("/sys/fs/cgroup/system.slice/sshd.service/cgroup.events").unwrap_or_default();
    if ev.lines().any(|l| l.trim() == "populated 1") {
        return true;
    }
    run_out(&["systemctl", "is-active", "sshd"]) == "active"
}

pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        // SAFETY: getuid has no preconditions and cannot fail.
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })))
}

/// "1767225600.123456 host ident[pid]: message" (journalctl -o short-unix) -> Unix seconds.
pub fn journal_ts(line: &str) -> Option<i64> {
    let end = line.find(['.', ' '])?;
    line[..end].parse().ok()
}

/// (identifier, message) of a short-unix journal line.
pub fn journal_msg(line: &str) -> Option<(&str, &str)> {
    let rest = line.splitn(3, ' ').nth(2)?;
    let (ident, msg) = rest.split_once(": ")?;
    Some((ident.split('[').next().unwrap_or(ident), msg))
}

/// "14s", "1m 5s", "2h 3m", "800ms" -> whole seconds.
pub fn parse_dur(s: &str) -> Option<i64> {
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
pub fn journal_incremental(unit: &str, midnight: i64, cursor: &mut Option<String>, all: bool) -> Option<(String, bool)> {
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

/// Today's cached state of one incremental journal reader: (hit for today, cursor, state).
pub fn load_day_cache(name: &str, midnight: i64) -> (bool, Option<String>, Value) {
    let p = crate::tokens::cache_dir(&home()).and_then(|d| read_json(&d.join(name)));
    match p {
        Some(p) if p["day"].as_i64() == Some(midnight) => {
            (true, p["cursor"].as_str().map(String::from), p["state"].clone())
        }
        _ => (false, None, Value::Null),
    }
}

pub fn save_day_cache(name: &str, midnight: i64, cursor: &Option<String>, state: Value) {
    if let Some(d) = crate::tokens::cache_dir(&home()) {
        write_private(&d, name, json!({ "day": midnight, "cursor": cursor, "state": state }).to_string().as_bytes());
    }
}

// ---------- single-pass /proc scan (Codex app-servers + interactive agy sessions) ----------

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

#[derive(Default)]
pub struct ProcScan {
    pub codex_managed: bool,
    pub codex_since: Option<i64>,
    pub codex_stray: i32,
    pub agy_total: i32,
    pub agy_connected: i32,
}

/// Single-pass `/proc` scan for both Codex app-servers and interactive `agy` terminal sessions.
/// Filters by `exe` before reading `cmdline` to avoid reading every unrelated desktop process.
pub fn scan_processes() -> ProcScan {
    use std::os::unix::fs::MetadataExt;
    let managed_dir = home().join(".codex/packages/app-server-daemon");
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
        let Ok(exe_link) = fs::read_link(format!("/proc/{pid}/exe")) else { continue };
        let exe = exe_link.to_string_lossy();
        let exe_path = Path::new(exe.strip_suffix(" (deleted)").unwrap_or(&exe));
        let exe_name = exe_path.file_name().and_then(|s| s.to_str());
        let is_agy = exe_name == Some("agy");
        let is_managed = exe_path.starts_with(&managed_dir);
        let is_codex = exe_name == Some("codex");
        if !is_agy && !is_managed && !is_codex {
            continue;
        }
        let Ok(raw) = fs::read(format!("/proc/{pid}/cmdline")) else { continue };
        let args: Vec<&[u8]> = raw.split(|b| *b == 0).filter(|a| !a.is_empty()).collect();
        if args.is_empty() {
            continue;
        }
        if is_agy {
            if let Some(conn) = crate::gemini::agy_interactive_session(&args[1..]) {
                out.agy_total += 1;
                if conn {
                    out.agy_connected += 1;
                }
            }
            continue;
        }
        if args.contains(&&b"app-server"[..]) && !args.iter().any(|a| *a == b"daemon" || *a == b"proxy") {
            if is_managed {
                if args.contains(&&b"--remote-control"[..]) {
                    out.codex_managed = true;
                    out.codex_since = process_start(pid);
                }
            } else if is_codex && args.contains(&&b"--listen"[..]) && args.iter().any(|a| a.starts_with(b"unix://")) {
                out.codex_stray += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_iso_rfc3339_offsets_and_validation() {
        let with_ns_off = parse_iso("2026-11-20T15:18:51.615636742-08:00").unwrap();
        let utc = parse_iso("2026-11-20T23:18:51Z").unwrap();
        let plus_off = parse_iso("2026-11-21T01:18:51.123456789+02:00").unwrap();
        let basic_off = parse_iso("2026-11-21T04:48:51+0530").unwrap();
        let hour_only_off = parse_iso("2026-11-21T01:18:51+02").unwrap();
        assert_eq!(with_ns_off, utc);
        assert_eq!(plus_off, utc);
        assert_eq!(basic_off, utc);
        assert_eq!(hour_only_off, utc);

        assert_eq!(parse_iso("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_iso("2026-11-20T25:00:00Z"), None);
        assert_eq!(parse_iso("2026/11/20T00:00:00Z"), None);
        assert_eq!(parse_iso("2026-+1-20T00:00:00Z"), None);
        assert_eq!(parse_iso("2026-11-20T12:00:00+25:00"), None);
        assert_eq!(parse_iso("2026-11-20T12:00:00+05:xx"), None);
    }

    #[test]
    fn strip_ansi_borrows_plain_and_handles_csi_and_osc() {
        assert!(matches!(strip_ansi("plain text"), Cow::Borrowed("plain text")));
        assert_eq!(strip_ansi("\x1b[32m✔ Connected\x1b[0m"), "✔ Connected");
        assert_eq!(strip_ansi("a\x1b[15~b\x1b]8;;https://x\x07link\x1b]8;;\x1b\\"), "ablink");
    }
}
