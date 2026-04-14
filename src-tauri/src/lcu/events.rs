use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use tauri::{AppHandle, Emitter};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async_tls_with_config, Connector};

use super::discovery::LcuInfo;
use super::http::LcuClient;

/// Opens the LCU WebSocket, subscribes to gameflow-phase and champ-select
/// session, and runs the event loop until the socket dies. Any error returns
/// out to the poller's outer reconnect loop.
pub async fn run(info: &LcuInfo, client: &LcuClient, app: &AppHandle) -> Result<()> {
    let alias_map = fetch_alias_map(client)
        .await
        .context("fetching champion-summary")?;
    eprintln!("[LCU] Loaded {} champion aliases", alias_map.len());

    let url = format!("wss://127.0.0.1:{}/", info.port);
    let mut request = url
        .into_client_request()
        .context("building WS request")?;
    request
        .headers_mut()
        .insert("Authorization", info.auth_header.parse()?);
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", "wamp".parse()?);

    let tls = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .context("building TLS connector")?;
    let connector = Connector::NativeTls(tls);

    let (mut ws, _resp) =
        connect_async_tls_with_config(request, None, false, Some(connector))
            .await
            .context("connecting WebSocket")?;
    eprintln!("[LCU] WebSocket connected");

    // WAMP v1 SUBSCRIBE: [5, "topic"]. OnJsonApiEvent_<uri> is the LCU's
    // filtered firehose — we only receive Create/Update/Delete events for
    // the exact URI we asked for. The carousel "currently hovered skin"
    // is not tracked here (see the bridge module for that); we only need
    // phase changes and champ-select session for champion detection.
    ws.send(Message::Text(
        r#"[5,"OnJsonApiEvent_lol-gameflow_v1_gameflow-phase"]"#.into(),
    ))
    .await?;
    ws.send(Message::Text(
        r#"[5,"OnJsonApiEvent_lol-champ-select_v1_session"]"#.into(),
    ))
    .await?;

    let mut last_phase: Option<String> = None;
    let mut last_alias: Option<String> = None;

    while let Some(msg) = ws.next().await {
        let msg = msg.context("reading WS frame")?;
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => return Err(anyhow!("WebSocket closed by remote")),
            _ => continue,
        };

        let Ok(parsed) = serde_json::from_str::<Value>(text.as_str()) else {
            continue;
        };
        let Some(arr) = parsed.as_array() else { continue };
        // WAMP v1 EVENT: [8, topic, payload]
        if arr.len() != 3 || arr[0].as_i64() != Some(8) {
            continue;
        }
        let topic = arr[1].as_str().unwrap_or("");
        let payload = &arr[2];
        let data = payload.get("data").unwrap_or(&Value::Null);

        if topic.contains("lol-gameflow_v1_gameflow-phase") {
            if let Some(phase) = data.as_str() {
                if last_phase.as_deref() != Some(phase) {
                    eprintln!("[Phase] {}", phase);
                    let _ = app.emit("lcu:phase-changed", phase);
                    last_phase = Some(phase.to_string());
                }
            }
        } else if topic.contains("lol-champ-select_v1_session") {
            let alias = extract_local_champion(data, &alias_map);
            if alias != last_alias {
                match &alias {
                    Some(a) => {
                        eprintln!("[Champion] {}", a);
                        let _ = app.emit("lcu:champion-selected", a);
                    }
                    None => {
                        eprintln!("[Champion] cleared");
                        let _ = app.emit("lcu:champion-selected", Value::Null);
                    }
                }
                last_alias = alias;
            }
        }
    }

    Err(anyhow!("WebSocket stream ended"))
}

async fn fetch_alias_map(client: &LcuClient) -> Result<HashMap<i64, String>> {
    let body = client
        .get("/lol-game-data/assets/v1/champion-summary.json")
        .await?;
    let arr: Vec<Value> = serde_json::from_str(&body)?;
    let mut map = HashMap::new();
    for entry in arr {
        let id = entry.get("id").and_then(|v| v.as_i64());
        let alias = entry.get("alias").and_then(|v| v.as_str());
        if let (Some(id), Some(alias)) = (id, alias) {
            if id > 0 {
                map.insert(id, alias.to_string());
            }
        }
    }
    Ok(map)
}

/// Returns the alias (e.g. "Hecarim") of the champion the local player currently
/// has on their cell, or `None` if they haven't picked yet.
fn extract_local_champion(
    data: &Value,
    alias_map: &HashMap<i64, String>,
) -> Option<String> {
    let local_cell = data.get("localPlayerCellId")?.as_i64()?;
    let my_team = data.get("myTeam")?.as_array()?;
    let me = my_team
        .iter()
        .find(|p| p.get("cellId").and_then(|v| v.as_i64()) == Some(local_cell))?;
    let champion_id = me.get("championId")?.as_i64()?;
    if champion_id == 0 {
        return None;
    }
    alias_map.get(&champion_id).cloned()
}
