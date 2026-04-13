// Talon preload — Step 3 of the carousel-injection rework.
//
// When the rcp-fe-lol-champ-select plugin announces itself via a
// `riotPlugin.announce:` DOM event, we wrap the event's
// `registrationHandler` so we can capture the plugin's exported API
// once init resolves (same trick PenguLoader uses internally).
//
// Once we have the API, we wrap `api.champSelectBinding.cache._data.set`
// to splice a hardcoded test skin entry into the carousel data before
// Ember renders it. The test skin reuses the base skin's splashPath and
// tilePath so no external image dependency is required — the user
// should see a second carousel entry named "Talon Test Skin" with the
// same artwork as the base skin.
//
// Scope: Step 3 is visual proof only. Clicking the test skin is not
// handled yet — that's a later step once we have real skin data flowing
// from Talon's Rust backend.

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
    // Custom skin IDs above 9_000_000 avoid colliding with official skin
    // IDs (which fit under championId * 1000 + variant).
    const CUSTOM_SKIN_ID = 9_000_099;

    const pluginApis = {};
    window.__talonPluginApis = pluginApis;

    // ── RCP plugin API capture (Step 2) ──────────────────────────────
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
        const cache = api && api.champSelectBinding && api.champSelectBinding.cache && api.champSelectBinding.cache._data;
        if (!cache || typeof cache.set !== 'function') {
            log(
                TARGET_PLUGIN,
                'cache._data.set not reachable — api keys:',
                Object.keys(api || {})
            );
            return;
        }
        installCacheHook(cache);
    }

    // ── Carousel cache hook (Step 3) ─────────────────────────────────
    function installCacheHook(cache) {
        const originalSet = cache.set.bind(cache);
        cache.set = function (key, value) {
            if (key !== CAROUSEL_CACHE_KEY) {
                return originalSet(key, value);
            }
            if (!value || !Array.isArray(value.data) || value.data.length === 0) {
                return originalSet(key, value);
            }
            // Idempotent: if we already injected into this data array, skip.
            if (value.data.some((s) => s && s.id === CUSTOM_SKIN_ID)) {
                return originalSet(key, value);
            }
            const baseSkin = value.data[0];
            const championId = baseSkin && baseSkin.championId;
            if (!championId) {
                log('carousel set: no championId on base skin, skipping injection');
                return originalSet(key, value);
            }

            value.data.splice(1, 0, makeTestSkin(baseSkin, championId));
            log(
                'injected test skin into carousel for championId',
                championId,
                '(total entries:',
                value.data.length + ')'
            );
            return originalSet(key, value);
        };
        log('cache._data.set hook installed — waiting for champ-select');
    }

    // Builds a synthetic carousel entry that matches the shape of an LCU
    // skin object closely enough for Ember to render it. Image paths reuse
    // the base skin's so the entry renders without any external assets.
    function makeTestSkin(baseSkin, championId) {
        return {
            championId: championId,
            childSkins: [],
            chromaPreviewPath: null,
            disabled: false,
            emblems: [],
            groupSplash: '',
            id: CUSTOM_SKIN_ID,
            isBase: false,
            isChampionUnlocked: true,
            name: 'Talon Test Skin',
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
