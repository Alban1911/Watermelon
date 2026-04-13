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
// Images still reuse the base skin's splashPath and tilePath as
// placeholders. Real previews via `https://talon/assets/<id>.png` are
// Step 5c.

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
    // Custom skin ids live above this floor so we can detect ones
    // already injected into a value.data array and skip double-inject.
    const CUSTOM_ID_FLOOR = 9_000_000;

    const pluginApis = {};
    window.__talonPluginApis = pluginApis;
    window.__talonSkinIndex = {};

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
        const cache =
            api && api.champSelectBinding && api.champSelectBinding.cache && api.champSelectBinding.cache._data;
        if (!cache || typeof cache.set !== 'function') {
            log(
                TARGET_PLUGIN,
                'cache._data.set not reachable — api keys:',
                Object.keys(api || {})
            );
            return;
        }

        // Pre-fetch the Talon skin index before installing the cache
        // hook so the hook can read it synchronously when Ember writes
        // the carousel data. Install the hook regardless of fetch
        // outcome — on failure the index stays empty and we simply
        // don't inject anything.
        fetch(TALON_INDEX_URL)
            .then((r) => r.json())
            .then((index) => {
                window.__talonSkinIndex = index || {};
                const champCount = Object.keys(window.__talonSkinIndex).length;
                log('talon skin index loaded:', champCount, 'champion(s)');
            })
            .catch((e) => {
                log('talon skin index fetch failed:', e && e.message);
            })
            .finally(() => {
                installCacheHook(cache);
            });
    }

    // ── Carousel cache hook ──────────────────────────────────────────
    function installCacheHook(cache) {
        const originalSet = cache.set.bind(cache);
        cache.set = function (key, value) {
            if (key !== CAROUSEL_CACHE_KEY) {
                return originalSet(key, value);
            }
            if (!value || !Array.isArray(value.data) || value.data.length === 0) {
                return originalSet(key, value);
            }

            const baseSkin = value.data[0];
            const championId = baseSkin && baseSkin.championId;
            if (!championId) {
                log('carousel set: no championId on base skin, skipping injection');
                return originalSet(key, value);
            }

            // Idempotent: if any Talon skin is already present (id in
            // the custom range) we've already handled this data array.
            if (value.data.some((s) => s && typeof s.id === 'number' && s.id >= CUSTOM_ID_FLOOR)) {
                return originalSet(key, value);
            }

            const talonSkins =
                (window.__talonSkinIndex || {})[String(championId)] || [];
            if (talonSkins.length === 0) {
                return originalSet(key, value);
            }

            talonSkins.forEach((entry, i) => {
                value.data.splice(1 + i, 0, makeCarouselSkin(entry, baseSkin, championId));
            });
            log(
                'injected',
                talonSkins.length,
                'talon skin(s) for championId',
                championId,
                '(total entries now',
                value.data.length + ')'
            );

            return originalSet(key, value);
        };
        log('cache._data.set hook installed — waiting for champ-select');
    }

    // Builds a carousel entry from a Talon index entry. Image paths
    // reuse the base skin's so the tile renders without external
    // assets. Real preview images via `https://talon/assets/...` come
    // in the next step.
    function makeCarouselSkin(entry, baseSkin, championId) {
        return {
            championId: championId,
            childSkins: [],
            chromaPreviewPath: null,
            disabled: false,
            emblems: [],
            groupSplash: '',
            id: entry.id,
            isBase: false,
            isChampionUnlocked: true,
            name: entry.name,
            ownership: {
                loyaltyReward: false,
                owned: true,
                rental: { rented: false },
                xboxGPReward: false,
            },
            productType: null,
            rarityGemPath: '',
            skinAugments: {},
            splashPath: baseSkin.splashPath || '',
            splashVideoPath: null,
            stillObtainable: false,
            tilePath: baseSkin.tilePath || '',
            unlocked: true,
        };
    }

    log('document.dispatchEvent hook installed');
})();
