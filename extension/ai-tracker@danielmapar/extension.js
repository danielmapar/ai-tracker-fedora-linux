// AI Tracker (Agent Usage) — loader.
// The UI lives in a build-specific module named in the file `ui-current` (e.g. "ui-20261101213000.js",
// written by install.sh). Each build has its own file name, so `gnome-extensions disable/enable`
// loads a new build without logging out (GNOME caches ES modules by URL for the whole session).
// Without a valid `ui-current`, the plain `ui.js` is loaded.

import GLib from 'gi://GLib';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

export default class AgentUsageExtension extends Extension {
    enable() {
        this._generation = (this._generation ?? 0) + 1;
        const generation = this._generation;
        import(`./${this._uiModule()}`)
            .then(ui => {
                if (generation !== this._generation)
                    return; // disabled (or re-enabled) while loading
                this._indicator = ui.createIndicator(this);
                Main.panel.addToStatusArea(this.uuid, this._indicator);
            })
            .catch(e => logError(e, 'agent-usage: could not load the UI module'));
    }

    disable() {
        this._generation = (this._generation ?? 0) + 1;
        this._indicator?.destroy();
        this._indicator = null;
    }

    _uiModule() {
        try {
            const [, bytes] = GLib.file_get_contents(GLib.build_filenamev([this.path, 'ui-current']));
            const name = new TextDecoder().decode(bytes).trim();
            if (/^ui-[A-Za-z0-9._-]+\.js$/.test(name))
                return name;
        } catch (_e) {
            // fall through to the default build
        }
        return 'ui.js';
    }
}
