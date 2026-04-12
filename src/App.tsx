import { useCallback, useEffect, useRef, useState } from "react";
import { FolderOpen, Plus, RotateCw } from "lucide-react";
import { invoke } from "@tauri-apps/api/core";
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
  enabled: boolean;
};

type SkinLibrary = {
  dir: string;
  skins: Skin[];
};

function App() {
  const [library, setLibrary] = useState<SkinLibrary | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [isDragging, setIsDragging] = useState(false);

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

  return (
    <div className="min-h-screen bg-background text-foreground">
      <header className="border-b">
        <div className="mx-auto flex max-w-3xl items-center justify-between px-6 py-5">
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

      <main className="mx-auto max-w-3xl px-6 py-8">
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
              <h2 className="text-sm font-medium">Skin library</h2>
              <div className="flex items-center gap-2">
                <p className="text-xs text-muted-foreground">
                  {library.skins.filter((s) => s.enabled).length} of{" "}
                  {library.skins.length} enabled
                </p>
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
            ) : (
              <div className="space-y-2">
                {library.skins.map((skin) => (
                  <SkinRow
                    key={skin.id}
                    skin={skin}
                    onToggle={(enabled) => setEnabled(skin.id, enabled)}
                  />
                ))}
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

function SkinRow({
  skin,
  onToggle,
}: {
  skin: Skin;
  onToggle: (enabled: boolean) => void;
}) {
  return (
    <Card>
      <CardContent className="flex items-center justify-between gap-4">
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
      </CardContent>
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
