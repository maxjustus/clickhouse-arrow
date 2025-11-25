use std::collections::HashMap;
use std::sync::Arc;

use clickhouse_arrow::{Client, NativeFormat, Qid, Settings};
use futures::StreamExt;
use tokio::sync::{RwLock, mpsc};

use crate::tui::app::{AppEvent, QueryCommand};

type QidMap = Arc<RwLock<HashMap<Qid, usize>>>;

pub fn spawn_backend(
    client: Client<NativeFormat>,
    mut cmd_rx: mpsc::Receiver<QueryCommand>,
    event_tx: mpsc::Sender<AppEvent>,
) -> tokio::task::JoinHandle<()> {
    // Shared mapping from Qid -> query_id
    let qid_map: QidMap = Arc::new(RwLock::new(HashMap::new()));

    // Subscribe to client events (profile, logs, progress)
    let mut events_rx = client.subscribe_events();
    let event_tx_clone = event_tx.clone();
    let qid_map_clone = qid_map.clone();

    // Spawn task to forward client events to TUI
    tokio::spawn(async move {
        while let Ok(event) = events_rx.recv().await {
            use clickhouse_arrow::ClickHouseEvent;

            // Look up query_id from Qid
            let query_id = {
                let map = qid_map_clone.read().await;
                match map.get(&event.qid) {
                    Some(&id) => id,
                    None => continue, // Unknown query, skip
                }
            };

            match event.event {
                ClickHouseEvent::Profile(events) => {
                    for profile_event in events {
                        let json = serde_json::json!({
                            "name": profile_event.name,
                            "value": profile_event.value,
                            "thread_id": profile_event.thread_id,
                            "current_time": profile_event.current_time,
                        });
                        let _ = event_tx_clone
                            .send(AppEvent::ProfileEvent { query_id, event: json })
                            .await;
                    }
                }
                ClickHouseEvent::Log(logs) => {
                    for log in logs {
                        // Extract timestamp from ClickHouse format like
                        // "parseDateTimeBestEffort('2025-11-25T00:32:19Z')"
                        let time = if let Some(start) = log.time.find('\'') {
                            if let Some(end) = log.time[start + 1..].find('\'') {
                                &log.time[start + 1..start + 1 + end]
                            } else {
                                &log.time
                            }
                        } else {
                            &log.time
                        };

                        let json = serde_json::json!({
                            "time": time,
                            "thread_id": log.thread_id,
                            "source": log.source,
                            "text": log.text,
                        });
                        let _ =
                            event_tx_clone.send(AppEvent::LogEvent { query_id, log: json }).await;
                    }
                }
                ClickHouseEvent::Progress(progress) => {
                    let json = serde_json::json!({
                        "read_rows": progress.read_rows,
                        "read_bytes": progress.read_bytes,
                        "total_rows_to_read": progress.total_rows_to_read,
                        "written_rows": progress.written_rows,
                        "written_bytes": progress.written_bytes,
                        "elapsed_ns": progress.elapsed_ns,
                    });
                    let _ = event_tx_clone
                        .send(AppEvent::ProgressEvent { query_id, progress: json })
                        .await;
                }
                ClickHouseEvent::ProfileInfo(_) => {
                    // ProfileInfo is end-of-query summary, can add if needed
                }
            }
        }
    });

    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                QueryCommand::Execute { query_id, sql } => {
                    let client = client.clone();
                    let event_tx = event_tx.clone();
                    let qid_map = qid_map.clone();
                    tokio::spawn(async move {
                        execute_query(&client, query_id, &sql, &event_tx, &qid_map).await;
                    });
                }
                QueryCommand::Cancel { query_id: _ } => {
                    // TODO: implement cancellation
                }
            }
        }
    })
}

async fn execute_query(
    client: &Client<NativeFormat>,
    query_id: usize,
    sql: &str,
    event_tx: &mpsc::Sender<AppEvent>,
    qid_map: &QidMap,
) {
    let qid = Qid::new();

    // Register the Qid -> query_id mapping
    {
        let mut map = qid_map.write().await;
        map.insert(qid, query_id);
    }

    // Send query started event
    let _ = event_tx.send(AppEvent::QueryStarted { query_id }).await;

    // Execute query with settings for logs and profile events
    let settings = Settings::default()
        .with_setting("send_logs_level", "trace")
        .with_setting("log_queries", 1);

    let stream = match client
        .query_raw_with_settings::<clickhouse_arrow::QueryParams, Settings>(
            sql.to_string(),
            None,
            Some(settings),
            qid,
        )
        .await
    {
        Ok(s) => s,
        Err(e) => {
            let _ = event_tx.send(AppEvent::QueryError { query_id, error: e.to_string() }).await;
            // Clean up mapping
            let mut map = qid_map.write().await;
            map.remove(&qid);
            return;
        }
    };

    futures::pin_mut!(stream);

    while let Some(result) = stream.next().await {
        match result {
            Ok(mut block) => {
                for row in block.take_iter_rows() {
                    let json_row = row_to_json(row);
                    let _ = event_tx.send(AppEvent::RowReceived { query_id, row: json_row }).await;
                }
            }
            Err(e) => {
                let _ =
                    event_tx.send(AppEvent::QueryError { query_id, error: e.to_string() }).await;
                // Clean up mapping
                let mut map = qid_map.write().await;
                map.remove(&qid);
                return;
            }
        }
    }

    let _ = event_tx.send(AppEvent::QueryComplete { query_id }).await;

    // Clean up mapping after query completes
    let mut map = qid_map.write().await;
    map.remove(&qid);
}

fn row_to_json(
    row: Vec<(&str, &clickhouse_arrow::native::types::Type, clickhouse_arrow::Value)>,
) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (name, _ty, value) in row {
        map.insert(name.to_string(), value_to_json(value));
    }
    serde_json::Value::Object(map)
}

fn value_to_json(value: clickhouse_arrow::Value) -> serde_json::Value {
    use clickhouse_arrow::Value;
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Int8(n) => serde_json::json!(n),
        Value::Int16(n) => serde_json::json!(n),
        Value::Int32(n) => serde_json::json!(n),
        Value::Int64(n) => serde_json::json!(n),
        Value::Int128(n) => serde_json::json!(n.to_string()),
        Value::Int256(n) => serde_json::json!(n.to_string()),
        Value::UInt8(n) => serde_json::json!(n),
        Value::UInt16(n) => serde_json::json!(n),
        Value::UInt32(n) => serde_json::json!(n),
        Value::UInt64(n) => serde_json::json!(n),
        Value::UInt128(n) => serde_json::json!(n.to_string()),
        Value::UInt256(n) => serde_json::json!(n.to_string()),
        Value::Float32(n) => serde_json::json!(n),
        Value::Float64(n) => serde_json::json!(n),
        Value::String(bytes) => {
            serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned())
        }
        Value::Uuid(u) => serde_json::json!(u.to_string()),
        Value::Date(d) => serde_json::json!(format!("{:?}", d)),
        Value::Date32(d) => serde_json::json!(format!("{:?}", d)),
        Value::DateTime(dt) => serde_json::json!(format!("{:?}", dt)),
        Value::DateTime64(dt) => serde_json::json!(format!("{:?}", dt)),
        Value::Decimal32(_, n) => serde_json::json!(n),
        Value::Decimal64(_, n) => serde_json::json!(n),
        Value::Decimal128(_, n) => serde_json::json!(n.to_string()),
        Value::Decimal256(_, n) => serde_json::json!(n.to_string()),
        Value::Enum8(name, _) | Value::Enum16(name, _) => serde_json::json!(name),
        Value::Array(arr) => serde_json::Value::Array(arr.into_iter().map(value_to_json).collect()),
        Value::Tuple(vals) => {
            serde_json::Value::Array(vals.into_iter().map(value_to_json).collect())
        }
        Value::Map(keys, vals) => {
            let pairs: Vec<_> = keys
                .into_iter()
                .zip(vals)
                .map(|(k, v)| serde_json::json!([value_to_json(k), value_to_json(v)]))
                .collect();
            serde_json::Value::Array(pairs)
        }
        Value::Ipv4(ip) => serde_json::json!(format!("{:?}", ip)),
        Value::Ipv6(ip) => serde_json::json!(format!("{:?}", ip)),
        Value::Variant(_, boxed) | Value::Dynamic(_, boxed) => value_to_json(*boxed),
        // For geo and other complex types, fall back to debug string
        other => serde_json::Value::String(format!("{:?}", other)),
    }
}
