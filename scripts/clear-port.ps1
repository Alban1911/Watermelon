# Kills any process holding TCP port 1420 (Vite dev server). Run by the
# `predev` npm hook before every `pnpm dev` so a leftover Vite from a
# previous tauri dev session doesn't block startup.
Get-NetTCPConnection -LocalPort 1420 -ErrorAction SilentlyContinue |
    ForEach-Object {
        Stop-Process -Id $_.OwningProcess -Force -ErrorAction SilentlyContinue
    }
