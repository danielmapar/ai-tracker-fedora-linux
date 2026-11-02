// Agent Usage: top-bar UI (GNOME Shell 50, GJS ES module). Build v3.
//
// Loader contract: the extension's tiny extension.js imports this file and calls
//     createIndicator(extension)  ->  PanelMenu.Button instance
// and adds it to the panel itself.
//
// Data comes from `~/.local/bin/agent-usage --stream`, which prints newline-delimited snapshots:
// the first right after the local checks (~15 ms) with "pending": [parts still loading], then one
// per part as it lands (Claude limits ~0.2 s, Codex ~0.4 s). Each frame is drawn as it arrives;
// parts still loading show their last saved values, dimmed. It runs ONLY when the menu opens, when
// the user clicks Refresh, or a few seconds after the user clicks a fix button; never in the
// background. At startup the last complete snapshot (~/.cache/agent-usage/last-snapshot.json,
// written by the collector) is read once from disk, so the badge and the first open are instant.
// The only timers are UI timers: a 30 s countdown-text tick while the menu is open, a watchdog
// that force-exits a hung collector, and short one-shot timers for button feedback.

import Cairo from 'cairo';
import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import Gio from 'gi://Gio';
import Pango from 'gi://Pango';
import St from 'gi://St';

import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

const HOME = GLib.get_home_dir();
const COLLECTOR = GLib.build_filenamev([HOME, '.local', 'bin', 'agent-usage']);
const CODEX_BIN = GLib.build_filenamev([HOME, '.local', 'bin', 'codex']);
const SNAPSHOT = GLib.build_filenamev([GLib.get_user_cache_dir(), 'agent-usage', 'last-snapshot.json']);
const COLLECTOR_TIMEOUT_S = 25; // the collector has its own 10 s timeouts; this only catches a hang
const CLOCK_TICK_S = 30; // countdown text refresh while the menu is open (no fetching)
const DIM = 150; // opacity of a stale snapshot while it is being refreshed (of 255)
const RECHECK_MS = 2500; // after a fix button's command finished: one re-check (user-initiated)
const CONFIRM_MS = 4000; // how long a two-step button stays armed

const CLAUDE_UNIT = 'claude-remote-control.service';
const CODEX_UNIT = 'codex-remote-control.service'; // the watchdog: starts/repairs the Codex service
const CODEX_TIMER = 'codex-remote-control.timer';

// Unique GType names per module instance, so a loader that re-imports this file (cache-busting
// URL after an update) never hits "type name already registered".
const UID = GLib.uuid_string_random().replaceAll('-', '').slice(0, 12);

const TABS = [
    {
        key: 'claude', name: 'Claude', iconFile: 'claude-symbolic.svg',
        fallbackIcons: ['starred-symbolic'], login: ['claude', 'auth', 'login'],
    },
    {
        key: 'codex', name: 'Codex', iconFile: 'codex-symbolic.svg',
        fallbackIcons: ['utilities-terminal-symbolic'], login: ['codex', 'login'],
    },
];
const PANEL_ICON = {
    iconFile: 'agent-usage-symbolic.svg',
    fallbackIcons: ['org.gnome.SystemMonitor-symbolic', 'power-profile-performance-symbolic'],
};

const ALIGN = Clutter.ActorAlign;

// The sections of a tab, top to bottom. Each one is rebuilt only when its fingerprint (the data it
// shows) changes: a streamed frame that brings in one part rebuilds one card, and switching tabs
// rebuilds nothing.
const SECTIONS = ['banner', 'account', 'limits', 'tokens', 'remote'];
const EASE_OUT = Clutter.AnimationMode.EASE_OUT_CUBIC;

// ---------------------------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------------------------

const nowS = () => Date.now() / 1000;
const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));
const num = v => (v === null || v === undefined || v === '' ? NaN : Number(v));

function fmtTokens(n) {
    n = num(n);
    if (!Number.isFinite(n))
        return '–';
    const a = Math.abs(n);
    if (a >= 1e12)
        return `${(n / 1e12).toFixed(2)}T`;
    if (a >= 1e9)
        return `${(n / 1e9).toFixed(2)}B`;
    if (a >= 1e6)
        return `${(n / 1e6).toFixed(1)}M`;
    if (a >= 1e3)
        return `${(n / 1e3).toFixed(1)}k`;
    return `${Math.round(n)}`;
}

function fmtCount(n) {
    try {
        return Math.round(n).toLocaleString();
    } catch {
        return `${Math.round(n)}`;
    }
}

function fmtDuration(secs) {
    const s = Math.max(0, Math.round(secs));
    if (s < 60)
        return '<1m';
    const m = Math.floor(s / 60);
    const h = Math.floor(m / 60);
    const d = Math.floor(h / 24);
    if (d >= 1)
        return h % 24 ? `${d}d ${h % 24}h` : `${d}d`;
    if (h >= 1)
        return m % 60 ? `${h}h ${m % 60}m` : `${h}h`;
    return `${m}m`;
}

function fmtClock(epoch) {
    if (!epoch)
        return '';
    const t = GLib.DateTime.new_from_unix_local(Math.floor(epoch));
    const now = GLib.DateTime.new_now_local();
    if (!t || !now)
        return '';
    if (t.format('%F') === now.format('%F'))
        return t.format('%H:%M');
    if (Math.abs(epoch - nowS()) < 6 * 86400)
        return t.format('%a %H:%M');
    return t.format('%b %-d, %H:%M');
}

// Short outage lengths: "3s", "14s", "2m", "1h 5m".
const fmtSecs = s => (s < 60 ? `${Math.max(0, Math.round(s))}s` : fmtDuration(s));

function localMidnight() {
    const n = GLib.DateTime.new_now_local();
    return GLib.DateTime.new_local(n.get_year(), n.get_month(), n.get_day_of_month(), 0, 0, 0).to_unix();
}

// Text that comes from outside (API/RPC error messages, journal lines) is shown at most this long.
const clip = (v, n = 240) => {
    const t = `${v ?? ''}`;
    return t.length > n ? `${t.slice(0, n - 1)}…` : t;
};

// The only links the widget opens or copies: https://claude.ai/...
function claudeLink(v) {
    if (typeof v !== 'string' || v.length > 512)
        return null;
    try {
        const u = GLib.Uri.parse(v, GLib.UriFlags.NONE);
        return u.get_scheme() === 'https' && u.get_host() === 'claude.ai' ? v : null;
    } catch {
        return null;
    }
}

const titleCase = s => `${s ?? ''}`.replace(/_/g, ' ').replace(/(^|\s|-)\w/g, m => m.toUpperCase());

const PLAN_NAMES = {
    ultra: 'Ultra', max: 'Max', pro: 'Pro', prolite: 'Pro Lite', plus: 'Plus', team: 'Team', enterprise: 'Enterprise',
    free: 'Free', business: 'Business', edu: 'Edu', go: 'Go',
};

function planName(plan) {
    if (!plan)
        return null;
    const k = `${plan}`.toLowerCase().replace(/[\s_-]/g, '').replace(/^claude/, '');
    return PLAN_NAMES[k] ?? titleCase(plan);
}

const AUTH_LABELS = {'claude.ai': 'claude.ai login', chatgpt: 'ChatGPT login', apikey: 'API key'};

function authLabel(method) {
    if (!method)
        return null;
    const k = `${method}`.toLowerCase().replace(/[\s_-]/g, '');
    return AUTH_LABELS[k] ?? `${method}`;
}

const tildify = p => (p.startsWith(HOME) ? `~${p.slice(HOME.length)}` : p);
const plural = (n, one, many = `${one}s`) => (n === 1 ? one : many);

// ---------------------------------------------------------------------------------------------
// Model: turn one collector snapshot into what each tab shows. Pure JS, no actors.
// ---------------------------------------------------------------------------------------------

const RANK = {ok: 0, idle: 0, warn: 1, bad: 2};
const LEVELS = ['ok', 'warn', 'bad', 'idle'];
const CHIP_TEXT = {ok: 'Healthy', warn: 'Attention', bad: 'Down', idle: 'Idle'};
const worst = levels => levels.reduce((a, b) => (RANK[b] > RANK[a] ? b : a), 'ok');
const pctLevel = pct => (pct >= 90 ? 'bad' : pct >= 70 ? 'warn' : 'ok');

// Length of a limit window, from the label the collector gives it ("5-hour", "Weekly · Opus",
// "3-day", "90-minute" ...). Used for the pace marker and the "full in" projection.
function windowSeconds(label) {
    const l = `${label ?? ''}`.toLowerCase();
    if (l.startsWith('weekly'))
        return 7 * 86400;
    const m = /^(\d+)-(minute|hour|day)/.exec(l);
    if (!m)
        return null;
    return Number(m[1]) * {minute: 60, hour: 3600, day: 86400}[m[2]];
}

function paceInfo(w, now) {
    const span = windowSeconds(w.label);
    if (!span || !w.resets_at)
        return null;
    const left = w.resets_at - now;
    if (left <= 0 || left > span * 1.05)
        return null;
    const elapsed = clamp(1 - left / span, 0, 1);
    const info = {elapsed, left, projected: null, fullIn: null};
    if (elapsed >= 0.08 && w.pct > 0 && w.pct < 100) {
        info.projected = w.pct / elapsed;
        if (info.projected >= 100) {
            const perSecond = w.pct / (elapsed * span);
            info.fullIn = (100 - w.pct) / perSecond;
        }
    }
    return info;
}

function paceText(w, p) {
    if (w.pct >= 100 || !p || p.projected === null)
        return null;
    if (p.fullIn !== null) {
        return {
            level: 'warn',
            text: `At this pace: full in ${fmtDuration(p.fullIn)}`,
            short: `full in ~${fmtDuration(p.fullIn)}`,
        };
    }
    return {level: 'ok', text: `On pace for ~${Math.round(p.projected)}% by reset`, short: null};
}

function resetText(w) {
    if (!w.resets_at)
        return w.pct > 0 ? 'Reset time unknown' : 'Window not started';
    const left = w.resets_at - nowS();
    if (left <= 0)
        return 'Reset due · refresh';
    return `Resets in ${fmtDuration(left)}`;
}

function accountModel(key, acc) {
    acc = acc && typeof acc === 'object' ? acc : {};
    if (acc.logged_in) {
        const method = authLabel(key === 'claude' ? acc.auth_method : acc.type);
        const org = acc.org && acc.org !== acc.email ? `${acc.org}` : null;
        return {
            loggedIn: true,
            title: acc.email ? `${acc.email}` : 'Signed in',
            plan: planName(acc.plan),
            meta: [org, method].filter(Boolean).join(' · '),
        };
    }
    if (acc.error)
        return {loggedIn: false, title: 'Account status unavailable', plan: null, meta: clip(acc.error)};
    return {
        loggedIn: false,
        title: 'Not signed in',
        plan: null,
        meta: key === 'claude' ? 'Log in to Claude Code to see your limits' : 'Log in to Codex to see your limits',
    };
}

function usageModel(u) {
    if (!u || typeof u !== 'object')
        return {error: 'This snapshot has no usage data', windows: [], extras: [], limitReached: null, missing: true};
    if (u.off) // live Claude limits are opt-in (see the README)
        return {error: null, windows: [], extras: [], limitReached: null, off: true};
    if (u.error) {
        // The collector returns the last successful numbers under `stale` when a live fetch fails
        // (e.g. claude.ai rate-limiting). Show them, clearly marked as saved.
        const s = u.stale;
        if (s && typeof s === 'object' && Array.isArray(s.windows) && s.windows.length) {
            const m = usageModel({...s, error: undefined, stale: undefined});
            const at = num(s.fetched_at);
            const when = (Number.isFinite(at) && GLib.DateTime.new_from_unix_local(at)?.format('%H:%M')) || 'earlier';
            return {...m, staleAt: when, staleNote: clip(u.error)};
        }
        return {error: clip(u.error), windows: [], extras: [], limitReached: null};
    }
    const windows = (Array.isArray(u.windows) ? u.windows : [])
        .filter(w => w && Number.isFinite(num(w.pct)))
        .map(w => ({
            label: `${w.label ?? 'Limit'}`,
            pct: Math.max(0, num(w.pct)),
            resets_at: Number.isFinite(num(w.resets_at)) ? num(w.resets_at) : null,
        }));
    const extras = [];
    if (u.extra_usage && typeof u.extra_usage === 'object') {
        const pct = Number.isFinite(num(u.extra_usage.pct)) ? num(u.extra_usage.pct) : 0;
        extras.push({kind: 'bar', label: 'Extra usage', pct, value: `${Math.round(pct)}% of monthly`});
    }
    if (u.credits === 'unlimited')
        extras.push({kind: 'value', label: 'Credits', value: 'Unlimited'});
    else if (Number.isFinite(num(u.credits)))
        extras.push({kind: 'value', label: 'Credits', value: fmtCount(num(u.credits))});
    return {error: null, windows, extras, limitReached: u.limit_reached ? `${u.limit_reached}` : null};
}

function tokensModel(key, t) {
    if (!t || typeof t !== 'object')
        return null;
    // A failed fetch (slow or unreachable service) carries the last good totals under `stale`.
    let staleAt = null;
    if (t.error) {
        const s = t.stale;
        if (!s || typeof s !== 'object')
            return null;
        const at = num(s.fetched_at);
        staleAt = (Number.isFinite(at) && GLib.DateTime.new_from_unix_local(at)?.format('%H:%M')) || 'earlier';
        // A saved "today" from an earlier day is not today's count.
        t = Number.isFinite(at) && at < localMidnight() ? {...s, today: null} : s;
    }
    if (key === 'claude') {
        const total = num(t.today_total);
        if (!Number.isFinite(total))
            return null;
        const out = num(t.today_output) || 0;
        const fresh = num(t.today_uncached_input) || 0;
        const cached = total > 0 ? Math.max(0, total - out - fresh) / total * 100 : NaN;
        return {
            title: 'Tokens today',
            aside: 'this computer',
            tiles: [
                {value: fmtTokens(total), caption: 'total'},
                {value: fmtTokens(out), caption: 'output'},
                {value: Number.isFinite(cached) ? `${cached.toFixed(cached >= 99.95 ? 0 : 1)}%` : '–', caption: 'from cache'},
            ],
        };
    }
    const today = num(t.today);
    const week = num(t.last_7_days);
    if (!Number.isFinite(today) && !Number.isFinite(week))
        return null;
    const avg = week > 0 ? week / 7 : NaN;
    const ratio = today > 0 && avg > 0 ? today / avg : NaN;
    return {
        title: 'Tokens',
        aside: staleAt ? `saved ${staleAt}` : 'whole account',
        staleAt,
        tiles: [
            {value: fmtTokens(today), caption: Number.isFinite(ratio) ? `today · ${ratio.toFixed(1)}× avg` : 'today'},
            {value: fmtTokens(week), caption: 'last 7 days'},
            {value: fmtTokens(t.lifetime), caption: 'lifetime'},
        ],
    };
}

// Today's history of a worker for its timeline strip: when it has been up since, and incidents.
// `text(now)` is evaluated live (the uptime keeps counting while the menu is open).
function claudeHistory(srv, loading = false) {
    const since = num(srv.since);
    const drops = Array.isArray(srv.drops_today)
        ? srv.drops_today.filter(d => d && Number.isFinite(num(d.at))).map(d => ({at: num(d.at), secs: num(d.secs) || 0}))
        : null;
    if (!srv.service_active && !drops?.length)
        return null;
    if (drops === null && !loading && !Number.isFinite(since))
        return null; // nothing to show (no history in this snapshot)
    const marks = (drops ?? []).map(d => ({at: d.at, level: d.secs >= 60 ? 'bad' : 'warn'}));
    const text = now => {
        const parts = [];
        if (srv.service_active && Number.isFinite(since))
            parts.push(`Up ${fmtDuration(now - since)}`);
        if (drops === null) {
            if (loading)
                parts.push('checking drops…');
        } else if (!drops.length) {
            parts.push('no drops today');
        } else {
            const longest = Math.max(...drops.map(d => d.secs));
            parts.push(`${drops.length} ${plural(drops.length, 'drop')} today, longest ${fmtSecs(longest)}`);
        }
        return parts.join(' · ');
    };
    return {
        up: srv.service_active && Number.isFinite(since) ? since : null, marks, text, parts: ['claude.drops'],
        count: drops?.length ?? 0, sig: JSON.stringify([srv.drops_today ?? null, srv.since ?? null, !!srv.service_active]),
    };
}

function codexHistory(svc, wd, loading = false) {
    const ev = wd.events_today && typeof wd.events_today === 'object' ? wd.events_today : null;
    const since = num(svc.since);
    const recent = Array.isArray(ev?.recent) ? ev.recent.filter(e => e && Number.isFinite(num(e.at))) : [];
    const isRestart = m => /^restarting|starting it|replacing it/.test(m);
    const marks = recent
        .map(e => ({at: num(e.at), msg: `${e.msg ?? ''}`}))
        .filter(e => isRestart(e.msg) || e.msg.includes('(miss '))
        .map(e => ({at: e.at, level: isRestart(e.msg) ? 'bad' : 'warn'}));
    if (!ev && !loading && !(svc.running && Number.isFinite(since)))
        return null; // nothing to show (no history in this snapshot)
    const restarts = num(ev?.restarts) || 0;
    const misses = num(ev?.relay_misses) || 0;
    const lastFix = [...recent].reverse().find(e => isRestart(`${e.msg ?? ''}`));
    const text = now => {
        const parts = [];
        if (svc.running && Number.isFinite(since))
            parts.push(`Up ${fmtDuration(now - since)}`);
        if (!ev) {
            if (loading)
                parts.push('checking today…');
        } else if (restarts) {
            parts.push(`${restarts} ${plural(restarts, 'restart')} today${lastFix ? `, last ${fmtClock(num(lastFix.at))}` : ''}`);
        } else if (misses) {
            parts.push(`${misses} phone-link ${plural(misses, 'drop')} today`);
        } else {
            parts.push('no incidents today');
        }
        return parts.join(' · ');
    };
    return {
        up: svc.running && Number.isFinite(since) ? since : null, marks, text, parts: ['codex.events'], count: restarts,
        sig: JSON.stringify([ev, svc.since ?? null, !!svc.running]),
    };
}

const sessionsInUse = capacity => {
    const n = parseInt(`${capacity ?? ''}`.split('/')[0], 10);
    return Number.isFinite(n) ? n : 0;
};

function claudeWorkers(w, pending = new Set()) {
    const rows = [];
    const srv = w.server && typeof w.server === 'object' ? w.server : {};
    const state = srv.state ? `${srv.state}` : null;
    const capacity = srv.capacity ? `${srv.capacity} sessions` : null;
    const history = claudeHistory(srv, pending.has('claude.drops'));
    const server = {icon: 'network-server-symbolic', name: 'Phone & web sessions', history};
    const restart = argvLabel => {
        const n = sessionsInUse(srv.capacity);
        const known = /^\d+\//.test(`${srv.capacity ?? ''}`);
        return {
            id: 'claude-restart', label: argvLabel, argv: ['systemctl', '--user', 'restart', CLAUDE_UNIT],
            // One click only when the server says it runs no sessions; unknown counts as "maybe some".
            confirm: n > 0 ? `Ends ${n} ${plural(n, 'session')} · click again`
                : known ? null : 'Ends its sessions · click again',
            done: 'Remote server restarted',
        };
    };
    if (!srv.service_active) {
        rows.push({
            ...server, level: 'bad', detail: 'Remote Control server not running', issue: 'Remote server down',
            // `start` is a no-op if the server is in fact running (e.g. systemctl timed out above).
            action: {id: 'claude-start', label: 'Start server', argv: ['systemctl', '--user', 'start', CLAUDE_UNIT], done: 'Remote server started'},
        });
    } else if (state === 'Connected' || state === 'Ready') {
        const unstable = (history?.count ?? 0) >= 10;
        rows.push({
            ...server, level: unstable ? 'warn' : 'ok', chip: unstable ? 'Unstable' : undefined,
            detail: [state, capacity].filter(Boolean).join(' · '),
            items: (Array.isArray(srv.sessions) ? srv.sessions : []).map(s => `${s}`),
            issue: unstable ? `Remote server dropped ${history.count}× today` : undefined,
        });
    } else {
        const bad = state === 'Error' || state === 'Disconnected';
        rows.push({
            ...server, level: bad ? 'bad' : 'warn',
            detail: [state ? `${state}${bad ? '' : '…'}` : 'Starting…', capacity].filter(Boolean).join(' · '),
            issue: `Remote server ${(state ?? 'starting').toLowerCase()}`,
            // A reconnect normally heals itself within seconds; the button is for when it does not.
            action: restart(bad ? 'Restart server' : 'Restart anyway'),
        });
    }

    const ses = w.sessions && typeof w.sessions === 'object' ? w.sessions : {};
    const total = num(ses.total) || 0;
    const connected = num(ses.connected) || 0;
    const term = {icon: 'utilities-terminal-symbolic', name: 'Terminal sessions'};
    if (!total) {
        rows.push({...term, level: 'idle', chip: 'None', detail: 'No interactive sessions open'});
    } else if (connected >= total) {
        rows.push({...term, level: 'ok', detail: total === 1 ? 'On Remote Control' : `All ${total} on Remote Control`});
    } else {
        const missing = total - connected;
        const names = Array.isArray(ses.not_connected) ? ses.not_connected.map(s => `${s}`) : [];
        rows.push({
            ...term, level: 'warn', detail: `${connected} of ${total} on Remote Control · type /rc in the others`,
            items: names.length ? [`Not connected: ${names.join(', ')}`] : [],
            issue: `${missing} ${plural(missing, 'session')} not on Remote Control`,
        });
    }

    rows.push(w.auto_connect
        ? {icon: 'mail-send-receive-symbolic', name: 'Auto-connect', level: 'ok', chip: 'On', detail: 'New sessions join Remote Control'}
        : {
            icon: 'mail-send-receive-symbolic', name: 'Auto-connect', level: 'warn', chip: 'Off',
            detail: 'New sessions stay local', issue: 'Auto-connect off',
        });

    const link = claudeLink(srv.link);
    return {rows, link};
}

function codexWorkers(w, pending = new Set()) {
    const rows = [];
    const svc = w.service && typeof w.service === 'object' ? w.service : {};
    const relay = svc.relay && typeof svc.relay === 'object' ? svc.relay : {};
    const wd = w.watchdog && typeof w.watchdog === 'object' ? w.watchdog : {};
    // Running the watchdog once does what its timer would do next minute: start a stopped service,
    // replace a stray server. It never restarts a running one on the first check.
    const runWatchdog = (id, label, done) => ({id, label, argv: ['systemctl', '--user', 'start', CODEX_UNIT], done});
    const service = {icon: 'system-run-symbolic', name: 'Background service', history: codexHistory(svc, wd, pending.has('codex.events')), parts: ['codex.relay']};
    if (!svc.running) {
        rows.push({
            ...service, level: 'bad', detail: 'Not running · phone access unavailable', issue: 'Codex service down',
            action: runWatchdog('codex-start', 'Start service', 'Codex service started'),
        });
    } else if (relay.status === 'connected') {
        rows.push({...service, level: 'ok', detail: ['Phone link connected', relay.name].filter(Boolean).join(' · ')});
    } else if (wd.relay_held_by) {
        // Another Codex app here (Codex Desktop) holds the relay under the same server identity: the
        // phone reaches this computer through it, and the watchdog leaves the service alone.
        rows.push({...service, level: 'ok', detail: `Phone link via ${clip(wd.relay_held_by, 40)}`});
    } else {
        const status = relay.status ? `${relay.status}` : 'unknown';
        rows.push({
            ...service, level: 'warn', detail: `Phone link ${status}`, issue: `Phone link ${status}`,
            action: {
                id: 'codex-restart', label: 'Restart service', confirm: 'Stops running Codex tasks · click again',
                // Its own scope, so the restarted service does not live inside GNOME Shell's cgroup.
                argv: ['systemd-run', '--user', '--scope', '--quiet', '--collect', '--', CODEX_BIN, 'app-server', 'daemon', 'restart'],
                done: 'Codex service restarted',
            },
        });
    }

    const stray = num(w.stray_servers) || 0;
    if (stray > 0) {
        rows.push({
            icon: 'dialog-warning-symbolic', name: stray === 1 ? 'Stray Codex server' : `${stray} stray Codex servers`,
            level: 'bad', chip: 'Blocking', detail: 'Blocks phone access',
            issue: `${stray} stray ${plural(stray, 'server')}`,
            action: runWatchdog('codex-fix', 'Fix now', 'Watchdog ran'),
        });
    }

    const message = wd.last_message ? clip(wd.last_message) : null;
    const watchdog = {icon: 'alarm-symbolic', name: 'Watchdog'};
    if (!wd.timer_active) {
        rows.push({
            ...watchdog, level: 'warn', chip: 'Off', detail: 'Timer is off', issue: 'Watchdog off',
            action: {id: 'codex-timer', label: 'Turn on', argv: ['systemctl', '--user', 'enable', '--now', CODEX_TIMER], done: 'Watchdog turned on'},
        });
    } else if (wd.last_ok === false) {
        rows.push({
            ...watchdog, level: 'warn',
            detail: ['Last check failed', wd.last_run ? fmtClock(wd.last_run) : null].filter(Boolean).join(' · '),
            items: message ? [message] : [], issue: 'Watchdog check failed',
            action: runWatchdog('codex-check', 'Check again', 'Watchdog ran'),
        });
    } else {
        rows.push({
            ...watchdog, level: 'ok',
            detail: [wd.last_run ? `Checked ${fmtClock(wd.last_run)}` : 'Waiting for first check', message]
                .filter(Boolean).join(' · '),
        });
    }

    rows.push(w.ssh?.active
        ? {icon: 'network-wired-symbolic', name: 'SSH for the Codex app', level: 'ok', detail: 'SSH server running'}
        : {
            icon: 'network-wired-symbolic', name: 'SSH for the Codex app', level: 'bad',
            detail: 'sshd is stopped', items: ['Fix: sudo systemctl start sshd'], issue: 'SSH down',
        });
    return {rows, link: null};
}

function buildModel(key, d, pending = new Set()) {
    const account = accountModel(key, d.account);
    if (!d.account && pending.has(`${key}.account`))
        Object.assign(account, {loggedIn: true, title: 'Loading account…', plan: null, meta: ''});
    const usage = usageModel(d.usage);
    const tokens = tokensModel(key, d.tokens);
    const w = d.workers && typeof d.workers === 'object' ? d.workers : null;
    const remote = w
        ? {...(key === 'claude' ? claudeWorkers(w, pending) : codexWorkers(w, pending)), unavailable: false}
        : {rows: [], link: null, unavailable: true};

    // Usage-side issue (only the most important one makes it into the summary line).
    let usageIssue = null;
    if (!account.loggedIn) {
        usageIssue = {level: 'bad', text: account.title === 'Not signed in' ? 'Signed out' : 'Account unavailable'};
    } else if (usage.missing && pending.has(`${key}.usage`)) {
        usageIssue = null; // first ever load: still on its way
    } else if (usage.off) {
        usageIssue = null; // a choice, not a problem
    } else if (usage.error) {
        usageIssue = {level: 'warn', text: 'Limits unavailable'};
    } else if (usage.staleAt) {
        usageIssue = {level: 'warn', text: `Limits from ${usage.staleAt}`};
    } else if (usage.limitReached) {
        usageIssue = {level: 'bad', text: 'Rate limit reached'};
    } else if (usage.windows.length) {
        const top = usage.windows.reduce((a, b) => (b.pct > a.pct ? b : a));
        const lv = pctLevel(top.pct);
        if (lv !== 'ok') {
            usageIssue = {
                level: lv,
                text: top.pct >= 100 ? `${top.label} limit reached` : `${top.label} at ${Math.round(top.pct)}%`,
            };
        }
    }

    const remoteIssues = remote.rows.filter(r => RANK[r.level] > 0);
    let remoteText;
    if (remote.unavailable)
        remoteText = 'Remote access status unknown';
    else if (!remoteIssues.length)
        remoteText = 'Remote access healthy';
    else if (remoteIssues.length === 1)
        remoteText = remoteIssues[0].issue ?? '1 remote-access issue';
    else
        remoteText = `${remoteIssues.length} remote issues`;

    const level = worst([
        usageIssue?.level ?? 'ok',
        remote.unavailable ? 'warn' : 'ok',
        ...remote.rows.map(r => r.level),
    ]);
    const summary = [usageIssue?.text, remoteText].filter(Boolean).join(' · ');
    return {key, account, usage, tokens, remote, level, summary};
}

// ---------------------------------------------------------------------------------------------
// Drawing helpers and custom widgets
// ---------------------------------------------------------------------------------------------

const rgba = c => [c.red / 255, c.green / 255, c.blue / 255, c.alpha / 255];
const mix = (a, b, t) => a.map((v, i) => v + (b[i] - v) * t);

function themeColor(node, prop, fallback) {
    const [found, color] = node.lookup_color(prop, false);
    return found ? rgba(color) : fallback;
}

function themeDouble(node, prop, fallback) {
    const [found, value] = node.lookup_double(prop, false);
    return found ? value : fallback;
}

function fillCircle(cr, x, y, r, c) {
    cr.newPath();
    cr.setSourceRGBA(...c);
    cr.arc(x, y, r, 0, 2 * Math.PI);
    cr.fill();
}

function pillPath(cr, x, y, w, h) {
    const r = Math.min(h / 2, w / 2);
    cr.newPath();
    cr.arc(x + r, y + r, r, Math.PI / 2, 3 * Math.PI / 2);
    cr.arc(x + w - r, y + r, r, -Math.PI / 2, Math.PI / 2);
    cr.closePath();
}

// Usage ring. Colours, thickness and the pace-marker colour come from the stylesheet
// (-au-ring-track, -au-ring-from, -au-ring-to, -au-ring-pace, -au-ring-thickness), so each tab
// accent and each warning level is pure CSS. `value` is a GObject property so the shell's
// ease_property() animates it (and honours the "reduce animations" setting).
const AuRing = GObject.registerClass({
    GTypeName: `AgentUsageRing_${UID}`,
    Properties: {
        'value': GObject.ParamSpec.double('value', 'Value', 'Displayed percentage',
            GObject.ParamFlags.READWRITE, 0, 100, 0),
    },
}, class AuRing extends St.DrawingArea {
    _init({target = null, pace = null, ...params} = {}) {
        super._init(params);
        this._target = target === null ? null : clamp(target, 0, 100);
        this._value = this._target ?? 0;
        this._pace = pace;
    }

    get value() {
        return this._value ?? 0;
    }

    set value(v) {
        if (this._value === v)
            return;
        this._value = v;
        this.notify('value');
        this.queue_repaint();
    }

    set pace(p) {
        if (this._pace === p)
            return;
        this._pace = p;
        this.queue_repaint();
    }

    animateFrom(from) {
        if (this._target === null)
            return;
        this.remove_transition('value');
        if (Math.abs(this._target - from) < 0.5) {
            this.value = this._target;
            return;
        }
        this.value = clamp(from, 0, 100);
        this.ease_property('value', this._target, {duration: 750, mode: EASE_OUT});
    }

    finish() {
        this.remove_transition('value');
        if (this._target !== null)
            this.value = this._target;
    }

    vfunc_repaint() {
        const cr = this.get_context();
        const [w, h] = this.get_surface_size();
        const node = this.get_theme_node();
        const fg = rgba(node.get_foreground_color());
        const track = themeColor(node, '-au-ring-track', [1, 1, 1, 0.1]);
        const c0 = themeColor(node, '-au-ring-from', fg);
        const c1 = themeColor(node, '-au-ring-to', c0);
        const paceColor = themeColor(node, '-au-ring-pace', [1, 1, 1, 0.9]);
        const size = Math.min(w, h);
        const lw = Math.max(2, Math.round(size * themeDouble(node, '-au-ring-thickness', 0.1)));
        const halo = Math.round(lw * 0.6);
        const r = (size - lw - halo) / 2;
        const cx = w / 2;
        const cy = h / 2;
        const TAU = 2 * Math.PI;
        const top = -Math.PI / 2;

        cr.setLineWidth(lw);
        cr.setSourceRGBA(...track);
        cr.arc(cx, cy, r, 0, TAU);
        cr.stroke();

        const frac = this._target === null ? 0 : clamp(this._value / 100, 0, 1);
        if (frac > 0.002) {
            const end = top + frac * TAU;
            // Soft halo underneath the arc.
            cr.setLineCap(Cairo.LineCap.ROUND);
            cr.setLineWidth(lw + halo);
            cr.setSourceRGBA(c1[0], c1[1], c1[2], 0.16 * c1[3]);
            cr.newPath();
            cr.arc(cx, cy, r, top, end);
            cr.stroke();
            // The arc itself: colour runs from c0 at 12 o'clock to c1 at the tip (cairo has no
            // conic gradient, so short overlapping segments fake one).
            cr.setLineCap(Cairo.LineCap.BUTT);
            cr.setLineWidth(lw);
            const span = end - top;
            const steps = Math.max(6, Math.ceil(frac * 96));
            for (let i = 0; i < steps; i++) {
                const a = top + span * (i / steps);
                const b = top + span * ((i + 1) / steps) + (i < steps - 1 ? 0.012 : 0);
                cr.setSourceRGBA(...mix(c0, c1, (i + 0.5) / steps));
                cr.newPath();
                cr.arc(cx, cy, r, a, b);
                cr.stroke();
            }
            fillCircle(cr, cx + r * Math.cos(top), cy + r * Math.sin(top), lw / 2, c0);
            fillCircle(cr, cx + r * Math.cos(end), cy + r * Math.sin(end), lw / 2, c1);
        }

        // Pace marker: how much of the window has elapsed. Arc behind the dot = under pace.
        if (this._target !== null && this._pace !== null) {
            const a = top + clamp(this._pace, 0, 1) * TAU;
            fillCircle(cr, cx + r * Math.cos(a), cy + r * Math.sin(a), Math.max(1.5, lw * 0.2), paceColor);
        }
        cr.$dispose();
    }
});

// Slim horizontal meter (extra usage). Same colour properties as the ring.
const AuBar = GObject.registerClass({
    GTypeName: `AgentUsageBar_${UID}`,
}, class AuBar extends St.DrawingArea {
    _init({pct = 0, ...params} = {}) {
        super._init(params);
        this._pct = clamp(pct, 0, 100);
    }

    vfunc_repaint() {
        const cr = this.get_context();
        const [w, h] = this.get_surface_size();
        const node = this.get_theme_node();
        const fg = rgba(node.get_foreground_color());
        const c0 = themeColor(node, '-au-ring-from', fg);
        const c1 = themeColor(node, '-au-ring-to', c0);
        cr.setSourceRGBA(...themeColor(node, '-au-ring-track', [1, 1, 1, 0.1]));
        pillPath(cr, 0, 0, w, h);
        cr.fill();
        if (this._pct > 0) {
            const fw = Math.max(h, w * this._pct / 100);
            const g = new Cairo.LinearGradient(0, 0, fw, 0);
            g.addColorStopRGBA(0, ...c0);
            g.addColorStopRGBA(1, ...c1);
            cr.setSource(g);
            pillPath(cr, 0, 0, fw, h);
            cr.fill();
        }
        cr.$dispose();
    }
});

// Today's timeline of a worker, midnight → now: a track, the stretch it has been up (tab accent
// gradient), and a dot per incident (amber: brief drop / relay miss, red: long drop / restart).
// Colours: -au-ring-track, -au-ring-from/-to (accent), -au-mark-warn, -au-mark-bad, -au-mark-edge.
const AuTimeline = GObject.registerClass({
    GTypeName: `AgentUsageTimeline_${UID}`,
}, class AuTimeline extends St.DrawingArea {
    _init({start = 0, up = null, marks = [], ...params} = {}) {
        super._init(params);
        this._start = start;
        this._up = up;
        this._marks = marks;
        this._end = nowS();
    }

    tick() {
        this._end = nowS();
        this.queue_repaint();
    }

    vfunc_repaint() {
        const cr = this.get_context();
        const [w, h] = this.get_surface_size();
        const node = this.get_theme_node();
        const scale = St.ThemeContext.get_for_stage(global.stage).scale_factor;
        const span = Math.max(1, this._end - this._start);
        const r = 3 * scale; // dot radius, also the inset so edge dots are not clipped
        const usable = Math.max(1, w - 2 * r);
        const xOf = t => r + clamp((t - this._start) / span, 0, 1) * usable;
        const th = Math.max(2, Math.round(3 * scale));
        const y = (h - th) / 2;

        cr.setSourceRGBA(...themeColor(node, '-au-ring-track', [1, 1, 1, 0.1]));
        pillPath(cr, r, y, usable, th);
        cr.fill();
        if (this._up !== null) {
            const x0 = xOf(Math.max(this._up, this._start));
            const x1 = xOf(this._end);
            if (x1 - x0 >= 1) {
                const c0 = themeColor(node, '-au-ring-from', [1, 1, 1, 0.6]);
                const c1 = themeColor(node, '-au-ring-to', c0);
                const g = new Cairo.LinearGradient(x0, 0, x1, 0);
                g.addColorStopRGBA(0, c0[0], c0[1], c0[2], 0.55 * c0[3]);
                g.addColorStopRGBA(1, ...c1);
                cr.setSource(g);
                pillPath(cr, x0, y, Math.max(th, x1 - x0), th);
                cr.fill();
            }
        }
        const edge = themeColor(node, '-au-mark-edge', [0.1, 0.1, 0.12, 1]);
        const colors = {
            warn: themeColor(node, '-au-mark-warn', [0.96, 0.77, 0.33, 1]),
            bad: themeColor(node, '-au-mark-bad', [1, 0.42, 0.42, 1]),
        };
        // Draw warn marks first so red ones stay on top where they overlap.
        for (const level of ['warn', 'bad']) {
            for (const m of this._marks) {
                if (m.level !== level || m.at < this._start)
                    continue;
                const x = xOf(m.at);
                fillCircle(cr, x, h / 2, r + scale, edge);
                fillCircle(cr, x, h / 2, r, colors[level]);
            }
        }
        cr.$dispose();
    }
});

// Indeterminate "updating" shimmer under the tabs. `phase` is eased 0→1 on repeat while a
// collector run is in flight and the menu is open; idle it draws nothing.
const AuProgress = GObject.registerClass({
    GTypeName: `AgentUsageProgress_${UID}`,
    Properties: {
        'phase': GObject.ParamSpec.double('phase', 'Phase', 'Shimmer position',
            GObject.ParamFlags.READWRITE, 0, 1, 0),
    },
}, class AuProgress extends St.DrawingArea {
    _init(params = {}) {
        super._init(params);
        this._phase = 0;
        this._running = false;
        this._static = false;
    }

    get phase() {
        return this._phase;
    }

    set phase(v) {
        this._phase = v;
        this.notify('phase');
        this.queue_repaint();
    }

    start() {
        if (this._running)
            return;
        this._running = true;
        this._static = !St.Settings.get().enable_animations;
        this.remove_transition('phase');
        this.phase = 0;
        if (!this._static) {
            this.ease_property('phase', 1, {
                duration: 1100, mode: Clutter.AnimationMode.EASE_IN_OUT_SINE, repeatCount: -1,
            });
        }
    }

    stop() {
        if (!this._running)
            return;
        this._running = false;
        this.remove_transition('phase');
        this.queue_repaint();
    }

    vfunc_repaint() {
        const cr = this.get_context();
        const [w, h] = this.get_surface_size();
        if (this._running && w > 0) {
            const c = themeColor(this.get_theme_node(), '-au-progress-color', [1, 1, 1, 0.8]);
            if (this._static) {
                cr.setSourceRGBA(c[0], c[1], c[2], 0.5 * c[3]);
                pillPath(cr, 0, 0, w, h);
                cr.fill();
            } else {
                const seg = w * 0.38;
                const x = -seg + (w + seg) * this._phase;
                const g = new Cairo.LinearGradient(x, 0, x + seg, 0);
                g.addColorStopRGBA(0, c[0], c[1], c[2], 0);
                g.addColorStopRGBA(0.5, ...c);
                g.addColorStopRGBA(1, c[0], c[1], c[2], 0);
                cr.setSource(g);
                pillPath(cr, 0, 0, w, h);
                cr.fill();
            }
        }
        cr.$dispose();
    }
});

// ---------------------------------------------------------------------------------------------
// Small actor helpers
// ---------------------------------------------------------------------------------------------

const vbox = (style, props = {}) => new St.BoxLayout({
    orientation: Clutter.Orientation.VERTICAL, style_class: style ?? '', ...props,
});
const hbox = (style, props = {}) => new St.BoxLayout({
    orientation: Clutter.Orientation.HORIZONTAL, style_class: style ?? '', ...props,
});
const label = (text, style, props = {}) => new St.Label({
    text: text ?? '', style_class: style ?? '', y_align: ALIGN.CENTER, ...props,
});
const blank = (style, props = {}) => new St.Widget({style_class: style ?? '', ...props});

function wrapped(text, style, props = {}) {
    const l = label(text, style, {x_expand: true, ...props});
    l.clutter_text.line_wrap = true;
    l.clutter_text.line_wrap_mode = Pango.WrapMode.WORD_CHAR;
    l.clutter_text.ellipsize = Pango.EllipsizeMode.NONE;
    return l;
}

function add(parent, ...kids) {
    for (const k of kids) {
        if (k)
            parent.add_child(k);
    }
    return parent;
}

// Changing a style class restyles the actor and everything inside it, synchronously while the
// menu is open, so touch classes only when they really change.
function setLevel(actor, level) {
    for (const l of LEVELS) {
        if (l !== level)
            actor.remove_style_class_name(`au-level-${l}`); // a no-op when absent
    }
    if (!actor.has_style_class_name(`au-level-${level}`))
        actor.add_style_class_name(`au-level-${level}`);
}

function setClass(actor, cls, on) {
    if (on !== actor.has_style_class_name(cls)) {
        if (on)
            actor.add_style_class_name(cls);
        else
            actor.remove_style_class_name(cls);
    }
}

function homogeneous(box) {
    if (box.layout_manager instanceof Clutter.BoxLayout)
        box.layout_manager.homogeneous = true;
    return box;
}

function loadIcon(extensionPath, spec) {
    if (extensionPath) {
        const path = GLib.build_filenamev([extensionPath, 'icons', spec.iconFile]);
        if (GLib.file_test(path, GLib.FileTest.IS_REGULAR))
            return Gio.FileIcon.new(Gio.File.new_for_path(path));
    }
    return Gio.ThemedIcon.new_from_names(spec.fallbackIcons);
}

// ---------------------------------------------------------------------------------------------
// The indicator
// ---------------------------------------------------------------------------------------------

const AgentUsageIndicator = GObject.registerClass({
    GTypeName: `AgentUsageIndicator_${UID}`,
}, class AgentUsageIndicator extends PanelMenu.Button {
    _init(extension) {
        super._init(0.5, 'Agent Usage', false);

        this._extPath = extension?.path ?? null;
        this._icons = {panel: loadIcon(this._extPath, PANEL_ICON)};
        for (const t of TABS)
            this._icons[t.key] = loadIcon(this._extPath, t);

        this._tab = 'claude';
        this._data = null; // last good snapshot
        this._models = null; // {claude, codex} built from it
        this._fetchedAt = 0;
        this._fetchSecs = 0;
        this._lastError = null; // {title, detail} of the last failed run
        this._proc = null;
        this._cancellable = null;
        this._watchdogId = 0;
        this._tickId = 0;
        this._stale = false;
        this._destroyed = false;
        this._liveFns = null; // collection target while a section is being built
        this._rings = null; // idem
        this._pages = {}; // tab key -> {box, missing, sections: {name -> {key, actor, live, rings}}}
        this._stateBox = null; // skeleton / fatal error (no data yet)
        this._skeleton = null;
        this._prevPct = {claude: new Map(), codex: new Map()};
        this._pending = new Set(); // parts of the shown snapshot that are still loading
        this._freshened = new Set(); // parts that just arrived (fade in)
        this._runFrames = 0; // frames received in the current collector run
        this._fromDisk = false; // the shown snapshot is the saved one from disk
        this._actionBusy = null; // id of the fix command running
        this._actionMsg = null; // {text, level} footer note about the last fix command
        this._actionErr = null; // {id, text} error of the last failed fix, shown under its button
        this._recheckId = 0;
        this._snapCancel = null;

        this._buildPanelButton();
        this._buildMenu();

        // NB: PanelMenu.Button already has _onOpenStateChanged(menu, open) wired in setMenu();
        // our handler must use a different name or it silently replaces the base one.
        this.menu.connect('open-state-changed', (_menu, open) => this._onMenuOpenChanged(open));
        this.connect('destroy', () => this._onDestroyed());
        this._render('init');
        this._loadSnapshot();
    }

    // The last complete snapshot, saved by the collector: one local file read at startup, so the
    // panel badge is right after a login and the first open shows numbers instead of a skeleton.
    _loadSnapshot() {
        const cancellable = new Gio.Cancellable();
        this._snapCancel = cancellable;
        Gio.File.new_for_path(SNAPSHOT).load_contents_async(cancellable, (f, res) => {
            let data = null;
            try {
                const [, bytes] = f.load_contents_finish(res);
                if (bytes.length > 1 << 20)
                    return; // not ours: a snapshot is a few kB
                data = JSON.parse(new TextDecoder().decode(bytes));
            } catch {
                return; // no snapshot yet, or unreadable: the first open shows the skeleton
            }
            if (this._snapCancel === cancellable)
                this._snapCancel = null;
            if (this._destroyed || this._data || this._proc || !data || typeof data !== 'object' ||
                (!data.claude && !data.codex))
                return;
            this._fromDisk = true;
            try {
                this._accept(data, 0, [], 'init');
            } catch (e) {
                this._fromDisk = false;
                logError(e, 'agent-usage: saved snapshot');
            }
        });
    }

    // ---------- chrome ----------

    _buildPanelButton() {
        // The stack is exactly the glyph box (no padding on the icon), so the badge sits on the
        // glyph's top-right corner; the outer bin supplies the usual panel-icon spacing.
        const icon = new St.Icon({
            gicon: this._icons.panel, style_class: 'au-panel-icon',
            fallback_icon_name: 'power-profile-performance-symbolic',
        });
        this._badge = blank('au-panel-badge', {x_align: ALIGN.END, y_align: ALIGN.START, visible: false});
        const stack = new St.Widget({
            layout_manager: new Clutter.BinLayout(), x_align: ALIGN.CENTER, y_align: ALIGN.CENTER,
        });
        add(stack, icon, this._badge);
        this.add_child(new St.Bin({child: stack, style_class: 'au-panel-box', y_align: ALIGN.CENTER}));
        const nudge = () => {
            const scale = St.ThemeContext.get_for_stage(global.stage).scale_factor;
            this._badge.translation_x = 4 * scale;
            this._badge.translation_y = -3 * scale;
        };
        nudge();
        this._badge.connect('style-changed', nudge);
    }

    _buildMenu() {
        this.menu.actor.add_style_class_name('au-menu');
        const item = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false, style_class: 'au-menu-item'});
        // No accent class on the root: each tab page carries its own (au-claude / au-codex) for good,
        // so switching tabs restyles only the progress bar and the footer, not the whole menu.
        this._root = vbox('au-root', {x_expand: true});
        this._accent = null;
        item.add_child(this._root);
        this.menu.addMenuItem(item);

        // Tabs: segmented control with per-tab accent and a status dot for the other tab.
        const tabs = homogeneous(hbox('au-tabs', {x_expand: true}));
        this._tabButtons = {};
        this._tabDots = {};
        for (const t of TABS) {
            const inner = hbox('au-tab-inner', {x_align: ALIGN.CENTER});
            const dot = blank('au-tab-dot', {y_align: ALIGN.CENTER, visible: false});
            add(inner,
                new St.Icon({gicon: this._icons[t.key], style_class: 'au-tab-icon', fallback_icon_name: t.fallbackIcons[0]}),
                label(t.name, 'au-tab-label'),
                dot);
            const b = new St.Button({
                child: inner, style_class: `au-tab au-tab-${t.key}`, x_expand: true, can_focus: true,
                accessible_name: t.name,
            });
            b.connect('clicked', () => this._selectTab(t.key));
            this._tabButtons[t.key] = b;
            this._tabDots[t.key] = dot;
            tabs.add_child(b);
        }
        this._root.add_child(tabs);

        this._progress = new AuProgress({style_class: 'au-progress', x_expand: true});
        this._root.add_child(this._progress);

        // Content viewport (clipped so the tab-switch slide never spills over the menu edge).
        this._viewport = new St.Widget({
            layout_manager: new Clutter.BinLayout(), x_expand: true, clip_to_allocation: true,
            style_class: 'au-viewport',
        });
        this._content = vbox('au-content-host', {x_expand: true}); // holds one page per tab
        this._viewport.add_child(this._content);
        this._root.add_child(this._viewport);

        // Footer: freshness + Refresh.
        const footer = hbox('au-footer', {x_expand: true});
        this._footer = footer;
        this._status = label('', 'au-footer-status', {x_expand: true});
        this._refreshIcon = new St.Icon({icon_name: 'view-refresh-symbolic', style_class: 'au-refresh-icon'});
        this._refreshIcon.set_pivot_point(0.5, 0.5);
        const refreshInner = add(hbox('au-refresh-inner'), this._refreshIcon, label('Refresh', 'au-refresh-label'));
        this._refreshButton = new St.Button({
            child: refreshInner, style_class: 'au-refresh', can_focus: true, accessible_name: 'Refresh usage',
        });
        this._refreshButton.connect('clicked', () => this._refresh());
        add(footer, this._status, this._refreshButton);
        this._root.add_child(footer);
    }

    _selectTab(key) {
        if (key === this._tab)
            return;
        const dir = TABS.findIndex(t => t.key === key) > TABS.findIndex(t => t.key === this._tab) ? 1 : -1;
        this._tab = key;
        this._render('tab', dir);
    }

    // ---------- menu lifecycle ----------

    _onMenuOpenChanged(open) {
        if (this._destroyed)
            return;
        if (open) {
            if (this._data) {
                // Instant: the last snapshot is already rendered; dim it while we refresh.
                this._updateLive();
                this._stale = true;
                this._content.remove_all_transitions();
                this._content.opacity = DIM;
            }
            this._startTick();
            this._refresh();
        } else {
            this._stopTick();
            if (this._actionMsg && this._actionMsg.level !== 'busy') {
                this._actionMsg = null;
                this._actionErr = null;
            }
            for (const r of this._allRings())
                r.ring.finish();
            this._content.remove_all_transitions();
            this._content.translation_x = 0;
            this._content.translation_y = 0;
            this._content.opacity = this._stale ? DIM : 255;
        }
        this._syncAnimations();
    }

    _startTick() {
        if (this._tickId)
            return;
        this._tickId = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, CLOCK_TICK_S, () => {
            this._updateLive();
            return GLib.SOURCE_CONTINUE;
        });
    }

    _stopTick() {
        if (this._tickId)
            GLib.source_remove(this._tickId);
        this._tickId = 0;
    }

    _syncAnimations() {
        const open = this.menu.isOpen && !this._destroyed;
        const busy = this._proc !== null;
        const animations = St.Settings.get().enable_animations;

        if (open && busy)
            this._progress.start();
        else
            this._progress.stop();

        const icon = this._refreshIcon;
        if (open && busy && animations) {
            if (!icon.get_transition('rotation-angle-z')) {
                icon.rotation_angle_z = 0;
                icon.ease({rotation_angle_z: 360, duration: 900, mode: Clutter.AnimationMode.LINEAR, repeatCount: -1});
            }
        } else {
            icon.remove_all_transitions();
            icon.rotation_angle_z = 0;
        }
        this._refreshButton.reactive = !busy;
        if (busy)
            this._refreshButton.add_style_pseudo_class('insensitive');
        else
            this._refreshButton.remove_style_pseudo_class('insensitive');

        const sk = this._skeleton;
        if (sk) {
            if (open && animations) {
                if (!sk.get_transition('opacity')) {
                    sk.opacity = 255;
                    sk.ease({
                        opacity: 110, duration: 850, mode: Clutter.AnimationMode.EASE_IN_OUT_SINE,
                        repeatCount: -1, autoReverse: true,
                    });
                }
            } else {
                sk.remove_all_transitions();
                sk.opacity = 255;
            }
        }
        this._updateFooter();
    }

    // ---------- data ----------

    _refresh() {
        if (this._destroyed)
            return;
        if (this._proc) {
            this._syncAnimations();
            return;
        }
        if (this._data && !this._stale) {
            this._stale = true;
            this._content.remove_all_transitions();
            this._content.ease({opacity: DIM, duration: 160, mode: Clutter.AnimationMode.EASE_OUT_QUAD});
        }
        if (!GLib.file_test(COLLECTOR, GLib.FileTest.IS_EXECUTABLE)) {
            this._fail('Collector not found', `Expected an executable at ${tildify(COLLECTOR)}.`);
            return;
        }
        let proc;
        try {
            proc = Gio.Subprocess.new([COLLECTOR, '--stream'],
                Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_SILENCE);
        } catch (e) {
            this._fail('Could not start the collector', e.message ?? `${e}`);
            return;
        }
        const started = GLib.get_monotonic_time();
        const elapsed = () => (GLib.get_monotonic_time() - started) / 1e6;
        const cancellable = new Gio.Cancellable();
        const lines = new Gio.DataInputStream({base_stream: proc.get_stdout_pipe(), close_base_stream: true});
        let timedOut = false;
        let frames = 0;
        let complete = false; // got the last frame ("pending": [])
        let junk = false;
        this._proc = proc;
        this._cancellable = cancellable;
        this._runFrames = 0;
        this._watchdogId = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, COLLECTOR_TIMEOUT_S, () => {
            this._watchdogId = 0;
            timedOut = true;
            proc.force_exit();
            return GLib.SOURCE_REMOVE;
        });
        this._syncAnimations();

        const finish = error => {
            try {
                lines.close(null);
            } catch {}
            if (this._destroyed || this._proc !== proc)
                return;
            this._proc = null;
            this._cancellable = null;
            this._clearWatchdog();
            if (error instanceof GLib.Error && error.matches(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED))
                return;
            if (complete) {
                if (this._actionMsg?.level === 'ok')
                    this._actionMsg = null; // the re-check after a fix is done; the rows say the rest
                this._fetchSecs = elapsed();
                this._syncAnimations();
                return;
            }
            // Parts that never arrived keep their saved values, no longer marked as loading.
            if (frames && this._pending.size) {
                this._pending = new Set();
                this._render('patch');
            }
            if (timedOut)
                this._fail('Collector timed out', `No complete answer within ${COLLECTOR_TIMEOUT_S} s.`);
            else if (error)
                this._fail('Could not read usage data', error.message ?? `${error}`);
            else if (frames)
                this._fail('Collector stopped early', 'Some parts did not finish loading.');
            else if (junk)
                this._fail('Could not read usage data', 'The collector printed something that is not JSON.');
            else {
                const status = proc.get_if_exited() ? proc.get_exit_status() : null;
                this._fail('Could not read usage data', status
                    ? `The collector exited with status ${status}.` : 'The collector printed nothing.');
            }
        };

        const next = () => lines.read_line_async(GLib.PRIORITY_DEFAULT, cancellable, (s, res) => {
            let line;
            try {
                [line] = s.read_line_finish_utf8(res);
            } catch (e) {
                finish(e);
                return;
            }
            if (this._destroyed || this._proc !== proc)
                return;
            if (line === null) {
                finish(null);
                return;
            }
            let data;
            try {
                data = line.length <= 1 << 20 ? JSON.parse(line) : null;
            } catch {
                data = null;
            }
            if (data && typeof data === 'object' && (data.claude || data.codex)) {
                const pending = Array.isArray(data.pending) ? data.pending.map(p => `${p}`) : [];
                try {
                    this._fromDisk = false;
                    this._accept(data, elapsed(), pending);
                    frames++;
                    complete = pending.length === 0;
                } catch (e) {
                    // Keep reading: the next frame may be fine, and the run must still finish.
                    logError(e, 'agent-usage: could not show a frame');
                    junk = true;
                }
            } else if (line.trim()) {
                junk = true;
            }
            next();
        });
        next();
    }

    _clearWatchdog() {
        if (this._watchdogId)
            GLib.source_remove(this._watchdogId);
        this._watchdogId = 0;
    }

    // One frame: a whole snapshot plus the parts in it that are still loading (shown with their
    // saved values, dimmed). The first frame of a run slides in; later ones patch in place.
    _accept(data, secs, pending = [], reason = null) {
        const before = this._pending;
        this._pending = new Set(pending);
        this._freshened = new Set([...before].filter(p => !this._pending.has(p)));
        this._data = data;
        this._fetchedAt = num(data.generated_at) || nowS();
        this._fetchSecs = secs;
        this._lastError = null;
        this._models = {};
        for (const t of TABS) {
            const d = data[t.key];
            this._models[t.key] = d && typeof d === 'object' ? buildModel(t.key, d, this._pending) : null;
        }
        this._stale = false;
        this._render(reason ?? (this._runFrames++ === 0 ? 'data' : 'patch'));
    }

    _fail(title, detail) {
        this._lastError = {title, detail, at: nowS()};
        if (this._data) {
            // Keep showing the last good snapshot; the footer says it is old.
            this._stale = false;
            this._content.remove_all_transitions();
            this._content.ease({opacity: 255, duration: 160, mode: Clutter.AnimationMode.EASE_OUT_QUAD});
            this._syncAnimations();
        } else {
            this._render('error');
        }
    }

    // ---------- rendering ----------

    _render(reason, dir = 0) {
        if (this._destroyed)
            return;
        for (const t of TABS)
            this._tabButtons[t.key].checked = t.key === this._tab;
        this._setAccent(this._tab);
        this._syncStatusMarks();

        const tab = TABS.find(t => t.key === this._tab);
        const built = []; // rings of the sections (re)built now, with their start values
        if (!this._data) {
            this._showState();
        } else {
            this._clearState();
            for (const t of TABS) {
                if (this._pages[t.key])
                    this._pages[t.key].box.visible = t.key === tab.key;
            }
            this._renderPage(tab, reason, built);
        }
        this._updateLive();
        this._animateIn(reason, dir, built);
        this._freshened = new Set();
        this._syncAnimations();
    }

    _setAccent(key) {
        if (this._accent === key)
            return;
        for (const a of [this._progress, this._footer]) {
            if (this._accent)
                a.remove_style_class_name(`au-${this._accent}`);
            a.add_style_class_name(`au-${key}`);
        }
        this._accent = key;
    }

    // No data yet: skeleton or fatal error instead of the tab pages.
    _showState() {
        for (const p of Object.values(this._pages))
            p.box.destroy();
        this._pages = {};
        this._clearState();
        const box = vbox(`au-content au-${this._tab}`, {x_expand: true});
        if (this._lastError)
            this._buildFatal(box, this._lastError);
        else
            this._buildSkeleton(box);
        this._content.add_child(box);
        this._stateBox = box;
    }

    _clearState() {
        this._stateBox?.destroy();
        this._stateBox = null;
        this._skeleton = null;
    }

    // Bring one tab's page up to date, rebuilding only the sections whose fingerprint changed.
    _renderPage(tab, reason, built) {
        let page = this._pages[tab.key];
        if (!page) {
            page = {box: vbox(`au-content au-${tab.key}`, {x_expand: true}), sections: {}, missing: false};
            this._content.add_child(page.box);
            this._pages[tab.key] = page;
        }
        page.box.visible = true;
        const m = this._models?.[tab.key];
        if (!m) {
            if (!page.missing) {
                this._dropSections(page);
                page.box.add_child(this._notice('dialog-warning-symbolic', `No ${tab.name} data`,
                    `The last snapshot has no "${tab.key}" section.`, 'warn'));
                page.missing = true;
            }
            return;
        }
        if (page.missing) {
            this._dropSections(page);
            page.missing = false;
        }
        let above = null;
        for (const name of SECTIONS) {
            const key = this._sectionKey(name, tab, m);
            const old = page.sections[name];
            if (old && old.key === key) {
                above = old.actor;
                continue;
            }
            const rec = {key, actor: null, live: [], rings: []};
            this._liveFns = rec.live;
            this._rings = rec.rings;
            try {
                rec.actor = this._buildSection(name, tab, m, reason);
            } finally {
                this._liveFns = null;
                this._rings = null;
            }
            if (old) {
                for (const r of old.rings)
                    r.ring.finish();
                page.box.replace_child(old.actor, rec.actor);
                old.actor.destroy();
            } else if (above) {
                page.box.insert_child_above(rec.actor, above);
            } else {
                page.box.insert_child_at_index(rec.actor, 0);
            }
            page.sections[name] = rec;
            built.push(...rec.rings);
            above = rec.actor;
        }
    }

    _dropSections(page) {
        for (const rec of Object.values(page.sections)) {
            for (const r of rec.rings)
                r.ring.finish();
        }
        page.box.destroy_all_children();
        page.sections = {};
    }

    // What a section shows; equal fingerprints mean the section on screen is still right.
    _sectionKey(name, tab, m) {
        const loading = parts => parts.filter(p => this._pending.has(p));
        switch (name) {
        case 'banner':
            return JSON.stringify([m.level, m.summary, m.remote.rows.map(r => r.level)]);
        case 'account':
            return JSON.stringify([m.account, loading([`${tab.key}.account`])]);
        case 'limits':
            return JSON.stringify([m.usage, m.account.loggedIn, m.account.meta, loading([`${tab.key}.usage`])]);
        case 'tokens':
            return JSON.stringify([m.tokens, loading(['codex.tokens'])]);
        default:
            return JSON.stringify([m.remote, loading(['codex.relay', 'claude.drops', 'codex.events']),
                this._actionBusy, this._actionErr]);
        }
    }

    _buildSection(name, tab, m, reason) {
        switch (name) {
        case 'banner':
            return this._banner(m);
        case 'account':
            return this._account(m, tab);
        case 'limits': {
            const actor = this._limits(m, tab, reason);
            this._prevPct[tab.key] = new Map(m.usage.windows.map(w => [w.label, clamp(w.pct, 0, 100)]));
            return actor;
        }
        case 'tokens':
            return this._tokens(m);
        default:
            return this._remote(m);
        }
    }

    _pageSections() {
        return this._data ? Object.values(this._pages[this._tab]?.sections ?? {}) : [];
    }

    _allRings() {
        return Object.values(this._pages).flatMap(p => Object.values(p.sections).flatMap(s => s.rings));
    }

    _animateIn(reason, dir, built = []) {
        const c = this._content;
        const open = this.menu.isOpen;
        const target = this._stale ? DIM : 255;
        c.remove_all_transitions();
        c.translation_x = 0;
        c.translation_y = 0;
        if (open && reason === 'tab') {
            c.opacity = 0;
            c.translation_x = 24 * dir;
            c.ease({opacity: target, translation_x: 0, duration: 240, mode: EASE_OUT});
        } else if (open && (reason === 'data' || reason === 'error')) {
            c.opacity = Math.min(c.opacity, DIM);
            c.translation_y = 6;
            c.ease({opacity: target, translation_y: 0, duration: 300, mode: EASE_OUT});
        } else if (open && reason === 'patch') {
            // A later frame of the same run: no slide; pieces that just arrived fade in by themselves.
            c.ease({opacity: target, duration: 160, mode: EASE_OUT});
        } else {
            c.opacity = target;
        }
        // A tab switch sweeps the page's rings up from zero (kept pages included); otherwise only
        // rings built now move, from the value shown before.
        const rings = reason === 'tab'
            ? this._pageSections().flatMap(s => s.rings).map(r => ({ring: r.ring, from: 0}))
            : built;
        for (const {ring, from} of rings) {
            if (open)
                ring.animateFrom(from);
            else
                ring.finish();
        }
    }

    _updateLive() {
        for (const rec of this._pageSections()) {
            for (const fn of rec.live) {
                try {
                    fn();
                } catch (e) {
                    logError(e, 'agent-usage: live update');
                }
            }
        }
        this._updateFooter();
    }

    _live(fn) {
        this._liveFns?.push(fn);
    }

    _syncStatusMarks() {
        const levels = {};
        for (const t of TABS) {
            const m = this._models?.[t.key];
            levels[t.key] = m ? m.level : null;
            const dot = this._tabDots[t.key];
            const lv = levels[t.key];
            dot.visible = lv === 'warn' || lv === 'bad';
            if (dot.visible)
                setLevel(dot, lv);
        }
        // Panel badge: the worst state seen in the last snapshot (it only changes on a fetch).
        const lv = worst(Object.values(levels).filter(Boolean));
        this._badge.visible = lv === 'warn' || lv === 'bad';
        if (this._badge.visible)
            setLevel(this._badge, lv);
    }

    // Dim a piece whose data is still loading; fade in one whose data just arrived.
    _pendingFx(actor, parts = []) {
        if (parts.some(p => this._pending.has(p))) {
            actor.opacity = DIM;
            return true;
        }
        if (this.menu.isOpen && parts.some(p => this._freshened.has(p))) {
            actor.opacity = DIM;
            actor.ease({opacity: 255, duration: 280, mode: EASE_OUT});
        }
        return false;
    }

    _updateFooter() {
        const s = this._status;
        let text = '';
        let tone = null; // busy | ok | warn
        const note = this._actionMsg;
        if (note?.level === 'busy' || (note && !this._proc)) {
            text = note.text;
            tone = {busy: 'busy', ok: 'ok', bad: 'warn'}[note.level];
        } else if (this._proc) {
            text = this._data ? 'Updating…' : 'Loading…';
            tone = 'busy';
        } else if (this._fromDisk && this._data) {
            text = `Saved ${fmtClock(this._fetchedAt)} · opens update it`;
        } else if (this._lastError && this._data) {
            text = `${this._lastError.title} · showing ${fmtClock(this._fetchedAt)}`;
            tone = 'warn';
        } else if (this._lastError) {
            // The card above already names the problem; the footer just says when we tried.
            text = `Last attempt ${fmtClock(this._lastError.at)}`;
            tone = 'warn';
        } else if (this._data) {
            const age = nowS() - this._fetchedAt;
            const when = age < 45 ? `Updated ${fmtClock(this._fetchedAt)}` : `Updated ${fmtDuration(age)} ago`;
            text = this._fetchSecs ? `${when} · ${this._fetchSecs.toFixed(1)} s` : when;
        }
        if (s.text !== text)
            s.text = text;
        for (const t of ['busy', 'ok', 'warn'])
            setClass(s, `au-footer-${t}`, t === tone);
    }

    // ---------- tab content ----------

    _banner(m) {
        const b = hbox(`au-banner au-level-${m.level}`, {x_expand: true});
        const text = label(m.summary, 'au-banner-text', {x_expand: true});
        add(b, blank('au-banner-dot', {y_align: ALIGN.CENTER}), text);
        if (m.remote.rows.length) {
            const pips = hbox('au-pips', {y_align: ALIGN.CENTER});
            for (const r of m.remote.rows)
                pips.add_child(blank(`au-pip au-level-${r.level}`, {y_align: ALIGN.CENTER}));
            b.add_child(pips);
        }
        return b;
    }

    _account(m, tab) {
        const a = m.account;
        const row = hbox('au-account', {x_expand: true});
        this._pendingFx(row, [`${tab.key}.account`]);
        const avatar = new St.Bin({
            style_class: a.loggedIn ? 'au-avatar' : 'au-avatar au-avatar-off', y_align: ALIGN.CENTER,
            child: new St.Icon({gicon: this._icons[tab.key], style_class: 'au-avatar-icon', fallback_icon_name: tab.fallbackIcons[0]}),
        });
        const text = vbox('au-account-text', {x_expand: true, y_align: ALIGN.CENTER});
        const name = label(a.title, 'au-account-name');
        name.clutter_text.ellipsize = Pango.EllipsizeMode.MIDDLE;
        const meta = hbox('au-account-meta');
        if (a.plan)
            meta.add_child(label(a.plan, 'au-plan'));
        if (a.meta)
            meta.add_child(label(a.meta, 'au-account-sub', {x_expand: true}));
        add(text, name, meta.get_n_children() ? meta : null);
        const button = new St.Button({
            label: a.loggedIn ? 'Switch…' : 'Log in…',
            style_class: a.loggedIn ? 'au-button' : 'au-button au-button-accent',
            can_focus: true, y_align: ALIGN.CENTER,
            accessible_name: a.loggedIn ? `Switch ${tab.name} account` : `Log in to ${tab.name}`,
        });
        button.connect('clicked', () => this._openTerminal(tab.login));
        return add(row, avatar, text, button);
    }

    _section(title, aside, loading = false, stale = false) {
        const sec = vbox('au-section', {x_expand: true});
        const head = hbox('au-section-head', {x_expand: true});
        const asideClass = stale ? 'au-section-aside au-section-stale' : 'au-section-aside';
        add(head, label(title, 'au-section-title', {x_expand: true}),
            loading ? label('updating…', 'au-section-aside au-section-busy') : aside ? label(aside, asideClass) : null);
        sec.add_child(head);
        return sec;
    }

    _skeletonGauges(card) {
        const rings = homogeneous(hbox('au-gauge-row', {x_expand: true}));
        for (let i = 0; i < 2; i++) {
            rings.add_child(add(vbox('au-gauge-col', {x_expand: true}),
                new AuRing({style_class: 'au-ring au-ring-lg au-skel-ring', x_align: ALIGN.CENTER}),
                blank('au-skel au-skel-line-xs', {x_align: ALIGN.CENTER})));
        }
        card.add_child(rings);
    }

    _limits(m, tab, reason) {
        const parts = [`${tab.key}.usage`];
        const loading = parts.some(p => this._pending.has(p));
        const sec = this._section('Plan limits', null, loading);
        const card = vbox('au-card au-limits', {x_expand: true});
        const u = m.usage;
        this._pendingFx(card, parts);
        if (loading && u.missing) {
            this._skeletonGauges(card);
        } else if (u.off) {
            card.add_child(this._notice('dialog-information-symbolic', 'Live limits are off',
                'They need your Claude Code login, which Anthropic reserves for its own apps. To turn ' +
                'them on anyway, add "claude_live_limits": true to ~/.config/ai-tracker/config.json.', 'idle'));
        } else if (!m.account.loggedIn && (u.error || !u.windows.length)) {
            card.add_child(this._notice('system-lock-screen-symbolic', 'Signed out',
                u.error && u.error !== 'Not signed in to Claude Code' ? u.error : m.account.meta, 'bad'));
        } else if (u.error) {
            card.add_child(this._notice('dialog-warning-symbolic', 'Limits unavailable', u.error, 'warn'));
        } else if (!u.windows.length) {
            card.add_child(this._notice('object-select-symbolic', 'No active limit windows',
                'Nothing is counting toward a limit right now.', 'idle'));
        } else {
            if (u.staleAt) {
                card.add_child(this._notice('dialog-warning-symbolic', `Saved numbers from ${u.staleAt}`,
                    u.staleNote, 'warn'));
            }
            this._gauges(card, u.windows, tab, reason);
        }
        if (u.limitReached) {
            card.add_child(this._notice('dialog-error-symbolic', 'Rate limit reached',
                `Codex reports the ${u.limitReached} limit as reached.`, 'bad'));
        }
        for (const x of u.extras)
            card.add_child(this._extraRow(x));
        sec.add_child(card);
        return sec;
    }

    _gauges(card, windows, tab, reason) {
        const prev = this._prevPct[tab.key];
        const fromOf = w => {
            if (reason === 'tab')
                return 0;
            if (reason === 'data' || reason === 'patch')
                return prev.get(w.label) ?? 0;
            return clamp(w.pct, 0, 100);
        };
        if (windows.length === 1) {
            card.add_child(this._heroGauge(windows[0], fromOf(windows[0])));
            return;
        }
        const perRow = windows.length === 4 ? 2 : Math.min(3, windows.length);
        const size = windows.length <= 2 || windows.length === 4 ? 'au-ring-lg' : 'au-ring-md';
        const grid = vbox('au-gauge-grid', {x_expand: true});
        for (let i = 0; i < windows.length; i += perRow) {
            const row = homogeneous(hbox('au-gauge-row', {x_expand: true}));
            const slice = windows.slice(i, i + perRow);
            for (const w of slice)
                row.add_child(this._columnGauge(w, size, fromOf(w)));
            for (let k = slice.length; k < perRow; k++)
                row.add_child(blank('', {x_expand: true}));
            grid.add_child(row);
        }
        card.add_child(grid);
    }

    _ring(w, sizeClass, from) {
        const lv = w.pct >= 100 ? 'bad' : pctLevel(w.pct);
        const target = clamp(w.pct, 0, 100);
        const ring = new AuRing({
            style_class: `au-ring ${sizeClass} au-level-${lv}`, target,
            pace: paceInfo(w, nowS())?.elapsed ?? null,
        });
        const wide = Math.round(w.pct) >= 100 ? ' au-ring-pct-wide' : '';
        const pct = label(`${Math.round(w.pct)}%`, `au-ring-pct${wide}`, {x_align: ALIGN.CENTER});
        const unit = label(w.pct >= 100 ? 'limit' : 'used', 'au-ring-unit', {x_align: ALIGN.CENTER});
        const center = add(vbox('au-ring-center', {x_align: ALIGN.CENTER, y_align: ALIGN.CENTER}), pct, unit);
        const stack = new St.Widget({
            layout_manager: new Clutter.BinLayout(), style_class: `au-ring-stack ${sizeClass}-stack au-level-${lv}`,
            x_align: ALIGN.CENTER, y_align: ALIGN.CENTER,
        });
        add(stack, ring, center);
        ring.connect('notify::value', () => {
            pct.text = `${Math.round(ring.value >= target - 0.01 ? w.pct : ring.value)}%`;
        });
        this._rings?.push({ring, from});
        this._live(() => {
            ring.pace = paceInfo(w, nowS())?.elapsed ?? null;
        });
        return stack;
    }

    _levelTag(w) {
        if (w.pct >= 100)
            return label('Limit reached', 'au-tag au-level-bad');
        const lv = pctLevel(w.pct);
        if (lv === 'ok')
            return null;
        return label(lv === 'bad' ? 'Near limit' : 'Running high', `au-tag au-level-${lv}`);
    }

    _heroGauge(w, from) {
        const row = hbox('au-gauge-hero', {x_expand: true});
        const info = vbox('au-gauge-info', {x_expand: true, y_align: ALIGN.CENTER});
        const title = add(hbox('au-gauge-title'), label(w.label, 'au-gauge-label'), this._levelTag(w));
        const reset = label('', 'au-gauge-reset');
        const when = label('', 'au-gauge-when');
        const pace = label('', 'au-gauge-pace');
        this._live(() => {
            const p = paceInfo(w, nowS());
            reset.text = resetText(w);
            when.text = [
                w.resets_at ? fmtClock(w.resets_at) : null,
                p ? `${Math.round(p.elapsed * 100)}% of window elapsed` : null,
            ].filter(Boolean).join(' · ');
            when.visible = when.text !== '';
            const pt = paceText(w, p);
            pace.visible = pt !== null;
            if (pt) {
                pace.text = pt.text;
                setLevel(pace, pt.level);
            }
        });
        add(info, title, reset, when, pace);
        return add(row, this._ring(w, 'au-ring-xl', from), info);
    }

    _columnGauge(w, size, from) {
        const col = vbox('au-gauge-col', {x_expand: true});
        const name = label(w.label, 'au-gauge-label au-gauge-label-sm', {x_align: ALIGN.CENTER});
        const reset = label('', 'au-gauge-reset-sm', {x_align: ALIGN.CENTER});
        const extra = label('', 'au-gauge-pace-sm', {x_align: ALIGN.CENTER});
        this._live(() => {
            reset.text = resetText(w).replace(/^Resets in /, 'resets in ');
            const tag = w.pct >= 100 ? {level: 'bad', short: 'limit reached'} : paceText(w, paceInfo(w, nowS()));
            const show = tag !== null && tag.level !== 'ok';
            extra.visible = show;
            if (show) {
                extra.text = tag.short;
                setLevel(extra, tag.level);
            }
        });
        return add(col, this._ring(w, size, from), name, reset, extra);
    }

    _extraRow(x) {
        const row = hbox('au-extra', {x_expand: true});
        row.add_child(label(x.label, 'au-extra-label', {x_expand: x.kind !== 'bar'}));
        if (x.kind === 'bar') {
            row.add_child(new AuBar({
                style_class: `au-bar au-level-${pctLevel(x.pct)}`, pct: x.pct, x_expand: true, y_align: ALIGN.CENTER,
            }));
        }
        row.add_child(label(x.value, 'au-extra-value'));
        return row;
    }

    _notice(iconName, title, text, level) {
        const n = hbox(`au-notice au-level-${level}`, {x_expand: true});
        const body = vbox('au-notice-body', {x_expand: true, y_align: ALIGN.CENTER});
        add(body, label(title, 'au-notice-title'), text ? wrapped(text, 'au-notice-text') : null);
        return add(n, new St.Icon({icon_name: iconName, style_class: 'au-notice-icon', y_align: ALIGN.START}), body);
    }

    _tokens(m) {
        const t = m.tokens;
        // Claude's are counted locally (always in the first frame); Codex's come from its account.
        const parts = m.key === 'codex' ? ['codex.tokens'] : [];
        const loading = parts.some(p => this._pending.has(p));
        const sec = this._section(t?.title ?? 'Tokens', t?.aside, loading, !!t?.staleAt);
        const tiles = homogeneous(hbox('au-tiles', {x_expand: true}));
        if (!t && loading) {
            for (let i = 0; i < 3; i++)
                tiles.add_child(blank('au-skel au-skel-tile', {x_expand: true}));
            sec.add_child(tiles);
            return sec;
        }
        if (!t) {
            sec.add_child(label('Token stats are not available right now.', 'au-muted'));
            return sec;
        }
        this._pendingFx(tiles, parts);
        for (const tile of t.tiles) {
            tiles.add_child(add(vbox('au-tile', {x_expand: true}),
                label(tile.value, 'au-tile-value'),
                label(tile.caption, 'au-tile-caption')));
        }
        sec.add_child(tiles);
        return sec;
    }

    _remote(m) {
        const r = m.remote;
        const healthy = r.rows.filter(x => x.level === 'ok').length;
        const counted = r.rows.filter(x => x.level !== 'idle').length;
        const sec = this._section('Remote access', counted ? `${healthy} of ${counted} healthy` : null);
        const card = vbox('au-card au-remote', {x_expand: true});
        if (r.unavailable) {
            card.add_child(this._notice('dialog-warning-symbolic', 'Worker status unavailable',
                'The collector did not report the remote-access workers.', 'warn'));
        }
        for (const row of r.rows)
            card.add_child(this._workerRow(row));
        if (r.link) {
            const inner = add(hbox('au-link-inner', {x_align: ALIGN.CENTER}),
                new St.Icon({icon_name: 'send-to-symbolic', style_class: 'au-link-icon'}),
                label('Open environment in browser', 'au-link-label'));
            const open = new St.Button({child: inner, style_class: 'au-link', can_focus: true, x_expand: true});
            open.connect('clicked', () => this._openUri(r.link));
            card.add_child(add(hbox('au-link-row', {x_expand: true}), open, this._copyButton(r.link)));
        }
        sec.add_child(card);
        return sec;
    }

    _copyButton(text) {
        const icon = new St.Icon({icon_name: 'edit-copy-symbolic', style_class: 'au-link-icon'});
        const b = new St.Button({
            child: icon, style_class: 'au-link au-link-copy', can_focus: true, accessible_name: 'Copy link',
        });
        let resetId = 0;
        b.connect('clicked', () => {
            St.Clipboard.get_default().set_text(St.ClipboardType.CLIPBOARD, text);
            icon.icon_name = 'object-select-symbolic';
            b.add_style_class_name('au-link-copied');
            if (resetId)
                GLib.source_remove(resetId);
            resetId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 1400, () => {
                resetId = 0;
                icon.icon_name = 'edit-copy-symbolic';
                b.remove_style_class_name('au-link-copied');
                return GLib.SOURCE_REMOVE;
            });
        });
        b.connect('destroy', () => {
            if (resetId)
                GLib.source_remove(resetId);
            resetId = 0;
        });
        return b;
    }

    _workerRow(r) {
        const items = r.items ?? [];
        const tall = items.length || r.history || r.action;
        const align = tall ? ALIGN.START : ALIGN.CENTER;
        const row = hbox(`au-row au-level-${r.level}`, {x_expand: true});
        this._pendingFx(row, r.parts);
        const text = vbox('au-row-text', {x_expand: true, y_align: ALIGN.CENTER});
        add(text, label(r.name, 'au-row-name'), r.detail ? label(r.detail, 'au-row-detail') : null);
        for (const it of items.slice(0, 4))
            text.add_child(label(`• ${it}`, 'au-row-item'));
        if (items.length > 4)
            text.add_child(label(`+ ${items.length - 4} more`, 'au-row-item'));
        if (r.history)
            text.add_child(this._history(r.history));
        if (r.action)
            text.add_child(this._actionButton(r.action));
        const chip = add(hbox(`au-chip au-level-${r.level}`, {y_align: align}),
            blank('au-chip-dot', {y_align: ALIGN.CENTER}),
            label(r.chip ?? CHIP_TEXT[r.level], 'au-chip-label'));
        return add(row,
            new St.Icon({icon_name: r.icon, style_class: 'au-row-icon', y_align: align}),
            text, chip);
    }

    // "Up 6h 40m · 3 brief drops today · longest 14s · last 18:47" over a midnight → now strip.
    _history(h) {
        const box = vbox('au-history', {x_expand: true});
        const text = wrapped('', 'au-history-text');
        const strip = new AuTimeline({
            style_class: 'au-timeline', x_expand: true, start: localMidnight(), up: h.up, marks: h.marks,
        });
        const axis = add(hbox('au-timeline-axis', {x_expand: true}),
            label('00:00', 'au-timeline-tick', {x_expand: true}), label('now', 'au-timeline-tick'));
        add(box, text, strip, axis);
        this._live(() => {
            text.text = h.text(nowS());
            strip.tick();
        });
        this._pendingFx(box, h.parts);
        return box;
    }

    // A fix button. With `confirm`, the first click arms it (label turns into the warning) and a
    // second click within CONFIRM_MS runs it.
    _actionButton(a) {
        const button = this._actionButtonOnly(a);
        const err = this._actionErr?.id === a.id ? this._actionErr.text : null;
        if (!err)
            return button;
        return add(vbox('au-action-box', {x_expand: true}), button, wrapped(err, 'au-action-error'));
    }

    _actionButtonOnly(a) {
        const running = this._actionBusy === a.id;
        const b = new St.Button({
            label: running ? 'Working…' : a.label, style_class: 'au-row-action',
            can_focus: true, x_align: ALIGN.START, reactive: !this._actionBusy, accessible_name: a.label,
        });
        if (this._actionBusy)
            b.add_style_pseudo_class('insensitive');
        let armedId = 0;
        const disarm = () => {
            if (armedId)
                GLib.source_remove(armedId);
            armedId = 0;
            b.label = a.label;
            b.remove_style_class_name('au-row-action-armed');
        };
        b.connect('clicked', () => {
            if (this._actionBusy)
                return;
            if (a.confirm && !armedId) {
                b.label = a.confirm;
                b.add_style_class_name('au-row-action-armed');
                armedId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, CONFIRM_MS, () => {
                    armedId = 0;
                    disarm();
                    return GLib.SOURCE_REMOVE;
                });
                return;
            }
            disarm();
            this._runAction(a);
        });
        b.connect('destroy', () => {
            if (armedId)
                GLib.source_remove(armedId);
            armedId = 0;
        });
        return b;
    }

    // Run a fix command (systemctl --user ...), then re-check once. The user clicked for this;
    // nothing here repeats.
    _runAction(a) {
        let proc;
        try {
            proc = Gio.Subprocess.new(a.argv, Gio.SubprocessFlags.STDOUT_SILENCE | Gio.SubprocessFlags.STDERR_PIPE);
        } catch (e) {
            this._actionMsg = {text: `${a.label} failed: ${e.message ?? e}`, level: 'bad'};
            this._updateFooter();
            return;
        }
        this._actionBusy = a.id;
        this._actionErr = null;
        this._actionMsg = {text: `${a.label}…`, level: 'busy'};
        this._render('patch');
        proc.communicate_utf8_async(null, null, (p, res) => {
            let ok = false;
            let err = '';
            try {
                const [, , stderr] = p.communicate_utf8_finish(res);
                ok = p.get_successful();
                err = `${stderr ?? ''}`.trim().split('\n').pop() ?? '';
            } catch (e) {
                err = e.message ?? `${e}`;
            }
            if (this._destroyed)
                return;
            this._actionBusy = null;
            this._actionErr = ok ? null : {id: a.id, text: err || 'The command failed.'};
            this._actionMsg = ok
                ? {text: `${a.done} · checking…`, level: 'ok'}
                : {text: `${a.label} failed`, level: 'bad'};
            this._render('patch');
            if (ok)
                this._recheckLater();
        });
    }

    _recheckLater() {
        if (this._recheckId)
            GLib.source_remove(this._recheckId);
        this._recheckId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, RECHECK_MS, () => {
            this._recheckId = 0;
            if (this._destroyed)
                return GLib.SOURCE_REMOVE;
            if (this.menu.isOpen)
                this._refresh();
            else
                this._actionMsg = null;
            return GLib.SOURCE_REMOVE;
        });
    }

    // ---------- non-data states ----------

    _buildSkeleton(box) {
        const sk = vbox('au-skeleton', {x_expand: true});
        const line = cls => blank(`au-skel ${cls}`);
        sk.add_child(blank('au-skel au-skel-banner', {x_expand: true}));

        const acc = hbox('au-account', {x_expand: true});
        add(acc, blank('au-skel au-skel-avatar', {y_align: ALIGN.CENTER}),
            add(vbox('au-skel-lines', {x_expand: true, y_align: ALIGN.CENTER}),
                line('au-skel-line-lg'), line('au-skel-line-sm')),
            blank('au-skel au-skel-button', {y_align: ALIGN.CENTER}));
        sk.add_child(acc);

        const card = vbox('au-card au-limits', {x_expand: true});
        const rings = homogeneous(hbox('au-gauge-row', {x_expand: true}));
        for (let i = 0; i < 2; i++) {
            rings.add_child(add(vbox('au-gauge-col', {x_expand: true}),
                new AuRing({style_class: 'au-ring au-ring-lg au-skel-ring', x_align: ALIGN.CENTER}),
                blank('au-skel au-skel-line-xs', {x_align: ALIGN.CENTER})));
        }
        card.add_child(rings);
        sk.add_child(add(vbox('au-section', {x_expand: true}), line('au-skel-title'), card));

        const tiles = homogeneous(hbox('au-tiles', {x_expand: true}));
        for (let i = 0; i < 3; i++)
            tiles.add_child(blank('au-skel au-skel-tile', {x_expand: true}));
        sk.add_child(add(vbox('au-section', {x_expand: true}), line('au-skel-title'), tiles));

        const remote = vbox('au-card au-remote', {x_expand: true});
        for (let i = 0; i < 3; i++) {
            remote.add_child(add(hbox('au-row', {x_expand: true}),
                blank('au-skel au-skel-dot', {y_align: ALIGN.CENTER}),
                add(vbox('au-skel-lines', {x_expand: true, y_align: ALIGN.CENTER}),
                    line('au-skel-line-md'), line('au-skel-line-sm')),
                blank('au-skel au-skel-chip', {y_align: ALIGN.CENTER})));
        }
        sk.add_child(add(vbox('au-section', {x_expand: true}), line('au-skel-title'), remote));
        box.add_child(sk);
        this._skeleton = sk;
    }

    _buildFatal(box, err) {
        const card = vbox('au-empty', {x_expand: true});
        const retry = new St.Button({label: 'Try again', style_class: 'au-button au-button-accent', can_focus: true, x_align: ALIGN.CENTER});
        retry.connect('clicked', () => this._refresh());
        add(card,
            new St.Icon({icon_name: 'dialog-warning-symbolic', style_class: 'au-empty-icon', x_align: ALIGN.CENTER}),
            label(err.title, 'au-empty-title', {x_align: ALIGN.CENTER}),
            err.detail ? wrapped(err.detail, 'au-empty-text') : null,
            retry);
        box.add_child(card);
    }

    // ---------- actions (terminal logic kept from the original extension) ----------

    _openTerminal(argv) {
        const exe = GLib.build_filenamev([HOME, '.local', 'bin', argv[0]]);
        const cmd = [GLib.file_test(exe, GLib.FileTest.IS_EXECUTABLE) ? exe : argv[0], ...argv.slice(1)];
        let full;
        if (GLib.find_program_in_path('ghostty'))
            full = ['ghostty', '-e', ...cmd];
        else if (GLib.find_program_in_path('ptyxis'))
            full = ['ptyxis', '--', ...cmd];
        else
            full = ['gnome-terminal', '--', ...cmd];
        try {
            Gio.Subprocess.new(full, Gio.SubprocessFlags.NONE);
        } catch (e) {
            logError(e, 'agent-usage: could not open a terminal');
        }
        this.menu.close();
    }

    _openUri(uri) {
        try {
            Gio.AppInfo.launch_default_for_uri(uri, global.create_app_launch_context(0, -1));
        } catch (e) {
            logError(e, 'agent-usage: could not open link');
        }
        this.menu.close();
    }

    // ---------- teardown ----------

    _onDestroyed() {
        this._destroyed = true;
        this._stopTick();
        this._clearWatchdog();
        if (this._recheckId)
            GLib.source_remove(this._recheckId);
        this._recheckId = 0;
        this._snapCancel?.cancel();
        this._snapCancel = null;
        this._cancellable?.cancel();
        this._cancellable = null;
        try {
            this._proc?.force_exit();
        } catch {}
        this._proc = null;
        this._liveFns = null;
        this._rings = null;
        this._pages = {};
        this._skeleton = null;
    }
});

export function createIndicator(extension) {
    return new AgentUsageIndicator(extension);
}
