// Verification harness (runs only inside verify/run.sh's sandboxed headless shell).
// A loader exactly like the contract (dynamic import of ui.js + createIndicator),
// plus a driver that walks through scenarios and screenshots the menu.
import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Shell from 'gi://Shell';
import St from 'gi://St';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const HOME = GLib.get_home_dir();
const SC = `${HOME}/scenarios`;
const OUT = GLib.getenv('AU_OUT');
const log = (...a) => console.log('AUHARNESS', ...a);
const sleep = ms => new Promise(r => GLib.timeout_add(GLib.PRIORITY_DEFAULT, ms, () => { r(); return GLib.SOURCE_REMOVE; }));
const write = (p, s) => GLib.file_set_contents(p, s);
const setScenario = (name, delay = 0.25) => { write(`${SC}/current`, name); write(`${SC}/delay`, `${delay}`); };

async function shotActor(name, actor, pad = 12) {
    const [x, y] = actor.get_transformed_position();
    const [w, h] = actor.get_transformed_size();
    if (!(w >= 2 && h >= 2)) { log('skip-empty', name, w, h); return; }
    const sx = Math.max(0, Math.floor(x) - pad), sy = Math.max(0, Math.floor(y) - pad);
    const sw = Math.ceil(w) + 2 * pad, sh = Math.ceil(h) + 2 * pad;
    const file = Gio.File.new_for_path(`${OUT}/${name}.png`);
    const stream = file.replace(null, false, Gio.FileCreateFlags.REPLACE_DESTINATION, null);
    const shooter = new Shell.Screenshot();
    await new Promise((resolve, reject) => shooter.screenshot_area(sx, sy, sw, sh, stream, (o, r) => {
        try { o.screenshot_area_finish(r); resolve(); } catch (e) { reject(e); }
    }));
    stream.close(null);
    log('shot', name, sw, sh);
}

// Depth-first search for a visible St.Button whose label (or accessible name) matches.
function findButton(actor, re) {
    if (actor instanceof St.Button && actor.visible) {
        const text = `${actor.label ?? ''} ${actor.accessible_name ?? ''}`;
        if (re.test(text))
            return actor;
    }
    for (const c of actor.get_children()) {
        const b = findButton(c, re);
        if (b)
            return b;
    }
    return null;
}

const readText = p => { try { return new TextDecoder().decode(GLib.file_get_contents(p)[1]); } catch { return ''; } };

export default class HarnessExtension extends Extension {
    enable() {
        this._run().catch(e => { logError(e, 'AUHARNESS failed'); write(`${OUT}/DONE`, `error ${e}`); });
    }

    async _load(tag = '') {
        const mod = await import(`file://${this.path}/ui.js${tag}`);
        const ind = mod.createIndicator(this);
        Main.panel.addToStatusArea(`${this.uuid}${tag}`, ind);
        return ind;
    }

    async _run() {
        const checks = [];
        const check = (ok, what) => { checks.push(`${ok ? 'PASS' : 'FAIL'} ${what}`); log(ok ? 'PASS' : 'FAIL', what); };
        await sleep(2500);
        Main.overview.hide();
        await sleep(800);
        let ind = await this._load();
        const instrument = i => {
            const ts = () => GLib.DateTime.new_now_local().format('%T.%f').slice(0, 12);
            const r = i._refresh.bind(i), a = i._accept.bind(i), f = i._fail.bind(i);
            i._refresh = () => { log('refresh', ts(), 'inflight', i._proc !== null); r(); };
            i._accept = (d, s, p = [], reason = null) => { log('accept', ts(), s.toFixed(2), 'pending', JSON.stringify(p), reason ?? ''); a(d, s, p, reason); };
            i._fail = (t, d) => { log('fail', ts(), t); f(t, d); };
        };
        instrument(ind);
        await sleep(1000);
        const menu = () => ind.menu.actor;
        const open = () => ind.menu.open(false);
        const close = () => ind.menu.close(false);
        const tab = k => ind._selectTab(k);
        const both = async (name, wait = 1200) => {
            open(); await sleep(wait); await shotActor(`${name}-claude`, menu());
            tab('codex'); await sleep(900); await shotActor(`${name}-codex`, menu());
            tab('claude'); await sleep(300); close(); await sleep(300);
        };
        const runs = () => (readText(`${SC}/runs.log`).match(/^start/gm) ?? []).length;

        // 0. saved snapshot from disk: badge before any fetch, no collector run yet
        await shotActor('00-panel-from-disk', Main.panel.statusArea[this.uuid], 6);
        check(ind._badge.visible && ind._fromDisk, 'badge shows from the saved snapshot at startup');
        check(runs() === 0, 'no collector run before the first open');
        // 1. first open: the saved snapshot instantly (dimmed), then the stream patches it
        setScenario('stream');
        open(); await sleep(30); await shotActor('01-open-instant', menu());
        await sleep(400); await shotActor('02-stream-frame1-claude', menu());
        check(ind._pending.has('claude.usage') && ind._pending.has('codex.tokens'), 'first frame marks usage/tokens as loading');
        await sleep(1200); await shotActor('03-stream-frame2-claude', menu());
        check(!ind._pending.has('claude.usage') && ind._pending.has('codex.usage'), 'Claude limits land before Codex');
        tab('codex'); await sleep(300); await shotActor('04-stream-frame2-codex', menu());
        await sleep(1300); await shotActor('05-stream-done-codex', menu());
        check(ind._pending.size === 0 && ind._proc === null && !ind._lastError, 'stream completes');
        tab('claude'); await sleep(900); await shotActor('06-history-claude', menu());
        close(); await sleep(300);
        check(runs() === 1 && readText(`${SC}/runs.log`).includes('--stream'), 'one --stream run per open');

        // 2. existing states (single-frame collector output)
        setScenario('healthy'); await both('07-healthy');
        setScenario('two', 1.0);
        open(); await sleep(350); await shotActor('08-stale-updating', menu());
        await sleep(1300); await shotActor('09-two-claude', menu()); close(); await sleep(300);
        setScenario('heavy'); await both('10-heavy');
        await shotActor('10-panel-badge', Main.panel.statusArea[this.uuid], 6);
        setScenario('loggedout'); await both('11-loggedout');
        setScenario('apierror'); await both('12-apierror');
        setScenario('many'); await both('13-many');
        setScenario('partial'); await both('14-partial');
        setScenario('history'); await both('15-history');
        setScenario('codexstale'); open(); await sleep(900); tab('codex'); await sleep(900);
        await shotActor('15c-codex-saved-values', menu());
        check(!!ind._models.codex.tokens?.staleAt && !!ind._models.codex.usage?.staleAt &&
              ind._models.codex.usage.windows.length > 0, 'Codex limits and tokens keep their saved values when a fetch fails');
        tab('claude'); await sleep(300); close(); await sleep(300);
        setScenario('ultra'); open(); await sleep(1200); await shotActor('15d-claude-ultra', menu());
        check(ind._models.claude.account.plan === 'Ultra' &&
              ind._models.claude.usage.windows.map(w => w.label).join() === 'Weekly,Weekly · Fable', 'Ultra plan with its weekly Fable limit');
        close(); await sleep(300);
        setScenario('codexdesktop'); open(); await sleep(900); tab('codex'); await sleep(900);
        await shotActor('15e-codex-desktop-holds-link', menu());
        const svcRow = ind._models.codex.remote.rows.find(r => r.name === 'Background service');
        check(svcRow?.level === 'ok' && /via Codex Desktop/.test(svcRow?.detail) && !svcRow?.action &&
              ind._models.codex.level === 'ok', 'phone link held by Codex Desktop: healthy, no restart offered');
        tab('claude'); await sleep(300); close(); await sleep(300);
        setScenario('claudeoff'); open(); await sleep(1200); await shotActor('15b-claude-limits-off', menu());
        check(ind._models.claude.usage.off === true && ind._models.claude.level === 'ok', 'live Claude limits off: calm notice, no warning');
        close(); await sleep(300);

        // 3. fix buttons (fake systemctl records the commands)
        setScenario('fixes', 0.2);
        open(); await sleep(1200); await shotActor('16-fixes-claude', menu());
        let b = findButton(menu(), /Restart/);
        check(b !== null, 'Claude tab offers a restart while the server reconnects');
        b?.emit('clicked', 1); await sleep(200); await shotActor('17-armed', menu());
        check(/click again/.test(b?.label ?? ''), 'two-step button arms on the first click');
        check(!readText(`${SC}/actions.log`).includes('restart claude'), 'armed click runs nothing');
        b?.emit('clicked', 1); await sleep(150); await shotActor('18-working', menu());
        check(ind._actionBusy === 'claude-restart', 'second click runs the command');
        await sleep(700); await shotActor('19-done-checking', menu());
        check(readText(`${SC}/actions.log`).includes(`systemctl --user restart claude-remote-control.service`), 'restart command ran');
        const before = runs();
        await sleep(3200); await shotActor('20-rechecked', menu());
        check(runs() === before + 1, 'exactly one re-check after the fix');
        tab('codex'); await sleep(900); await shotActor('21-fixes-codex', menu());
        write(`${SC}/action-fail`, '1');
        b = findButton(menu(), /Fix now/);
        b?.emit('clicked', 1); await sleep(900); await shotActor('22-fix-failed', menu());
        check(/failed/.test(ind._status.text) && ind._actionErr?.text.includes('simulated'), 'a failed fix shows in the footer and under its button');
        GLib.unlink(`${SC}/action-fail`);
        tab('claude'); await sleep(300);
        b = findButton(menu(), /Copy link/);
        b?.emit('clicked', 1); await sleep(150); await shotActor('23-copied', menu());
        close(); await sleep(400);

        // 4. collector dies after the first frame: keep the data, footer warns, nothing stays dim
        setScenario('dies'); open(); await sleep(900); await shotActor('24-died-midway', menu());
        check(ind._pending.size === 0 && ind._lastError !== null, 'partial run ends cleanly');
        close(); await sleep(300);
        // failed refresh while a snapshot exists: keep it, footer warns
        setScenario('garbage'); open(); await sleep(1200); await shotActor('25-refresh-failed', menu()); close(); await sleep(300);

        // 5. destroy + re-import with a cache-busting URL (GType names must not clash), no snapshot
        ind.destroy(); await sleep(300);
        GLib.unlink(`${HOME}/.cache/agent-usage/last-snapshot.json`);
        GLib.mkdir_with_parents(`${this.path}/icons`, 0o755);
        for (const f of ['agent-usage-symbolic.svg', 'claude-symbolic.svg', 'codex-symbolic.svg'])
            Gio.File.new_for_path(`${HOME}/icons-src/${f}`).copy(Gio.File.new_for_path(`${this.path}/icons/${f}`), Gio.FileCopyFlags.OVERWRITE, null, null);
        ind = await this._load('?v=2');
        instrument(ind);
        await sleep(600);
        check(!ind._badge.visible && !ind._data, 'no snapshot on disk: no badge, no data');
        // first ever run: skeletons for the parts still loading
        setScenario('firstever');
        open(); await sleep(400); await shotActor('26-firstever-claude', menu());
        tab('codex'); await sleep(300); await shotActor('27-firstever-codex', menu());
        await sleep(1500); await shotActor('28-firstever-done-codex', menu());
        tab('claude'); close(); await sleep(300);
        ind.destroy(); await sleep(300);
        ind = await this._load('?v=3');
        instrument(ind);
        await sleep(600);
        setScenario('healthy', 2.5);
        open(); await sleep(500); await shotActor('29-skeleton', menu()); await sleep(2600); close(); await sleep(300);
        setScenario('garbage'); open(); await sleep(1200); await shotActor('30-fatal-garbage', menu()); close(); await sleep(300);
        // 6. collector missing
        GLib.rename(`${HOME}/.local/bin/agent-usage`, `${HOME}/.local/bin/agent-usage.off`);
        open(); await sleep(800); await shotActor('31-fatal-missing', menu()); close(); await sleep(200);
        GLib.rename(`${HOME}/.local/bin/agent-usage.off`, `${HOME}/.local/bin/agent-usage`);
        // 7. recovery + Refresh through the real button while open
        setScenario('healthy', 0.2); open(); await sleep(1600); await shotActor('32-recovered', menu());
        setScenario('two', 1.0); ind._refreshButton.emit('clicked', 1); await sleep(250); await shotActor('33-refresh-click', menu());
        await sleep(1600); await shotActor('34-refresh-done', menu()); close();
        setScenario('heavy', 0.2); await both('35-heavy-icons', 1300);
        await shotActor('35-panel-badge-icons', Main.panel.statusArea[`${this.uuid}?v=3`], 6);
        // 8. destroy while a stream is in flight
        setScenario('stream'); open(); await sleep(300); ind.destroy(); await sleep(1500);
        log('destroy-in-flight ok');
        write(`${OUT}/checks.txt`, checks.join('\n') + '\n');
        write(`${OUT}/DONE`, checks.some(c => c.startsWith('FAIL')) ? 'checks failed' : 'ok');
        log('done');
    }

    disable() {}
}
