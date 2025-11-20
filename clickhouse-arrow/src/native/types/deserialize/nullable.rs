use super::{ClickHouseNativeDeserializer, Deserializer, DeserializerState, Type};
use crate::io::ClickHouseRead;
use crate::native::sync::{ParseStatus, SyncReader};
use crate::native::values::Value;
use crate::{Error, Result};

pub(crate) struct NullableDeserializer;

impl Deserializer for NullableDeserializer {
    async fn read_prefix<R: ClickHouseRead>(
        type_: &Type,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<()> {
        let inner_type = match type_ {
            Type::Nullable(inner) => &**inner,
            _ => {
                return Err(Error::DeserializeError("Expected Nullable type".to_string()));
            }
        };
        inner_type.deserialize_prefix_async(reader, state).await
    }
}

pub(crate) fn parse_with_path(
    type_: &Type,
    rows: usize,
    state: &mut DeserializerState,
    path: &mut Vec<u16>,
    reader: &mut SyncReader<'_>,
) -> Result<ParseStatus<Vec<Value>>> {
    // mask: if mask[i] == 0, item is present
    let mask = match crate::native::sync::parse_fixed_bytes(rows, reader.remaining())? {
        ParseStatus::Complete { value, consumed } => {
            reader.advance(consumed)?;
            value
        }
        ParseStatus::NeedMore { needed } => return Ok(ParseStatus::NeedMore { needed }),
    };

    path.push(0);
    let mut out = match type_.strip_null().parse_column_sync_with_path(rows, state, path, reader)? {
        ParseStatus::Complete { value, .. } => value,
        ParseStatus::NeedMore { needed } => {
            let _ = path.pop();
            return Ok(ParseStatus::NeedMore { needed });
        }
    };
    let _ = path.pop();

    for (i, mask_byte) in mask.iter().enumerate() {
        if *mask_byte != 0 {
            out[i] = Value::Null;
        }
    }

    Ok(ParseStatus::Complete { value: out, consumed: reader.consumed() })
}
