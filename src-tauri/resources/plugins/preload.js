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
    const CHAMP_SELECT_SESSION_ENDPOINT = '/lol-champ-select/v1/session';
    const GAMEFLOW_ENDPOINT = '/lol-gameflow/v1/gameflow-phase';
    const SELECTION_ENDPOINT = '/lol-champ-select/v1/session/my-selection';
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
    const GOLDEN_BORDER_ENFORCE_MS = 50;
    const TALON_GOLDEN_STYLE_ID = 'talon-golden-border-style';
    const FINALIZATION_DEFAULT_PATCH_BEFORE_MS = 400;
    const FINALIZATION_INJECT_BEFORE_MS = 300;

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
    const skinIdToSkin = new Map();
    let bridgeSocket = null;
    let bridgeRetryDelay = BRIDGE_RETRY_BASE_MS;
    let lastSentSkinId = null;
    let skinMonitorInstalled = false;
    let skinMonitorPollTimer = null;
    let goldenBorderTimer = null;
    let goldenBorderSkinId = null;
    let goldenBorderMode = null;
    let goldenBorderSkin = null;
    let styledGoldenItem = null;
    let styledGoldenThumbnail = null;

    // ── champ-select desync state ───────────────────────────────────
    let desyncChampionId = null;
    let defaultSkinPatchSent = false;
    let patchBlockingEnabled = false;
    let allowNextSelectionPatch = false;
    let champSelectActive = false;
    let finalSelectionKind = null;
    let finalSelectionSkinId = null;
    let lastLoggedCandidateKey = null;
    let lastCustomSkinId = null;
    let customOverlayPrepared = false;
    let suppressBaseBackendClearUntil = 0;
    let finalizationDefaultPatchTimer = null;
    let finalizationInjectTimer = null;

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

    hookDesyncFetch();
    hookDesyncXhr();
    hookDesyncWebSocket();

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
    // Talon custom skins are not server-owned carousel selections, so the
    // League client can leave the base skin visually selected. Mirror upstream's
    // fake selected frame by styling the centered carousel item while the
    // visible skin resolves to one of our custom IDs.
    // ── Champ-select desync ─────────────────────────────────────────
    function hookDesyncFetch() {
        const originalFetch = window.fetch;
        window.fetch = function (input, init) {
            const url =
                typeof input === 'string'
                    ? input
                    : input instanceof URL
                      ? input.href
                      : input && input.url;
            const method = (init && init.method ? init.method : 'GET').toUpperCase();
            if (
                url &&
                url.includes(SELECTION_ENDPOINT) &&
                method === 'PATCH' &&
                shouldBlockSelectionPatch(init && init.body)
            ) {
                log('desync blocked fetch selection PATCH');
                return Promise.resolve(
                    new Response(null, { status: 204, statusText: 'No Content' })
                );
            }
            return originalFetch.call(this, input, init);
        };
    }

    function hookDesyncXhr() {
        const OriginalXHR = window.XMLHttpRequest;
        const originalOpen = OriginalXHR.prototype.open;
        const originalSend = OriginalXHR.prototype.send;

        OriginalXHR.prototype.open = function (method, url) {
            this.__talonMethod = typeof method === 'string' ? method.toUpperCase() : '';
            this.__talonUrl = typeof url === 'string' ? url : String(url || '');
            return originalOpen.apply(this, arguments);
        };

        OriginalXHR.prototype.send = function (body) {
            if (
                this.__talonUrl &&
                this.__talonUrl.includes(SELECTION_ENDPOINT) &&
                this.__talonMethod === 'PATCH' &&
                shouldBlockSelectionPatch(body)
            ) {
                log('desync blocked XHR selection PATCH');
                fakeXhrNoContent(this);
                return;
            }
            return originalSend.apply(this, arguments);
        };
    }

    function hookDesyncWebSocket() {
        const OriginalWebSocket = window.WebSocket;

        function TalonWebSocket(url, protocols) {
            const socket =
                protocols === undefined
                    ? new OriginalWebSocket(url)
                    : new OriginalWebSocket(url, protocols);

            socket.addEventListener('message', (event) => {
                processDesyncWsMessage(event.data);
            });

            return socket;
        }

        TalonWebSocket.prototype = OriginalWebSocket.prototype;
        Object.setPrototypeOf(TalonWebSocket, OriginalWebSocket);
        for (const key of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) {
            try {
                Object.defineProperty(TalonWebSocket, key, {
                    value: OriginalWebSocket[key],
                    configurable: true,
                });
            } catch (_) {}
        }
        window.WebSocket = TalonWebSocket;
    }

    function fakeXhrNoContent(xhr) {
        setTimeout(() => {
            try {
                Object.defineProperty(xhr, 'readyState', { value: 4, configurable: true });
                Object.defineProperty(xhr, 'status', { value: 204, configurable: true });
                Object.defineProperty(xhr, 'statusText', {
                    value: 'No Content',
                    configurable: true,
                });
                Object.defineProperty(xhr, 'response', { value: null, configurable: true });
                Object.defineProperty(xhr, 'responseText', { value: '', configurable: true });

                const readystatechange = new Event('readystatechange');
                const load = new ProgressEvent('load');
                const loadend = new ProgressEvent('loadend');
                xhr.dispatchEvent(readystatechange);
                xhr.dispatchEvent(load);
                xhr.dispatchEvent(loadend);
                if (typeof xhr.onreadystatechange === 'function') {
                    xhr.onreadystatechange(readystatechange);
                }
                if (typeof xhr.onload === 'function') {
                    xhr.onload(load);
                }
                if (typeof xhr.onloadend === 'function') {
                    xhr.onloadend(loadend);
                }
            } catch (e) {
                log('desync fake XHR response failed:', e && e.message);
            }
        }, 0);
    }

    function shouldBlockSelectionPatch(body) {
        const skinId = extractSelectedSkinId(body);
        if (!skinId) {
            return false;
        }
        if (allowNextSelectionPatch) {
            allowNextSelectionPatch = false;
            return false;
        }
        if (skinId >= CUSTOM_ID_FLOOR) {
            setFinalSelection('custom', skinId);
            return true;
        }
        if (skinId % 1000 !== 0) {
            setFinalSelection('native', skinId);
            if (patchBlockingEnabled) {
                return true;
            }
        } else {
            setFinalSelection('base', skinId);
        }
        return false;
    }

    function extractSelectedSkinId(body) {
        try {
            const data = typeof body === 'string' ? JSON.parse(body) : body;
            const skinId = data && data.selectedSkinId;
            return typeof skinId === 'number' ? skinId : null;
        } catch (_) {
            return null;
        }
    }

    function processDesyncWsMessage(raw) {
        if (
            typeof raw !== 'string' ||
            (!raw.includes('lol-champ-select') && !raw.includes('lol-gameflow'))
        ) {
            return;
        }
        let parsed;
        try {
            parsed = JSON.parse(raw);
        } catch (_) {
            return;
        }
        if (!Array.isArray(parsed) || parsed.length < 3) {
            return;
        }
        const payload = parsed[2];
        const uri = payload && payload.uri;
        const data = payload && payload.data;
        if (uri === CHAMP_SELECT_SESSION_ENDPOINT) {
            handleChampSelectSession(data);
        } else if (uri === GAMEFLOW_ENDPOINT) {
            handleGameflowPhase(data);
        }
    }

    function handleGameflowPhase(phase) {
        champSelectActive = phase === 'ChampSelect';
        if (
            phase === 'None' ||
            phase === 'Lobby' ||
            phase === 'Matchmaking' ||
            phase === 'ReadyCheck'
        ) {
            resetDesyncState();
        }
    }

    function handleChampSelectSession(data) {
        if (!data || typeof data !== 'object') {
            return;
        }

        const localCell = data.localPlayerCellId;
        const me =
            Array.isArray(data.myTeam) &&
            data.myTeam.find((player) => player && player.cellId === localCell);
        const championId = me && me.championId;

        if (typeof championId === 'number' && championId > 0) {
            if (desyncChampionId !== championId) {
                resetDesyncState();
                desyncChampionId = championId;
                defaultSkinPatchSent = true;
                forceDefaultSkin(championId);
                setTimeout(() => {
                    if (desyncChampionId === championId) {
                        patchBlockingEnabled = true;
                        log('desync patch blocking enabled for champion', championId);
                    }
                }, 100);
            } else if (!defaultSkinPatchSent) {
                defaultSkinPatchSent = true;
                forceDefaultSkin(championId);
            }
        }

        const timer = data.timer;
        if (timer && timer.phase === 'FINALIZATION') {
            scheduleFinalization(timer.adjustedTimeLeftInPhase || 0);
        }
    }

    function forceDefaultSkin(championId) {
        const defaultSkinId = championId * 1000;
        allowNextSelectionPatch = true;
        // The forced default PATCH can briefly make the centered DOM look like
        // a genuine base hover. Keep the visual cleanup, but do not clear the
        // prepared overlay during this window.
        suppressBaseBackendClearUntil = Date.now() + 1500;
        log('desync forcing default skin', defaultSkinId);
        fetch(SELECTION_ENDPOINT, {
            method: 'PATCH',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ selectedSkinId: defaultSkinId }),
        }).catch((e) => {
            allowNextSelectionPatch = false;
            log('desync default PATCH failed:', e && e.message);
        });
    }

    function scheduleFinalization(remainingMs) {
        if (finalizationDefaultPatchTimer !== null || finalizationInjectTimer !== null) {
            return;
        }
        const championId = desyncChampionId;
        if (!championId) {
            return;
        }

        const defaultDelay = Math.max(0, remainingMs - FINALIZATION_DEFAULT_PATCH_BEFORE_MS);
        const injectDelay = Math.max(0, remainingMs - FINALIZATION_INJECT_BEFORE_MS);

        log(
            'desync finalization remaining',
            remainingMs,
            'defaultPatchDelay',
            defaultDelay,
            'injectDelay',
            injectDelay
        );

        finalizationDefaultPatchTimer = setTimeout(() => {
            finalizationDefaultPatchTimer = null;
            if (desyncChampionId === championId) {
                applyFinalSelection(championId);
            }
        }, defaultDelay);

        finalizationInjectTimer = setTimeout(() => {
            finalizationInjectTimer = null;
            if (finalSelectionKind === 'custom' && finalSelectionSkinId !== null) {
                log('desync final custom inject', finalSelectionSkinId);
                customOverlayPrepared = true;
                sendBridgeMessage({ type: 'skin-hovered', skinId: finalSelectionSkinId });
            }
        }, injectDelay);
    }

    function applyFinalSelection(championId) {
        if (finalSelectionKind === 'native' && finalSelectionSkinId !== null) {
            log('desync final native PATCH', finalSelectionSkinId);
            clearPreparedCustomSkin();
            allowNextSelectionPatch = true;
            fetch(SELECTION_ENDPOINT, {
                method: 'PATCH',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ selectedSkinId: finalSelectionSkinId }),
            }).catch((e) => {
                allowNextSelectionPatch = false;
                log('desync native final PATCH failed:', e && e.message);
            });
            return;
        }

        if (finalSelectionKind !== 'custom') {
            clearPreparedCustomSkin();
        }
        forceDefaultSkin(championId);
    }

    function clearFinalizationTimers() {
        if (finalizationDefaultPatchTimer !== null) {
            clearTimeout(finalizationDefaultPatchTimer);
            finalizationDefaultPatchTimer = null;
        }
        if (finalizationInjectTimer !== null) {
            clearTimeout(finalizationInjectTimer);
            finalizationInjectTimer = null;
        }
    }

    function clearPreparedCustomSkin() {
        if (lastCustomSkinId === null) {
            return;
        }
        lastCustomSkinId = null;
        if (customOverlayPrepared) {
            customOverlayPrepared = false;
            sendBridgeMessage({ type: 'skin-cleared' });
        }
    }

    function setFinalSelection(kind, skinId) {
        finalSelectionKind = kind;
        finalSelectionSkinId = skinId;
        logDesyncCandidate(kind, skinId);
        if (kind === 'custom') {
            lastCustomSkinId = skinId;
        } else if (kind === 'native' || kind === 'base') {
            clearPreparedCustomSkin();
        }
    }

    function logDesyncCandidate(kind, skinId) {
        const key = `${kind}:${skinId ?? 'null'}`;
        if (key === lastLoggedCandidateKey) {
            return;
        }
        lastLoggedCandidateKey = key;
        let label = kind;
        if (kind === 'custom') {
            label = 'custom talon';
        } else if (kind === 'native') {
            label = 'owned official';
        } else if (kind === 'locked') {
            label = 'unowned official';
        } else if (kind === 'base') {
            label = 'base';
        }
        const message = `desync hovered skin: ${skinId} (${label})`;
        log(message);
        sendBridgeMessage({ type: 'log', message });
    }

    function resetDesyncState() {
        clearFinalizationTimers();
        clearPreparedCustomSkin();
        desyncChampionId = null;
        defaultSkinPatchSent = false;
        patchBlockingEnabled = false;
        allowNextSelectionPatch = false;
        finalSelectionKind = null;
        finalSelectionSkinId = null;
        lastLoggedCandidateKey = null;
        customOverlayPrepared = false;
        suppressBaseBackendClearUntil = 0;
    }

    function injectGoldenBorderStyle() {
        if (document.getElementById(TALON_GOLDEN_STYLE_ID)) {
            return;
        }
        const style = document.createElement('style');
        style.id = TALON_GOLDEN_STYLE_ID;
        style.textContent = `
.skin-selection-carousel .skin-selection-item.skin-selection-item-selected:not(.talon-golden-selected) {
  background: #3c3c41 !important;
  outline: 1px solid transparent !important;
}
.skin-selection-carousel .skin-selection-item.skin-selection-item-selected:not(.talon-golden-selected) .skin-selection-thumbnail {
  height: calc(100% - 2px) !important;
  margin: 1px !important;
}
.skin-selection-carousel .skin-selection-item.skin-carousel-offset-2:hover .skin-selection-thumbnail,
.skin-selection-carousel .skin-selection-item.skin-carousel-offset-2.skin-selection-item-selected:hover .skin-selection-thumbnail {
  filter: none !important;
}
.skin-selection-carousel .skin-selection-item.skin-selection-item-selected:not(.talon-golden-selected):hover {
  background: linear-gradient(180deg, #f0e6b2 0%, #f5ecc4 30%, #d4a83c 70%, #c89b3c 100%) !important;
  outline: 1px solid rgba(1, 10, 19, 0.6) !important;
}
.skin-selection-carousel .skin-selection-item.skin-selection-item-selected:not(.talon-golden-selected):hover .skin-selection-thumbnail {
  filter: brightness(1.2) saturate(1.1) !important;
}
`;
        document.head.appendChild(style);
    }

    function removeGoldenBorderStyle() {
        const style = document.getElementById(TALON_GOLDEN_STYLE_ID);
        if (style && style.parentNode) {
            style.parentNode.removeChild(style);
        }
    }

    function clearGoldenBorderItem() {
        if (styledGoldenItem) {
            styledGoldenItem.classList.remove('talon-golden-selected');
            styledGoldenItem.style.removeProperty('background');
            styledGoldenItem.style.removeProperty('outline');
            styledGoldenItem = null;
        }
        if (styledGoldenThumbnail) {
            styledGoldenThumbnail.style.removeProperty('height');
            styledGoldenThumbnail.style.removeProperty('margin');
            styledGoldenThumbnail = null;
        }
    }

    function normalizeAssetUrl(url) {
        if (typeof url !== 'string' || url.length === 0) {
            return null;
        }
        const q = url.indexOf('?');
        return q === -1 ? url : url.slice(0, q);
    }

    function collectSkinAssetUrls(skin) {
        if (!skin) {
            return [];
        }
        return [
            skin.tilePath,
            skin.splashPath,
            skin.uncenteredSplashPath,
            skin.centeredSplashPath,
            skin.loadScreenPath,
            skin.loadscreenPath,
            skin.cardSplashPath,
            skin.iconPath,
            skin.squarePortraitPath,
            skin.chromaPreviewPath,
        ]
            .map(normalizeAssetUrl)
            .filter(Boolean);
    }

    function elementUsesAsset(element, assetUrls) {
        if (!element || assetUrls.length === 0) {
            return false;
        }
        const src = normalizeAssetUrl(element.currentSrc || element.src || '');
        if (src && assetUrls.some((url) => src.includes(url) || url.includes(src))) {
            return true;
        }
        const style = window.getComputedStyle(element);
        const background = style && style.backgroundImage;
        return (
            typeof background === 'string' &&
            assetUrls.some((url) => background.includes(url))
        );
    }

    function findCarouselItemForSkin(skin) {
        const assetUrls = collectSkinAssetUrls(skin);
        if (assetUrls.length === 0) {
            return null;
        }
        const items = document.querySelectorAll(
            '.skin-selection-carousel .skin-selection-item'
        );
        for (const item of items) {
            if (elementUsesAsset(item, assetUrls)) {
                return item;
            }
            const media = item.querySelectorAll('img, [style]');
            for (const element of media) {
                if (elementUsesAsset(element, assetUrls)) {
                    return item;
                }
            }
        }
        return null;
    }

    function enforceGoldenBorder() {
        const centeredItem = document.querySelector(
            '.skin-selection-carousel .skin-selection-item.skin-carousel-offset-2'
        );
        if (!centeredItem) {
            return;
        }
        const thumbnail = centeredItem.querySelector('.skin-selection-thumbnail');
        if (!thumbnail) {
            return;
        }

        if (styledGoldenItem && styledGoldenItem !== centeredItem) {
            clearGoldenBorderItem();
        }

        styledGoldenItem = centeredItem;
        styledGoldenThumbnail = thumbnail;

        centeredItem.classList.add('talon-golden-selected');
        centeredItem.style.setProperty(
            'background',
            'linear-gradient(0deg, #c8aa6e 0, #c89b3c 44%, #a07b32 59%, #785a28)',
            'important'
        );
        centeredItem.style.setProperty(
            'outline',
            '1px solid rgba(1, 10, 19, 0.6)',
            'important'
        );
        thumbnail.style.setProperty('height', 'calc(100% - 4px)', 'important');
        thumbnail.style.setProperty('margin', '2px', 'important');
    }

    function enforceHeldGoldenBorder() {
        const item = findCarouselItemForSkin(goldenBorderSkin);
        if (item && item !== styledGoldenItem) {
            clearGoldenBorderItem();
            styledGoldenItem = item;
            styledGoldenThumbnail = item.querySelector('.skin-selection-thumbnail');
        }
        if (!styledGoldenItem || !styledGoldenItem.isConnected) {
            stopGoldenBorder();
            return;
        }
        const thumbnail =
            styledGoldenThumbnail && styledGoldenThumbnail.isConnected
                ? styledGoldenThumbnail
                : styledGoldenItem.querySelector('.skin-selection-thumbnail');
        if (!thumbnail) {
            stopGoldenBorder();
            return;
        }

        styledGoldenThumbnail = thumbnail;
        styledGoldenItem.classList.add('talon-golden-selected');
        styledGoldenItem.style.setProperty(
            'background',
            'linear-gradient(0deg, #c8aa6e 0, #c89b3c 44%, #a07b32 59%, #785a28)',
            'important'
        );
        styledGoldenItem.style.setProperty(
            'outline',
            '1px solid rgba(1, 10, 19, 0.6)',
            'important'
        );
        thumbnail.style.setProperty('height', 'calc(100% - 4px)', 'important');
        thumbnail.style.setProperty('margin', '2px', 'important');
    }

    function startGoldenBorder(skinId) {
        if (
            goldenBorderSkinId === skinId &&
            goldenBorderTimer !== null &&
            goldenBorderMode === 'centered'
        ) {
            return;
        }
        stopGoldenBorder();
        goldenBorderSkinId = skinId;
        goldenBorderSkin = skinIdToSkin.get(skinId) || null;
        goldenBorderMode = 'centered';
        injectGoldenBorderStyle();
        enforceGoldenBorder();
        goldenBorderTimer = setInterval(enforceGoldenBorder, GOLDEN_BORDER_ENFORCE_MS);
        log('started golden border enforcement for talon skin', skinId);
    }

    function holdGoldenBorderOnLastCustom() {
        if (goldenBorderSkinId === null) {
            return;
        }
        if (goldenBorderTimer !== null && goldenBorderMode !== 'held') {
            clearInterval(goldenBorderTimer);
            goldenBorderTimer = null;
        }
        goldenBorderMode = 'held';
        injectGoldenBorderStyle();
        enforceHeldGoldenBorder();
        if (goldenBorderTimer === null) {
            goldenBorderTimer = setInterval(
                enforceHeldGoldenBorder,
                GOLDEN_BORDER_ENFORCE_MS
            );
        }
    }

    function stopGoldenBorder() {
        if (goldenBorderTimer !== null) {
            clearInterval(goldenBorderTimer);
            goldenBorderTimer = null;
        }
        goldenBorderSkinId = null;
        goldenBorderSkin = null;
        goldenBorderMode = null;
        removeGoldenBorderStyle();
        clearGoldenBorderItem();
    }

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
        skinIdToSkin.clear();
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
        skinIdToSkin.set(skin.id, skin);
        if (typeof skin.name === 'string' && skin.name.length > 0) {
            skinNameToId.set(skin.name.trim().toLowerCase(), skin.id);
        }
    }

    function isLockedCarouselSkin(skin) {
        if (!skin) {
            return false;
        }
        const ownership = skin.ownership || {};
        return (
            skin.disabled === true ||
            skin.unlocked === false ||
            ownership.owned === false ||
            ownership.rental?.rented === false && ownership.owned === false
        );
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

    function currentCenteredSkinLooksBase() {
        const centeredItem = document.querySelector(
            '.skin-selection-carousel .skin-selection-item.skin-carousel-offset-2'
        );
        if (!centeredItem) {
            return false;
        }
        if (centeredItem.classList.contains('talon-golden-selected')) {
            return false;
        }
        const style = window.getComputedStyle(centeredItem);
        const background = style && style.backgroundImage;
        if (typeof background === 'string' && background.includes('talon/assets')) {
            return false;
        }
        const media = centeredItem.querySelectorAll('img, [style]');
        for (const element of media) {
            if (elementUsesAsset(element, collectSkinAssetUrls(goldenBorderSkin))) {
                return false;
            }
        }
        return centeredItem.classList.contains('skin-selection-item-selected');
    }

    function tickSkinMonitor() {
        const name = readCurrentSkinName();
        if (!name) {
            stopGoldenBorder();
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
        const skin = skinIdToSkin.get(id);
        if (currentCenteredSkinLooksBase() && id % 1000 === 0) {
            const suppressBackendClear = Date.now() < suppressBaseBackendClearUntil;
            stopGoldenBorder();
            if (!suppressBackendClear) {
                setFinalSelection('base', id);
            }
            if (lastSentSkinId !== null && !suppressBackendClear) {
                lastSentSkinId = null;
                if (!champSelectActive) {
                    sendBridgeMessage({ type: 'skin-cleared' });
                }
            }
            return;
        }
        if (id >= CUSTOM_ID_FLOOR) {
            setFinalSelection('custom', id);
            startGoldenBorder(id);
        } else if (isLockedCarouselSkin(skin)) {
            logDesyncCandidate('locked', id);
            holdGoldenBorderOnLastCustom();
        } else if (id % 1000 === 0) {
            setFinalSelection('base', id);
            stopGoldenBorder();
        } else {
            setFinalSelection('native', id);
            startGoldenBorder(id);
        }
        if (id === lastSentSkinId) {
            return;
        }
        lastSentSkinId = id;
        if (champSelectActive) {
            return;
        }
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
