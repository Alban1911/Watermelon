// Talon preload — Step 5b: real skins via the `https://talon` scheme.
//
// Flow:
//   1. Wait for the rcp-fe-lol-champ-select plugin to announce itself
//      via a riotPlugin.announce:* DOM event, capture its API.
//   2. Pre-fetch `https://talon/skins/all` (served by core.dll from
//      Talon's on-disk skins_index.json), cache the result on
//      `window.__talonSkinIndex`.
//   3. Install a wrapper around `champSelectBinding.cache._data.set`
//      that, on every carousel update, reads the cached index and
//      splices the current champion's Talon skins into the data array
//      before Ember renders it.
//
// Prefer separate Talon-served assets for background vs tile:
//   - `https://talon/assets/background/<fileStem>.png`
//   - `https://talon/assets/splash/<fileStem>.png`
//   - `https://talon/assets/tile/<fileStem>.png`
// so the carousel tile can use a HUD-style icon while the background
// keeps using full splash art.

(function () {
    'use strict';

    const log = (...args) => {
        try {
            console.log('[Talon]', ...args);
        } catch (_) {}
    };

    log('preload.js loaded');

    const ANNOUNCE_PREFIX = 'riotPlugin.announce:';
    const TARGET_PLUGIN = 'rcp-fe-lol-champ-select';
    const CAROUSEL_CACHE_KEY = '/lol-champ-select/v1/skin-carousel-skins';
    const TALON_INDEX_URL = 'https://talon/skins/all';
    const TALON_INDEX_VERSION_URL = 'https://talon/skins/version';
    const TALON_BACKGROUND_ASSET_BASE_URL = 'https://talon/assets/background/';
    const TALON_SPLASH_ASSET_BASE_URL = 'https://talon/assets/splash/';
    const TALON_TILE_ASSET_BASE_URL = 'https://talon/assets/tile/';
    const INDEX_REFRESH_MS = 400;
    // Custom skin ids live above this floor so we can detect ones
    // already injected into a value.data array and skip double-inject.
    const CUSTOM_ID_FLOOR = 9_000_000;

    // Local WS bridge to the Talon Rust side. The LCU has no real event
    // for "skin currently previewed in carousel" (its session.selectedSkinId
    // only updates when the pick is confirmed), so we DOM-scrape the skin
    // name element and forward the resolved id through this socket.
    const TALON_BRIDGE_URL = 'ws://127.0.0.1:51234';
    const BRIDGE_RETRY_BASE_MS = 1000;
    const BRIDGE_RETRY_MAX_MS = 30000;
    const SKIN_NAME_SELECTORS = ['.skin-name-text', '.skin-name'];
    const SKIN_MONITOR_POLL_MS = 250;

    const pluginApis = {};
    window.__talonPluginApis = pluginApis;
    window.__talonSkinIndex = {};
    let cacheRef = null;
    let parentCacheRef = null;
    let lastIndexJson = '{}';
    let lastIndexVersion = '0';
    let lastLiveCarouselValue = null;
    let refreshTimer = null;

    // ── skin monitor / bridge state ─────────────────────────────────
    // lowercased skin name → id, rebuilt every time the carousel cache
    // is set. Includes the base skin, every carousel entry (native +
    // injected Talon custom), and each entry's childSkins (chromas).
    const skinNameToId = new Map();
    let bridgeSocket = null;
    let bridgeRetryDelay = BRIDGE_RETRY_BASE_MS;
    let lastSentSkinId = null;
    let skinMonitorInstalled = false;
    let skinMonitorPollTimer = null;

    // ── RCP plugin API capture ───────────────────────────────────────
    const originalDispatchEvent = document.dispatchEvent.bind(document);
    document.dispatchEvent = function (event) {
        if (
            event &&
            typeof event.type === 'string' &&
            event.type.startsWith(ANNOUNCE_PREFIX)
        ) {
            const pluginName = event.type.substring(ANNOUNCE_PREFIX.length);
            const originalHandler = event.registrationHandler;

            if (typeof originalHandler === 'function') {
                try {
                    Object.defineProperty(event, 'registrationHandler', {
                        configurable: true,
                        value: function (registrar) {
                            return originalHandler.call(this, async function (provider) {
                                const api = await registrar(provider);
                                pluginApis[pluginName] = api;
                                if (pluginName === TARGET_PLUGIN) {
                                    onChampSelectReady(api);
                                }
                                return api;
                            });
                        },
                    });
                } catch (e) {
                    log('failed to wrap registrationHandler for', pluginName, ':', e && e.message);
                }
            }
        }
        return originalDispatchEvent(event);
    };

    function onChampSelectReady(api) {
        const parentCache = api && api.champSelectBinding && api.champSelectBinding.cache;
        const cacheData = parentCache && parentCache._data;
        if (!cacheData || typeof cacheData.set !== 'function') {
            log(
                TARGET_PLUGIN,
                'cache._data.set not reachable — api keys:',
                Object.keys(api || {})
            );
            return;
        }

        parentCacheRef = parentCache;

        // Pre-fetch the Talon skin index before installing the cache
        // hook so the hook can read it synchronously when Ember writes
        // the carousel data. Install the hook regardless of fetch
        // outcome — on failure the index stays empty and we simply
        // don't inject anything.
        fetchIndex()
            .then(() => fetchIndexVersion())
            .then((version) => {
                lastIndexVersion = version;
            })
            .catch((e) => {
                log('talon skin index fetch failed:', e && e.message);
            })
            .finally(() => {
                installCacheHook(cacheData);
                startIndexRefreshLoop();
                installSkinMonitor();
                connectBridge();
            });
    }

    function fetchIndex() {
        return fetch(TALON_INDEX_URL, { cache: 'no-store' })
            .then((r) => r.json())
            .then((index) => {
                const normalized = index || {};
                window.__talonSkinIndex = normalized;
                lastIndexJson = JSON.stringify(normalized);
                const champCount = Object.keys(normalized).length;
                log('talon skin index loaded:', champCount, 'champion(s)');
                return normalized;
            });
    }

    function fetchIndexVersion() {
        return fetch(TALON_INDEX_VERSION_URL, { cache: 'no-store' })
            .then((r) => r.text())
            .then((version) => version || '0');
    }

    // ── Carousel cache hook ──────────────────────────────────────────
    function installCacheHook(cache) {
        cacheRef = cache;
        const originalSet = cache.set.bind(cache);
        cache.set = function (key, value) {
            if (key !== CAROUSEL_CACHE_KEY) {
                return originalSet(key, value);
            }
            if (!value || !Array.isArray(value.data) || value.data.length === 0) {
                return originalSet(key, value);
            }
            lastLiveCarouselValue = value;
            const injectedCount = injectTalonSkins(value);
            if (injectedCount > 0) {
                log('injected', injectedCount, 'talon skin(s) into carousel');
            }
            updateSkinNameToIdMap(value);
            return originalSet(key, value);
        };
        log('cache._data.set hook installed — waiting for champ-select');
    }

    function startIndexRefreshLoop() {
        if (refreshTimer !== null) {
            return;
        }
        refreshTimer = setInterval(() => {
            fetchIndexVersion()
                .then((version) => {
                    if (version === lastIndexVersion) {
                        return;
                    }
                    lastIndexVersion = version;
                    return fetchIndex().then(() => {
                        log('talon skin index changed — refreshing carousel');
                        refreshCurrentCarousel();
                    });
                })
                .catch((e) => {
                    log('talon skin index refresh failed:', e && e.message);
                });
        }, INDEX_REFRESH_MS);
    }

    function refreshCurrentCarousel() {
        if (!cacheRef || !lastLiveCarouselValue) {
            return;
        }
        const live = lastLiveCarouselValue;
        if (!Array.isArray(live.data) || live.data.length === 0) {
            return;
        }

        const cleaned = {
            ...live,
            data: live.data.filter(
                (s) => !(s && typeof s.id === 'number' && s.id >= CUSTOM_ID_FLOOR)
            ),
        };

        try {
            // The _data.set hook re-injects Talon skins into cleaned.data
            // in place, then stores via originalSet.
            cacheRef.set(CAROUSEL_CACHE_KEY, cleaned);
        } catch (e) {
            log('carousel refresh set failed:', e && e.message);
            return;
        }

        // _data.set only stores — observers live on the parent cache and
        // must be fired separately with the unwrapped payload (the array).
        if (parentCacheRef && typeof parentCacheRef._triggerResourceObservers === 'function') {
            try {
                parentCacheRef._triggerResourceObservers(CAROUSEL_CACHE_KEY, cleaned.data);
            } catch (e) {
                log('_triggerResourceObservers failed:', e && e.message);
            }
        }
    }

    function injectTalonSkins(value) {
        const baseSkin = value.data[0];
        const championId = baseSkin && baseSkin.championId;
        if (!championId) {
            log('carousel set: no championId on base skin, skipping injection');
            return 0;
        }

        // Idempotent: if any Talon skin is already present (id in
        // the custom range) we've already handled this data array.
        if (value.data.some((s) => s && typeof s.id === 'number' && s.id >= CUSTOM_ID_FLOOR)) {
            return 0;
        }

        const talonSkins =
            (window.__talonSkinIndex || {})[String(championId)] || [];
        if (talonSkins.length === 0) {
            return 0;
        }

        talonSkins.forEach((entry, i) => {
            value.data.splice(1 + i, 0, makeCarouselSkin(entry, baseSkin, championId));
        });
        return talonSkins.length;
    }

    function makeAssetUrl(baseUrl, fileStem, version) {
        if (!fileStem) {
            return null;
        }
        const url = baseUrl + encodeURIComponent(fileStem) + '.png';
        // Append `?v=<mtime>` when we know the file version so a custom
        // upload or warmup regeneration busts any in-memory image cache
        // the client may be holding from a previous URL visit.
        if (version !== undefined && version !== null && version !== 0) {
            return url + '?v=' + encodeURIComponent(version);
        }
        return url;
    }

    function setIfPresent(target, key, value) {
        if (Object.prototype.hasOwnProperty.call(target, key)) {
            target[key] = value;
        }
    }

    // Builds a carousel entry from a Talon index entry. Start from the
    // native base-skin object so we preserve whatever extra fields the
    // current League client expects, then override the identity and the
    // image paths we know about.
    function makeCarouselSkin(entry, baseSkin, championId) {
        const splashAssetUrl =
            entry && entry.hasSplashAsset
                ? makeAssetUrl(TALON_SPLASH_ASSET_BASE_URL, entry.fileStem, entry.splashVersion)
                : null;
        const backgroundAssetUrl =
            entry && entry.hasBackgroundAsset
                ? makeAssetUrl(TALON_BACKGROUND_ASSET_BASE_URL, entry.fileStem, entry.backgroundVersion)
                : null;
        const tileAssetUrl =
            entry && entry.hasTileAsset
                ? makeAssetUrl(TALON_TILE_ASSET_BASE_URL, entry.fileStem, entry.tileVersion)
                : null;
        const skin = {
            ...baseSkin,
            championId: championId,
            childSkins: Array.isArray(baseSkin.childSkins) ? [] : baseSkin.childSkins,
            chromaPreviewPath: null,
            disabled: false,
            emblems: Array.isArray(baseSkin.emblems) ? [] : baseSkin.emblems,
            groupSplash: '',
            id: entry.id,
            isBase: false,
            isChampionUnlocked: true,
            name: entry.name,
            ownership: {
                ...(baseSkin.ownership || {}),
                loyaltyReward: false,
                owned: true,
                rental: { rented: false },
                xboxGPReward: false,
            },
            stillObtainable: false,
            unlocked: true,
        };

        const finalSplashUrl = backgroundAssetUrl || splashAssetUrl || baseSkin.splashPath || '';
        const finalTileUrl = tileAssetUrl || baseSkin.tilePath || finalSplashUrl;

        skin.splashPath = finalSplashUrl;
        skin.tilePath = finalTileUrl;

        // Riot has changed the exact image-field mix across client builds.
        // Override every art-like field we commonly see so Talon entries
        // track native behavior more closely.
        setIfPresent(skin, 'uncenteredSplashPath', finalSplashUrl);
        setIfPresent(skin, 'centeredSplashPath', finalSplashUrl);
        setIfPresent(skin, 'loadScreenPath', finalSplashUrl);
        setIfPresent(skin, 'loadscreenPath', finalSplashUrl);
        setIfPresent(skin, 'groupSplash', finalSplashUrl);
        setIfPresent(skin, 'cardSplashPath', finalTileUrl);
        setIfPresent(skin, 'tilePath', finalTileUrl);
        setIfPresent(skin, 'iconPath', finalTileUrl);
        setIfPresent(skin, 'squarePortraitPath', finalTileUrl);
        setIfPresent(skin, 'chromaPreviewPath', finalTileUrl);

        return skin;
    }

    // ── Skin hover monitor ──────────────────────────────────────────
    // Rebuilds the name→id table from a freshly-set carousel cache value.
    // Includes the top-level carousel entries (base skin + every style
    // variant + every Talon custom) and each entry's childSkins array
    // (chromas), since the client promotes a chroma's name into the
    // `.skin-name-text` element when one is selected.
    function updateSkinNameToIdMap(value) {
        if (!value || !Array.isArray(value.data)) {
            return;
        }
        skinNameToId.clear();
        for (const skin of value.data) {
            addSkinToMap(skin);
            if (skin && Array.isArray(skin.childSkins)) {
                for (const child of skin.childSkins) {
                    addSkinToMap(child);
                }
            }
        }
    }

    function addSkinToMap(skin) {
        if (!skin || typeof skin.id !== 'number') {
            return;
        }
        if (typeof skin.name === 'string' && skin.name.length > 0) {
            skinNameToId.set(skin.name.trim().toLowerCase(), skin.id);
        }
    }

    // Reads the currently-displayed skin name from the carousel UI.
    // The carousel keeps many `.skin-name-text` nodes in the DOM at once
    // (one per slot); only the active slot is visible. Iterate all matches
    // and prefer a visible one (`offsetParent !== null`), falling back to
    // the last non-empty candidate so we still report something before
    // the carousel has fully laid out.
    function readCurrentSkinName() {
        for (const selector of SKIN_NAME_SELECTORS) {
            const nodes = document.querySelectorAll(selector);
            if (!nodes.length) continue;
            let candidate = null;
            nodes.forEach((node) => {
                const name = (node.textContent || '').trim();
                if (!name) return;
                if (node.offsetParent !== null) {
                    candidate = name;
                } else if (!candidate) {
                    candidate = name;
                }
            });
            if (candidate) return candidate;
        }
        return null;
    }

    function tickSkinMonitor() {
        const name = readCurrentSkinName();
        if (!name) {
            if (lastSentSkinId !== null) {
                lastSentSkinId = null;
                sendBridgeMessage({ type: 'skin-hovered', skinId: null });
            }
            return;
        }
        const id = skinNameToId.get(name.toLowerCase());
        if (id === undefined || id === null) {
            // Name not in the map — carousel hasn't sent its cache yet,
            // or it's a chroma whose name lives on a childSkin we
            // haven't indexed. Don't update `lastSentSkinId` so the
            // next tick retries once the map catches up.
            return;
        }
        if (id === lastSentSkinId) {
            return;
        }
        lastSentSkinId = id;
        sendBridgeMessage({ type: 'skin-hovered', skinId: id });
    }

    function installSkinMonitor() {
        if (skinMonitorInstalled) {
            return;
        }
        skinMonitorInstalled = true;

        const observer = new MutationObserver(() => {
            tickSkinMonitor();
        });
        try {
            observer.observe(document.body, {
                childList: true,
                subtree: true,
            });
        } catch (e) {
            log('skin monitor observer.observe failed:', e && e.message);
        }

        // The Ember client plants chunks of champ-select UI inside shadow
        // roots on custom elements; a plain `document.body` observer never
        // sees mutations inside them. Walk every element with a shadowRoot
        // and attach the same observer so carousel clicks inside a shadow
        // tree still fire the tick.
        try {
            document.querySelectorAll('*').forEach((node) => {
                if (!node.shadowRoot || !(node.shadowRoot instanceof Node)) return;
                try {
                    observer.observe(node.shadowRoot, {
                        childList: true,
                        subtree: true,
                    });
                } catch (_) {}
            });
        } catch (e) {
            log('shadow root walk failed:', e && e.message);
        }

        // Safety-net poll: MutationObserver can still miss text updates on
        // nodes replaced wholesale between ticks, and shadow roots attached
        // after install never get observed. 250ms is cheap.
        skinMonitorPollTimer = setInterval(tickSkinMonitor, SKIN_MONITOR_POLL_MS);
        log('skin monitor installed');
    }

    // ── Bridge WebSocket client ─────────────────────────────────────
    function connectBridge() {
        let socket;
        try {
            socket = new WebSocket(TALON_BRIDGE_URL);
        } catch (e) {
            log('bridge connect threw:', e && e.message);
            scheduleBridgeReconnect();
            return;
        }
        bridgeSocket = socket;

        socket.onopen = () => {
            log('bridge connected');
            bridgeRetryDelay = BRIDGE_RETRY_BASE_MS;
            // Re-announce the currently visible skin so the Rust side
            // recovers its state after a reconnect (Talon restart, etc).
            if (lastSentSkinId !== null) {
                try {
                    socket.send(
                        JSON.stringify({ type: 'skin-hovered', skinId: lastSentSkinId })
                    );
                } catch (_) {}
            }
        };

        socket.onclose = () => {
            if (bridgeSocket === socket) {
                bridgeSocket = null;
            }
            scheduleBridgeReconnect();
        };

        // onerror always fires before onclose; let onclose do the retry.
        socket.onerror = () => {};
    }

    function scheduleBridgeReconnect() {
        const delay = bridgeRetryDelay;
        bridgeRetryDelay = Math.min(bridgeRetryDelay * 2, BRIDGE_RETRY_MAX_MS);
        setTimeout(connectBridge, delay);
    }

    function sendBridgeMessage(msg) {
        if (!bridgeSocket || bridgeSocket.readyState !== WebSocket.OPEN) {
            return;
        }
        try {
            bridgeSocket.send(JSON.stringify(msg));
        } catch (e) {
            log('bridge send failed:', e && e.message);
        }
    }

    log('document.dispatchEvent hook installed');
})();
