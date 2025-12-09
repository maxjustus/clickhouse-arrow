use tokio::io::AsyncWriteExt;

use super::{Serializer, SerializerState, Type};
use crate::io::ClickHouseWrite;
use crate::{Error, Result, Value};

pub(crate) struct StringSerializer;

async fn emit_bytes<W: ClickHouseWrite>(type_: &Type, bytes: &[u8], writer: &mut W) -> Result<()> {
    if let Type::FixedSizedString(s) = type_ {
        if bytes.len() >= *s {
            writer.write_all(&bytes[..*s]).await?;
        } else {
            writer.write_all(bytes).await?;
            let padding = *s - bytes.len();
            for _ in 0..padding {
                writer.write_u8(0).await?;
            }
        }
    } else {
        writer.write_string(bytes).await?;
    }
    Ok(())
}

impl Serializer for StringSerializer {
    async fn write<W: ClickHouseWrite>(
        type_: &Type,
        values: Vec<Value>,
        writer: &mut W,
        _state: &mut SerializerState,
    ) -> Result<()> {
        for value in values {
            let value = if value == Value::Null { type_.default_value() } else { value };
            match value {
                Value::String(bytes) => {
                    emit_bytes(type_, &bytes, writer).await?;
                }
                Value::Array(items) => {
                    let bytes: Vec<u8> = items
                        .into_iter()
                        .map(|x| match x {
                            Value::UInt8(x) => Ok(x),
                            #[expect(clippy::cast_sign_loss)]
                            Value::Int8(x) => Ok(x as u8),
                            _ => Err(Error::SerializeError(format!(
                                "StringSerializer called with non-byte value: {x:?}"
                            ))),
                        })
                        .collect::<Result<Vec<u8>, _>>()?;
                    emit_bytes(type_, &bytes, writer).await?;
                }
                _ => {
                    return Err(Error::SerializeError(format!(
                        "StringSerializer unimplemented: {type_:?} for value = {value:?}",
                    )));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_array_with_non_byte_values_returns_error() {
        let mut buf = Vec::new();
        let mut state = SerializerState::default();

        // Array with a Float64 mixed in - should error, not silently drop
        let values = vec![Value::Array(vec![
            Value::UInt8(65),
            Value::Float64(3.14), // Invalid - not a byte
            Value::UInt8(67),
        ])];

        let result = StringSerializer::write(&Type::String, values, &mut buf, &mut state).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("non-byte value"));
    }

    #[tokio::test]
    async fn test_array_with_valid_bytes_succeeds() {
        let mut buf = Vec::new();
        let mut state = SerializerState::default();

        let values = vec![Value::Array(vec![
            Value::UInt8(65), // 'A'
            Value::UInt8(66), // 'B'
            Value::Int8(67),  // 'C' (as signed)
        ])];

        let result = StringSerializer::write(&Type::String, values, &mut buf, &mut state).await;

        assert!(result.is_ok());
        // String format: varint length + bytes
        assert_eq!(&buf[1..], b"ABC");
    }
}
