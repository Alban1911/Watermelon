use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde_json::Value;
use std::net::SocketAddr;
use tauri::{AppHandle, Emitter};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

const BRIDGE_ADDR: &str = "127.0.0.1:51234";

/// Runs a localhost WebSocket server that the in-client preload plugin
/// connects to. The plugin DOM-scrapes the skin carousel (the LCU has no
/// real event for "currently hovered skin") and sends the resolved skin
/// id here. We forward it to the frontend via the same `lcu:skin-hovered`
/// Tauri event the rest of the app already listens on.
///
/// Best-effort: if the port is already in use we log and exit this task
/// — Talon keeps running, hover detection is simply disabled for the
/// session. This mirrors how `events::run` returning is non-fatal.
pub async fn run(app: AppHandle) {
    let listener = match TcpListener::bind(BRIDGE_ADDR).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "[Bridge] failed to bind {} ({}) — skin hover detection disabled",
                BRIDGE_ADDR, e
            );
            return;
        }
    };
    eprintln!("[Bridge] listening on {}", BRIDGE_ADDR);

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("[Bridge] accept error: {}", e);
                continue;
            }
        };
        let handle = app.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = serve_client(stream, peer, handle).await {
                eprintln!("[Bridge] client {} ended: {}", peer, e);
            }
        });
    }
}

async fn serve_client(stream: TcpStream, peer: SocketAddr, app: AppHandle) -> Result<()> {
    let ws = accept_async(stream).await.context("WebSocket handshake")?;
    eprintln!("[Bridge] plugin connected: {}", peer);

    let (_write, mut read) = ws.split();
    while let Some(msg) = read.next().await {
        let msg = msg.context("reading frame")?;
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };

        let Ok(parsed) = serde_json::from_str::<Value>(text.as_str()) else {
            continue;
        };
        let Some(kind) = parsed.get("type").and_then(|v| v.as_str()) else {
            continue;
        };

        match kind {
            "skin-hovered" => match parsed.get("skinId") {
                Some(v) if v.is_i64() => {
                    let id = v.as_i64().unwrap();
                    eprintln!("[Bridge] skin hovered {}", id);
                    let _ = app.emit("lcu:skin-hovered", id);
                }
                Some(v) if v.is_null() => {
                    eprintln!("[Bridge] skin hover cleared");
                    let _ = app.emit("lcu:skin-hovered", Value::Null);
                }
                _ => {}
            },
            _ => {}
        }
    }

    eprintln!("[Bridge] plugin disconnected: {}", peer);
    Ok(())
}
