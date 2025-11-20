use std::future::Future;

use super::{Deserializer, DeserializerState, Type};
use crate::Result;
use crate::io::ClickHouseRead;
use crate::native::sync::{ParseStatus, SyncReader, parse_string_column};
use crate::native::values::Value;

/// Read a single string or binary value from the reader
pub(crate) struct StringDeserializer;

impl Deserializer for StringDeserializer {
    fn read_prefix<R: ClickHouseRead>(
        _type_: &Type,
        _reader: &mut R,
        _state: &mut DeserializerState,
    ) -> impl Future<Output = Result<()>> {
        async move { Ok(()) }
    }
}

pub(crate) fn parse_with_path(
    type_: &Type,
    rows: usize,
    _state: &mut DeserializerState,
    _path: &mut Vec<u16>,
    reader: &mut SyncReader<'_>,
) -> Result<ParseStatus<Vec<Value>>> {
    match parse_string_column(type_, rows, reader.remaining())? {
        ParseStatus::Complete { value, consumed } => {
            reader.advance(consumed)?;
            Ok(ParseStatus::Complete { value, consumed: reader.consumed() })
        }
        ParseStatus::NeedMore { needed } => Ok(ParseStatus::NeedMore { needed }),
    }
}
