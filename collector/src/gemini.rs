//! Gemini (Google Antigravity CLI) credentials, live quota windows, daemon status and drops.

use crate::system::{
    env_on, is_offline, journal_incremental, journal_ts, load_day_cache, now, parse_dur, parse_iso, run,
    save_day_cache, status_from_line, strip_ansi, trace, with_last_good, GEMINI_UNIT, TIMEOUT,
};
use serde_json::{json, Value};
use std::thread;
use std::time::{Duration, Instant};

/// Live Gemini quota limits are on by default; turn off with `{"gemini_live_limits": false}`.
pub fn gemini_live_enabled(cfg: Option<&Value>) -> bool {
    cfg.and_then(|c| c["gemini_live_limits"].as_bool()) != Some(false)
}

pub fn gemini_unreachable() -> Value {
    json!({ "error": if is_offline() { "offline (AGENT_USAGE_OFFLINE)" } else { "Could not reach Google Cloud Code (offline?)" } })
}

pub enum GeminiCreds {
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

pub fn load_gemini_creds(settings: Option<&Value>, with_token: bool) -> GeminiCreds {
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

pub fn gemini_account(creds: &GeminiCreds, last_snap: Option<&Value>) -> Value {
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

/// Security invariant: never send Bearer token over non-loopback HTTP (rejects RFC 3986 userinfo).
fn is_valid_gemini_base_url(u: &str) -> bool {
    if u.starts_with("https://") {
        return true;
    }
    if let Some(rest) = u.strip_prefix("http://127.0.0.1:") {
        let port = rest.split('/').next().unwrap_or("");
        return !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit());
    }
    false
}

fn gemini_base_url() -> String {
    if let Ok(u) = std::env::var("AGENT_USAGE_GEMINI_URL") {
        let u = u.trim_end_matches('/');
        if is_valid_gemini_base_url(u) {
            return u.to_string();
        }
    }
    "https://daily-cloudcode-pa.googleapis.com".to_string()
}

fn has_token(s: &str, needles: &[&str]) -> bool {
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|tok| !tok.is_empty() && needles.iter().any(|n| tok.eq_ignore_ascii_case(n)))
}

fn gemini_tier_plan(tier: &Value) -> Option<&'static str> {
    let obj = tier.as_object()?;
    let id = obj.get("id").and_then(Value::as_str).unwrap_or("");
    let name = obj.get("name").and_then(Value::as_str).unwrap_or("");
    ["ultra", "enterprise", "standard", "free", "pro"]
        .into_iter()
        .find(|&plan| has_token(id, &[plan]) || has_token(name, &[plan]))
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

fn gemini_bucket_used_pct(b: &Value, resets_at: Option<i64>) -> Option<f64> {
    if let Some(rem) = b["remainingFraction"].as_f64().filter(|x| x.is_finite()) {
        return Some(((1.0 - rem.clamp(0.0, 1.0)) * 1000.0).round() / 10.0);
    }
    // Proto3 JSON omits zero-valued `remainingFraction` when a bucket is exhausted (0% left).
    let positive_remaining = b["remainingAmount"]
        .as_str()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .or_else(|| b["remainingAmount"].as_f64())
        .is_some_and(|n| n.is_finite() && n > 0.0);
    if resets_at.is_some() && !positive_remaining {
        return Some(100.0);
    }
    None
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
        let resets_at = b["resetTime"].as_str().and_then(parse_iso);
        let Some(used_pct) = gemini_bucket_used_pct(b, resets_at) else { continue };
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

/// Versioned Antigravity User-Agent required by `retrieveUserQuotaSummary` to include first-party
/// Gemini buckets. If Google raises the minimum client version, update this string or first-party
/// Gemini buckets may be omitted by the summary endpoint.
const GEMINI_USER_AGENT: &str = "antigravity/hub/2.24.0 (aidev_client; os_type=linux; arch=x64; cl=963137146)";

enum GeminiHttpErr {
    Status(u16),
    Network,
    BadJson,
}

fn gemini_post(url: &str, auth: &str, body: Value, deadline: Instant) -> Result<Value, GeminiHttpErr> {
    let rem = deadline.saturating_duration_since(Instant::now());
    if rem.is_zero() {
        return Err(GeminiHttpErr::Network);
    }
    match ureq::post(url)
        .set("Authorization", auth)
        .set("Content-Type", "application/json")
        .set("User-Agent", GEMINI_USER_AGENT)
        .timeout(rem.min(Duration::from_secs(8)))
        .send_json(body)
    {
        Ok(r) => r.into_json().map_err(|_| GeminiHttpErr::BadJson),
        Err(ureq::Error::Status(code, _)) => Err(GeminiHttpErr::Status(code)),
        Err(_) => Err(GeminiHttpErr::Network),
    }
}

fn gemini_http_error(err: GeminiHttpErr) -> Value {
    let msg = match err {
        GeminiHttpErr::Status(401 | 403) => "Login token rejected. Use any Antigravity session and it renews.".into(),
        GeminiHttpErr::Status(429) => "Google is rate-limiting quota checks. Try Refresh again in a minute.".into(),
        GeminiHttpErr::Status(code) => format!("Quota endpoint returned HTTP {code}"),
        GeminiHttpErr::BadJson => "Unreadable quota response".into(),
        GeminiHttpErr::Network => "Could not reach Google Cloud Code (offline?)".into(),
    };
    json!({ "error": msg })
}

fn gemini_project_id(lca: &Value) -> Option<&str> {
    let p = &lca["cloudaicompanionProject"];
    p.as_str()
        .or_else(|| p["id"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn match_window_cadence(s: &str) -> Option<bool> {
    let mut prev: Option<&str> = None;
    for tok in s.split(|c: char| !c.is_ascii_alphanumeric()).filter(|t| !t.is_empty()) {
        if tok.eq_ignore_ascii_case("5h")
            || (tok.eq_ignore_ascii_case("hour")
                && prev.is_some_and(|p| p.eq_ignore_ascii_case("five") || p == "5"))
        {
            return Some(false);
        }
        if tok.eq_ignore_ascii_case("weekly")
            || tok.eq_ignore_ascii_case("168h")
            || tok.eq_ignore_ascii_case("7d")
            || (tok.eq_ignore_ascii_case("day")
                && prev.is_some_and(|p| p.eq_ignore_ascii_case("seven") || p == "7"))
        {
            return Some(true);
        }
        prev = Some(tok);
    }
    None
}

fn summary_bucket_is_cg(bid: &str, group_hint: &str) -> Option<bool> {
    const CG: &[&str] = &["3p", "claude", "gpt", "third", "thirdparty"];
    if has_token(bid, CG) {
        Some(true)
    } else if has_token(bid, &["gemini"]) {
        Some(false)
    } else if has_token(group_hint, CG) {
        Some(true)
    } else if has_token(group_hint, &["gemini"]) {
        Some(false)
    } else {
        None
    }
}

fn summary_bucket_is_weekly(b: &Value, bid: &str, resets_at: Option<i64>, now_secs: i64) -> Option<bool> {
    if let Some(w) = b["window"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(is_wk) = match_window_cadence(w) {
            return Some(is_wk);
        }
        // Explicit cadences like "daily" or "monthly" shouldn't silently merge into Weekly.
        if !has_token(w, &["unspecified", "unknown"]) {
            return None;
        }
    }
    match_window_cadence(bid)
        .or_else(|| b["displayName"].as_str().and_then(match_window_cadence))
        .or_else(|| resets_at.map(|r| r - now_secs > 6 * 3600))
}

/// Untouched summary buckets (`remainingFraction == 1.0`, no description) report a rolling
/// full-window `resetTime` (`now + 5h` or `now + 7d`). Drop it so the UI shows "Window not started".
fn summary_reset_time(b: &Value, weekly: bool, resets_at: Option<i64>, requested_at: i64) -> Option<i64> {
    let r = resets_at?;
    let no_description = b["description"].as_str().is_none_or(|s| s.trim().is_empty());
    let span = if weekly { 7 * 86_400 } else { 5 * 3600 };
    let untouched = b["remainingFraction"].as_f64() == Some(1.0)
        && no_description
        && (r - requested_at - span).abs() <= 60;
    if untouched { None } else { Some(r) }
}

/// Parse `v1internal:retrieveUserQuotaSummary`, which exposes both 5-hour and weekly buckets
/// per model group (Gemini vs. Claude & GPT). Emits each group's 5-hour then weekly ring.
fn gemini_summary_windows(summary: &Value, requested_at: i64) -> Vec<Value> {
    let mut gem_5h: Option<WinAgg> = None;
    let mut gem_wk: Option<WinAgg> = None;
    let mut cg_5h: Option<WinAgg> = None;
    let mut cg_wk: Option<WinAgg> = None;

    let mut visit = |group_hint: &str, b: &Value| {
        if b["disabled"].as_bool() == Some(true) {
            return;
        }
        let bid = b["bucketId"].as_str().unwrap_or("");
        if bid.as_bytes().get(..4).is_some_and(|p| p.eq_ignore_ascii_case(b"tab_")) {
            return;
        }
        let Some(is_cg) = summary_bucket_is_cg(bid, group_hint) else { return };
        let raw_reset = b["resetTime"].as_str().and_then(parse_iso);
        let Some(pct) = gemini_bucket_used_pct(b, raw_reset) else { return };
        let Some(weekly) = summary_bucket_is_weekly(b, bid, raw_reset, requested_at) else { return };
        let resets_at = summary_reset_time(b, weekly, raw_reset, requested_at);
        let slot = match (is_cg, weekly) {
            (false, false) => &mut gem_5h,
            (false, true) => &mut gem_wk,
            (true, false) => &mut cg_5h,
            (true, true) => &mut cg_wk,
        };
        WinAgg::add(slot, pct, resets_at);
    };

    if let Some(groups) = summary["groups"]
        .as_array()
        .filter(|a| !a.is_empty())
        .or_else(|| summary["quotaGroups"].as_array().filter(|a| !a.is_empty()))
    {
        for g in groups {
            let hint = format!(
                "{} {}",
                g["modelGroup"].as_str().unwrap_or(""),
                g["displayName"].as_str().unwrap_or(""),
            );
            for b in g["buckets"].as_array().into_iter().flatten() {
                visit(&hint, b);
            }
        }
    } else if let Some(buckets) = summary["buckets"].as_array() {
        for b in buckets {
            visit("", b);
        }
    }

    let has_cg = cg_5h.is_some() || cg_wk.is_some();
    let mut out = Vec::new();
    for (span, slot) in [("5-hour", gem_5h), ("Weekly", gem_wk)] {
        if let Some(w) = slot {
            let label = if has_cg { format!("{span} · Gemini") } else { span.to_string() };
            out.push(json!({ "label": label, "pct": w.pct, "resets_at": w.resets_at }));
        }
    }
    for (span, slot) in [("5-hour", cg_5h), ("Weekly", cg_wk)] {
        if let Some(w) = slot {
            out.push(json!({ "label": format!("{span} · Claude & GPT"), "pct": w.pct, "resets_at": w.resets_at }));
        }
    }
    out
}

pub fn gemini_usage(creds: &GeminiCreds, live: bool) -> Value {
    match creds {
        GeminiCreds::ApiKey => json!({ "windows": [], "credits": Value::Null }),
        GeminiCreds::SignedOut => {
            if !live {
                json!({ "off": true })
            } else {
                // Signed out: never attach a previous account's stale limits/plan.
                json!({ "error": "Not signed in to Gemini (Antigravity)" })
            }
        }
        GeminiCreds::OAuth { email, expiry, access_token, .. } => {
            if !live {
                return json!({ "off": true });
            }
            let v = gemini_usage_live(*expiry, access_token.as_deref());
            let ok = v.get("windows").is_some();
            with_last_good("gemini-usage-last.json", v, ok, email.as_deref())
        }
    }
}

fn gemini_usage_live(expiry: Option<i64>, access_token: Option<&str>) -> Value {
    if expiry.is_some_and(|exp| exp < now()) {
        return json!({ "error": "Login token expired. Use any Antigravity session and it renews." });
    }
    let Some(token) = access_token else {
        return json!({ "error": "Not signed in to Gemini (Antigravity)" });
    };
    if is_offline() {
        return json!({ "error": "offline (AGENT_USAGE_OFFLINE)" });
    }
    let base = gemini_base_url();
    let auth = format!("Bearer {token}");
    let summary_url = format!("{base}/v1internal:retrieveUserQuotaSummary");
    let deadline = Instant::now() + TIMEOUT;
    let mut requested_at = now();
    let (lca_res, sum_res) = thread::scope(|s| {
        let lca = s.spawn(|| {
            gemini_post(
                &format!("{base}/v1internal:loadCodeAssist"),
                &auth,
                json!({
                    "metadata": {
                        "ideType": "IDE_UNSPECIFIED",
                        "platform": "PLATFORM_UNSPECIFIED",
                        "pluginType": "GEMINI",
                    }
                }),
                deadline,
            )
        });
        let sum = gemini_post(&summary_url, &auth, json!({}), deadline);
        (lca.join().ok(), sum)
    });
    trace("gemini usage: responses");

    let lca = lca_res.as_ref().and_then(|r| r.as_ref().ok());
    let lca_auth_rejected = matches!(lca_res, Some(Err(GeminiHttpErr::Status(401 | 403))));
    let project = lca.and_then(gemini_project_id);

    // Some accounts/backends require `project` in the retrieveUserQuotaSummary body.
    let sum_res = match (sum_res, project) {
        (Err(GeminiHttpErr::Status(400)), Some(p)) => {
            trace("gemini usage: summary 400 retry with project");
            requested_at = now();
            gemini_post(&summary_url, &auth, json!({ "project": p }), deadline)
        }
        (other, _) => other,
    };

    let fetch_fallback = || {
        trace("gemini usage: fallback retrieveUserQuota");
        let body = project.map(|p| json!({ "project": p })).unwrap_or_else(|| json!({}));
        gemini_post(&format!("{base}/v1internal:retrieveUserQuota"), &auth, body, deadline)
            .map(|ruq| gemini_windows(&ruq, now()))
    };

    let windows = match sum_res {
        Ok(sum) => {
            let w = gemini_summary_windows(&sum, requested_at);
            if !w.is_empty() {
                w
            } else {
                match fetch_fallback() {
                    Ok(fb) => fb,
                    Err(err @ (GeminiHttpErr::Status(401 | 429 | 500..=599) | GeminiHttpErr::Network)) => {
                        return gemini_http_error(err);
                    }
                    Err(_) => w,
                }
            }
        }
        Err(err @ (GeminiHttpErr::Status(401 | 429) | GeminiHttpErr::Network)) => {
            return gemini_http_error(err);
        }
        Err(GeminiHttpErr::Status(403)) if lca_auth_rejected => {
            return gemini_http_error(GeminiHttpErr::Status(403));
        }
        Err(_) => match fetch_fallback() {
            Ok(fb) => fb,
            Err(err) => return gemini_http_error(err),
        },
    };

    let plan = lca.and_then(gemini_plan);
    let credits = lca.map(gemini_credits).unwrap_or(Value::Null);
    json!({
        "windows": windows,
        "credits": credits,
        "plan": plan,
    })
}

pub fn gemini_daemon(active: bool, since: Option<i64>, lines: &[String]) -> Value {
    let mut state = None;
    let mut instance_name = None;
    let mut link = None;
    for l in lines.iter().rev() {
        if state.is_none() {
            state = status_from_line(l, "Reconnected").map(String::from);
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
                link = l[i..].split_whitespace().next().map(String::from);
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
pub fn gemini_drops(midnight: i64) -> Value {
    let (cached, mut cursor, state) = load_day_cache(GEMINI_DROPS_FILE, midnight);
    let prev_cursor = cursor.clone();
    let mut events: Vec<(i64, i64)> = state["events"]
        .as_array()
        .or_else(|| state.as_array())
        .map(|a| a.iter().filter_map(|e| Some((e[0].as_i64()?, e[1].as_i64()?))).collect())
        .unwrap_or_default();
    let mut disc_at: Option<i64> = state["disc_at"].as_i64();
    let prev_disc = disc_at;
    let Some((out, rescanned)) = journal_incremental(GEMINI_UNIT, midnight, &mut cursor, true) else {
        return Value::Null;
    };
    trace(format!("gemini drops: {} read", if rescanned { "full-day" } else { "incremental" }));
    if rescanned {
        events.clear();
        disc_at = None;
    }
    let before = events.len();
    for line in out.lines() {
        let Some(at) = journal_ts(line) else { continue };
        if at < midnight {
            continue;
        }
        let clean = strip_ansi(line);
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
    if !cached || rescanned || cursor != prev_cursor || events.len() != before || disc_at != prev_disc {
        save_day_cache(GEMINI_DROPS_FILE, midnight, &cursor, json!({ "events": events, "disc_at": disc_at }));
    }
    json!(events.iter().map(|(at, secs)| json!({ "at": at, "secs": secs })).collect::<Vec<_>>())
}

/// Classify an `agy` process's arguments (after argv[0]):
/// `Some(connected)` for an interactive terminal session (`connected` = `--remote-control` present),
/// `None` for non-interactive flags (`-p`, `--print`, `--prompt`, `-h`, `--help`, `-v`, `--version`)
/// or CLI subcommands (`agy models`, `agy remote-control`, `agy mcp`, ...).
pub fn agy_interactive_session(args: &[&[u8]]) -> Option<bool> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agy_cli_session_classification() {
        let s = |v: &[&[u8]]| agy_interactive_session(v);
        assert_eq!(s(&[]), Some(false));
        assert_eq!(s(&[b"--remote-control"]), Some(true));
        assert_eq!(s(&[b"-m", b"gemini-4-carbon", b"--remote-control"]), Some(true));
        assert_eq!(s(&[b"--model", b"models"]), Some(false));
        assert_eq!(s(&[b"fix the tests"]), Some(false));
        assert_eq!(s(&[b"--", b"models"]), None);
        assert_eq!(s(&[b"models"]), None);
        assert_eq!(s(&[b"remote-control", b"start"]), None);
        assert_eq!(s(&[b"mcp", b"list"]), None);
        assert_eq!(s(&[b"-p", b"hello"]), None);
        assert_eq!(s(&[b"--prompt=hello"]), None);
        assert_eq!(s(&[b"--help"]), None);
        assert_eq!(s(&[b"-v"]), None);
    }

    #[test]
    fn gemini_base_url_rejects_userinfo_and_non_loopback_http() {
        assert!(is_valid_gemini_base_url("https://daily-cloudcode-pa.googleapis.com"));
        assert!(is_valid_gemini_base_url("http://127.0.0.1:41234"));
        assert!(is_valid_gemini_base_url("http://127.0.0.1:41234/api"));
        assert!(!is_valid_gemini_base_url("http://127.0.0.1:80@evil.com"));
        assert!(!is_valid_gemini_base_url("http://127.0.0.1:"));
        assert!(!is_valid_gemini_base_url("http://evil.com"));
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
        assert_eq!(w2[0]["resets_at"], parse_iso(t_2h).unwrap());
        assert_eq!(w2[1]["pct"], 10.0);
        assert_eq!(w2[2]["pct"], 20.0);
    }

    #[test]
    fn gemini_summary_windows_both_groups_and_edge_cases() {
        let base = parse_iso("2026-11-22T12:00:00Z").unwrap();
        let t_2h = "2026-11-22T14:00:00Z";
        let t_3h = "2026-11-22T15:00:00Z";
        let t_5h = "2026-11-22T17:00:02Z"; // untouched rolling 5h reset (within ±60s)
        let t_1d = "2026-11-23T12:00:00Z";
        let t_5d = "2026-11-27T12:00:00Z";
        let t_7d = "2026-11-29T11:59:58Z"; // untouched rolling 7d reset (within ±60s)

        let full = json!({
            "groups": [
                {
                    "displayName": "Gemini Models",
                    "buckets": [
                        { "bucketId": "gemini-weekly", "window": "weekly", "remainingFraction": 0.541, "resetTime": t_5d },
                        { "bucketId": "gemini-5h", "window": "5h", "remainingFraction": 0.903, "resetTime": t_3h },
                        { "bucketId": "gemini-disabled", "window": "5h", "remainingFraction": 0.0, "disabled": true, "resetTime": t_3h }
                    ]
                },
                {
                    "displayName": "Claude and GPT models",
                    "buckets": [
                        { "bucketId": "3p-weekly", "window": "weekly", "resetTime": t_5d },
                        { "bucketId": "3p-5h", "window": "5h", "remainingFraction": 1.0, "description": "", "resetTime": t_5h }
                    ]
                }
            ]
        });
        let w = gemini_summary_windows(&full, base);
        let labels: Vec<_> = w.iter().map(|x| x["label"].as_str().unwrap()).collect();
        assert_eq!(
            labels,
            ["5-hour · Gemini", "Weekly · Gemini", "5-hour · Claude & GPT", "Weekly · Claude & GPT"]
        );
        assert_eq!(w[0]["pct"], 9.7);
        assert_eq!(w[0]["resets_at"], parse_iso(t_3h).unwrap());
        assert_eq!(w[1]["pct"], 45.9);
        assert_eq!(w[1]["resets_at"], parse_iso(t_5d).unwrap());
        assert_eq!(w[2]["pct"], 0.0);
        assert!(w[2]["resets_at"].is_null());
        assert_eq!(w[3]["pct"], 100.0);
        assert_eq!(w[3]["resets_at"], parse_iso(t_5d).unwrap());

        let quota_groups = json!({
            "groups": [],
            "quotaGroups": [
                {
                    "modelGroup": "MODEL_GROUP_GEMINI_3PRO",
                    "displayName": "Gemini 3Pro",
                    "buckets": [
                        { "bucketId": "b1", "window": "QUOTA_WINDOW_FIVE_HOUR", "remainingFraction": 0.80, "resetTime": t_3h },
                        { "bucketId": "b2", "window": "5-hour", "remainingFraction": 0.60, "resetTime": t_2h },
                        { "bucketId": "b3", "window": "daily", "remainingFraction": 0.10, "resetTime": t_1d }
                    ]
                },
                {
                    "displayName": "Claude and GPT",
                    "buckets": [
                        { "bucketId": "3p-weekly", "window": "weekly", "remainingAmount": "50", "resetTime": t_5d },
                        { "bucketId": "3p-5h", "window": "5h", "remainingFraction": 1.0, "description": "Active window", "resetTime": t_5h }
                    ]
                }
            ]
        });
        let wq = gemini_summary_windows(&quota_groups, base);
        let labelsq: Vec<_> = wq.iter().map(|x| x["label"].as_str().unwrap()).collect();
        assert_eq!(labelsq, ["5-hour · Gemini", "5-hour · Claude & GPT"]);
        assert_eq!(wq[0]["pct"], 40.0);
        assert_eq!(wq[0]["resets_at"], parse_iso(t_2h).unwrap());
        assert_eq!(wq[1]["pct"], 0.0);
        assert_eq!(wq[1]["resets_at"], parse_iso(t_5h).unwrap());

        let gem_only = json!({
            "buckets": [
                { "bucketId": "antigravity_gemini_weekly", "window": "WEEKLY", "remainingFraction": 1.0, "description": null, "resetTime": t_7d },
                { "bucketId": "antigravity_gemini_5h", "window": "QUOTA_WINDOW_UNSPECIFIED", "remainingFraction": 1.0, "resetTime": t_3h },
                { "bucketId": "tab_autocomplete", "window": "5h", "remainingFraction": 0.1, "resetTime": t_3h }
            ]
        });
        let w2 = gemini_summary_windows(&gem_only, base);
        let labels2: Vec<_> = w2.iter().map(|x| x["label"].as_str().unwrap()).collect();
        assert_eq!(labels2, ["5-hour", "Weekly"]);
        assert_eq!(w2[0]["pct"], 0.0);
        assert_eq!(w2[0]["resets_at"], parse_iso(t_3h).unwrap());
        assert_eq!(w2[1]["pct"], 0.0);
        assert!(w2[1]["resets_at"].is_null());

        assert_eq!(gemini_project_id(&json!({ "cloudaicompanionProject": "proj-a" })), Some("proj-a"));
        assert_eq!(gemini_project_id(&json!({ "cloudaicompanionProject": { "id": "proj-b" } })), Some("proj-b"));
        assert_eq!(gemini_project_id(&json!({ "cloudaicompanionProject": "  " })), None);
    }
}
