mod client;

use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use clickhouse_arrow::file_stream::FileStreamWriter;
use clickhouse_arrow::native::types::Type;
use clickhouse_arrow::native::values::Value as ChValue;
use clickhouse_arrow::native::values::serde_impls::RowSer;
use clickhouse_arrow::{
    ArrowOptions, Client, CompressionMethod, NativeFormat, Qid, QueryParams, SettingValue, Settings,
};
use client::ClickHouseClient;
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt as _, BufReader as AsyncBufReader, BufWriter as AsyncBufWriter,
};
use tokio::sync::{Mutex, mpsc};

struct DataPayload {
    cols:       Arc<Vec<(String, Type)>>,
    row:        Vec<ChValue>,
    request_id: Option<Arc<String>>,
}

#[derive(Serialize)]
struct DataEvent<'a> {
    #[serde(rename = "type")]
    output_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id:  Option<&'a str>,
    #[serde(rename = "data")]
    data:        RowSer<'a>,
}

enum WriterCmd {
    Data(DataPayload),
    Json(JsonOutput),
}

#[derive(Parser, Debug)]
#[command(name = "clickhouse-test-client")]
#[command(about = "A test client for the clickhouse-arrow native format implementation")]
#[command(
    version,
    after_help = r#"JSONL session mode:
  Run without --query/--insert/--info/--test-types to enter a newline-delimited JSON session.
  Send one command per line; every response echoes the request_id so wrappers can demultiplex results.

  Supported command types:
    query         : {"type":"query","request_id":"req-1","sql":"SELECT 1","settings":{"send_logs_level":"trace"}}
    insert        : {"type":"insert","request_id":"req-2","table":"db.tbl","rows":[{"id":1}],"columns":[...]}
    insert_begin  : {"type":"insert_begin","request_id":"req-3","table":"db.tbl","columns":[...]}
    insert_rows   : {"type":"insert_rows","request_id":"req-3","rows":[{"id":2},{"id":3}]}
    insert_end    : {"type":"insert_end","request_id":"req-3"}
    insert_abort  : {"type":"insert_abort","request_id":"req-3"}
    cancel        : {"type":"cancel","request_id":"req-1"}
    shutdown      : {"type":"shutdown"}

  Response types:
    {"type":"data","request_id":"req-1","data":{...}}
    {"type":"error","request_id":"req-1","error":"..."}
    {"type":"message","request_id":"req-1","message":"...","level":"info"}
    {"type":"progress","request_id":"req-1","data":{...}}
    {"type":"profile","request_id":"req-1","data":{...}}
    {"type":"profile_info","request_id":"req-1","data":{...}}
    {"type":"log","request_id":"req-1","data":{...}}
    {"type":"started","request_id":"req-3","data":{...}}
    {"type":"rows","request_id":"req-3","data":{"rows":2,"total_rows":3}}
    {"type":"complete","request_id":"req-1","data":{"status":"ok"}}
    {"type":"cancel_requested","request_id":"req-1","data":{...}}
    {"type":"cancelled","request_id":"req-1","data":{...}}
    {"type":"aborted","request_id":"req-3","data":{}}

  Only one request may be active at a time. See test-client/README.md for full details."#
)]
struct Args {
    /// SQL query to execute
    #[arg(long, group = "mode")]
    query: Option<String>,

    /// Table name for insert mode (format: [database.]table) - reads JSON from stdin
    #[arg(long, group = "mode")]
    insert: Option<String>,

    /// Optional comma-separated column list for INSERT (server applies defaults for others)
    #[arg(long)]
    columns: Option<String>,

    /// Get server information
    #[arg(long, group = "mode")]
    info: bool,

    /// Test various ClickHouse native types
    #[arg(long, group = "mode")]
    test_types: bool,

    /// Query parameters as JSON object
    #[arg(long)]
    params: Option<String>,

    /// Query settings as JSON object
    #[arg(long)]
    settings: Option<String>,

    /// ClickHouse host
    #[arg(long, default_value = "localhost")]
    host: String,

    /// ClickHouse port
    #[arg(long, default_value = "9000")]
    port: u16,

    /// Database user
    #[arg(long, default_value = "default")]
    user: String,

    /// Database password
    #[arg(long, default_value = "")]
    password: String,

    /// Database name
    #[arg(long, default_value = "default")]
    database: String,

    /// Use secure connection (TLS)
    #[arg(long)]
    secure: bool,

    /// Compression codec: none, lz4, zstd
    #[arg(long, default_value = "lz4")]
    compression: String,

    /// Enable debug logging
    #[arg(long)]
    debug: bool,

    /// Output format: json, pretty
    #[arg(long, default_value = "json")]
    format: String,

    /// Path to write query results in native format (requires --query)
    #[arg(long)]
    native_output: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct JsonOutput {
    #[serde(rename = "type")]
    output_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data:        Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error:       Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message:     Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    level:       Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id:  Option<String>,
}

impl JsonOutput {
    fn data(data: serde_json::Value) -> Self {
        Self {
            output_type: "data".to_string(),
            data:        Some(data),
            error:       None,
            message:     None,
            level:       None,
            request_id:  None,
        }
    }

    fn error(error: String) -> Self {
        Self {
            output_type: "error".to_string(),
            data:        None,
            error:       Some(error),
            message:     None,
            level:       None,
            request_id:  None,
        }
    }

    fn message(message: String, level: &str) -> Self {
        Self {
            output_type: "message".to_string(),
            data:        None,
            error:       None,
            message:     Some(message),
            level:       Some(level.to_string()),
            request_id:  None,
        }
    }

    fn event(kind: &str, payload: serde_json::Value) -> Self {
        Self {
            output_type: kind.to_string(),
            data:        Some(payload),
            error:       None,
            message:     None,
            level:       None,
            request_id:  None,
        }
    }

    fn with_request_id(mut self, request_id: Option<&str>) -> Self {
        if let Some(id) = request_id {
            self.request_id = Some(id.to_string());
        }
        self
    }
}

fn spawn_stdout_writer(is_pretty: bool) -> mpsc::Sender<WriterCmd> {
    let (tx, mut rx) = mpsc::channel::<WriterCmd>(1024);

    tokio::task::spawn_blocking(move || {
        let stdout = std::io::stdout();
        let mut pending = Vec::with_capacity(128);

        loop {
            match rx.blocking_recv() {
                Some(cmd) => pending.push(cmd),
                None => break,
            }

            while let Ok(cmd) = rx.try_recv() {
                pending.push(cmd);
            }

            let mut handle = stdout.lock();
            let mut writer = BufWriter::with_capacity(1024 * 1024, &mut handle);
            let mut error_occurred = false;

            for cmd in pending.drain(..) {
                let result = match cmd {
                    WriterCmd::Json(output) => {
                        if is_pretty {
                            serde_json::to_writer_pretty(&mut writer, &output)
                        } else {
                            serde_json::to_writer(&mut writer, &output)
                        }
                    }
                    WriterCmd::Data(payload) => {
                        let request_id = payload.request_id.as_ref().map(|id| id.as_str());
                        let event = DataEvent {
                            output_type: "data",
                            request_id,
                            data: RowSer {
                                cols: payload.cols.as_slice(),
                                row:  payload.row.as_slice(),
                            },
                        };
                        if is_pretty {
                            serde_json::to_writer_pretty(&mut writer, &event)
                        } else {
                            serde_json::to_writer(&mut writer, &event)
                        }
                    }
                };

                if let Err(err) = result {
                    let _ = serde_json::to_writer(
                        &mut writer,
                        &JsonOutput::error(format!("writer serialization error: {err}")),
                    );
                    error_occurred = true;
                }

                if writer.write_all(b"\n").is_err() {
                    error_occurred = true;
                    break;
                }
            }

            if writer.flush().is_err() {
                break;
            }

            if error_occurred {
                break;
            }
        }
    });

    tx
}

#[derive(Clone)]
enum ActiveKind {
    Query { qid: Qid },
    Insert { table: String, columns: Option<Vec<String>>, total_rows: usize },
}

#[derive(Clone)]
struct ActiveRequest {
    request_id: String,
    kind:       ActiveKind,
}

#[derive(Clone, Default)]
struct SessionState {
    active: Arc<Mutex<Option<ActiveRequest>>>,
}

impl SessionState {
    async fn activate(
        &self,
        request_id: String,
        kind: ActiveKind,
    ) -> std::result::Result<(), ActiveRequest> {
        let mut guard = self.active.lock().await;
        if let Some(existing) = guard.as_ref().cloned() {
            return Err(existing);
        }

        *guard = Some(ActiveRequest { request_id, kind });
        Ok(())
    }

    async fn clear_if_matches(&self, request_id: &str) {
        let mut guard = self.active.lock().await;
        if guard.as_ref().is_some_and(|active| active.request_id == request_id) {
            *guard = None;
        }
    }

    async fn request_for_event(&self, qid: Qid) -> Option<String> {
        let guard = self.active.lock().await;
        guard.as_ref().and_then(|active| match active.kind {
            ActiveKind::Query { qid: active_qid } if active_qid == qid => {
                Some(active.request_id.clone())
            }
            ActiveKind::Query { .. } => None,
            ActiveKind::Insert { .. } => Some(active.request_id.clone()),
        })
    }

    async fn active_for(&self, request_id: &str) -> Option<ActiveRequest> {
        let guard = self.active.lock().await;
        guard
            .as_ref()
            .and_then(|active| (active.request_id == request_id).then(|| active.clone()))
    }

    async fn insert_target(&self, request_id: &str) -> Option<(String, Option<Vec<String>>)> {
        let guard = self.active.lock().await;
        guard.as_ref().and_then(|active| {
            if active.request_id == request_id {
                if let ActiveKind::Insert { table, columns, .. } = &active.kind {
                    return Some((table.clone(), columns.clone()));
                }
            }
            None
        })
    }

    async fn add_insert_rows(&self, request_id: &str, delta: usize) -> Option<usize> {
        let mut guard = self.active.lock().await;
        guard.as_mut().and_then(|active| {
            if active.request_id == request_id {
                if let ActiveKind::Insert { total_rows, .. } = &mut active.kind {
                    *total_rows += delta;
                    return Some(*total_rows);
                }
            }
            None
        })
    }

    async fn insert_total_rows(&self, request_id: &str) -> Option<usize> {
        let guard = self.active.lock().await;
        guard.as_ref().and_then(|active| {
            if active.request_id == request_id {
                if let ActiveKind::Insert { total_rows, .. } = &active.kind {
                    return Some(*total_rows);
                }
            }
            None
        })
    }
}

fn spawn_event_forwarder(
    client: Client<NativeFormat>,
    tx: mpsc::Sender<WriterCmd>,
    session_state: Option<SessionState>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut events = client.subscribe_events();
        while let Ok(evt) = events.recv().await {
            let request_id = if let Some(state) = &session_state {
                state.request_for_event(evt.qid).await
            } else {
                None
            };

            for output in event_to_outputs(evt.event, request_id.as_deref()) {
                let _ = tx.send(WriterCmd::Json(output)).await;
            }
        }
    })
}

fn event_to_outputs(
    event: clickhouse_arrow::ClickHouseEvent,
    request_id: Option<&str>,
) -> Vec<JsonOutput> {
    match event {
        clickhouse_arrow::ClickHouseEvent::Progress(p) => {
            let payload = serde_json::json!({
                "read_rows": p.read_rows,
                "read_bytes": p.read_bytes,
                "total_rows_to_read": p.total_rows_to_read,
                "total_bytes_to_read": p.total_bytes_to_read,
                "written_rows": p.written_rows,
                "written_bytes": p.written_bytes,
                "elapsed_ns": p.elapsed_ns,
            });
            vec![JsonOutput::event("progress", payload).with_request_id(request_id)]
        }
        clickhouse_arrow::ClickHouseEvent::Profile(events) => {
            // Group by (time, host, thread); type is now per metric entry
            let mut grouped: HashMap<(_, _, u64), serde_json::Map<String, serde_json::Value>> =
                HashMap::new();

            for event in events {
                let key = (event.current_time.clone(), event.host_name.clone(), event.thread_id);
                let entry = grouped.entry(key.clone()).or_insert_with(|| {
                    let mut map = serde_json::Map::with_capacity(8);
                    map.insert(
                        "current_time".to_string(),
                        serde_json::Value::String(key.0.clone()),
                    );
                    map.insert("host_name".to_string(), serde_json::Value::String(key.1.clone()));
                    map.insert("thread_id".to_string(), serde_json::Value::from(key.2));
                    map
                });

                // Per-metric object: { value, type }
                entry.insert(
                    event.name,
                    serde_json::json!({
                        "value": event.value,
                        "type": event.type_name,
                    }),
                );
            }

            let mut values: Vec<serde_json::Value> =
                grouped.into_values().map(serde_json::Value::Object).collect();

            let payload = if values.len() == 1 {
                values.pop().unwrap()
            } else {
                serde_json::Value::Array(values)
            };

            vec![JsonOutput::event("profile", payload).with_request_id(request_id)]
        }
        clickhouse_arrow::ClickHouseEvent::Log(logs) => logs
            .into_iter()
            .map(|log| {
                let payload = serde_json::json!({
                    "time": log.time,
                    "time_microseconds": log.time_microseconds,
                    "host_name": log.host_name,
                    "query_id": log.query_id,
                    "thread_id": log.thread_id,
                    "priority": log.priority,
                    "source": log.source,
                    "text": log.text,
                });
                JsonOutput::event("log", payload).with_request_id(request_id)
            })
            .collect(),
        clickhouse_arrow::ClickHouseEvent::ProfileInfo(info) => {
            let payload = serde_json::json!({
                "rows": info.rows,
                "blocks": info.blocks,
                "bytes": info.bytes,
                "applied_limit": info.applied_limit,
                "rows_before_limit": info.rows_before_limit,
                "calculated_rows_before_limit": info.calculated_rows_before_limit,
                "applied_aggregation": info.applied_aggregation,
                "rows_before_aggregation": info.rows_before_aggregation,
            });
            vec![JsonOutput::event("profile_info", payload).with_request_id(request_id)]
        }
    }
}

fn json_to_setting_value(value: &Value) -> Result<SettingValue> {
    match value {
        Value::Null => Err(anyhow!("settings do not support null values")),
        Value::Bool(b) => Ok(SettingValue::Bool(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(SettingValue::Int(i))
            } else if let Some(u) = n.as_u64() {
                if u <= i64::MAX as u64 {
                    Ok(SettingValue::Int(u as i64))
                } else {
                    Ok(SettingValue::String(u.to_string()))
                }
            } else if let Some(f) = n.as_f64() {
                Ok(SettingValue::Float(f))
            } else {
                Ok(SettingValue::String(n.to_string()))
            }
        }
        Value::String(s) => Ok(SettingValue::String(s.clone())),
        Value::Array(_) | Value::Object(_) => Ok(SettingValue::String(value.to_string())),
    }
}

fn map_to_query_params(map: &HashMap<String, Value>) -> Result<Option<QueryParams>> {
    if map.is_empty() {
        return Ok(None);
    }

    let mut items = Vec::with_capacity(map.len());
    for (key, value) in map {
        let v = json_to_setting_value(value)?;
        items.push((key.clone(), v));
    }

    Ok(Some(QueryParams(items)))
}

fn map_to_settings(map: &HashMap<String, Value>) -> Result<Option<Settings>> {
    if map.is_empty() {
        return Ok(None);
    }

    let mut settings = Settings::default();
    for (key, value) in map {
        match json_to_setting_value(value)? {
            SettingValue::Int(i) => settings.add_setting(key.clone(), i),
            SettingValue::Bool(b) => settings.add_setting(key.clone(), b),
            SettingValue::Float(f) => settings.add_setting(key.clone(), f),
            SettingValue::String(s) => settings.add_setting(key.clone(), s),
        }
    }

    Ok(Some(settings))
}

fn output_json(output: &JsonOutput) {
    println!("{}", serde_json::to_string(output).unwrap());
}

fn output_pretty(data: &serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(data).unwrap());
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize logging
    let filter = if args.debug { "debug" } else { "info" };

    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).json().init();

    // Validate compression
    match args.compression.as_str() {
        "none" | "lz4" | "zstd" => {}
        _ => {
            output_json(&JsonOutput::error(format!(
                "Invalid compression: {}. Supported: none, lz4, zstd",
                args.compression
            )));
            std::process::exit(1);
        }
    }

    if args.native_output.is_some() && args.query.is_none() {
        output_json(&JsonOutput::error(
            "--native-output currently requires --query mode".to_string(),
        ));
        std::process::exit(1);
    }

    // Initialize ClickHouse client
    let client = match ClickHouseClient::new(
        &args.host,
        args.port,
        &args.user,
        &args.password,
        &args.database,
        args.secure,
        &args.compression,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            output_json(&JsonOutput::error(format!("Failed to connect: {e}")));
            std::process::exit(1);
        }
    };

    // Execute based on mode
    if let Some(query) = args.query {
        execute_query(
            client,
            &query,
            args.params,
            args.settings,
            &args.format,
            &args.compression,
            args.native_output,
        )
        .await?;
    } else if let Some(table) = args.insert {
        execute_insert(client, &table, args.columns.clone(), &args.format).await?;
    } else if args.info {
        get_server_info(client, &args.format).await?;
    } else if args.test_types {
        test_types(client, &args.format).await?;
    } else {
        run_session(client, &args.format).await?;
    }

    Ok(())
}

async fn execute_query(
    client: ClickHouseClient,
    query: &str,
    params: Option<String>,
    settings: Option<String>,
    format: &str,
    compression: &str,
    native_output: Option<PathBuf>,
) -> Result<()> {
    let params = if let Some(params_str) = params {
        let map: HashMap<String, Value> = serde_json::from_str(&params_str)
            .context("Failed to parse query parameters as JSON")?;
        map_to_query_params(&map)?
    } else {
        None
    };

    let settings = if let Some(settings_str) = settings {
        let map: HashMap<String, Value> = serde_json::from_str(&settings_str)
            .context("Failed to parse query settings as JSON")?;
        map_to_settings(&map)?
    } else {
        None
    };

    // Split query into statements (basic approach) - should def be more robust
    let statements = split_statements(query);

    use clickhouse_arrow::Qid;
    use clickhouse_arrow::native::block::Block;

    // Single writer task to serialize lines to stdout to avoid interleaving and contention
    let is_pretty = format == "pretty";
    let tx = spawn_stdout_writer(is_pretty);

    let ch = client.native_client();
    let _ev_task = spawn_event_forwarder(ch.clone(), tx.clone(), None);

    let compression_method = match compression {
        "lz4" => CompressionMethod::LZ4,
        "zstd" => CompressionMethod::ZSTD,
        _ => CompressionMethod::None,
    };

    let native_output_path = native_output;
    let mut native_writer = if let Some(ref path) = native_output_path {
        let file = tokio::fs::File::create(path).await.with_context(|| {
            format!("Failed to create native output file at {}", path.display())
        })?;
        let buf_writer = AsyncBufWriter::with_capacity(8 * 1024 * 1024, file);
        Some(FileStreamWriter::<NativeFormat, _>::new(
            buf_writer,
            compression_method,
            ArrowOptions::default(),
            None,
        ))
    } else {
        None
    };

    if native_writer.is_some() {
        let message =
            JsonOutput::message("Writing query results to native output file".to_string(), "info");
        let _ = tx.send(WriterCmd::Json(message)).await;
    }

    for statement in statements {
        let qid = Qid::new();
        let mut stream = ch
            .query_raw_with_settings(statement.to_string(), params.clone(), settings.clone(), qid)
            .await
            .context("Failed to send query")?;

        // Clone sender for events task
        let tx_events = tx.clone();
        while let Some(item) = stream.next().await {
            let mut block: Block = item.context("stream error")?;

            if let Some(writer) = native_writer.as_mut() {
                writer.write(block.clone()).await.context("Failed to write native block")?;
            }
            let cols = Arc::new(block.column_types.clone());

            // Stream rows without O(n^2) front removals; move values out per row
            for row in block.take_iter_rows() {
                let row_values: Vec<clickhouse_arrow::Value> =
                    row.into_iter().map(|(_name, _ty, v)| v).collect();
                let payload = DataPayload {
                    cols:       cols.clone(),
                    row:        row_values,
                    request_id: None,
                };
                let _ = tx_events.send(WriterCmd::Data(payload)).await;
            }
        }
    }

    if let Some(writer) = native_writer.as_mut() {
        writer.finish().await.context("Failed to finish native output stream")?;
        writer.writer_mut().flush().await.context("Failed to flush native output")?;
    }

    if let Some(path) = native_output_path {
        let message = format!("Native output written to {}", path.display());
        let _ = tx.send(WriterCmd::Json(JsonOutput::message(message, "info"))).await;
    }

    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SessionCommand {
    Query {
        sql:        String,
        #[serde(default)]
        params:     Option<HashMap<String, Value>>,
        #[serde(default)]
        settings:   Option<HashMap<String, Value>>,
        #[serde(default)]
        request_id: Option<String>,
    },
    Insert {
        table:      String,
        rows:       Vec<Value>,
        #[serde(default)]
        columns:    Option<Vec<String>>,
        #[serde(default)]
        request_id: Option<String>,
    },
    InsertBegin {
        table:      String,
        #[serde(default)]
        columns:    Option<Vec<String>>,
        request_id: String,
    },
    InsertRows {
        rows:       Vec<Value>,
        request_id: String,
    },
    InsertEnd {
        request_id: String,
    },
    InsertAbort {
        request_id: String,
    },
    Cancel {
        request_id: String,
    },
    Shutdown,
}

async fn run_session(client: ClickHouseClient, format: &str) -> Result<()> {
    if format != "json" {
        output_json(&JsonOutput::error(format!(
            "Session mode currently supports only json output (got {format})"
        )));
        std::process::exit(1);
    }

    let native = client.native_client().clone();
    let tx = spawn_stdout_writer(false);
    let session_state = SessionState::default();
    let _event_task =
        spawn_event_forwarder(native.clone(), tx.clone(), Some(session_state.clone()));

    let stdin = tokio::io::stdin();
    let reader = AsyncBufReader::new(stdin);
    let mut lines = reader.lines();

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let command: SessionCommand = match serde_json::from_str(trimmed) {
            Ok(cmd) => cmd,
            Err(err) => {
                let _ = tx
                    .send(WriterCmd::Json(JsonOutput::error(format!(
                        "Invalid session command: {err}"
                    ))))
                    .await;
                continue;
            }
        };

        match command {
            SessionCommand::Query { sql, params, settings, request_id } => {
                handle_session_query(
                    &native,
                    &tx,
                    &session_state,
                    sql,
                    params,
                    settings,
                    request_id,
                )
                .await;
            }
            SessionCommand::Insert { table, rows, columns, request_id } => {
                handle_session_insert(
                    &client,
                    &tx,
                    &session_state,
                    table,
                    rows,
                    columns,
                    request_id,
                )
                .await;
            }
            SessionCommand::InsertBegin { table, columns, request_id } => {
                handle_session_insert_begin(&tx, &session_state, table, columns, request_id).await;
            }
            SessionCommand::InsertRows { rows, request_id } => {
                handle_session_insert_rows(&client, &tx, &session_state, request_id, rows).await;
            }
            SessionCommand::InsertEnd { request_id } => {
                handle_session_insert_end(&tx, &session_state, request_id).await;
            }
            SessionCommand::InsertAbort { request_id } => {
                handle_session_insert_abort(&tx, &session_state, request_id).await;
            }
            SessionCommand::Cancel { request_id } => {
                handle_session_cancel(&native, &tx, &session_state, request_id).await;
            }
            SessionCommand::Shutdown => break,
        }
    }

    Ok(())
}

async fn handle_session_query(
    client: &Client<NativeFormat>,
    tx: &mpsc::Sender<WriterCmd>,
    session_state: &SessionState,
    sql: String,
    params: Option<HashMap<String, Value>>,
    settings: Option<HashMap<String, Value>>,
    request_id: Option<String>,
) {
    let Some(mut request_id) = request_id.filter(|s| !s.trim().is_empty()) else {
        let _ = tx
            .send(WriterCmd::Json(JsonOutput::error(
                "Session query requires request_id".to_string(),
            )))
            .await;
        return;
    };
    request_id = request_id.trim().to_string();

    let params = match params {
        Some(map) => match map_to_query_params(&map) {
            Ok(v) => v,
            Err(err) => {
                let msg = JsonOutput::error(format!("Invalid params: {err}"))
                    .with_request_id(Some(request_id.as_str()));
                let _ = tx.send(WriterCmd::Json(msg)).await;
                return;
            }
        },
        None => None,
    };

    let settings = match settings {
        Some(map) => match map_to_settings(&map) {
            Ok(v) => v,
            Err(err) => {
                let msg = JsonOutput::error(format!("Invalid settings: {err}"))
                    .with_request_id(Some(request_id.as_str()));
                let _ = tx.send(WriterCmd::Json(msg)).await;
                return;
            }
        },
        None => None,
    };

    let statements = split_statements(&sql);
    if statements.len() != 1 {
        let msg =
            JsonOutput::error("Session queries must include exactly one statement".to_string())
                .with_request_id(Some(request_id.as_str()));
        let _ = tx.send(WriterCmd::Json(msg)).await;
        return;
    }
    let statement = statements[0];

    let qid = Qid::new();
    if let Err(existing) =
        session_state.activate(request_id.clone(), ActiveKind::Query { qid }).await
    {
        let msg = JsonOutput::error(format!(
            "Another request ({}) is still in progress",
            existing.request_id
        ))
        .with_request_id(Some(request_id.as_str()));
        let _ = tx.send(WriterCmd::Json(msg)).await;
        return;
    }

    let start = JsonOutput::event(
        "started",
        serde_json::json!({
            "query_id": qid.to_string(),
        }),
    )
    .with_request_id(Some(request_id.as_str()));
    let _ = tx.send(WriterCmd::Json(start)).await;

    use clickhouse_arrow::native::block::Block;

    let result: Result<()> = async {
        let mut stream = client
            .query_raw_with_settings(statement.to_string(), params.clone(), settings.clone(), qid)
            .await
            .context("Failed to send query")?;

        let request_arc = Arc::new(request_id.clone());
        while let Some(item) = stream.next().await {
            let mut block: Block = item.context("stream error")?;
            let cols = Arc::new(block.column_types.clone());

            for row in block.take_iter_rows() {
                let row_values: Vec<clickhouse_arrow::Value> =
                    row.into_iter().map(|(_name, _ty, v)| v).collect();
                let payload = DataPayload {
                    cols:       cols.clone(),
                    row:        row_values,
                    request_id: Some(Arc::clone(&request_arc)),
                };
                let _ = tx.send(WriterCmd::Data(payload)).await;
            }
        }

        Ok(())
    }
    .await;

    match result {
        Ok(_) => {
            let complete = JsonOutput::event(
                "complete",
                serde_json::json!({
                    "status": "ok",
                }),
            )
            .with_request_id(Some(request_id.as_str()));
            let _ = tx.send(WriterCmd::Json(complete)).await;
        }
        Err(err) => {
            let msg =
                JsonOutput::error(format!("{err}")).with_request_id(Some(request_id.as_str()));
            let _ = tx.send(WriterCmd::Json(msg)).await;
        }
    }

    session_state.clear_if_matches(&request_id).await;
}

async fn handle_session_insert(
    client: &ClickHouseClient,
    tx: &mpsc::Sender<WriterCmd>,
    session_state: &SessionState,
    table: String,
    rows: Vec<Value>,
    columns: Option<Vec<String>>,
    request_id: Option<String>,
) {
    let Some(request_id) = request_id.filter(|s| !s.trim().is_empty()) else {
        let _ = tx
            .send(WriterCmd::Json(JsonOutput::error(
                "Session insert requires request_id".to_string(),
            )))
            .await;
        return;
    };
    let request_id = request_id.trim().to_string();

    let table_trimmed = table.trim().to_string();
    if table_trimmed.is_empty() {
        let msg = JsonOutput::error("Insert requires non-empty table".to_string())
            .with_request_id(Some(request_id.as_str()));
        let _ = tx.send(WriterCmd::Json(msg)).await;
        return;
    }

    let normalized_columns = normalize_columns(columns);

    if session_insert_begin(
        tx,
        session_state,
        &request_id,
        &table_trimmed,
        normalized_columns.clone(),
    )
    .await
    .is_err()
    {
        return;
    }

    let mut ok = true;
    if session_insert_rows(client, tx, session_state, &request_id, rows).await.is_err() {
        ok = false;
    }

    if ok {
        let _ = session_insert_end(tx, session_state, &request_id).await;
    } else {
        session_state.clear_if_matches(&request_id).await;
    }
}

async fn handle_session_insert_begin(
    tx: &mpsc::Sender<WriterCmd>,
    session_state: &SessionState,
    table: String,
    columns: Option<Vec<String>>,
    request_id: String,
) {
    let request_id = request_id.trim();
    if request_id.is_empty() {
        let _ = tx
            .send(WriterCmd::Json(JsonOutput::error(
                "insert_begin requires request_id".to_string(),
            )))
            .await;
        return;
    }

    let table_trimmed = table.trim();
    if table_trimmed.is_empty() {
        let msg = JsonOutput::error("insert_begin requires non-empty table".to_string())
            .with_request_id(Some(request_id));
        let _ = tx.send(WriterCmd::Json(msg)).await;
        return;
    }

    let normalized_columns = normalize_columns(columns);
    let _ = session_insert_begin(tx, session_state, request_id, table_trimmed, normalized_columns)
        .await;
}

async fn handle_session_insert_rows(
    client: &ClickHouseClient,
    tx: &mpsc::Sender<WriterCmd>,
    session_state: &SessionState,
    request_id: String,
    rows: Vec<Value>,
) {
    let request_id = request_id.trim();
    if request_id.is_empty() {
        let _ = tx
            .send(WriterCmd::Json(JsonOutput::error("insert_rows requires request_id".to_string())))
            .await;
        return;
    }

    let _ = session_insert_rows(client, tx, session_state, request_id, rows).await;
}

async fn handle_session_insert_end(
    tx: &mpsc::Sender<WriterCmd>,
    session_state: &SessionState,
    request_id: String,
) {
    let request_id = request_id.trim();
    if request_id.is_empty() {
        let _ = tx
            .send(WriterCmd::Json(JsonOutput::error("insert_end requires request_id".to_string())))
            .await;
        return;
    }

    let _ = session_insert_end(tx, session_state, request_id).await;
}

async fn handle_session_insert_abort(
    tx: &mpsc::Sender<WriterCmd>,
    session_state: &SessionState,
    request_id: String,
) {
    let request_id = request_id.trim();
    if request_id.is_empty() {
        let _ = tx
            .send(WriterCmd::Json(JsonOutput::error(
                "insert_abort requires request_id".to_string(),
            )))
            .await;
        return;
    }

    let _ = session_insert_abort(tx, session_state, request_id).await;
}

async fn session_insert_begin(
    tx: &mpsc::Sender<WriterCmd>,
    session_state: &SessionState,
    request_id: &str,
    table: &str,
    columns: Option<Vec<String>>,
) -> Result<(), ()> {
    if let Err(existing) = session_state
        .activate(request_id.to_string(), ActiveKind::Insert {
            table:      table.to_string(),
            columns:    columns.clone(),
            total_rows: 0,
        })
        .await
    {
        let msg = JsonOutput::error(format!(
            "Another request ({}) is still in progress",
            existing.request_id
        ))
        .with_request_id(Some(request_id));
        let _ = tx.send(WriterCmd::Json(msg)).await;
        return Err(());
    }

    let start = JsonOutput::event(
        "started",
        serde_json::json!({
            "operation": "insert",
            "table": table,
            "columns": columns,
            "rows": 0,
        }),
    )
    .with_request_id(Some(request_id));
    let _ = tx.send(WriterCmd::Json(start)).await;
    Ok(())
}

async fn session_insert_rows(
    client: &ClickHouseClient,
    tx: &mpsc::Sender<WriterCmd>,
    session_state: &SessionState,
    request_id: &str,
    rows: Vec<Value>,
) -> Result<(), ()> {
    let Some((table, columns)) = session_state.insert_target(request_id).await else {
        let msg = JsonOutput::error("No active insert for request".to_string())
            .with_request_id(Some(request_id));
        let _ = tx.send(WriterCmd::Json(msg)).await;
        return Err(());
    };

    if rows.is_empty() {
        let total = session_state.insert_total_rows(request_id).await.unwrap_or(0);
        let ack = JsonOutput::event(
            "rows",
            serde_json::json!({
                "operation": "insert",
                "rows": 0,
                "total_rows": total,
            }),
        )
        .with_request_id(Some(request_id));
        let _ = tx.send(WriterCmd::Json(ack)).await;
        return Ok(());
    }

    let chunk_len = rows.len();
    let result = client.insert_batch(&table, rows, columns.clone()).await;
    match result {
        Ok(_) => {
            let total = session_state.add_insert_rows(request_id, chunk_len).await.unwrap_or(0);
            let ack = JsonOutput::event(
                "rows",
                serde_json::json!({
                    "operation": "insert",
                    "rows": chunk_len,
                    "total_rows": total,
                }),
            )
            .with_request_id(Some(request_id));
            let _ = tx.send(WriterCmd::Json(ack)).await;
            Ok(())
        }
        Err(err) => {
            let msg = JsonOutput::error(format!("Insert failed: {err}"))
                .with_request_id(Some(request_id));
            let _ = tx.send(WriterCmd::Json(msg)).await;
            Err(())
        }
    }
}

async fn session_insert_end(
    tx: &mpsc::Sender<WriterCmd>,
    session_state: &SessionState,
    request_id: &str,
) -> Result<(), ()> {
    let Some(active) = session_state.active_for(request_id).await else {
        let msg = JsonOutput::error("No active insert for request".to_string())
            .with_request_id(Some(request_id));
        let _ = tx.send(WriterCmd::Json(msg)).await;
        return Err(());
    };

    match active.kind {
        ActiveKind::Insert { .. } => {
            let total = session_state.insert_total_rows(request_id).await.unwrap_or(0);
            let complete = JsonOutput::event(
                "complete",
                serde_json::json!({
                    "status": "ok",
                    "operation": "insert",
                    "rows": total,
                }),
            )
            .with_request_id(Some(request_id));
            session_state.clear_if_matches(request_id).await;
            let _ = tx.send(WriterCmd::Json(complete)).await;
            Ok(())
        }
        ActiveKind::Query { .. } => {
            let msg = JsonOutput::error("Request is not an insert".to_string())
                .with_request_id(Some(request_id));
            let _ = tx.send(WriterCmd::Json(msg)).await;
            Err(())
        }
    }
}

async fn session_insert_abort(
    tx: &mpsc::Sender<WriterCmd>,
    session_state: &SessionState,
    request_id: &str,
) -> Result<(), ()> {
    let Some(active) = session_state.active_for(request_id).await else {
        let msg = JsonOutput::error("No active request with that id".to_string())
            .with_request_id(Some(request_id));
        let _ = tx.send(WriterCmd::Json(msg)).await;
        return Err(());
    };

    match active.kind {
        ActiveKind::Insert { .. } => {
            session_state.clear_if_matches(request_id).await;
            let aborted = JsonOutput::event(
                "aborted",
                serde_json::json!({
                    "operation": "insert",
                }),
            )
            .with_request_id(Some(request_id));
            let _ = tx.send(WriterCmd::Json(aborted)).await;
            Ok(())
        }
        ActiveKind::Query { .. } => {
            let msg = JsonOutput::error("Request is not an insert".to_string())
                .with_request_id(Some(request_id));
            let _ = tx.send(WriterCmd::Json(msg)).await;
            Err(())
        }
    }
}

async fn handle_session_cancel(
    client: &Client<NativeFormat>,
    tx: &mpsc::Sender<WriterCmd>,
    session_state: &SessionState,
    request_id: String,
) {
    let trimmed = request_id.trim();
    if trimmed.is_empty() {
        let _ = tx
            .send(WriterCmd::Json(JsonOutput::error(
                "Cancel command requires request_id".to_string(),
            )))
            .await;
        return;
    }

    let Some(active) = session_state.active_for(trimmed).await else {
        let msg = JsonOutput::error("No active request with that id".to_string())
            .with_request_id(Some(trimmed));
        let _ = tx.send(WriterCmd::Json(msg)).await;
        return;
    };

    match active.kind {
        ActiveKind::Query { qid } => {
            let cancel_result =
                client.execute(format!("KILL QUERY WHERE query_id = '{}' SYNC", qid), None).await;

            match cancel_result {
                Ok(_) => {
                    let msg = JsonOutput::event(
                        "cancel_requested",
                        serde_json::json!({
                            "query_id": qid.to_string(),
                        }),
                    )
                    .with_request_id(Some(trimmed));
                    let _ = tx.send(WriterCmd::Json(msg)).await;
                }
                Err(err) => {
                    let msg = JsonOutput::error(format!("Cancel failed: {err}"))
                        .with_request_id(Some(trimmed));
                    let _ = tx.send(WriterCmd::Json(msg)).await;
                }
            }
        }
        ActiveKind::Insert { .. } => {
            session_state.clear_if_matches(trimmed).await;
            let msg = JsonOutput::event(
                "cancelled",
                serde_json::json!({
                    "operation": "insert",
                }),
            )
            .with_request_id(Some(trimmed));
            let _ = tx.send(WriterCmd::Json(msg)).await;
        }
    }
}

async fn execute_insert(
    client: ClickHouseClient,
    table: &str,
    columns: Option<String>,
    format: &str,
) -> Result<()> {
    if atty::is(atty::Stream::Stdin) {
        output_json(&JsonOutput::error("Insert mode requires JSON data from stdin".to_string()));
        std::process::exit(1);
    }

    // Read JSON lines from stdin
    let stdin = tokio::io::stdin();
    let reader = AsyncBufReader::new(stdin);
    let mut lines = reader.lines();
    let mut batch = Vec::new();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        match serde_json::from_str::<Value>(&line) {
            Ok(json) => {
                batch.push(json);

                // Insert in batches (simple approach)
                if batch.len() >= 1000 {
                    let cols = columns.as_ref().map(|s| {
                        s.split(',')
                            .map(|x| x.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                    });
                    match client.insert_batch(table, batch.clone(), cols).await {
                        Ok(_) => {
                            if format == "pretty" {
                                println!("Inserted {} rows", batch.len());
                            } else {
                                output_json(&JsonOutput::message(
                                    format!("Inserted {} rows", batch.len()),
                                    "info",
                                ));
                            }
                        }
                        Err(e) => {
                            output_json(&JsonOutput::error(format!("Insert failed: {e}")));
                        }
                    }
                    batch.clear();
                }
            }
            Err(e) => {
                output_json(&JsonOutput::error(format!("Invalid JSON: {e}")));
                // Continue processing other lines
            }
        }
    }

    // Insert remaining rows
    if !batch.is_empty() {
        let cols = columns.as_ref().map(|s| {
            s.split(',').map(|x| x.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>()
        });
        match client.insert_batch(table, batch.clone(), cols).await {
            Ok(_) => {
                if format == "pretty" {
                    println!("Inserted {} rows", batch.len());
                } else {
                    output_json(&JsonOutput::message(
                        format!("Inserted {} rows", batch.len()),
                        "info",
                    ));
                }
            }
            Err(e) => {
                output_json(&JsonOutput::error(format!("Insert failed: {e}")));
            }
        }
    }

    Ok(())
}

fn split_statements(query: &str) -> Vec<&str> {
    query.split(';').map(|s| s.trim()).filter(|s| !s.is_empty()).collect()
}

fn normalize_columns(columns: Option<Vec<String>>) -> Option<Vec<String>> {
    columns.and_then(|cols| {
        let normalized: Vec<String> =
            cols.into_iter().map(|c| c.trim().to_string()).filter(|s| !s.is_empty()).collect();
        if normalized.is_empty() { None } else { Some(normalized) }
    })
}

async fn get_server_info(client: ClickHouseClient, format: &str) -> Result<()> {
    match client.get_server_info().await {
        Ok(info) => match format {
            "pretty" => output_pretty(&info),
            _ => output_json(&JsonOutput::data(info)),
        },
        Err(e) => {
            output_json(&JsonOutput::error(format!("Failed to get server info: {e}")));
            std::process::exit(1);
        }
    }

    Ok(())
}

async fn test_types(client: ClickHouseClient, format: &str) -> Result<()> {
    let test_queries = vec![
        // Basic types
        ("Basic integers", "SELECT 42 as int32, -123 as negative, 18446744073709551615 as uint64"),
        ("Boolean", "SELECT true as bool_true, false as bool_false"),
        ("Strings", "SELECT 'hello' as string, 'world' as another_string"),
        ("Floats", "SELECT 3.14159 as float64, 2.718::Float32 as float32"),
        // Date/time types
        ("Dates", "SELECT today() as date, now() as datetime, now64() as datetime64"),
        // Arrays
        ("Arrays", "SELECT [1, 2, 3, 4, 5] as int_array, ['a', 'b', 'c'] as string_array"),
        // Tuples
        ("Tuples", "SELECT (1, 'hello', 3.14) as tuple_example"),
        // Nullable types
        ("Nullable", "SELECT NULL as null_val, toNullable(42) as nullable_int"),
        // New types we implemented
        ("Bool type", "SELECT true::Bool as native_bool, false::Bool as native_false"),
        ("Nothing type", "SELECT NULL::Nothing as nothing_val"),
        // UUID and IPs
        (
            "UUID and IPs",
            "SELECT generateUUIDv4() as uuid, toIPv4('127.0.0.1') as ipv4, toIPv6('::1') as ipv6",
        ),
        // Enums
        (
            "Enums",
            "SELECT CAST('Red', 'Enum8(\\'Red\\' = 1, \\'Green\\' = 2, \\'Blue\\' = 3)') as color",
        ),
        // Decimals
        ("Decimals", "SELECT toDecimal32(123.45, 2) as dec32, toDecimal64(123.456789, 6) as dec64"),
    ];

    for (description, query) in test_queries {
        if format == "pretty" {
            println!("\n=== {description} ===");
        } else {
            output_json(&JsonOutput::message(format!("Testing: {description}"), "info"));
        }

        match client.execute_query(query, HashMap::new(), HashMap::new()).await {
            Ok(results) => {
                for row in results {
                    match format {
                        "pretty" => output_pretty(&row),
                        _ => output_json(&JsonOutput::data(row)),
                    }
                }
            }
            Err(e) => {
                let error_msg = format!("Test '{description}' failed: {e}");
                if format == "pretty" {
                    println!("ERROR: {error_msg}");
                } else {
                    output_json(&JsonOutput::error(error_msg));
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_output_serialization() {
        let output = JsonOutput::data(serde_json::json!({"test": "value"}));
        let json_str = serde_json::to_string(&output).unwrap();
        assert!(json_str.contains("\"type\":\"data\""));
        assert!(json_str.contains("\"test\":\"value\""));
    }

    #[test]
    fn test_error_output() {
        let output = JsonOutput::error("Test error".to_string());
        let json_str = serde_json::to_string(&output).unwrap();
        assert!(json_str.contains("\"type\":\"error\""));
        assert!(json_str.contains("\"error\":\"Test error\""));
    }
}
