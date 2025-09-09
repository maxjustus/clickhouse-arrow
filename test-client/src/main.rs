mod client;
mod tcp_dump;

use std::collections::HashMap;

use anyhow::{Context, Result};
use clap::Parser;
use client::ClickHouseClient;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tcp_dump::{DumpFormat, TcpDumpConfig};
use tokio::io::{AsyncBufReadExt, BufReader as AsyncBufReader};

#[derive(Parser, Debug)]
#[command(name = "clickhouse-test-client")]
#[command(about = "A test client for the clickhouse-arrow native format implementation")]
#[command(version)]
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

    /// Enable TCP dump output to stdout
    #[arg(long)]
    tcp_dump: bool,

    /// Write TCP dump to file instead of stdout
    #[arg(long)]
    tcp_dump_file: Option<String>,

    /// TCP dump format: hex, binary, json, pcap
    #[arg(long, default_value = "hex")]
    tcp_dump_format: String,

    /// Include verbose TCP dump analysis
    #[arg(long)]
    tcp_dump_verbose: bool,
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
}

impl JsonOutput {
    fn data(data: serde_json::Value) -> Self {
        Self {
            output_type: "data".to_string(),
            data:        Some(data),
            error:       None,
            message:     None,
            level:       None,
        }
    }

    fn error(error: String) -> Self {
        Self {
            output_type: "error".to_string(),
            data:        None,
            error:       Some(error),
            message:     None,
            level:       None,
        }
    }

    fn message(message: String, level: &str) -> Self {
        Self {
            output_type: "message".to_string(),
            data:        None,
            error:       None,
            message:     Some(message),
            level:       Some(level.to_string()),
        }
    }
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

    // Validate TCP dump format
    let tcp_dump_format = match args.tcp_dump_format.as_str() {
        "hex" => DumpFormat::Hex,
        "binary" => DumpFormat::Binary,
        "json" => DumpFormat::Json,
        "pcap" => DumpFormat::Pcap,
        _ => {
            output_json(&JsonOutput::error(format!(
                "Invalid TCP dump format: {}. Supported: hex, binary, json, pcap",
                args.tcp_dump_format
            )));
            std::process::exit(1);
        }
    };

    // Create TCP dump config
    let tcp_dump_config = TcpDumpConfig {
        enabled:   args.tcp_dump,
        format:    tcp_dump_format,
        file_path: args.tcp_dump_file,
        verbose:   args.tcp_dump_verbose,
    };

    // Initialize ClickHouse client
    let client = match ClickHouseClient::new(
        &args.host,
        args.port,
        &args.user,
        &args.password,
        &args.database,
        args.secure,
        &args.compression,
        tcp_dump_config,
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
        execute_query(client, &query, args.params, args.settings, &args.format).await?;
    } else if let Some(table) = args.insert {
        execute_insert(client, &table, args.columns.clone(), &args.format).await?;
    } else if args.info {
        get_server_info(client, &args.format).await?;
    } else if args.test_types {
        test_types(client, &args.format).await?;
    }

    Ok(())
}

async fn execute_query(
    client: ClickHouseClient,
    query: &str,
    params: Option<String>,
    settings: Option<String>,
    format: &str,
) -> Result<()> {
    // Parse parameters
    let params: HashMap<String, Value> = if let Some(params_str) = params {
        serde_json::from_str(&params_str).context("Failed to parse query parameters as JSON")?
    } else {
        HashMap::new()
    };

    // Parse settings
    let settings: HashMap<String, Value> = if let Some(settings_str) = settings {
        serde_json::from_str(&settings_str).context("Failed to parse query settings as JSON")?
    } else {
        HashMap::new()
    };

    // Split query into statements (basic approach)
    let statements: Vec<&str> =
        query.split(';').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();

    for statement in statements {
        match client.execute_query(statement, settings.clone(), params.clone()).await {
            Ok(results) => {
                // Output each row as JSON
                for row in results {
                    match format {
                        "pretty" => output_pretty(&row),
                        _ => output_json(&JsonOutput::data(row)),
                    }
                }
            }
            Err(e) => {
                output_json(&JsonOutput::error(format!("Query failed: {e}")));
                std::process::exit(1);
            }
        }
    }

    Ok(())
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
                    let cols = columns
                        .as_ref()
                        .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>());
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
        let cols = columns
            .as_ref()
            .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>());
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
