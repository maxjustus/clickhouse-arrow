use tokio::io::AsyncWriteExt;

use super::{Serializer, SerializerState, Type};
use crate::io::{ClickHouseBytesWrite, ClickHouseWrite};
use crate::{Error, Result, Value};

pub(crate) struct JsonSerializer;

// JSON serialization versions from ClickHouse
const JSON_DEPRECATED_OBJECT_SERIALIZATION_VERSION: u64 = 0;
const JSON_STRING_SERIALIZATION_VERSION: u64 = 1;
const JSON_OBJECT_SERIALIZATION_VERSION: u64 = 3;

impl Serializer for JsonSerializer {
    async fn write_prefix<W: ClickHouseWrite>(
        _type_: &Type,
        writer: &mut W,
        _state: &mut SerializerState,
    ) -> Result<()> {
        // For now, always use string serialization (version 1)
        // This matches what ClickHouse does for simple JSON values
        writer.write_u64_le(JSON_STRING_SERIALIZATION_VERSION).await?;
        Ok(())
    }

    async fn write<W: ClickHouseWrite>(
        type_: &Type,
        values: Vec<Value>,
        writer: &mut W,
        _state: &mut SerializerState,
    ) -> Result<()> {
        // For string serialization, we just write each value as a string
        for value in values {
            let value = if value == Value::Null { type_.default_value() } else { value };
            
            // Convert the value to JSON string representation
            let json_str = match value {
                Value::String(bytes) => {
                    // If it's already a string, check if it's valid JSON or treat as string literal
                    if let Ok(s) = String::from_utf8(bytes.clone()) {
                        // Try to parse as JSON to see if it's already JSON
                        if serde_json::from_str::<serde_json::Value>(&s).is_ok() {
                            bytes // Already JSON
                        } else {
                            // Not JSON, so wrap in quotes
                            serde_json::to_string(&s).unwrap().into_bytes()
                        }
                    } else {
                        // Invalid UTF-8, treat as binary and convert to JSON string
                        serde_json::to_string(&format!("binary:bytes")).unwrap().into_bytes()
                    }
                }
                Value::Int32(i) => i.to_string().into_bytes(),
                Value::Int64(i) => i.to_string().into_bytes(), 
                Value::UInt64(i) => i.to_string().into_bytes(),
                Value::Float64(f) => f.to_string().into_bytes(),
                Value::Null => "null".as_bytes().to_vec(),
                // For more complex types, serialize to JSON
                _ => {
                    return Err(Error::SerializeError(format!(
                        "JsonSerializer unimplemented for value type: {value:?}",
                    )));
                }
            };
            
            writer.write_string(json_str).await?;
        }
        Ok(())
    }

    fn write_sync(
        type_: &Type,
        values: Vec<Value>,
        writer: &mut impl ClickHouseBytesWrite,
        _state: &mut SerializerState,
    ) -> Result<()> {
        // Write values as JSON strings
        for value in values {
            let value = if value == Value::Null { type_.default_value() } else { value };
            
            let json_str = match value {
                Value::String(bytes) => {
                    if let Ok(s) = String::from_utf8(bytes.clone()) {
                        if serde_json::from_str::<serde_json::Value>(&s).is_ok() {
                            bytes // Already JSON
                        } else {
                            serde_json::to_string(&s).unwrap().into_bytes()
                        }
                    } else {
                        serde_json::to_string(&format!("binary:bytes")).unwrap().into_bytes()
                    }
                }
                Value::Int32(i) => i.to_string().into_bytes(),
                Value::Int64(i) => i.to_string().into_bytes(),
                Value::UInt64(i) => i.to_string().into_bytes(), 
                Value::Float64(f) => f.to_string().into_bytes(),
                Value::Null => "null".as_bytes().to_vec(),
                _ => {
                    return Err(Error::SerializeError(format!(
                        "JsonSerializer unimplemented for value type: {value:?}",
                    )));
                }
            };
            
            writer.put_string(json_str)?;
        }
        Ok(())
    }
}