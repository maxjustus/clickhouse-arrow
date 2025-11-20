use super::{Deserializer, DeserializerState, Type};
use crate::Result;
use crate::native::sync::{ParseStatus, SyncReader, parse_sized_column};
use crate::native::values::Value;

pub(crate) struct SizedDeserializer;
impl Deserializer for SizedDeserializer {}

pub(crate) fn parse_with_path(
    type_: &Type,
    rows: usize,
    _state: &mut DeserializerState,
    _path: &mut Vec<u16>,
    reader: &mut SyncReader<'_>,
) -> Result<ParseStatus<Vec<Value>>> {
    match parse_sized_column(type_, rows, reader.remaining())? {
        ParseStatus::Complete { value, consumed } => {
            reader.advance(consumed)?;
            Ok(ParseStatus::Complete { value, consumed: reader.consumed() })
        }
        ParseStatus::NeedMore { needed } => Ok(ParseStatus::NeedMore { needed }),
    }
}
