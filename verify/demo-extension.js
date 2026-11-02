// Records docs/demo.gif (run by verify/demo.sh inside run.sh's sandboxed headless shell): loads the
// widget like the real loader, drives it with a virtual mouse (real clicks and hovers), and takes a
// screenshot of the region around the menu every FRAME_MS. Cursor positions are logged so
// make-gif.py can draw the pointer (area screenshots don't include it).
import Clutter from 'gi://Clutter';
import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import Shell from 'gi://Shell';
import St from 'gi://St';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const HOME = GLib.get_home_dir();
const SC = `${HOME}/scenarios`;
const OUT = GLib.getenv('AU_OUT');
const FRAME_MS = 80;
const log = (...a) => console.log('AUDEMO', ...a);
const sleep = ms => new Promise(r => GLib.timeout_add(GLib.PRIORITY_DEFAULT, ms, () => { r(); return GLib.SOURCE_REMOVE; }));
const write = (p, s) => GLib.file_set_contents(p, s);
const setScenario = (name, delay = 0.25) => { write(`${SC}/current`, name); write(`${SC}/delay`, `${delay}`); };
const now = () => GLib.get_monotonic_time();
const center = a => {
    const [x, y] = a.get_transformed_position();
    const [w, h] = a.get_transformed_size();
    return [x + w / 2, y + h / 2];
};

function findButton(actor, re) {
    if (actor instanceof St.Button && actor.visible && re.test(`${actor.label ?? ''} ${actor.accessible_name ?? ''}`))
        return actor;
    for (const c of actor.get_children()) {
        const b = findButton(c, re);
        if (b)
            return b;
    }
    return null;
}

async function shot(path, r) {
    const stream = Gio.File.new_for_path(path).replace(null, false, Gio.FileCreateFlags.REPLACE_DESTINATION, null);
    await new Promise((resolve, reject) => new Shell.Screenshot().screenshot_area(r.x, r.y, r.w, r.h, stream, (o, res) => {
        try { o.screenshot_area_finish(res); resolve(); } catch (e) { reject(e); }
    }));
    stream.close(null);
}

export default class DemoExtension extends Extension {
    enable() {
        this._run().catch(e => { logError(e, 'AUDEMO failed'); write(`${OUT}/DONE`, `error ${e}`); });
    }

    async _run() {
        // The shell starts in the Activities overview, and its startup animation can open it again
        // after an early hide: wait for startup to finish, then close it and check it stays closed.
        for (let i = 0; i < 60 && Main.layoutManager._startingUp; i++)
            await sleep(250);
        const closeOverview = async () => {
            for (let i = 0; i < 40 && (Main.overview.visible || Main.overview.animationInProgress); i++) {
                Main.overview.hide();
                await sleep(250);
            }
        };
        await closeOverview();
        await sleep(1500);
        await closeOverview();
        log('overview settled', 'visible', Main.overview.visible);
        const mod = await import(`file://${this.path}/ui.js`);
        const ind = mod.createIndicator(this);
        Main.panel.addToStatusArea(this.uuid, ind);
        await sleep(1500);

        // Measure the menu once (same data as the saved snapshot, so nothing changes on screen).
        setScenario('demo-before', 0.05);
        ind.menu.open(false);
        await sleep(800);
        const [mx] = ind.menu.actor.get_transformed_position();
        const [mw] = ind.menu.actor.get_transformed_size();
        ind.menu.close(false);
        await sleep(800);
        const sw = global.stage.width;
        const x = Math.max(0, Math.floor(mx) - 28);
        const region = {x, y: 0, w: Math.min(sw - x, Math.ceil(mw) + 56), h: global.stage.height};

        const seat = Clutter.get_default_backend().get_default_seat();
        const ptr = seat.create_virtual_device(Clutter.InputDeviceType.POINTER_DEVICE);
        const t0 = now();
        const cursor = [];
        let cur = {x: region.x + region.w * 0.35, y: 330};
        const logPos = (tag = '') => cursor.push(`${((now() - t0) / 1000).toFixed(1)} ${(cur.x - region.x).toFixed(1)} ${(cur.y - region.y).toFixed(1)}${tag}`);
        ptr.notify_absolute_motion(now(), cur.x, cur.y);
        logPos();
        const moveTo = async ([tx, ty], ms = 650) => {
            const [sx, sy] = [cur.x, cur.y];
            const steps = Math.max(1, Math.round(ms / 16));
            for (let i = 1; i <= steps; i++) {
                const t = i / steps;
                const e = t < 0.5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2;
                cur = {x: sx + (tx - sx) * e, y: sy + (ty - sy) * e};
                ptr.notify_absolute_motion(now(), cur.x, cur.y);
                logPos();
                await sleep(16);
            }
        };
        const click = async () => {
            ptr.notify_button(now(), Clutter.BUTTON_PRIMARY, Clutter.ButtonState.PRESSED);
            logPos(' click');
            await sleep(70);
            ptr.notify_button(now(), Clutter.BUTTON_PRIMARY, Clutter.ButtonState.RELEASED);
        };

        // Record continuously while the story plays.
        const frames = [];
        let recording = true;
        const recorder = (async () => {
            for (let n = 0; recording; n++) {
                const wait = (t0 + n * FRAME_MS * 1000 - now()) / 1000;
                if (wait > 1)
                    await sleep(Math.floor(wait));
                const t = (now() - t0) / 1000;
                const name = `f${String(n).padStart(4, '0')}.png`;
                await shot(`${OUT}/frames/${name}`, region);
                frames.push(`${name} ${t.toFixed(1)}`);
            }
        })();

        // 1. Open: saved numbers instantly (dimmed), then the live parts stream in.
        await sleep(700);
        setScenario('demo-stream');
        await moveTo(center(ind), 750);
        await sleep(200);
        await click();
        await sleep(3000);
        // 2. Codex tab.
        await moveTo(center(ind._tabButtons.codex), 650);
        await sleep(150);
        await click();
        await sleep(2300);
        // 3. Refresh into a problem: the phone link dropped and a stray server blocks it.
        setScenario('demo-problem', 0.35);
        await moveTo(center(ind._refreshButton), 700);
        await sleep(150);
        await click();
        await sleep(2100);
        // 4. One click fixes it; the widget re-checks by itself 2.5 s later.
        setScenario('demo-healthy', 0.3);
        const fix = findButton(ind.menu.actor, /Fix now/);
        if (fix) {
            await moveTo(center(fix), 750);
            await sleep(250);
            await click();
        }
        await sleep(4400);
        // 5. Back to Claude, then close.
        await moveTo(center(ind._tabButtons.claude), 750);
        await sleep(150);
        await click();
        await sleep(1900);
        await moveTo(center(ind), 750);
        await sleep(150);
        await click();
        await sleep(900);

        recording = false;
        await recorder;
        write(`${OUT}/frames.txt`, frames.join('\n') + '\n');
        write(`${OUT}/cursor.txt`, cursor.join('\n') + '\n');
        log('frames', frames.length, 'fix button found', fix !== null, 'overview visible', Main.overview.visible);
        write(`${OUT}/DONE`, fix ? 'ok' : 'no Fix now button');
    }

    disable() {}
}
