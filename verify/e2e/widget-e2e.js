// End-to-end tests of the widget (extension/ai-tracker@danielmapar/ui.js) in a real GNOME Shell 50.
// Runs only inside verify/run.sh's sandbox (started by verify/e2e.sh): headless shell, throwaway
// HOME, a fake collector serving made-up scenarios, and fake systemctl / systemd-run / terminals
// that record what they were asked to run. Nothing here signs in or out, opens a browser or
// touches a real service.
//
// Each test drives the widget like a person would (open the menu, switch tabs, click buttons) and
// checks what is on screen and which commands were run. Results: e2e-results.json, checks.txt and
// one screenshot per test in the output folder.
import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Shell from 'gi://Shell';
import St from 'gi://St';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const HOME = GLib.get_home_dir();
const SC = `${HOME}/scenarios`;
const OUT = GLib.getenv('AU_OUT');
const log = (...a) => console.log('AUE2E', ...a);
const sleep = ms => new Promise(r => GLib.timeout_add(GLib.PRIORITY_DEFAULT, ms, () => { r(); return GLib.SOURCE_REMOVE; }));
const write = (p, s) => GLib.file_set_contents(p, s);
const readText = p => { try { return new TextDecoder().decode(GLib.file_get_contents(p)[1]); } catch { return ''; } };
const exists = p => GLib.file_test(p, GLib.FileTest.EXISTS);

// ---------- scenarios and recorded commands ----------

const scenario = (name, delay = 0.15) => { write(`${SC}/current`, name); write(`${SC}/delay`, `${delay}`); };
// A scenario made from another one, changed by `mutate(data)`.
function custom(name, base, mutate) {
    const d = JSON.parse(readText(`${SC}/${base}.json`));
    mutate(d);
    write(`${SC}/${name}.json`, JSON.stringify(d));
    return name;
}
const runs = () => (readText(`${SC}/runs.log`).match(/^start/gm) ?? []).length;
const actions = () => readText(`${SC}/actions.log`);
const clearActions = () => { write(`${SC}/actions.log`, ''); if (exists(`${SC}/terminal-last.args`)) GLib.unlink(`${SC}/terminal-last.args`); };
const terminalArgs = () => readText(`${SC}/terminal-last.args`).split('\0').slice(0, -1);
const nowS = () => Math.floor(Date.now() / 1000);
const clock = s => GLib.DateTime.new_from_unix_local(s).format('%H:%M');

// ---------- assertions ----------

class Failure extends Error {}
const ok = (cond, what) => { if (!cond) throw new Failure(what); };
const eq = (got, want, what) => {
    if (JSON.stringify(got) !== JSON.stringify(want))
        throw new Failure(`${what}: got ${JSON.stringify(got)}, want ${JSON.stringify(want)}`);
};

async function waitFor(cond, what, ms = 6000) {
    const end = Date.now() + ms;
    while (!cond()) {
        if (Date.now() > end)
            throw new Failure(`timed out waiting for: ${what}`);
        await sleep(25);
    }
}

// ---------- reading the screen ----------

function walk(actor, fn) {
    fn(actor);
    for (const c of actor.get_children())
        walk(c, fn);
}

// Every label a person can see in `root`, in order.
function visibleTexts(root) {
    const out = [];
    walk(root, a => {
        if (a instanceof St.Label && a.mapped && a.text)
            out.push(a.text);
        else if (a instanceof St.Button && a.mapped && a.label) // its label child is internal
            out.push(a.label);
    });
    return out;
}

function buttonsIn(root, re) {
    const out = [];
    walk(root, a => {
        if (a instanceof St.Button && a.mapped && [a.label ?? '', a.accessible_name ?? '', ...visibleTexts(a)].some(t => re.test(t)))
            out.push(a);
    });
    return out;
}

async function shot(name, actor) {
    const [x, y] = actor.get_transformed_position();
    const [w, h] = actor.get_transformed_size();
    if (!(w >= 2 && h >= 2))
        return;
    const file = Gio.File.new_for_path(`${OUT}/${name}.png`);
    const stream = file.replace(null, false, Gio.FileCreateFlags.REPLACE_DESTINATION, null);
    const pad = 10;
    await new Promise((resolve, reject) => new Shell.Screenshot().screenshot_area(
        Math.max(0, Math.floor(x) - pad), Math.max(0, Math.floor(y) - pad), Math.ceil(w) + 2 * pad, Math.ceil(h) + 2 * pad,
        stream, (o, r) => { try { o.screenshot_area_finish(r); resolve(); } catch (e) { reject(e); } }));
    stream.close(null);
}

function runCommand(argv) {
    const p = Gio.Subprocess.new(argv, Gio.SubprocessFlags.STDIN_PIPE | Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_MERGE);
    return new Promise((resolve, reject) => p.communicate_utf8_async('', null, (proc, res) => {
        try {
            const [, out] = proc.communicate_utf8_finish(res);
            resolve({status: proc.get_exit_status(), out: out ?? ''});
        } catch (e) {
            reject(e);
        }
    }));
}

const clipboardText = () => new Promise(r => St.Clipboard.get_default().get_text(St.ClipboardType.CLIPBOARD, (_c, t) => r(t)));

// ---------- the suite ----------

const TESTS = [];
const test = (group, name, fn, timeout = 20000) => TESTS.push({group, name, fn, timeout});

export default class E2EExtension extends Extension {
    enable() {
        this._main().catch(e => { logError(e, 'AUE2E crashed'); write(`${OUT}/DONE`, `error ${e}`); });
    }

    disable() {}

    async _load(tag = '') {
        const mod = await import(`file://${this.path}/ui.js${tag}`);
        const ind = mod.createIndicator(this);
        Main.panel.addToStatusArea(`${this.uuid}${tag}`, ind);
        // timing of every frame the widget draws, for the performance tests
        const accept = ind._accept.bind(ind);
        ind.__frameMs = [];
        ind._accept = (...a) => {
            const t0 = GLib.get_monotonic_time();
            accept(...a);
            ind.__frameMs.push((GLib.get_monotonic_time() - t0) / 1000);
        };
        return ind;
    }

    // Helpers bound to the indicator under test.
    _ctx() {
        const self = this;
        const ind = () => self.ind;
        const menu = () => ind().menu.actor;
        const c = {
            get ind() { return ind(); },
            menu,
            text: () => visibleTexts(menu()).join('\n'),
            has: re => re.test(visibleTexts(menu()).join('\n')),
            button: re => buttonsIn(menu(), re)[0] ?? null,
            buttons: re => buttonsIn(menu(), re),
            open: () => ind().menu.open(false),
            close: () => ind().menu.close(false),
            tab: k => ind().selectTab ? ind().selectTab(k) : ind()._selectTab(k),
            model: k => ind()._models?.[k],
            row: (k, name) => ind()._models?.[k]?.remote.rows.find(r => r.name === name),
            idle: () => waitFor(() => ind()._proc === null, 'the collector run to finish', 30000),
            // Open the menu on scenario `name` and wait until its run has finished and drawn.
            async settle() {
                await sleep(300);
                for (const r of c.ind._allRings()) // rings count up when drawn: read final values
                    r.ring.finish();
                await sleep(150);
            },
            async show(name, tabKey = 'claude', delay = 0.1) {
                c.close(); // an open menu does not fetch again
                await c.idle();
                scenario(name, delay);
                c.tab(tabKey);
                c.open();
                await c.idle();
                await c.settle();
            },
            async click(re, what) {
                const b = c.button(re);
                ok(b, `no button matching ${re} (${what ?? ''})`);
                b.emit('clicked', 1);
                await sleep(60);
                return b;
            },
            expectText(re, what) { ok(c.has(re), `${what ?? 'text'}: ${re} not on screen. Visible: ${c.text().replace(/\n/g, ' | ').slice(0, 700)}`); },
            expectNoText(re, what) { ok(!c.has(re), `${what ?? 'text'}: ${re} should not be on screen`); },
        };
        return c;
    }

    async _reset(c) {
        c.close();
        await waitFor(() => c.ind._proc === null && !c.ind._actionBusy && !c.ind._recheckId, 'widget to settle', 12000);
        c.tab('claude');
        clearActions();
        if (exists(`${SC}/action-fail`))
            GLib.unlink(`${SC}/action-fail`);
        await sleep(150);
    }

    async _main() {
        await sleep(2500);
        Main.overview.hide();
        await sleep(800);
        this.ind = await this._load();
        await sleep(800);
        defineTests(this);
        // AU_E2E_ONLY=<regex>: run only the tests whose "group › name" matches (sh verify/e2e.sh)
        const only = GLib.getenv('AU_E2E_ONLY');
        if (only) {
            const re = new RegExp(only, 'i');
            TESTS.splice(0, TESTS.length, ...TESTS.filter(t => re.test(`${t.group} › ${t.name}`)));
        }
        const c = this._ctx();
        const results = [];
        let i = 0;
        for (const t of TESTS) {
            i++;
            const t0 = Date.now();
            let status = 'PASS', error = null;
            try {
                await Promise.race([
                    t.fn(c),
                    sleep(t.timeout).then(() => { throw new Failure(`test took longer than ${t.timeout / 1000} s`); }),
                ]);
            } catch (e) {
                if (e?.skip) {
                    status = 'SKIP';
                    error = e.message;
                } else {
                    status = 'FAIL';
                    error = e instanceof Failure ? e.message : `${e}\n${e?.stack ?? ''}`;
                }
            }
            const slug = `${String(i).padStart(2, '0')}-${t.name.toLowerCase().replace(/[^a-z0-9]+/g, '-').slice(0, 60)}`;
            try {
                if (this.ind?.menu?.isOpen)
                    await shot(slug, this.ind.menu.actor);
            } catch {}
            results.push({group: t.group, name: t.name, status, ms: Date.now() - t0, error});
            log(status, `${t.group} › ${t.name}`, error ?? '');
            try {
                await this._reset(c);
            } catch (e) {
                log('reset failed', `${e}`);
            }
        }
        write(`${OUT}/e2e-results.json`, JSON.stringify(results, null, 1));
        write(`${OUT}/checks.txt`, results.map(r => `${r.status} ${r.group} › ${r.name}${r.error ? `\n    ${r.error.split('\n')[0]}` : ''}`).join('\n') + '\n');
        write(`${OUT}/DONE`, results.some(r => r.status === 'FAIL') ? 'checks failed' : 'ok');
        log('done');
    }
}

const skip = msg => Object.assign(new Error(msg), {skip: true});

function defineTests(ext) {
    // ---------------- startup and fetch policy ----------------
    test('Startup', 'shows the saved snapshot\'s warning dot at login, without fetching', async c => {
        ok(c.ind._fromDisk, 'snapshot loaded from disk');
        ok(c.ind._badge.visible, 'panel dot visible');
        ok(c.ind._badge.has_style_class_name('au-level-bad'), 'panel dot is red for the saved trouble');
        eq(runs(), 0, 'collector runs before any click');
    });

    test('Startup', 'the first open shows the saved numbers instantly, dimmed, while they update', async c => {
        scenario('stream');
        c.open();
        await sleep(40);
        ok(c.ind._data !== null, 'data on screen at once');
        ok(c.ind._content.opacity < 255, 'saved snapshot dimmed while refreshing');
        await waitFor(() => c.ind._pending.size > 0, 'first streamed frame');
        c.expectText(/updating…/, 'loading parts marked');
        await c.idle();
    });

    test('Fetch policy', 'opening the menu runs the collector exactly once, in streaming mode', async c => {
        const before = runs();
        await c.show('healthy');
        eq(runs() - before, 1, 'runs per open');
        ok(/start healthy --stream/.test(readText(`${SC}/runs.log`)), 'run used --stream');
    });

    test('Fetch policy', 'nothing runs while the menu stays closed: no fetch, no timers', async c => {
        await c.show('healthy');
        c.close();
        const before = runs();
        await sleep(6000);
        eq(runs(), before, 'collector runs while closed');
        eq([c.ind._tickId, c.ind._watchdogId, c.ind._recheckId], [0, 0, 0], 'timers left running (tick, watchdog, re-check)');
    }, 15000);

    test('Fetch policy', 'reopening while a run is still going does not start a second one', async c => {
        scenario('healthy', 1.2);
        const before = runs();
        c.open(); await sleep(150); c.close(); await sleep(100); c.open();
        await c.idle();
        eq(runs() - before, 1, 'runs');
    });

    test('Fetch policy', 'Refresh runs it again once, even when clicked twice quickly', async c => {
        await c.show('healthy');
        scenario('two', 0.6);
        const before = runs();
        c.ind._refreshButton.emit('clicked', 1);
        await sleep(50);
        c.ind._refreshButton.emit('clicked', 1);
        await c.idle();
        await c.settle();
        eq(runs() - before, 1, 'runs after two clicks');
        c.expectText(/38%/, 'new numbers after the refresh');
    });

    test('Fetch policy', 'the countdown text ticks only while the menu is open', async c => {
        await c.show('healthy');
        ok(c.ind._tickId !== 0, 'tick running while open');
        c.close();
        await sleep(100);
        eq(c.ind._tickId, 0, 'tick after closing');
    });

    // ---------------- streaming and errors ----------------
    test('Streaming', 'parts arrive one by one: Claude limits first, Codex after, then done', async c => {
        scenario('stream');
        c.open();
        await waitFor(() => c.ind._pending.has('claude.usage'), 'first frame');
        await waitFor(() => !c.ind._pending.has('claude.usage'), 'Claude limits');
        ok(c.ind._pending.has('codex.usage'), 'Codex still loading after Claude landed');
        await c.idle();
        eq(c.ind._pending.size, 0, 'parts still loading at the end');
        ok(!c.ind._lastError, 'no error');
        c.expectText(/^Updated \d\d:\d\d · \d+\.\d s$/m, 'footer');
    });

    test('Streaming', 'the collector dies mid-run: the data stays, nothing stays dimmed, the footer says so', async c => {
        await c.show('dies');
        eq(c.ind._pending.size, 0, 'parts left loading');
        eq(c.ind._content.opacity, 255, 'content opacity');
        c.expectText(/Collector stopped early/, 'footer warning');
    });

    test('Streaming', 'garbage output: the last good data stays, the footer warns', async c => {
        await c.show('healthy');
        await c.show('garbage');
        ok(c.ind._data !== null, 'data kept');
        ok(c.ind._lastError !== null, 'error recorded');
        c.expectText(/user@example\.com/, 'last data still shown');
        ok(c.ind._status.has_style_class_name('au-footer-warn'), 'footer in warning colour');
    });

    test('Streaming', 'a hung collector is stopped after 25 s and the footer says it timed out', async c => {
        scenario('healthy', 60);
        c.open();
        await waitFor(() => c.ind._proc === null, 'the watchdog to stop the run', 32000);
        ok(/timed out/.test(c.ind._lastError?.title ?? ''), `error title: ${c.ind._lastError?.title}`);
        c.expectText(/Collector timed out/, 'footer');
        // the fake collector's child still holds the pipe: Refresh must work anyway
        scenario('two', 0.1);
        const before = runs();
        c.ind._refreshButton.emit('clicked', 1);
        await c.idle();
        eq(runs() - before, 1, 'Refresh right after a timeout');
        ok(!c.ind._lastError, 'error cleared by the next good run');
    }, 45000);

    // ---------------- tabs ----------------
    test('Tabs', 'Claude shows first; the Codex tab shows Codex, and back', async c => {
        await c.show('healthy');
        ok(c.ind._tabButtons.claude.checked && !c.ind._tabButtons.codex.checked, 'Claude tab selected');
        c.expectText(/claude\.ai login/, 'Claude page');
        c.tab('codex'); await sleep(500);
        ok(c.ind._tabButtons.codex.checked, 'Codex tab selected');
        c.expectText(/ChatGPT login/, 'Codex page');
        c.expectNoText(/claude\.ai login/, 'Claude page hidden');
        c.tab('claude'); await sleep(500);
        c.expectText(/claude\.ai login/, 'back on Claude');
    });

    test('Tabs', 'a tab with trouble gets a dot; a healthy one does not', async c => {
        await c.show('fixes');
        ok(c.ind._tabDots.claude.visible && c.ind._tabDots.codex.visible, 'dots on both troubled tabs');
        await c.show('healthy');
        ok(!c.ind._tabDots.claude.visible && !c.ind._tabDots.codex.visible, 'no dots when healthy');
    });

    // ---------------- account ----------------
    test('Account', 'shows the email, plan and login method on each tab', async c => {
        await c.show('healthy');
        c.expectText(/user@example\.com/); c.expectText(/^Pro$/m, 'Claude plan'); c.expectText(/claude\.ai login/);
        c.tab('codex'); await sleep(500);
        c.expectText(/^Plus$/m, 'Codex plan'); c.expectText(/ChatGPT login/);
    });

    test('Account', 'plan names: Ultra (renamed Max 20x) and Max', async c => {
        await c.show('ultra');
        c.expectText(/^Ultra$/m, 'Ultra');
        await c.show(custom('plan-max', 'healthy', d => { d.claude.account.plan = 'max'; }));
        c.expectText(/^Max$/m, 'Max');
    });

    test('Account', 'signed out: "Log in…" opens the login command in a terminal at once', async c => {
        await c.show('loggedout');
        c.expectText(/Not signed in/);
        await c.click(/^Log in…/, 'Claude log in');
        await waitFor(() => exists(`${SC}/terminal-last.args`), 'a terminal');
        eq(terminalArgs().slice(1), ['-e', 'claude', 'auth', 'login'], 'terminal command');
        ok(!c.ind.menu.isOpen, 'menu closed after launching');
    });

    test('Account', 'signed out of Codex: "Log in…" runs codex login', async c => {
        await c.show('loggedout', 'codex');
        await c.click(/^Log in…/, 'Codex log in');
        await waitFor(() => exists(`${SC}/terminal-last.args`), 'a terminal');
        eq(terminalArgs().slice(1), ['-e', 'codex', 'login'], 'terminal command');
    });

    test('Account', '"Switch…" asks first: the first click warns and runs nothing', async c => {
        await c.show('healthy');
        const b = await c.click(/^Switch…/, 'Switch');
        eq(b.label, 'Confirm', 'armed label');
        ok(b.has_style_class_name('au-button-armed'), 'armed style');
        c.expectText(/Signs out every Claude Code session on this computer, then opens sign-in\. Click Confirm to go ahead\./, 'warning');
        await sleep(300);
        ok(!exists(`${SC}/terminal-last.args`), 'a terminal opened on the first click');
        ok(c.ind.menu.isOpen, 'menu stays open');
    });

    test('Account', 'an armed "Switch…" goes back to normal after 4 s', async c => {
        await c.show('healthy');
        const b = await c.click(/^Switch…/, 'Switch');
        await sleep(4300);
        eq(b.label, 'Switch…', 'label after the timeout');
        c.expectNoText(/Click Confirm/, 'warning gone');
        c.expectText(/claude\.ai login/, 'account details back');
    });

    test('Account', 'closing the menu disarms an armed "Switch…"', async c => {
        await c.show('healthy');
        await c.click(/^Switch…/);
        c.close(); await sleep(100);
        c.open(); await c.idle(); await sleep(300);
        ok(c.button(/^Switch…/), '"Switch…" back after reopening');
        c.expectNoText(/Click Confirm/, 'warning gone');
        await c.click(/^Switch…/);
        await sleep(200);
        ok(!exists(`${SC}/terminal-last.args`), 'one click after reopening opened a terminal');
    });

    test('Account', 'confirming opens a terminal that signs out of Claude, then signs in', async c => {
        await c.show('healthy');
        await c.click(/^Switch…/);
        await c.click(/^Confirm$/);
        await waitFor(() => exists(`${SC}/terminal-last.args`), 'a terminal');
        const a = terminalArgs();
        eq(a.slice(1, 4), ['-e', 'sh', '-c'], 'terminal runs a shell script');
        ok(/"\$0" auth logout/.test(a[4]) && /"\$0" auth login/.test(a[4]), 'script signs out, then in');
        ok(a[4].indexOf('auth logout') < a[4].lastIndexOf('"$0" auth login'), 'sign-out comes first');
        eq(a[5], 'claude', 'the script\'s tool');
        ok(!c.ind.menu.isOpen, 'menu closed');
    });

    test('Account', 'the Codex switch signs out of Codex, then signs in', async c => {
        await c.show('healthy', 'codex');
        await c.click(/^Switch…/);
        c.expectText(/Signs out Codex on this computer \(phone access too\), then opens sign-in\./);
        await c.click(/^Confirm$/);
        await waitFor(() => exists(`${SC}/terminal-last.args`), 'a terminal');
        const a = terminalArgs();
        ok(/"\$0" logout/.test(a[4]) && /"\$0" login/.test(a[4]), 'codex logout, then login');
        eq(a[5], 'codex', 'tool');
    });

    test('Account', 'the switch script, run against a fake tool: signs out then in; stops if sign-out fails', async c => {
        await c.show('healthy');
        await c.click(/^Switch…/);
        await c.click(/^Confirm$/);
        await waitFor(() => exists(`${SC}/terminal-last.args`), 'a terminal');
        const script = terminalArgs()[4];
        const tool = `${HOME}/fake-claude`;
        const calls = `${HOME}/fake-claude.calls`;
        write(tool, `#!/bin/sh\necho "$*" >> ${calls}\n[ "$*" = "auth logout" ] && [ -f ${HOME}/fake-fail ] && exit 1\nexit 0\n`);
        GLib.chmod(tool, 0o755);
        write(calls, '');
        let r = await runCommand(['sh', '-c', script, tool]);
        eq(readText(calls).trim().split('\n'), ['auth logout', 'auth login'], 'commands run');
        ok(/choose the account you want/.test(r.out), 'tells the user to pick the account');
        write(calls, '');
        write(`${HOME}/fake-fail`, '1');
        r = await runCommand(['sh', '-c', script, tool]);
        eq(readText(calls).trim().split('\n'), ['auth logout'], 'commands when sign-out fails');
        ok(/Sign-out failed/.test(r.out) && r.status !== 0, 'reports the failure');
        GLib.unlink(`${HOME}/fake-fail`);
    });

    // ---------------- plan limits ----------------
    test('Limits', 'one ring per window, with its percentage, label and reset countdown', async c => {
        await c.show('two');
        eq(c.model('claude').usage.windows.map(w => w.label), ['5-hour', 'Weekly'], 'windows');
        c.expectText(/^38%$/m); c.expectText(/^24%$/m);
        c.expectText(/^5-hour$/m); c.expectText(/^Weekly$/m);
        c.expectText(/resets in 2h \d+m/i, '5-hour countdown');
        c.expectText(/resets in 3d \d+h/i, 'weekly countdown');
    });

    test('Limits', 'heavy use: warning level, projection, and the summary names the window', async c => {
        await c.show('heavy');
        eq(c.model('claude').level, 'bad', 'Claude level');
        c.expectText(/5-hour at 92%/, 'summary');
        c.expectText(/full in ~|At this pace/, 'projection');
    });

    test('Limits', 'a reached limit is shown as reached', async c => {
        await c.show('heavy', 'codex');
        c.expectText(/Rate limit reached/, 'Codex rate limit');
        c.expectText(/limit reached/i, 'ring tag');
    });

    test('Limits', 'extra usage and credits', async c => {
        await c.show('heavy');
        c.expectText(/Extra usage/); c.expectText(/45% of monthly/);
        c.tab('codex'); await sleep(500);
        c.expectText(/^Credits$/m); c.expectText(/^500$/m);
        await c.show('apierror', 'codex');
        c.expectText(/^Unlimited$/m, 'unlimited credits');
    });

    test('Limits', 'Ultra\'s separate weekly Fable limit gets its own ring', async c => {
        await c.show('ultra');
        c.expectText(/Weekly · Fable/);
        c.expectText(/^32%$/m); c.expectText(/^0%$/m);
    });

    test('Limits', 'six windows fit in the menu without widening it', async c => {
        await c.show('healthy');
        const w0 = c.menu().width;
        await c.show('many');
        eq(c.model('claude').usage.windows.length, 6, 'windows');
        c.expectText(/Weekly · Cowork/);
        ok(c.menu().width <= w0 + 1, `menu widened from ${w0} to ${c.menu().width}`);
    });

    test('Limits', 'claude.ai refuses: the saved numbers stay, marked with their time', async c => {
        await c.show('apierror');
        c.expectText(/Saved numbers from \d\d:\d\d/);
        c.expectText(/rate-limiting/);
        c.expectText(/^25%$/m, 'saved value');
        c.expectText(/Limits from \d\d:\d\d/, 'summary');
    });

    test('Limits', 'live Claude limits off: a calm notice, no warning', async c => {
        await c.show('claudeoff');
        c.expectText(/Live limits are off/);
        eq(c.model('claude').level, 'ok', 'level');
    });

    test('Limits', 'no numbers and nothing saved: says why', async c => {
        await c.show(custom('limits-error', 'healthy', d => { d.claude.usage = {error: 'Could not reach claude.ai (offline?)'}; }));
        c.expectText(/Limits unavailable/);
        c.expectText(/Could not reach claude\.ai/);
    });

    // ---------------- tokens ----------------
    test('Tokens', 'Claude: today\'s total, output and share from cache', async c => {
        await c.show('healthy');
        c.expectText(/^182\.0M$/m); c.expectText(/^940\.0k$/m); c.expectText(/from cache/);
        c.expectText(/this computer/);
    });

    test('Tokens', 'Codex: today, last 7 days and lifetime', async c => {
        await c.show('healthy', 'codex');
        c.expectText(/^24\.5M$/m); c.expectText(/^152\.0M$/m); c.expectText(/^1\.84B$/m);
        c.expectText(/whole account/);
    });

    test('Tokens', 'Codex slow: saved limits and totals stay, marked with their time', async c => {
        await c.show('codexstale', 'codex');
        c.expectText(/^saved \d\d:\d\d$/m, 'tokens aside');
        c.expectText(/Saved numbers from \d\d:\d\d/, 'limits notice');
        c.expectText(/^152\.0M$/m, 'saved 7-day total');
    });

    test('Tokens', 'saved totals from an earlier day do not pass as today\'s', async c => {
        const y = nowS() - 2 * 86400;
        await c.show(custom('tokens-yesterday', 'codexstale', d => { d.codex.tokens.stale.fetched_at = y; }), 'codex');
        ok(!c.has(/^24\.5M$/m), 'yesterday\'s "today" shown as today');
        c.expectText(/^1\.84B$/m, 'lifetime still shown');
    });

    // ---------------- remote access: Claude ----------------
    test('Remote · Claude', 'a connected server shows its sessions; all terminal sessions on Remote Control', async c => {
        await c.show('healthy');
        c.expectText(/Connected · 2\/32 sessions/);
        c.expectText(/• Refactor auth flow/); c.expectText(/• Fix flaky tests/);
        c.expectText(/All 2 on Remote Control/);
        c.expectText(/Remote access healthy/);
        eq(c.row('claude', 'Phone & web sessions').level, 'ok', 'server row');
    });

    test('Remote · Claude', 'a terminal session not on Remote Control is named, with the /rc hint', async c => {
        await c.show('heavy');
        c.expectText(/2 of 3 on Remote Control · type \/rc in the others/);
        c.expectText(/Not connected: api-refactor/);
    });

    test('Remote · Claude', 'auto-connect off is flagged', async c => {
        await c.show(custom('autooff', 'healthy', d => { d.claude.workers.auto_connect = false; }));
        eq(c.row('claude', 'Auto-connect').level, 'warn', 'auto-connect row');
        c.expectText(/Auto-connect off/, 'summary names the one issue');
    });

    test('Remote · Claude', 'today\'s drops on the timeline', async c => {
        await c.show('history');
        c.expectText(/3 drops today, longest 14s/);
        let marks = null;
        walk(c.menu(), a => { if (a.mapped && Array.isArray(a._marks) && marks === null) marks = a._marks; });
        eq(marks?.length, 3, 'dots on the Claude timeline');
    });

    test('Remote · Claude', 'a reconnecting server offers a restart that names the sessions it ends', async c => {
        await c.show('fixes');
        const b = await c.click(/^Restart anyway/, 'restart');
        eq(b.label, 'Ends 2 sessions · click again', 'armed warning');
        await sleep(300);
        ok(!/restart claude/.test(actions()), 'ran on the first click');
        b.emit('clicked', 1);
        await waitFor(() => /systemctl --user restart claude-remote-control\.service/.test(actions()), 'the restart command');
    });

    test('Remote · Claude', 'a stopped server offers "Start server", one click', async c => {
        await c.show('apierror');
        await c.click(/^Start server/);
        await waitFor(() => /systemctl --user start claude-remote-control\.service/.test(actions()), 'the start command');
    });

    test('Remote · Claude', '"Open environment in browser" opens the claude.ai link (and only that)', async c => {
        await c.show('healthy');
        const orig = Gio.AppInfo.launch_default_for_uri;
        let opened = null;
        Gio.AppInfo.launch_default_for_uri = uri => { opened = uri; return true; };
        try {
            await c.click(/Open environment in browser/);
        } finally {
            Gio.AppInfo.launch_default_for_uri = orig;
        }
        eq(opened, 'https://claude.ai/code?environment=env_EXAMPLE', 'opened link');
    });

    test('Remote · Claude', 'the copy button copies the link', async c => {
        await c.show('healthy');
        St.Clipboard.get_default().set_text(St.ClipboardType.CLIPBOARD, 'before');
        await c.click(/Copy link/);
        eq(await clipboardText(), 'https://claude.ai/code?environment=env_EXAMPLE', 'clipboard');
    });

    test('Remote · Claude', 'a link that is not https://claude.ai is never offered', async c => {
        await c.show(custom('badlink', 'healthy', d => { d.claude.workers.server.link = 'https://evil.example/code?environment=x'; }));
        ok(!c.button(/Open environment in browser/), 'open button shown');
        ok(!c.button(/Copy link/), 'copy button shown');
    });

    // ---------------- remote access: Codex ----------------
    test('Remote · Codex', 'connected service, watchdog and SSH all healthy', async c => {
        await c.show('healthy', 'codex');
        c.expectText(/Phone link connected · my-laptop/);
        c.expectText(/SSH server running/);
        eq(c.model('codex').level, 'ok', 'Codex level');
    });

    test('Remote · Codex', 'phone link held by Codex Desktop: healthy, no restart offered', async c => {
        await c.show('codexdesktop', 'codex');
        c.expectText(/Phone link via Codex Desktop/);
        ok(!c.button(/Restart service/), 'restart offered');
        eq(c.model('codex').level, 'ok', 'Codex level');
    });

    test('Remote · Codex', 'phone link down: "Restart service" asks first, then restarts it in its own scope', async c => {
        await c.show(custom('relaydown', 'healthy', d => { d.codex.workers.service.relay = {status: 'errored', name: 'my-laptop'}; }), 'codex');
        c.expectText(/Phone link errored/);
        const b = await c.click(/^Restart service/);
        eq(b.label, 'Stops running Codex tasks · click again', 'armed warning');
        b.emit('clicked', 1);
        await waitFor(() => /systemd-run --user --scope --quiet --collect -- \S*codex app-server daemon restart/.test(actions()), 'the restart command');
    });

    test('Remote · Codex', 'service not running: "Start service" runs the watchdog once', async c => {
        await c.show('loggedout', 'codex');
        c.expectText(/Not running · phone access unavailable/);
        await c.click(/^Start service/);
        await waitFor(() => /systemctl --user start codex-remote-control\.service/.test(actions()), 'the start command');
    });

    test('Remote · Codex', 'a stray server: "Fix now" runs the watchdog', async c => {
        await c.show('heavy', 'codex');
        c.expectText(/Stray Codex server/); c.expectText(/Blocks phone access/);
        await c.click(/^Fix now/);
        await waitFor(() => /systemctl --user start codex-remote-control\.service/.test(actions()), 'the watchdog run');
    });

    test('Remote · Codex', 'watchdog timer off: "Turn on" enables it', async c => {
        await c.show(custom('timeroff', 'healthy', d => { d.codex.workers.watchdog.timer_active = false; }), 'codex');
        c.expectText(/Timer is off/);
        await c.click(/^Turn on/);
        await waitFor(() => /systemctl --user enable --now codex-remote-control\.timer/.test(actions()), 'the enable command');
    });

    test('Remote · Codex', 'a failed watchdog check shows its message and offers "Check again"', async c => {
        await c.show('heavy', 'codex');
        c.expectText(/Last check failed/);
        c.expectText(/stray app-server found; restarting service/);
        ok(c.button(/^Check again/), 'Check again button');
    });

    test('Remote · Codex', 'SSH down is flagged', async c => {
        await c.show('heavy', 'codex');
        ok(RANKED(c.row('codex', 'SSH for the Codex app')?.level), 'SSH row not flagged');
    });

    test('Remote · Codex', 'restarts on the timeline, with the last one\'s time', async c => {
        await c.show('history', 'codex');
        c.expectText(/2 restarts today, last (\w{3} )?\d\d:\d\d/);
    });

    // ---------------- fix buttons ----------------
    test('Fix buttons', 'a fix shows "Working…", then re-checks exactly once', async c => {
        await c.show('fixes');
        const b = c.button(/^Restart anyway/);
        b.emit('clicked', 1); await sleep(60); b.emit('clicked', 1);
        await waitFor(() => c.ind._actionBusy === 'claude-restart', 'busy state');
        c.expectText(/Working…/);
        const before = runs();
        await waitFor(() => c.ind._actionBusy === null, 'the command to finish');
        await sleep(3500);
        eq(runs() - before, 1, 're-checks after the fix');
    });

    test('Fix buttons', 'closing the menu disarms an armed fix: one click after reopening runs nothing', async c => {
        await c.show('fixes');
        await c.click(/^Restart anyway/);
        c.close(); await sleep(100);
        c.open(); await c.idle(); await sleep(300);
        const b = await c.click(/^Restart anyway/);
        eq(b.label, 'Ends 2 sessions · click again', 'arms again instead of running');
        await sleep(700);
        ok(!/restart claude/.test(actions()), 'ran after a single click');
    });

    test('Fix buttons', 'other fix buttons are disabled while one runs', async c => {
        await c.show('fixes', 'codex');
        const fix = c.button(/^Fix now/);
        fix.emit('clicked', 1);
        await waitFor(() => c.ind._actionBusy !== null, 'busy');
        const others = c.buttons(/^(Restart service|Check again)/);
        ok(others.length > 0 && others.every(b => !b.reactive), 'other buttons still clickable');
    });

    test('Fix buttons', 'a failed fix shows the error in the footer and under its button', async c => {
        await c.show('fixes', 'codex');
        write(`${SC}/action-fail`, '1');
        await c.click(/^Fix now/);
        await waitFor(() => c.ind._actionBusy === null && c.ind._actionErr, 'the failure');
        ok(/failed/.test(c.ind._status.text), `footer: ${c.ind._status.text}`);
        c.expectText(/simulated failure/, 'error under the button');
    });

    // ---------------- health summary and panel ----------------
    test('Health', 'healthy everywhere: green summary, no panel dot', async c => {
        await c.show('healthy');
        c.expectText(/Remote access healthy/);
        ok(!c.ind._badge.visible, 'panel dot visible');
    });

    test('Health', 'trouble: the summary counts the issues and the panel dot turns red', async c => {
        await c.show('fixes');
        c.expectText(/5-hour at 92% · 3 remote issues/);
        ok(c.ind._badge.visible && c.ind._badge.has_style_class_name('au-level-bad'), 'red panel dot');
    });

    test('Health', 'partial data: missing parts are said to be unavailable, not hidden', async c => {
        await c.show('partial');
        c.expectText(/Worker status unavailable/);
        c.expectText(/Remote access status unknown/);
    });

    // ---------------- the real collector's output ----------------
    test('Contract', 'the real collector\'s output (made-up fixture data) is drawn correctly', async c => {
        if (!exists(`${SC}/real-collector.stream`))
            throw skip('run verify/e2e/collector_e2e.py --out first (verify/test-all.sh does)');
        await c.show('real-collector');
        c.expectText(/user@example\.com/); c.expectText(/Example Org/); c.expectText(/^Ultra$/m);
        c.expectText(/Live limits are off/);
        c.expectText(/Connected · 2\/32 sessions/); c.expectText(/• Refactor auth flow/);
        c.expectText(/2 drops today, longest 14s/);
        c.tab('codex'); await sleep(500);
        c.expectText(/codex-user@example\.com/); c.expectText(/^Plus$/m);
        c.expectText(/^7%$/m); c.expectText(/^11%$/m); c.expectText(/^500$/m);
        c.expectText(/^1\.0M$/m); c.expectText(/^28\.0M$/m); c.expectText(/^1\.84B$/m);
        c.expectText(/Phone link connected · my-laptop/);
    });

    // ---------------- performance ----------------
    test('Performance', 'each streamed frame redraws in under 40 ms', async c => {
        c.ind.__frameMs = [];
        await c.show('stream');
        await c.show('history');
        const ms = c.ind.__frameMs;
        ok(ms.length >= 3, `frames measured: ${ms.length}`);
        const worst = Math.max(...ms);
        log('frame ms', ms.map(x => x.toFixed(1)).join(' '));
        ok(worst < 40, `slowest frame ${worst.toFixed(1)} ms`);
    });

    test('Performance', 'switching tabs takes under 40 ms', async c => {
        await c.show('history');
        const times = [];
        for (const k of ['codex', 'claude', 'codex', 'claude']) {
            const t0 = GLib.get_monotonic_time();
            c.tab(k);
            times.push((GLib.get_monotonic_time() - t0) / 1000);
            await sleep(400);
        }
        log('tab ms', times.map(x => x.toFixed(1)).join(' '));
        ok(Math.max(...times) < 40, `slowest switch ${Math.max(...times).toFixed(1)} ms`);
    });

    // ---------------- lifecycle (last: these replace the indicator) ----------------
    test('Lifecycle', 'destroying the widget mid-run stops the collector', async c => {
        scenario('healthy', 30);
        c.open();
        await waitFor(() => c.ind._proc !== null, 'a run');
        // Not in the same frame as the open: GNOME Shell's own panel menus log errors when opened
        // and destroyed within one frame (a plain PanelMenu.Button does too); no person can do that.
        await sleep(300);
        const pid = c.ind._proc.get_identifier();
        ok(pid && exists(`/proc/${pid}`), 'collector process running');
        c.ind.destroy();
        await sleep(800);
        ok(!exists(`/proc/${pid}`), 'collector still running after destroy');
        ext.ind = await ext._load('?v=2');
        await sleep(500);
    });

    test('Lifecycle', 'reloading the UI module in place (an update) works', async c => {
        ok(c.ind && c.ind.menu, 'indicator from a re-imported module');
        await c.show('healthy');
        c.expectText(/user@example\.com/);
    });

    test('Lifecycle', 'no saved snapshot: no dot at login, skeletons on the first open', async c => {
        c.ind.destroy();
        await sleep(300);
        const snap = `${HOME}/.cache/agent-usage/last-snapshot.json`;
        if (exists(snap))
            GLib.unlink(snap);
        ext.ind = await ext._load('?v=3');
        await sleep(500);
        ok(!c.ind._badge.visible && !c.ind._data, 'dot or data without a snapshot');
        scenario('firstever');
        c.open();
        await waitFor(() => c.ind._pending.size > 0, 'first frame');
        ok(c.has(/updating…/), 'loading parts marked');
        await c.idle();
        eq(c.ind._pending.size, 0, 'parts still loading');
    });

    test('Lifecycle', 'collector missing: a clear message instead of a blank menu', async c => {
        GLib.rename(`${HOME}/.local/bin/agent-usage`, `${HOME}/.local/bin/agent-usage.off`);
        try {
            c.ind.destroy();
            await sleep(300);
            const snap = `${HOME}/.cache/agent-usage/last-snapshot.json`;
            if (exists(snap))
                GLib.unlink(snap);
            ext.ind = await ext._load('?v=4');
            await sleep(400);
            c.open();
            await sleep(600);
            c.expectText(/Collector not found/);
        } finally {
            GLib.rename(`${HOME}/.local/bin/agent-usage.off`, `${HOME}/.local/bin/agent-usage`);
        }
    });
}

const RANKED = level => level === 'warn' || level === 'bad';
