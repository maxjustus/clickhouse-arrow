use clickhouse_arrow::native::types::{Type, Value};
use clickhouse_arrow::native::types::serialize::{ClickHouseNativeSerializer, SerializerState};

fn main() {
    // Test LowCardinality serialization directly
    let lc_type = Type::LowCardinality(Box::new(Type::String));
    
    // Create some test values
    let values = vec![
        Value::String(b"active".to_vec()),
        Value::String(b"inactive".to_vec()),
        Value::String(b"pending".to_vec()),
    ];
    
    // Serialize with our serializer
    let mut buffer = Vec::new();
    let mut state = SerializerState::default();
    
    // First serialize prefix
    println!("Serializing prefix...");
    lc_type.serialize_prefix(&mut buffer, &mut state);
    println!("After prefix: {} bytes written", buffer.len());
    println!("Bytes: {:?}", buffer);
    
    // Then serialize column
    println!("\nSerializing column data...");
    let start = buffer.len();
    lc_type.serialize_column_sync(values.clone(), &mut buffer, &mut state).unwrap();
    println!("Column data: {} bytes written", buffer.len() - start);
    
    println!("\nTotal bytes: {}", buffer.len());
    println!("First 100 bytes: {:02x?}", &buffer[..buffer.len().min(100)]);
    
    // Now let's see what the JSON serializer would do
    println!("\n=== Testing JSON serialization ===");
    
    let json_type = Type::JSON {
        max_dynamic_paths: None,
        max_dynamic_types: None,
        typed_paths: vec![
            ("status".to_string(), Box::new(Type::LowCardinality(Box::new(Type::String)))),
        ],
        skip_paths: vec![],
    };
    
    let json_values = vec![
        Value::String(br#"{"status": "active"}"#.to_vec()),
        Value::String(br#"{"status": "inactive"}"#.to_vec()),
        Value::String(br#"{"status": "pending"}"#.to_vec()),
    ];
    
    let mut json_buffer = Vec::new();
    let mut json_state = SerializerState::default();
    
    // Analyze values first (important for JSON)
    println!("Analyzing JSON values...");
    let analyzed_state = json_type.analyze_values(&json_values).unwrap();
    json_state.type_specific = analyzed_state;
    
    // Serialize prefix
    println!("Serializing JSON prefix...");
    json_type.serialize_prefix(&mut json_buffer, &mut json_state);
    println!("After JSON prefix: {} bytes", json_buffer.len());
    
    // Serialize column
    println!("Serializing JSON column...");
    json_type.serialize_column_sync(json_values, &mut json_buffer, &mut json_state).unwrap();
    println!("Total JSON bytes: {}", json_buffer.len());
    
    // Show first part of the buffer
    println!("\nFirst 200 JSON bytes: {:02x?}", &json_buffer[..json_buffer.len().min(200)]);
}