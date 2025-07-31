use clickhouse_arrow::prelude::*;
use clickhouse_arrow::test_utils::ClickHouseContainer;
use clickhouse_arrow::{CompressionMethod};
use futures_util::StreamExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("clickhouse_arrow=trace")
        .init();

    // Start container with specific version
    let container = ClickHouseContainer::new("25.1");
    container.start().await?;

    let native_url = container.get_native_url();
    println!("ClickHouse URL: {}", native_url);

    // Create client without v3 format setting
    let client: NativeClient = ClientBuilder::new()
        .with_endpoint(native_url)
        .with_username(&container.user)
        .with_password(&container.password)
        .with_ipv4_only(true)
        .with_compression(CompressionMethod::None)
        .build()
        .await?;

    // Enable Dynamic type
    println!("Setting enable_dynamic_type...");
    client.execute("SET enable_dynamic_type = 1", None).await?;

    // Create table
    println!("Creating table...");
    client.execute("DROP TABLE IF EXISTS test_dynamic", None).await?;
    client.execute("CREATE TABLE test_dynamic (d Dynamic) ENGINE = Memory", None).await?;

    // Insert a simple value
    println!("Inserting data...");
    let block = Block {
        info: Default::default(),
        rows: 1,
        column_types: vec![("d".to_string(), Type::Dynamic)],
        column_data: vec![Value::Dynamic(Box::new(Value::Int32(42)))],
    };
    
    let mut stream = client.insert("INSERT INTO test_dynamic VALUES", block, None).await?;
    while let Some(result) = stream.next().await {
        result?;
    }

    println!("Data inserted successfully!");

    // Try to query back
    println!("Querying data...");
    let query = "SELECT * FROM test_dynamic";
    let mut stream = client.query_raw(query, None).await?;

    while let Some(result) = stream.next().await {
        let block = result?;
        println!("Received block with {} rows", block.rows);
        for (i, value) in block.column_data.iter().enumerate() {
            println!("  Value {}: {:?}", i, value);
        }
    }

    Ok(())
}