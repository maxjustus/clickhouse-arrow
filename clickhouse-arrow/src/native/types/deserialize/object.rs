use tokio::io::AsyncReadExt;

use super::{Deserializer, DeserializerState, Type};
use crate::io::ClickHouseRead;
use crate::native::sync::{ParseStatus, SyncReader, parse_string_column};
use crate::native::values::Value;
use crate::{Error, Result};

pub(crate) struct ObjectDeserializer;

#[allow(clippy::uninit_vec)]
impl Deserializer for ObjectDeserializer {
    async fn read_prefix<R: ClickHouseRead>(
        type_: &Type,
        reader: &mut R,
        _state: &mut DeserializerState,
    ) -> Result<()> {
        match type_ {
            Type::Object => {
                let _ = reader.read_i8().await?;
            }
            _ => {
                return Err(Error::DeserializeError(
                    "ObjectDeserializer called with non-json type".to_string(),
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn parse_with_path(
    type_: &Type,
    rows: usize,
    _state: &mut DeserializerState,
    _path: &mut Vec<u16>,
    reader: &mut SyncReader<'_>,
) -> Result<ParseStatus<Vec<Value>>> {
    match type_ {
        Type::Object | Type::String | Type::Binary => {}
        _ => {
            return Err(Error::DeserializeError(
                "ObjectDeserializer called with non-json type".to_string(),
            ));
        }
    }

    match parse_string_column(type_, rows, reader.remaining())? {
        ParseStatus::Complete { value, consumed } => {
            reader.advance(consumed)?;
            Ok(ParseStatus::Complete { value, consumed: reader.consumed() })
        }
        ParseStatus::NeedMore { needed } => Ok(ParseStatus::NeedMore { needed }),
    }
}
