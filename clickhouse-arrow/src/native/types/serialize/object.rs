use tokio::io::AsyncWriteExt;

use super::{Serializer, SerializerState, Type};
use crate::io::ClickHouseWrite;
use crate::{Error, Result, Value};

pub(crate) struct ObjectSerializer;

const JSON_OBJECT_VERSION_STRING: u64 = 1;

impl Serializer for ObjectSerializer {
    async fn write_prefix<W: ClickHouseWrite>(
        _type_: &Type,
        writer: &mut W,
        _state: &mut SerializerState,
    ) -> Result<()> {
        // Emit the STRING serialization version header (v1)
        writer.write_u64_le(JSON_OBJECT_VERSION_STRING).await?;
        Ok(())
    }

    async fn write<W: ClickHouseWrite>(
        type_: &Type,
        values: Vec<Value>,
        writer: &mut W,
        _state: &mut SerializerState,
    ) -> Result<()> {
        for value in values {
            let value = if value == Value::Null { type_.default_value() } else { value };
            match value {
                Value::Object(bytes) => {
                    writer.write_string(bytes).await?;
                }
                #[cfg(feature = "serde")]
                Value::Json(v) => {
                    let bytes =
                        serde_json::to_vec(&v).map_err(|e| Error::SerializeError(e.to_string()))?;
                    writer.write_string(bytes).await?;
                }
                _ => {
                    return Err(Error::SerializeError(format!(
                        "ObjectSerializer unimplemented: {type_:?} for value = {value:?}",
                    )));
                }
            }
        }
        Ok(())
    }
}
