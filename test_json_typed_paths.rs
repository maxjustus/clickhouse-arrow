use clickhouse_arrow::prelude::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create client
    let client = ClientBuilder::default()
        .host("localhost")
        .build()
        .await?;

    // Drop and create test table with JSON column and typed paths
    client.execute("DROP TABLE IF EXISTS test_json_typed", None).await?;
    
    client.execute(
        "CREATE TABLE test_json_typed (
            data JSON(
                id UInt32,
                name String
            )
        ) ENGINE = MergeTree() ORDER BY tuple()",
        None
    ).await?;

    println!("Table created successfully");

    // Insert some test data
    let insert_query = "INSERT INTO test_json_typed FORMAT JSONEachRow";
    let json_data = r#"{"data": {"id": 1, "name": "Alice", "extra": "field1"}}
{"data": {"id": 2, "name": "Bob", "score": 95.5}}
{"data": {"id": 3, "name": "Charlie", "active": true}}"#;
    
    // For now, let's just try the SQL insert to verify the table works
    client.execute(
        &format!("INSERT INTO test_json_typed VALUES ('{}')", 
                r#"{"id": 1, "name": "Alice", "extra": "field1"}"#),
        None
    ).await?;
    
    println!("Data inserted successfully");

    // Query the data back
    let mut cursor = client
        .query("SELECT data FROM test_json_typed")
        .await?;

    while let Some(block) = cursor.next().await? {
        println!("Got block with {} rows", block.rows);
        for (name, col_type) in &block.column_types {
            println!("  Column: {} -> {:?}", name, col_type);
        }
        // Print first few values
        for value in block.column_data.iter().take(3) {
            println!("  Value: {:?}", value);
        }
    }

    // Clean up
    client.execute("DROP TABLE test_json_typed", None).await?;
    
    Ok(())
}