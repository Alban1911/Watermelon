import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ArrowLeft, FolderOpen, Group, Plus, RotateCw } from "lucide-react";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { open as openFileDialog } from "@tauri-apps/plugin-dialog";

import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";

type Skin = {
  id: string;
  name: string;
  champion: string;
  author: string | null;
  version: string | null;
  description: string | null;
  preview: string | null;
  champion_icon: string | null;
  enabled: boolean;
};

type SkinLibrary = {
  dir: string;
  skins: Skin[];
};

const GROUP_STORAGE_KEY = "talon:groupByChampion";

function App() {
  const [library, setLibrary] = useState<SkinLibrary | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [isDragging, setIsDragging] = useState(false);
  const [groupByChampion, setGroupByChampion] = useState(
    () => localStorage.getItem(GROUP_STORAGE_KEY) === "1",
  );
  const [selectedChampion, setSelectedChampion] = useState<string | null>(null);

  useEffect(() => {
    localStorage.setItem(GROUP_STORAGE_KEY, groupByChampion ? "1" : "0");
  }, [groupByChampion]);

  // Drop any drill-down when the user toggles grouping — keeps the mental
  // model simple: each mode opens at its top level.
  useEffect(() => {
    setSelectedChampion(null);
  }, [groupByChampion]);

  const skinsByChampion = useMemo(() => {
    const map = new Map<string, Skin[]>();
    if (!library) return map;
    for (const skin of library.skins) {
      const bucket = map.get(skin.champion);
      if (bucket) {
        bucket.push(skin);
      } else {
        map.set(skin.champion, [skin]);
      }
    }
    return map;
  }, [library]);

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
      setLibrary(result);
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  // Re-scan whenever the window regains focus — the user has likely
  // just dropped a file into the skins folder via Explorer.
  useEffect(() => {
    const onFocus = () => load();
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
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

  const importFiles = useCallback(
    async (files: File[]) => {
      const fantomes = files.filter((f) =>
        f.name.toLowerCase().endsWith(".fantome"),
      );
      if (fantomes.length === 0) {
        setError("Only .fantome files can be imported.");
        return;
      }
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
      for (const path of paths) {
        await invoke("import_skin", { source: path });
      }
      await load();
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
                {(selectedChampion ? drilledSkins : library.skins).map(
                  (skin) => (
                    <SkinCard
                      key={skin.id}
                      skin={skin}
                      onToggle={(enabled) => setEnabled(skin.id, enabled)}
                    />
                  ),
                )}
              </div>
            )}
          </>
        )}
      </main>

      {isDragging && (
        <div className="pointer-events-none fixed inset-0 z-50 flex items-center justify-center bg-primary/10 backdrop-blur-sm">
          <div className="flex flex-col items-center gap-2 rounded-xl bg-card px-8 py-6 text-center ring-2 ring-primary/50">
            <Plus className="size-6" />
            <p className="text-sm font-medium">
              Drop .fantome files to import
            </p>
          </div>
        </div>
      )}
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
      className="group flex flex-col items-center gap-2 rounded-lg p-3 transition-colors hover:bg-muted"
    >
      <div className="size-24 overflow-hidden rounded-full bg-muted ring-1 ring-border transition-transform group-hover:scale-105">
        {icon ? (
          <img
            src={convertFileSrc(icon)}
            alt=""
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
}: {
  skin: Skin;
  onToggle: (enabled: boolean) => void;
}) {
  return (
    <Card className="overflow-hidden p-0 gap-0">
      <div className="aspect-square w-full overflow-hidden bg-muted">
        {skin.preview ? (
          <img
            src={convertFileSrc(skin.preview)}
            alt=""
            className="h-full w-full object-cover object-[center_25%]"
          />
        ) : null}
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

export default App;
