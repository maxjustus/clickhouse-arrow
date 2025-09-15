use super::{ClickHouseNativeSerializer, Serializer, SerializerState, Type};
use crate::io::ClickHouseWrite;
use crate::{Error, Result, Value};

pub(crate) struct TupleSerializer;

impl Serializer for TupleSerializer {
    async fn write_prefix<W: ClickHouseWrite>(
        type_: &Type,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        match type_ {
            Type::Tuple(inner) => {
                for item in inner {
                    item.serialize_prefix_async(writer, state).await?;
                }
            }
            Type::TupleNamed(fields) => {
                for (_, item) in fields {
                    item.serialize_prefix_async(writer, state).await?;
                }
            }
            _ => {
                return Err(Error::SerializeError(format!(
                    "TupleSerializer called with non-tuple type: {type_:?}"
                )));
            }
        }
        Ok(())
    }

    async fn write<W: ClickHouseWrite>(
        type_: &Type,
        values: Vec<Value>,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        let (mut columns, inner_types): (Vec<Vec<Value>>, Vec<&Type>) = match type_ {
            Type::Tuple(inner) => (
                vec![Vec::with_capacity(values.len()); inner.len()],
                inner.iter().collect(),
            ),
            Type::TupleNamed(fields) => (
                vec![Vec::with_capacity(values.len()); fields.len()],
                fields.iter().map(|(_, t)| t).collect(),
            ),
            _ => {
                return Err(Error::SerializeError(
                    "TupleSerializer called with non-tuple type".to_string(),
                ));
            }
        };

        for value in values {
            let tuple = value.unwrap_tuple()?;
            for (i, value) in tuple.into_iter().enumerate() {
                columns[i].push(value);
            }
        }
        for (inner_type, column) in inner_types.iter().zip(columns.into_iter()) {
            inner_type.serialize_column(column, writer, state).await?;
        }
        Ok(())
    }

}
