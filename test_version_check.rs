use clickhouse_arrow::client::ClientBuilder;
use clickhouse_arrow::formats::SerializerState;
use clickhouse_arrow::native::types::{Type, Value};
use clickhouse_arrow::native::types::serialize::ClickHouseNativeSerializer;

#[tokio::main]
async fn main() {
    let mut state = SerializerState::default();
    state.server_version = Some((25, 1, 0)); // Set version < 25.6
    
    let values = vec\![Value::Int32(42), Value::String(b"test".to_vec())];
    let mut buffer = Vec::new();
    
    // Try Dynamic serialization - should fail
    println\!("Testing Dynamic serialization with server version 25.1...");
    match Type::Dynamic.serialize_prefix_async(&mut buffer, &mut state).await {
        Ok(_) => println\!("ERROR: Dynamic serialization should have failed\!"),
        Err(e) => println\!("SUCCESS: Dynamic serialization failed as expected: {}", e),
    }
    
    // Try JSON serialization - should fail
    println\!("\nTesting JSON serialization with server version 25.1...");
    buffer.clear();
    match Type::JSON.serialize_prefix_async(&mut buffer, &mut state).await {
        Ok(_) => println\!("ERROR: JSON serialization should have failed\!"),
        Err(e) => println\!("SUCCESS: JSON serialization failed as expected: {}", e),
    }
    
    // Try with server version >= 25.6
    println\!("\nTesting with server version 25.6...");
    state.server_version = Some((25, 6, 0));
    buffer.clear();
    
    match Type::Dynamic.serialize_prefix_async(&mut buffer, &mut state).await {
        Ok(_) => println\!("SUCCESS: Dynamic serialization works with server 25.6"),
        Err(e) => println\!("ERROR: Dynamic serialization failed: {}", e),
    }
}
