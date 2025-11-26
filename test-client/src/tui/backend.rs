use std::collections::HashMap;
use std::sync::Arc;

use chrono::NaiveDate;
use clickhouse_arrow::{ClickHouseEvent, Client, Event, NativeFormat, Qid, Settings, Tz};
use futures::StreamExt;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};

use crate::tui::app::{AppEvent, QueryCommand};
use crate::tui::query_store::{QueryCacheWriter, QueryStore};

type QidMap = Arc<RwLock<HashMap<Qid, usize>>>;
type TaskMap = Arc<RwLock<HashMap<usize, tokio::task::JoinHandle<()>>>>;
type CacheWriterMap = Arc<RwLock<HashMap<usize, Arc<Mutex<Option<QueryCacheWriter>>>>>>;

pub fn spawn_backend(
    client: Client<NativeFormat>,
    mut cmd_rx: mpsc::Receiver<QueryCommand>,
    event_tx: mpsc::Sender<AppEvent>,
) -> tokio::task::JoinHandle<()> {
    // Shared mapping from Qid -> query_id
    let qid_map: QidMap = Arc::new(RwLock::new(HashMap::new()));
    // Shared mapping from query_id -> task handle for cancellation
    let task_map: TaskMap = Arc::new(RwLock::new(HashMap::new()));
    // Shared mapping from query_id -> cache writer
    let cache_writers: CacheWriterMap = Arc::new(RwLock::new(HashMap::new()));
    // Shared QueryStore for persistence
    let query_store: Arc<Mutex<Option<QueryStore>>> = Arc::new(Mutex::new(None));

    // Initialize query store asynchronously
    let store_init = query_store.clone();
    tokio::spawn(async move {
        match QueryStore::load().await {
            Ok(store) => {
                *store_init.lock().await = Some(store);
            }
            Err(e) => {
                tracing::warn!("Failed to load query store: {}", e);
            }
        }
    });

    // Two-stage event pipeline to prevent event loss from broadcast lagging:
    // Stage 1: Fast drainer reads from broadcast into unbounded buffer
    // Stage 2: Process buffered events at TUI pace

    let mut events_rx = client.subscribe_events();
    let (buffer_tx, mut buffer_rx) = mpsc::unbounded_channel::<Event>();

    // Stage 1: Fast drainer - reads broadcast as quickly as possible
    tokio::spawn(async move {
        loop {
            match events_rx.recv().await {
                Ok(event) => {
                    // Unbounded send never blocks
                    let _ = buffer_tx.send(event);
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("Broadcast lagged {} events (before buffer)", n);
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::info!("Event broadcast closed");
                    break;
                }
            }
        }
    });

    // Stage 2: Process buffered events and forward to TUI
    let event_tx_clone = event_tx.clone();
    let qid_map_clone = qid_map.clone();
    let cache_writers_clone = cache_writers.clone();
    tokio::spawn(async move {
        while let Some(event) = buffer_rx.recv().await {
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

                        // Write to cache
                        if let Some(writer_arc) =
                            cache_writers_clone.read().await.get(&query_id).cloned()
                        {
                            if let Some(writer) = writer_arc.lock().await.as_mut() {
                                writer.write_profile_event(&json);
                            }
                        }

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

                        // Write to cache
                        if let Some(writer_arc) =
                            cache_writers_clone.read().await.get(&query_id).cloned()
                        {
                            if let Some(writer) = writer_arc.lock().await.as_mut() {
                                writer.write_log(&json);
                            }
                        }

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
                    let task_map_clone = task_map.clone();
                    let cache_writers = cache_writers.clone();
                    let query_store = query_store.clone();
                    let handle = tokio::spawn(async move {
                        execute_query(
                            &client,
                            query_id,
                            &sql,
                            &event_tx,
                            &qid_map,
                            &task_map_clone,
                            &cache_writers,
                            &query_store,
                        )
                        .await;
                    });
                    // Store handle for cancellation
                    task_map.write().await.insert(query_id, handle);
                }
                QueryCommand::Cancel { query_id } => {
                    // Remove cache writer on cancel
                    cache_writers.write().await.remove(&query_id);
                    if let Some(handle) = task_map.write().await.remove(&query_id) {
                        handle.abort();
                        // Send completion event so UI updates
                        let _ = event_tx.send(AppEvent::QueryComplete { query_id }).await;
                    }
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
    task_map: &TaskMap,
    cache_writers: &CacheWriterMap,
    query_store: &Arc<Mutex<Option<QueryStore>>>,
) {
    let qid = Qid::new();
    let start_time = std::time::Instant::now();

    // Register the Qid -> query_id mapping
    {
        let mut map = qid_map.write().await;
        map.insert(qid, query_id);
    }

    // Create cache writer if store is available
    let cache_writer: Option<Arc<Mutex<Option<QueryCacheWriter>>>> =
        if let Some(store) = query_store.lock().await.as_ref() {
            match store.start_query(sql).await {
                Ok(writer) => {
                    let writer_arc = Arc::new(Mutex::new(Some(writer)));
                    cache_writers.write().await.insert(query_id, writer_arc.clone());
                    Some(writer_arc)
                }
                Err(e) => {
                    tracing::warn!("Failed to create cache writer: {}", e);
                    None
                }
            }
        } else {
            None
        };

    // Send query started event
    let _ = event_tx.send(AppEvent::QueryStarted { query_id }).await;

    // Execute query with settings for logs, profile events, and progress
    let settings = Settings::default()
        .with_setting("send_logs_level", "trace")
        .with_setting("log_queries", 1)
        .with_setting("send_profile_events", 1);

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
            let error_msg = e.to_string();
            let _ =
                event_tx.send(AppEvent::QueryError { query_id, error: error_msg.clone() }).await;

            // Finish cache with error
            finish_cache(
                cache_writer,
                cache_writers,
                query_store,
                query_id,
                start_time,
                Some(error_msg),
                event_tx,
            )
            .await;

            // Clean up mappings
            qid_map.write().await.remove(&qid);
            task_map.write().await.remove(&query_id);
            return;
        }
    };

    futures::pin_mut!(stream);

    while let Some(result) = stream.next().await {
        match result {
            Ok(mut block) => {
                // Write block to cache before converting to rows
                if let Some(ref writer_arc) = cache_writer {
                    if let Some(writer) = writer_arc.lock().await.as_mut() {
                        // Clone block for cache (block will be consumed by take_iter_rows)
                        let cache_block = block.clone();
                        if let Err(e) = writer.write_block(cache_block).await {
                            tracing::warn!("Failed to write block to cache: {}", e);
                        }
                    }
                }

                for row in block.take_iter_rows() {
                    let json_row = row_to_json(row);
                    let _ = event_tx.send(AppEvent::RowReceived { query_id, row: json_row }).await;
                }
            }
            Err(e) => {
                let error_msg = e.to_string();
                let _ = event_tx
                    .send(AppEvent::QueryError { query_id, error: error_msg.clone() })
                    .await;

                // Finish cache with error
                finish_cache(
                    cache_writer,
                    cache_writers,
                    query_store,
                    query_id,
                    start_time,
                    Some(error_msg),
                    event_tx,
                )
                .await;

                // Clean up mappings
                qid_map.write().await.remove(&qid);
                task_map.write().await.remove(&query_id);
                return;
            }
        }
    }

    let _ = event_tx.send(AppEvent::QueryComplete { query_id }).await;

    // Finish cache successfully
    finish_cache(cache_writer, cache_writers, query_store, query_id, start_time, None, event_tx)
        .await;

    // Clean up mappings after query completes
    qid_map.write().await.remove(&qid);
    task_map.write().await.remove(&query_id);
}

async fn finish_cache(
    cache_writer: Option<Arc<Mutex<Option<QueryCacheWriter>>>>,
    cache_writers: &CacheWriterMap,
    query_store: &Arc<Mutex<Option<QueryStore>>>,
    query_id: usize,
    start_time: std::time::Instant,
    error: Option<String>,
    event_tx: &mpsc::Sender<AppEvent>,
) {
    // Remove from shared map
    cache_writers.write().await.remove(&query_id);

    // Take ownership of the writer and finish it
    if let Some(writer_arc) = cache_writer {
        if let Some(writer) = writer_arc.lock().await.take() {
            let duration_ms = Some(start_time.elapsed().as_millis() as u64);
            match writer.finish(duration_ms, error).await {
                Ok(entry) => {
                    // Save entry to store
                    if let Some(store) = query_store.lock().await.as_mut() {
                        if let Err(e) = store.finish_query(entry.clone()).await {
                            tracing::warn!("Failed to save query to store: {}", e);
                        } else {
                            // Notify app of cached query
                            let _ = event_tx.send(AppEvent::QueryCached { query_id, entry }).await;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to finish cache: {}", e);
                }
            }
        }
    }
}

pub(crate) fn row_to_json(
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
        Value::Date(d) => {
            let date: NaiveDate = d.into();
            serde_json::json!(date.format("%Y-%m-%d").to_string())
        }
        Value::Date32(d) => {
            let date: NaiveDate = d.into();
            serde_json::json!(date.format("%Y-%m-%d").to_string())
        }
        Value::DateTime(dt) => match TryInto::<chrono::DateTime<Tz>>::try_into(dt) {
            Ok(chrono_dt) => {
                serde_json::json!(chrono_dt.format("%Y-%m-%d %H:%M:%S %Z").to_string())
            }
            Err(_) => serde_json::json!(format!("{:?}", dt)),
        },
        Value::DateTime64(dt) => match TryInto::<chrono::DateTime<Tz>>::try_into(dt) {
            Ok(chrono_dt) => {
                let fmt = match dt.2 {
                    3 => "%Y-%m-%d %H:%M:%S%.3f %Z",
                    6 => "%Y-%m-%d %H:%M:%S%.6f %Z",
                    9 => "%Y-%m-%d %H:%M:%S%.9f %Z",
                    _ => "%Y-%m-%d %H:%M:%S %Z",
                };
                serde_json::json!(chrono_dt.format(fmt).to_string())
            }
            Err(_) => serde_json::json!(format!("{:?}", dt)),
        },
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
