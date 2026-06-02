import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  ArrowLeft,
  FolderOpen,
  Group,
  Loader2,
  MoreHorizontal,
  Moon,
  Power,
  Plus,
  RotateCw,
  Settings,
  Sun,
  Trash2,
  X,
} from "lucide-react";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";

import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { cn } from "@/lib/utils";

type Skin = {
  id: string;
  name: string;
  champion: string;
  author: string | null;
  version: string | null;
  description: string | null;
  preview: string | null;
  champion_icon: string | null;
  tile_preview_custom: boolean;
  background_preview_custom: boolean;
  enabled: boolean;
};

type SkinLibrary = {
  dir: string;
  skins: Skin[];
};

type LeaguePathState = {
  path: string | null;
  isResolving: boolean;
  error: string | null;
};

type HookState = {
  active: boolean;
  isLoading: boolean;
  error: string | null;
};

type CslolDllState = {
  path: string;
  exists: boolean;
  isChecking: boolean;
  error: string | null;
};

const GROUP_STORAGE_KEY = "talon:groupByChampion";
const THEME_STORAGE_KEY = "talon:theme";

/** Computes the "enabled first, then alphabetical by champion, then by
 *  skin name" ordering as a flat array of skin IDs. Used as a display
 *  snapshot so sorting only happens at navigation breakpoints, not on
 *  every toggle. */
function computeSortOrder(skins: Skin[]): string[] {
  return [...skins]
    .sort((a, b) => {
      if (a.enabled !== b.enabled) return a.enabled ? -1 : 1;
      const champ = a.champion.localeCompare(b.champion, undefined, {
        sensitivity: "base",
      });
      if (champ !== 0) return champ;
      return a.name.localeCompare(b.name, undefined, { sensitivity: "base" });
    })
    .map((s) => s.id);
}

/** Reads the stored theme preference, falling back to the OS setting. */
function readInitialTheme(): boolean {
  const stored = localStorage.getItem(THEME_STORAGE_KEY);
  if (stored === "dark") return true;
  if (stored === "light") return false;
  return window.matchMedia("(prefers-color-scheme: dark)").matches;
}

// Apply the theme class *before* React renders so there's no flash of
// the wrong palette on first paint.
if (readInitialTheme()) {
  document.documentElement.classList.add("dark");
}

function App() {
  const [library, setLibrary] = useState<SkinLibrary | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [leaguePath, setLeaguePath] = useState<LeaguePathState>({
    path: null,
    isResolving: true,
    error: null,
  });
  const [hookState, setHookState] = useState<HookState>({
    active: false,
    isLoading: true,
    error: null,
  });
  const [cslolDll, setCslolDll] = useState<CslolDllState>({
    path: "",
    exists: false,
    isChecking: true,
    error: null,
  });
  const [isDragging, setIsDragging] = useState(false);
  const [isImporting, setIsImporting] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<Skin | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [isDark, setIsDark] = useState(readInitialTheme);
  const [groupByChampion, setGroupByChampion] = useState(() => {
    // Default to grouped on first launch; respect the user's explicit
    // choice on subsequent launches.
    const stored = localStorage.getItem(GROUP_STORAGE_KEY);
    return stored === null ? true : stored === "1";
  });
  const [selectedChampion, setSelectedChampion] = useState<string | null>(null);

  useEffect(() => {
    document.documentElement.classList.toggle("dark", isDark);
    localStorage.setItem(THEME_STORAGE_KEY, isDark ? "dark" : "light");
  }, [isDark]);

  useEffect(() => {
    localStorage.setItem(GROUP_STORAGE_KEY, groupByChampion ? "1" : "0");
  }, [groupByChampion]);

  // Drop any drill-down when the user toggles grouping — keeps the mental
  // model simple: each mode opens at its top level.
  useEffect(() => {
    setSelectedChampion(null);
  }, [groupByChampion]);

  // Display order snapshot — an array of skin IDs in the order we want to
  // render them. Refreshed only at "natural breakpoints" (library load,
  // view mode toggle, champion drill-in/out). Toggling a skin's enabled
  // flag flips its state but leaves its position alone until the next
  // breakpoint, which avoids the jarring "card flies away on click" effect
  // while still paying off the "enabled first" intuition on every
  // navigation step.
  const [displayOrder, setDisplayOrder] = useState<string[]>([]);

  const sortedSkins = useMemo(() => {
    if (!library) return [] as Skin[];
    const indexMap = new Map<string, number>();
    displayOrder.forEach((id, i) => indexMap.set(id, i));
    return [...library.skins].sort((a, b) => {
      const ia = indexMap.get(a.id) ?? Number.MAX_SAFE_INTEGER;
      const ib = indexMap.get(b.id) ?? Number.MAX_SAFE_INTEGER;
      return ia - ib;
    });
  }, [library, displayOrder]);

  const skinsByChampion = useMemo(() => {
    const map = new Map<string, Skin[]>();
    for (const skin of sortedSkins) {
      const bucket = map.get(skin.champion);
      if (bucket) {
        bucket.push(skin);
      } else {
        map.set(skin.champion, [skin]);
      }
    }
    return map;
  }, [sortedSkins]);

  const championGroups = useMemo(() => {
    return Array.from(skinsByChampion.entries()).sort((a, b) =>
      a[0].localeCompare(b[0], undefined, { sensitivity: "base" }),
    );
  }, [skinsByChampion]);

  // If the selected champion disappears (e.g. last skin for that champion
  // removed), bail back to the champion grid.
  useEffect(() => {
    if (selectedChampion && !skinsByChampion.has(selectedChampion)) {
      setSelectedChampion(null);
    }
  }, [selectedChampion, skinsByChampion]);

  const load = useCallback(async () => {
    try {
      const result = await invoke<SkinLibrary>("list_skins");
      setDisplayOrder(computeSortOrder(result.skins));
      setLibrary(result);
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  const refreshHookStatus = useCallback(async () => {
    try {
      const active = await invoke<boolean>("pengu_status");
      setHookState({ active, isLoading: false, error: null });
    } catch (e) {
      setHookState({ active: false, isLoading: false, error: String(e) });
    }
  }, []);

  // Re-snapshot the sort order whenever the user changes what they're
  // looking at — view mode swap (flat ↔ grouped) or champion drill-in/out.
  // The library itself hasn't changed, so this is purely a re-sort: any
  // skins toggled since the last breakpoint now cluster to the top of the
  // view they just navigated into.
  useEffect(() => {
    if (!library) return;
    setDisplayOrder(computeSortOrder(library.skins));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [groupByChampion, selectedChampion]);

  useEffect(() => {
    load();
    refreshHookStatus();
  }, [load, refreshHookStatus]);

  const refreshCslolDll = useCallback(async () => {
    setCslolDll((cur) => ({ ...cur, isChecking: true, error: null }));
    try {
      const result = await invoke<{ path: string; exists: boolean }>(
        "get_cslol_dll_status",
      );
      setCslolDll({
        path: result.path,
        exists: result.exists,
        isChecking: false,
        error: null,
      });
    } catch (e) {
      setCslolDll((cur) => ({
        ...cur,
        isChecking: false,
        error: String(e),
      }));
    }
  }, []);

  const refreshLeaguePath = useCallback(async () => {
    setLeaguePath((cur) => ({ ...cur, isResolving: true, error: null }));
    try {
      const saved = await invoke<string | null>("get_league_install_path");
      if (saved) {
        setLeaguePath({ path: saved, isResolving: false, error: null });
        return;
      }
      const detected = await invoke<string | null>("detect_league_install_path");
      setLeaguePath({ path: detected, isResolving: false, error: null });
    } catch (e) {
      setLeaguePath({
        path: null,
        isResolving: false,
        error: String(e),
      });
    }
  }, []);

  useEffect(() => {
    void refreshLeaguePath();
  }, [refreshLeaguePath]);

  useEffect(() => {
    void refreshCslolDll();
  }, [refreshCslolDll]);

  const handlePickLeagueFolder = async () => {
    try {
      const selected = await openFileDialog({
        directory: true,
        multiple: false,
        title: "Select your League of Legends install folder",
      });
      if (!selected) return;
      const chosen = Array.isArray(selected) ? selected[0] : selected;
      if (!chosen) return;
      setLeaguePath((cur) => ({ ...cur, isResolving: true, error: null }));
      const saved = await invoke<string>("set_league_install_path", {
        path: chosen,
      });
      setLeaguePath({ path: saved, isResolving: false, error: null });
    } catch (e) {
      setLeaguePath((cur) => ({
        ...cur,
        isResolving: false,
        error: String(e),
      }));
    }
  };

  const handleAutoDetectLeague = async () => {
    setLeaguePath((cur) => ({ ...cur, isResolving: true, error: null }));
    try {
      const detected = await invoke<string | null>("detect_league_install_path");
      setLeaguePath({
        path: detected,
        isResolving: false,
        error: detected ? null : "League install path not detected automatically.",
      });
    } catch (e) {
      setLeaguePath({
        path: null,
        isResolving: false,
        error: String(e),
      });
    }
  };

  const handleOpenCslolDllFolder = async () => {
    try {
      await invoke("open_cslol_dll_folder");
      await refreshCslolDll();
    } catch (e) {
      setCslolDll((cur) => ({ ...cur, error: String(e) }));
    }
  };

  // Re-scan whenever the window regains focus — the user has likely
  // just dropped a file into the skins folder via Explorer.
  useEffect(() => {
    const onFocus = () => {
      void load();
      void refreshCslolDll();
    };
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [load, refreshCslolDll]);

  useEffect(() => {
    let unlisten: (() => void) | null = null;
    void listen("library:assets-updated", () => {
      void load();
    }).then((dispose) => {
      unlisten = dispose;
    });
    return () => {
      unlisten?.();
    };
  }, [load]);

  const setEnabled = async (id: string, enabled: boolean) => {
    setLibrary((cur) =>
      cur
        ? {
            ...cur,
            skins: cur.skins.map((s) => (s.id === id ? { ...s, enabled } : s)),
          }
        : cur,
    );
    try {
      await invoke("set_skin_enabled", { id, enabled });
    } catch (e) {
      setError(String(e));
      load();
    }
  };

  const handleOpenFolder = async () => {
    try {
      await invoke("open_skins_folder");
    } catch (e) {
      setError(String(e));
    }
  };

  const setHookActive = async (active: boolean) => {
    setHookState((cur) => ({ ...cur, isLoading: true, error: null }));
    try {
      await invoke(active ? "activate_pengu" : "deactivate_pengu");
      setHookState({ active, isLoading: false, error: null });
    } catch (e) {
      setHookState((cur) => ({ ...cur, isLoading: false, error: String(e) }));
      await refreshHookStatus();
    }
  };

  const handleDeleteSkin = (skin: Skin) => {
    setDeleteTarget(skin);
  };

  const confirmDelete = async () => {
    if (!deleteTarget) return;
    const id = deleteTarget.id;
    setDeleteTarget(null);
    try {
      await invoke("delete_skin", { id });
      await load();
    } catch (e) {
      setError(String(e));
    }
  };

  // Close the delete dialog on Escape.
  useEffect(() => {
    if (!deleteTarget) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setDeleteTarget(null);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [deleteTarget]);

  // Close the settings dialog on Escape.
  useEffect(() => {
    if (!settingsOpen) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setSettingsOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [settingsOpen]);

  const importFiles = useCallback(
    async (files: File[]) => {
      const fantomes = files.filter((f) =>
        f.name.toLowerCase().endsWith(".fantome"),
      );
      if (fantomes.length === 0) {
        setError("Only .fantome files can be imported.");
        return;
      }
      setIsImporting(true);
      try {
        for (const file of fantomes) {
          const bytes = new Uint8Array(await file.arrayBuffer());
          await invoke("import_skin_bytes", {
            filename: file.name,
            bytes,
          });
        }
        await load();
      } catch (e) {
        setError(String(e));
      } finally {
        setIsImporting(false);
      }
    },
    [load],
  );

  const handleImport = async () => {
    try {
      const selected = await openFileDialog({
        multiple: true,
        filters: [{ name: "Skin Mod", extensions: ["fantome"] }],
      });
      if (!selected) return;
      const paths = Array.isArray(selected) ? selected : [selected];
      setIsImporting(true);
      try {
        for (const path of paths) {
          await invoke("import_skin", { source: path });
        }
        await load();
      } finally {
        setIsImporting(false);
      }
    } catch (e) {
      setError(String(e));
    }
  };

  // HTML5 drag-and-drop. We can't use Tauri's internal drag-drop system
  // because its events don't reliably fire on Windows — so we set
  // dragDropEnabled: false in tauri.conf.json, which lets DOM drag events
  // flow through. File paths aren't exposed to the browser, so we read
  // the bytes and pass them to the Rust import_skin_bytes command.
  const dragCounterRef = useRef(0);
  useEffect(() => {
    const onDragEnter = (e: DragEvent) => {
      e.preventDefault();
      dragCounterRef.current += 1;
      if (dragCounterRef.current === 1) setIsDragging(true);
    };
    const onDragOver = (e: DragEvent) => {
      e.preventDefault();
    };
    const onDragLeave = (e: DragEvent) => {
      e.preventDefault();
      dragCounterRef.current = Math.max(0, dragCounterRef.current - 1);
      if (dragCounterRef.current === 0) setIsDragging(false);
    };
    const onDrop = (e: DragEvent) => {
      e.preventDefault();
      dragCounterRef.current = 0;
      setIsDragging(false);
      const files = Array.from(e.dataTransfer?.files ?? []);
      if (files.length > 0) importFiles(files);
    };

    document.addEventListener("dragenter", onDragEnter);
    document.addEventListener("dragover", onDragOver);
    document.addEventListener("dragleave", onDragLeave);
    document.addEventListener("drop", onDrop);
    return () => {
      document.removeEventListener("dragenter", onDragEnter);
      document.removeEventListener("dragover", onDragOver);
      document.removeEventListener("dragleave", onDragLeave);
      document.removeEventListener("drop", onDrop);
    };
  }, [importFiles]);

  const drilledSkins = selectedChampion
    ? (skinsByChampion.get(selectedChampion) ?? [])
    : [];

  return (
    <div className="min-h-screen bg-background text-foreground">
      <header className="border-b">
        <div className="mx-auto flex max-w-6xl items-center justify-between px-6 py-5">
          <div>
            <h1 className="text-xl font-semibold">Talon</h1>
            <p className="text-xs text-muted-foreground">
              League of Legends custom skin manager
            </p>
          </div>
          <div className="flex items-center gap-2">
            <Button
              size="icon"
              variant="ghost"
              onClick={() => setSettingsOpen(true)}
              aria-label="Open settings"
              title="Settings"
            >
              <Settings />
            </Button>
            <Button
              size="icon"
              variant={hookState.active ? "default" : "outline"}
              onClick={() => setHookActive(!hookState.active)}
              disabled={hookState.isLoading}
              aria-label={hookState.active ? "Disable League hook" : "Enable League hook"}
              title={hookState.active ? "Disable League hook" : "Enable League hook"}
            >
              {hookState.isLoading ? <Loader2 className="animate-spin" /> : <Power />}
            </Button>
            <Button
              size="icon"
              variant="ghost"
              onClick={() => setIsDark((v) => !v)}
              aria-label={isDark ? "Switch to light mode" : "Switch to dark mode"}
              title={isDark ? "Switch to light mode" : "Switch to dark mode"}
            >
              {isDark ? <Sun /> : <Moon />}
            </Button>
            <Button variant="outline" onClick={handleOpenFolder}>
              <FolderOpen />
              Open folder
            </Button>
            <Button onClick={handleImport}>
              <Plus />
              Import skin
            </Button>
          </div>
        </div>
      </header>

      <main className="mx-auto max-w-6xl px-6 py-8">
        {error && (
          <div className="mb-4 rounded-lg border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
            {error}
          </div>
        )}
        {hookState.error && (
          <div className="mb-4 rounded-lg border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
            {hookState.error}
          </div>
        )}

        {library === null ? (
          <p className="text-xs text-muted-foreground">Loading…</p>
        ) : (
          <>
            <div className="mb-4 flex items-baseline justify-between">
              {selectedChampion ? (
                <div className="flex items-center gap-2">
                  <Button
                    size="xs"
                    variant="ghost"
                    onClick={() => setSelectedChampion(null)}
                  >
                    <ArrowLeft />
                    Back
                  </Button>
                  <h2 className="text-sm font-medium capitalize">
                    {selectedChampion}
                  </h2>
                </div>
              ) : (
                <h2 className="text-sm font-medium">Skin library</h2>
              )}
              <div className="flex items-center gap-2">
                <p className="text-xs text-muted-foreground">
                  {selectedChampion
                    ? `${drilledSkins.filter((s) => s.enabled).length} of ${drilledSkins.length} enabled`
                    : `${library.skins.filter((s) => s.enabled).length} of ${library.skins.length} enabled`}
                </p>
                <Button
                  size="xs"
                  variant={groupByChampion ? "default" : "outline"}
                  onClick={() => setGroupByChampion((v) => !v)}
                  aria-pressed={groupByChampion}
                  title="Group by champion"
                >
                  <Group />
                  Group
                </Button>
                <Button
                  size="icon-xs"
                  variant="ghost"
                  onClick={load}
                  aria-label="Reload"
                >
                  <RotateCw />
                </Button>
              </div>
            </div>

            {library.skins.length === 0 ? (
              <EmptyState onImport={handleImport} />
            ) : groupByChampion && !selectedChampion ? (
              <div className="grid gap-4 [grid-template-columns:repeat(auto-fill,minmax(140px,1fr))]">
                {championGroups.map(([champion, skins]) => (
                  <ChampionTile
                    key={champion}
                    champion={champion}
                    skins={skins}
                    onOpen={() => setSelectedChampion(champion)}
                  />
                ))}
              </div>
            ) : (
              <div className="grid gap-4 [grid-template-columns:repeat(auto-fill,minmax(200px,1fr))]">
                {(selectedChampion ? drilledSkins : sortedSkins).map(
                  (skin) => (
                    <SkinCard
                      key={skin.id}
                      skin={skin}
                      onToggle={(enabled) => setEnabled(skin.id, enabled)}
                      onDelete={() => handleDeleteSkin(skin)}
                      onCustomChanged={load}
                    />
                  ),
                )}
              </div>
            )}
          </>
        )}
      </main>

      {!leaguePath.path && (
        <div className="fixed inset-0 z-[70] flex items-center justify-center bg-background/92 backdrop-blur-md">
          <div className="w-full max-w-xl rounded-2xl border border-amber-500/40 bg-card px-6 py-6 shadow-2xl">
            <LeaguePathPrompt
              path={leaguePath.path}
              isResolving={leaguePath.isResolving}
              error={leaguePath.error}
              onBrowse={handlePickLeagueFolder}
              onDetect={handleAutoDetectLeague}
              blocking
            />
          </div>
        </div>
      )}

      {leaguePath.path && (!cslolDll.exists || cslolDll.isChecking) && (
        <div className="fixed inset-0 z-[75] flex items-center justify-center bg-background px-6 py-8">
          <div className="w-full max-w-2xl rounded-2xl border bg-card px-6 py-6 shadow-2xl">
            <CslolDllPrompt
              path={cslolDll.path}
              exists={cslolDll.exists}
              isChecking={cslolDll.isChecking}
              error={cslolDll.error}
              onOpenFolder={handleOpenCslolDllFolder}
              onRefresh={refreshCslolDll}
              blocking
            />
          </div>
        </div>
      )}

      {settingsOpen && (
        <SettingsDialog
          leaguePath={leaguePath}
          cslolDll={cslolDll}
          hookState={hookState}
          onSetHookActive={setHookActive}
          onBrowseLeague={handlePickLeagueFolder}
          onDetectLeague={handleAutoDetectLeague}
          onOpenCslolDllFolder={handleOpenCslolDllFolder}
          onRefreshCslolDll={refreshCslolDll}
          onClose={() => setSettingsOpen(false)}
        />
      )}

      {isDragging && !isImporting && (
        <div className="pointer-events-none fixed inset-0 z-50 flex items-center justify-center bg-primary/10 backdrop-blur-sm">
          <div className="flex flex-col items-center gap-2 rounded-xl bg-card px-8 py-6 text-center ring-2 ring-primary/50">
            <Plus className="size-6" />
            <p className="text-sm font-medium">
              Drop .fantome files to import
            </p>
          </div>
        </div>
      )}

      {isImporting && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-background/40 backdrop-blur-md">
          <div className="flex flex-col items-center gap-3 rounded-xl bg-card px-10 py-7 ring-2 ring-border shadow-lg">
            <Loader2 className="size-8 animate-spin text-primary" />
            <p className="text-sm font-medium">Importing skin and generating art…</p>
            <p className="text-xs text-muted-foreground">
              Talon will finish when the splash, tile, and background are ready.
            </p>
          </div>
        </div>
      )}

      {deleteTarget && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-background/60 backdrop-blur-sm"
          onClick={() => setDeleteTarget(null)}
        >
          <div
            className="w-full max-w-sm rounded-xl bg-card p-6 shadow-2xl ring-1 ring-border"
            onClick={(e) => e.stopPropagation()}
          >
            <h2 className="text-base font-semibold">Delete skin?</h2>
            <p className="mt-2 text-sm text-muted-foreground">
              This permanently removes{" "}
              <span className="font-medium capitalize text-foreground">
                {deleteTarget.name}
              </span>{" "}
              and its cached preview.
            </p>
            <div className="mt-6 flex justify-end gap-2">
              <Button
                variant="outline"
                onClick={() => setDeleteTarget(null)}
              >
                Cancel
              </Button>
              <Button variant="destructive" onClick={confirmDelete}>
                <Trash2 />
                Delete
              </Button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

function SettingsDialog({
  leaguePath,
  cslolDll,
  hookState,
  onSetHookActive,
  onBrowseLeague,
  onDetectLeague,
  onOpenCslolDllFolder,
  onRefreshCslolDll,
  onClose,
}: {
  leaguePath: LeaguePathState;
  cslolDll: CslolDllState;
  hookState: HookState;
  onSetHookActive: (active: boolean) => void | Promise<void>;
  onBrowseLeague: () => void;
  onDetectLeague: () => void;
  onOpenCslolDllFolder: () => void;
  onRefreshCslolDll: () => void;
  onClose: () => void;
}) {
  return (
    <div
      className="fixed inset-0 z-[80] flex items-center justify-center bg-background/60 px-4 backdrop-blur-sm"
      onClick={onClose}
    >
      <div
        className="w-full max-w-lg rounded-xl bg-card p-5 shadow-2xl ring-1 ring-border"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="mb-5 flex items-center justify-between gap-3">
          <div>
            <h2 className="text-base font-semibold">Settings</h2>
            <p className="text-xs text-muted-foreground">
              App preferences and League installation.
            </p>
          </div>
          <Button
            size="icon-sm"
            variant="ghost"
            onClick={onClose}
            aria-label="Close settings"
            title="Close"
          >
            <X />
          </Button>
        </div>

        <div className="space-y-5">
          <section>
            <div className="mb-2">
              <h3 className="text-sm font-medium">League client hook</h3>
              <p className="text-xs text-muted-foreground">
                Enable the League client hook when you want Talon in champ select.
              </p>
            </div>
            <div className="flex items-center justify-between gap-3 rounded-lg border bg-background px-3 py-3">
              <div>
                <p className="text-sm font-medium">
                  {hookState.active ? "Hook enabled" : "Hook disabled"}
                </p>
                <p className="text-xs text-muted-foreground">
                  {hookState.active
                    ? "LeagueClientUx will relaunch through Talon."
                    : "Talon will not hook the League client on launch."}
                </p>
                {hookState.error && (
                  <p className="mt-2 text-xs text-destructive">
                    {hookState.error}
                  </p>
                )}
              </div>
              <Switch
                checked={hookState.active}
                disabled={hookState.isLoading}
                onCheckedChange={onSetHookActive}
              />
            </div>
          </section>

          <section>
            <div className="mb-2">
              <h3 className="text-sm font-medium">League game path</h3>
              <p className="text-xs text-muted-foreground">
                Select the League of Legends install folder.
              </p>
            </div>
            <div className="rounded-lg border bg-background px-3 py-3">
              <p
                className={cn(
                  "break-all text-xs",
                  leaguePath.path ? "text-foreground" : "text-muted-foreground",
                )}
              >
                {leaguePath.path ?? "No League install path selected"}
              </p>
              {leaguePath.error && (
                <p className="mt-2 text-xs text-destructive">
                  {leaguePath.error}
                </p>
              )}
              <div className="mt-3 flex flex-wrap items-center gap-2">
                <Button
                  size="sm"
                  variant="outline"
                  onClick={onDetectLeague}
                  disabled={leaguePath.isResolving}
                >
                  {leaguePath.isResolving ? (
                    <Loader2 className="animate-spin" />
                  ) : (
                    <RotateCw />
                  )}
                  Detect
                </Button>
                <Button
                  size="sm"
                  onClick={onBrowseLeague}
                  disabled={leaguePath.isResolving}
                >
                  <FolderOpen />
                  Choose folder
                </Button>
              </div>
            </div>
          </section>

          <section>
            <div className="mb-2">
              <h3 className="text-sm font-medium">CSLOL DLL</h3>
              <p className="text-xs text-muted-foreground">
                Add your own <code>runtime-hook.dll</code> file in Talon&apos;s app data folder.
              </p>
            </div>
            <div className="rounded-lg border bg-background px-3 py-3">
              <p className="break-all text-xs text-muted-foreground">
                {cslolDll.path || "Resolving DLL path..."}
              </p>
              <p className="mt-2 text-xs">
                {cslolDll.exists ? "DLL detected." : "DLL missing."}
              </p>
              {cslolDll.error && (
                <p className="mt-2 text-xs text-destructive">{cslolDll.error}</p>
              )}
              <div className="mt-3 flex flex-wrap items-center gap-2">
                <Button
                  size="sm"
                  variant="outline"
                  onClick={onRefreshCslolDll}
                  disabled={cslolDll.isChecking}
                >
                  {cslolDll.isChecking ? (
                    <Loader2 className="animate-spin" />
                  ) : (
                    <RotateCw />
                  )}
                  Check again
                </Button>
                <Button size="sm" onClick={onOpenCslolDllFolder}>
                  <FolderOpen />
                  Open folder
                </Button>
              </div>
            </div>
          </section>
        </div>
      </div>
    </div>
  );
}

function ChampionTile({
  champion,
  skins,
  onOpen,
}: {
  champion: string;
  skins: Skin[];
  onOpen: () => void;
}) {
  const enabled = skins.filter((s) => s.enabled).length;
  // Prefer the official Data Dragon champion tile (square face portrait);
  // fall back to the first skin's splash if the icon fetch failed.
  const icon =
    skins.find((s) => s.champion_icon)?.champion_icon ??
    skins.find((s) => s.preview)?.preview ??
    null;
  return (
    <button
      onClick={onOpen}
      className="group flex cursor-pointer flex-col items-center gap-2 rounded-lg p-3 transition-colors hover:bg-muted"
    >
      <div className="size-24 overflow-hidden rounded-full bg-muted ring-1 ring-border transition-transform group-hover:scale-105">
        {icon ? (
          <img
            src={convertFileSrc(icon)}
            alt=""
            draggable={false}
            className="h-full w-full object-cover"
          />
        ) : null}
      </div>
      <div className="w-full text-center">
        <div className="truncate text-sm font-medium capitalize">
          {champion}
        </div>
        <div className="text-xs text-muted-foreground">
          {skins.length} skin{skins.length !== 1 ? "s" : ""}
          {enabled > 0 && (
            <>
              {" · "}
              <span className="text-primary">{enabled} on</span>
            </>
          )}
        </div>
      </div>
    </button>
  );
}

function SkinCard({
  skin,
  onToggle,
  onDelete,
  onCustomChanged,
}: {
  skin: Skin;
  onToggle: (enabled: boolean) => void;
  onDelete: () => void;
  onCustomChanged: () => void | Promise<void>;
}) {
  const [menuOpen, setMenuOpen] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);
  const menuTriggerRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (!menuOpen) return;
    const onDocClick = (e: MouseEvent) => {
      const target = e.target as Node;
      // Clicks on the trigger button itself are handled by its own onClick
      // (it toggles open/closed) — don't double-handle them here.
      if (menuTriggerRef.current?.contains(target)) return;
      if (menuRef.current?.contains(target)) return;
      setMenuOpen(false);
    };
    document.addEventListener("mousedown", onDocClick);
    return () => document.removeEventListener("mousedown", onDocClick);
  }, [menuOpen]);

  const pickImage = async (): Promise<string | null> => {
    const selected = await openFileDialog({
      multiple: false,
      title: "Choose an image",
      filters: [
        { name: "Image", extensions: ["png", "jpg", "jpeg", "webp"] },
      ],
    });
    if (!selected) return null;
    return Array.isArray(selected) ? (selected[0] ?? null) : selected;
  };

  const runCustomAction = async (action: () => Promise<void>) => {
    setMenuOpen(false);
    try {
      await action();
      await onCustomChanged();
    } catch (e) {
      console.error("[Talon] custom asset action failed:", e);
    }
  };

  const setCustomTile = () =>
    runCustomAction(async () => {
      const source = await pickImage();
      if (!source) return;
      await invoke("set_custom_tile", { id: skin.id, source });
    });

  const setCustomBackground = () =>
    runCustomAction(async () => {
      const source = await pickImage();
      if (!source) return;
      await invoke("set_custom_background", { id: skin.id, source });
    });

  const clearCustomTile = () =>
    runCustomAction(() => invoke("clear_custom_tile", { id: skin.id }));
  const clearCustomBackground = () =>
    runCustomAction(() => invoke("clear_custom_background", { id: skin.id }));

  return (
    <Card
      className={cn(
        "overflow-hidden p-0 gap-0 transition-all",
        skin.enabled && "ring-2 ring-primary",
      )}
    >
      <div className="relative aspect-square w-full overflow-hidden bg-muted">
        {skin.preview ? (
          <img
            src={convertFileSrc(skin.preview)}
            alt=""
            draggable={false}
            className="h-full w-full object-cover object-[center_25%]"
          />
        ) : null}
        <button
          type="button"
          onClick={onDelete}
          aria-label="Delete skin"
          title="Delete skin"
          className="absolute right-2 top-2 cursor-pointer rounded-full bg-background/80 p-1.5 text-foreground opacity-0 backdrop-blur-sm transition-all hover:bg-destructive hover:text-destructive-foreground focus-visible:opacity-100 group-hover/card:opacity-100"
        >
          <Trash2 className="size-4" />
        </button>
        <button
          ref={menuTriggerRef}
          type="button"
          onClick={() => setMenuOpen((v) => !v)}
          aria-label="Customize skin assets"
          title="Customize skin assets"
          className="absolute right-2 top-11 cursor-pointer rounded-full bg-background/80 p-1.5 text-foreground opacity-0 backdrop-blur-sm transition-all hover:bg-accent hover:text-accent-foreground focus-visible:opacity-100 group-hover/card:opacity-100"
        >
          <MoreHorizontal className="size-4" />
        </button>
        {menuOpen && (
          <div
            ref={menuRef}
            onClick={(e) => e.stopPropagation()}
            className="absolute inset-x-2 bottom-2 flex flex-col gap-0.5 rounded-lg bg-background/95 p-1 ring-1 ring-border backdrop-blur-sm"
          >
            <CustomMenuItem onClick={setCustomTile}>
              Set tile…
            </CustomMenuItem>
            <CustomMenuItem onClick={setCustomBackground}>
              Set background…
            </CustomMenuItem>
            {skin.tile_preview_custom && (
              <CustomMenuItem onClick={clearCustomTile}>
                Reset tile
              </CustomMenuItem>
            )}
            {skin.background_preview_custom && (
              <CustomMenuItem onClick={clearCustomBackground}>
                Reset background
              </CustomMenuItem>
            )}
          </div>
        )}
      </div>
      <div className="flex items-center gap-2 p-3">
        <div className="min-w-0 flex-1">
          <div className="truncate font-medium capitalize">{skin.name}</div>
          <div className="truncate text-xs text-muted-foreground">
            <span className="capitalize">{skin.champion}</span>
            {skin.author && (
              <>
                {" · by "}
                <span className="capitalize">{skin.author}</span>
              </>
            )}
          </div>
        </div>
        <Switch checked={skin.enabled} onCheckedChange={onToggle} />
      </div>
    </Card>
  );
}

function CustomMenuItem({
  children,
  onClick,
}: {
  children: React.ReactNode;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="w-full cursor-pointer rounded px-3 py-1.5 text-left text-xs font-medium text-foreground transition-colors hover:bg-accent hover:text-accent-foreground"
    >
      {children}
    </button>
  );
}

function LeaguePathPrompt({
  path,
  isResolving,
  error,
  onBrowse,
  onDetect,
  blocking = false,
}: {
  path: string | null;
  isResolving: boolean;
  error: string | null;
  onBrowse: () => void;
  onDetect: () => void;
  blocking?: boolean;
}) {
  if (path) {
    return (
      <div className="mb-4 rounded-lg border bg-card px-4 py-3">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="min-w-0">
            <p className="text-xs font-medium">League install path</p>
            <p className="truncate text-xs text-muted-foreground">{path}</p>
          </div>
          <div className="flex items-center gap-2">
            <Button size="xs" variant="outline" onClick={onDetect} disabled={isResolving}>
              {isResolving ? <Loader2 className="animate-spin" /> : <RotateCw />}
              Detect
            </Button>
            <Button size="xs" variant="outline" onClick={onBrowse} disabled={isResolving}>
              <FolderOpen />
              Change
            </Button>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div
      className={cn(
        "rounded-lg border border-amber-500/40 bg-amber-500/10 px-4 py-3",
        !blocking && "mb-4",
      )}
    >
      <div className="flex flex-wrap items-center justify-between gap-4">
        <div className="min-w-0">
          <p className="text-base font-semibold">League install path required</p>
          <p className="mt-1 text-sm text-muted-foreground">
            Talon needs your League of Legends install directory before the app can be used. Auto-detect works when the client is running, or you can choose the folder manually.
          </p>
          {error && <p className="mt-1 text-xs text-destructive">{error}</p>}
        </div>
        <div className="flex items-center gap-2">
          <Button size={blocking ? "sm" : "xs"} variant="outline" onClick={onDetect} disabled={isResolving}>
            {isResolving ? <Loader2 className="animate-spin" /> : <RotateCw />}
            Detect
          </Button>
          <Button size={blocking ? "sm" : "xs"} onClick={onBrowse} disabled={isResolving}>
            <FolderOpen />
            Choose folder
          </Button>
        </div>
      </div>
    </div>
  );
}

function EmptyState({ onImport }: { onImport: () => void }) {
  return (
    <Card>
      <CardContent className="flex flex-col items-center gap-3 py-12 text-center">
        <p className="text-sm font-medium">No skins yet</p>
        <p className="text-xs text-muted-foreground">
          Import a <code>.fantome</code> file to add it to your library.
        </p>
        <Button onClick={onImport}>
          <Plus />
          Import skin
        </Button>
      </CardContent>
    </Card>
  );
}

function CslolDllPrompt({
  path,
  exists,
  isChecking,
  error,
  onOpenFolder,
  onRefresh,
  blocking = false,
}: {
  path: string;
  exists: boolean;
  isChecking: boolean;
  error: string | null;
  onOpenFolder: () => void;
  onRefresh: () => void;
  blocking?: boolean;
}) {
  return (
    <div
      className={cn(
        "rounded-lg border border-amber-500/40 bg-amber-500/10 px-4 py-4",
        !blocking && "mb-4",
      )}
    >
      <div className="space-y-4">
        <div>
          <p className="text-base font-semibold">runtime-hook.dll required</p>
          <p className="mt-1 text-sm text-muted-foreground">
            Talon cannot continue until you place your own <code>runtime-hook.dll</code> file in the folder below.
          </p>
        </div>
        <div className="rounded-lg border bg-background px-3 py-3">
          <p className="break-all text-xs text-muted-foreground">
            {path || "Resolving DLL path..."}
          </p>
          <p className="mt-2 text-xs">
            {isChecking
              ? "Checking folder..."
              : exists
                ? "DLL detected."
                : "DLL not found yet."}
          </p>
          {error && <p className="mt-2 text-xs text-destructive">{error}</p>}
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <Button size={blocking ? "sm" : "xs"} onClick={onOpenFolder}>
            <FolderOpen />
            Open folder
          </Button>
          <Button
            size={blocking ? "sm" : "xs"}
            variant="outline"
            onClick={onRefresh}
            disabled={isChecking}
          >
            {isChecking ? <Loader2 className="animate-spin" /> : <RotateCw />}
            Check again
          </Button>
        </div>
      </div>
    </div>
  );
}

export default App;
