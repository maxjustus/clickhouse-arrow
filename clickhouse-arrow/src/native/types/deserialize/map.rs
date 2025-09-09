use tokio::io::AsyncReadExt;

use super::{ClickHouseNativeDeserializer, Deserializer, DeserializerState, Type};
use crate::io::{ClickHouseBytesRead, ClickHouseRead};
use crate::native::protocol::MAX_STRING_SIZE;
use crate::native::values::Value;
use crate::{Error, Result};

pub(crate) struct MapDeserializer;

impl Deserializer for MapDeserializer {
    async fn read_prefix<R: ClickHouseRead>(
        type_: &Type,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<()> {
        match type_ {
            Type::Map(key, value) => {
                let nested =
                    Type::Array(Box::new(Type::Tuple(vec![(**key).clone(), (**value).clone()])));
                nested.deserialize_prefix_async(reader, state).await?;
            }
            _ => {
                return Err(Error::DeserializeError(
                    "MapDeserializer called with non-map type".to_string(),
                ));
            }
        }
        Ok(())
    }

    async fn read<R: ClickHouseRead>(
        type_: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        if rows > MAX_STRING_SIZE {
            return Err(Error::Protocol(format!(
                "read_n response size too large for map. {rows} > {MAX_STRING_SIZE}"
            )));
        }
        if rows == 0 {
            return Ok(vec![]);
        }

        let Type::Map(key, value) = type_ else {
            return Err(Error::DeserializeError(
                "MapDeserializer called with non-map type".to_string(),
            ));
        };

        let mut offsets: Vec<u64> = Vec::with_capacity(rows);
        for _ in 0..rows {
            offsets.push(reader.read_u64_le().await?);
        }

        #[expect(clippy::cast_possible_truncation)]
        let total_length = *offsets.last().unwrap() as usize;

        let keys = key.deserialize_column(reader, total_length, state).await?;
        assert_eq!(keys.len(), total_length);
        let values = value.deserialize_column(reader, total_length, state).await?;
        assert_eq!(values.len(), total_length);

        let mut keys = keys.into_iter();
        let mut values = values.into_iter();
        let mut out = Vec::with_capacity(rows);
        let mut last_offset = 0u64;
        for offset in offsets {
            let mut key_out = vec![];
            let mut value_out = vec![];
            while last_offset < offset {
                key_out.push(keys.next().unwrap());
                value_out.push(values.next().unwrap());
                last_offset += 1;
            }
            out.push(Value::Map(key_out, value_out));
        }
        Ok(out)
    }

    fn read_sync(
        type_: &Type,
        reader: &mut impl ClickHouseBytesRead,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        if rows == 0 {
            return Ok(vec![]);
        }

        let Type::Map(key, value) = type_ else {
            return Err(Error::DeserializeError(
                "MapDeserializer called with non-map type".to_string(),
            ));
        };

        // Read offsets (number of key/value pairs per row via cumulative offsets)
        let mut offsets: Vec<u64> = Vec::with_capacity(rows);
        for _ in 0..rows {
            offsets.push(reader.try_get_u64_le()?);
        }
        let total_length = *offsets
            .last()
            .ok_or_else(|| Error::DeserializeError("Missing map offsets".to_string()))? as usize;

        // Read keys and values columns
        let keys = key.deserialize_column_sync(reader, total_length, state)?;
        if keys.len() != total_length {
            return Err(Error::DeserializeError("Map keys length mismatch".to_string()));
        }
        let values = value.deserialize_column_sync(reader, total_length, state)?;
        if values.len() != total_length {
            return Err(Error::DeserializeError("Map values length mismatch".to_string()));
        }

        // Reconstruct per-row maps from flat columns using offsets
        let mut keys = keys.into_iter();
        let mut values = values.into_iter();
        let mut out = Vec::with_capacity(rows);
        let mut last_offset = 0u64;
        for offset in offsets {
            let mut key_out = Vec::new();
            let mut value_out = Vec::new();
            while last_offset < offset {
                key_out.push(keys.next().unwrap_or(Value::Null));
                value_out.push(values.next().unwrap_or(Value::Null));
                last_offset += 1;
            }
            out.push(Value::Map(key_out, value_out));
        }
        Ok(out)
    }
}
