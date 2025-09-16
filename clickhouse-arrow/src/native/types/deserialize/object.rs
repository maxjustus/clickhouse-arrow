use super::json::JsonDeserializer;
use super::{Deserializer, DeserializerState, Type};
use crate::io::ClickHouseRead;
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
            Type::Object => JsonDeserializer::read_prefix(type_, reader, _state).await?,
            _ => {
                return Err(Error::DeserializeError(
                    "ObjectDeserializer called with non-json type".to_string(),
                ));
            }
        }
        Ok(())
    }

    async fn read<R: ClickHouseRead>(
        type_: &Type,
        reader: &mut R,
        rows: usize,
        _state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        match type_ {
            Type::Object => JsonDeserializer::read(type_, reader, rows, _state).await,
            Type::String | Type::Binary => {
                let mut out = Vec::with_capacity(rows);
                for _ in 0..rows {
                    out.push(Value::String(reader.read_string().await?));
                }
                Ok(out)
            }
            _ => Err(Error::DeserializeError(
                "ObjectDeserializer called with non-json type".to_string(),
            )),
        }
    }
}
