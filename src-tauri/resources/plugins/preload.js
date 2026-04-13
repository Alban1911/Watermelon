// Loaded by core.dll into the main page's V8 context. Hooks the LCU
// client API so the champ-select skin carousel renders every skin as
// owned/selectable. Force-unlocks for now; per-skin overrides are wired
// up in a later phase.
//
// `window.__native` and `window.Pengu` are pre-populated by the native
// loader before this script runs but are not used here.

(function () {
    'use strict';

    const log = (...args) => {
        try {
            console.log('[Talon]', ...args);
        } catch (_) {}
    };

    log('preload.js loaded');

    function unlockSkin(skin) {
        if (!skin || typeof skin !== 'object') return;
        skin.unlocked = true;
        skin.ownership = {
            owned: true,
            loyaltyReward: false,
            xboxGPReward: false,
            rental: { rented: false },
        };
        if (Array.isArray(skin.childSkins)) {
            skin.childSkins.forEach(unlockSkin);
        }
    }

    // Walks an arbitrary LCU payload and unlocks any object that looks
    // like a skin (heuristic: it has `unlocked` or `ownership`). LCU
    // session/carousel responses nest skins under varying keys, so the
    // walker is more robust than addressing fields by name.
    function walkAndUnlock(value) {
        if (!value || typeof value !== 'object') return;
        if (Array.isArray(value)) {
            value.forEach(walkAndUnlock);
            return;
        }
        if ('unlocked' in value || 'ownership' in value) {
            unlockSkin(value);
        }
        for (const key in value) {
            walkAndUnlock(value[key]);
        }
    }

    // ── HTTP fetch hook ───────────────────────────────────────────
    const originalFetch = window.fetch.bind(window);
    window.fetch = async function (input, init) {
        const response = await originalFetch(input, init);
        const url =
            typeof input === 'string' ? input : (input && input.url) || '';

        if (url.includes('/lol-champ-select/v1/skin-carousel-skins')) {
            try {
                const data = await response.clone().json();
                if (Array.isArray(data)) {
                    data.forEach(unlockSkin);
                    log('unlocked', data.length, 'carousel skins via fetch');
                    return new Response(JSON.stringify(data), {
                        status: response.status,
                        statusText: response.statusText,
                        headers: response.headers,
                    });
                }
            } catch (e) {
                log('fetch hook error:', e);
            }
        }
        return response;
    };

    // ── WebSocket push hook ───────────────────────────────────────
    // LCU events arrive as `[8, "topic", { uri, eventType, data }]`.
    const OriginalWebSocket = window.WebSocket;
    function PatchedWebSocket(...args) {
        const ws = new OriginalWebSocket(...args);
        const origAdd = ws.addEventListener.bind(ws);

        ws.addEventListener = function (type, listener, opts) {
            if (type !== 'message' || typeof listener !== 'function') {
                return origAdd(type, listener, opts);
            }
            const wrapped = function (event) {
                let forwarded = event;
                try {
                    const parsed = JSON.parse(event.data);
                    if (Array.isArray(parsed) && parsed.length === 3) {
                        const payload = parsed[2];
                        const uri = (payload && payload.uri) || '';
                        if (
                            uri.includes('/lol-champ-select/v1/session') ||
                            uri.includes('/lol-champ-select/v1/skin-carousel-skins')
                        ) {
                            walkAndUnlock(payload.data);
                            forwarded = new MessageEvent('message', {
                                data: JSON.stringify(parsed),
                                origin: event.origin,
                            });
                        }
                    }
                } catch (_) {
                    // not JSON — ignore
                }
                return listener.call(this, forwarded);
            };
            return origAdd(type, wrapped, opts);
        };

        return ws;
    }
    PatchedWebSocket.prototype = OriginalWebSocket.prototype;
    window.WebSocket = PatchedWebSocket;

    log('hooks installed');
})();
