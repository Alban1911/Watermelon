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

    const pluginApis = {};
    window.__talonPluginApis = pluginApis;
    window.__talonSkinIndex = {};
    let cacheRef = null;
    let parentCacheRef = null;
    let lastIndexJson = '{}';
    let lastIndexVersion = '0';
    let lastLiveCarouselValue = null;
    let refreshTimer = null;

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

    function makeAssetUrl(baseUrl, fileStem) {
        if (!fileStem) {
            return null;
        }
        return baseUrl + encodeURIComponent(fileStem) + '.png';
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
                ? makeAssetUrl(TALON_SPLASH_ASSET_BASE_URL, entry.fileStem)
                : null;
        const backgroundAssetUrl =
            entry && entry.hasBackgroundAsset
                ? makeAssetUrl(TALON_BACKGROUND_ASSET_BASE_URL, entry.fileStem)
                : null;
        const tileAssetUrl =
            entry && entry.hasTileAsset
                ? makeAssetUrl(TALON_TILE_ASSET_BASE_URL, entry.fileStem)
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

    log('document.dispatchEvent hook installed');
})();
