use super::{ClickHouseNativeDeserializer, Deserializer, DeserializerState, Type};
use crate::io::ClickHouseRead;
use crate::native::sync::{ParseStatus, SyncReader, parse_array_offsets};
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
}

fn parse_offsets(rows: usize, reader: &mut SyncReader<'_>) -> Result<ParseStatus<Vec<u64>>> {
    match parse_array_offsets(rows, reader.remaining())? {
        ParseStatus::Complete { value, consumed } => {
            reader.advance(consumed)?;
            Ok(ParseStatus::Complete { value, consumed: reader.consumed() })
        }
        ParseStatus::NeedMore { needed } => Ok(ParseStatus::NeedMore { needed }),
    }
}

pub(crate) fn parse_with_path(
    type_: &Type,
    rows: usize,
    state: &mut DeserializerState,
    path: &mut Vec<u16>,
    reader: &mut SyncReader<'_>,
) -> Result<ParseStatus<Vec<Value>>> {
    if rows == 0 {
        return Ok(ParseStatus::Complete { value: Vec::new(), consumed: reader.consumed() });
    }
    let Type::Map(key, value) = type_ else {
        return Err(Error::DeserializeError(
            "MapDeserializer called with non-map type".to_string(),
        ));
    };

    let offsets = match parse_offsets(rows, reader)? {
        ParseStatus::Complete { value, .. } => value,
        ParseStatus::NeedMore { needed } => return Ok(ParseStatus::NeedMore { needed }),
    };

    #[expect(clippy::cast_possible_truncation)]
    let total_length = *offsets.last().unwrap() as usize;

    // keys path 0
    path.push(0);
    let keys = match key.parse_column_sync_with_path(total_length, state, path, reader)? {
        ParseStatus::Complete { value, .. } => value,
        ParseStatus::NeedMore { needed } => {
            let _ = path.pop();
            return Ok(ParseStatus::NeedMore { needed });
        }
    };
    let _ = path.pop();

    // values path 1
    path.push(1);
    let values = match value.parse_column_sync_with_path(total_length, state, path, reader)? {
        ParseStatus::Complete { value, .. } => value,
        ParseStatus::NeedMore { needed } => {
            let _ = path.pop();
            return Ok(ParseStatus::NeedMore { needed });
        }
    };
    let _ = path.pop();

    let mut keys_iter = keys.into_iter();
    let mut vals_iter = values.into_iter();
    let mut out = Vec::with_capacity(rows);
    let mut last_offset = 0u64;
    for offset in offsets {
        let mut kvec = Vec::new();
        let mut vvec = Vec::new();
        while last_offset < offset {
            kvec.push(keys_iter.next().unwrap());
            vvec.push(vals_iter.next().unwrap());
            last_offset += 1;
        }
        out.push(Value::Map(kvec, vvec));
    }

    Ok(ParseStatus::Complete { value: out, consumed: reader.consumed() })
}
