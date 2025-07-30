use super::{ClickHouseNativeDeserializer, Deserializer, DeserializerState, Type};
use crate::io::{ClickHouseBytesRead, ClickHouseRead};
use crate::native::values::Value;
use crate::Result;

pub(crate) struct TupleDeserializer;

/// Build tuple values from column data
fn build_tuples(rows: usize, inner_types: &[Type], column_data: Vec<Vec<Value>>) -> Vec<Value> {
    let mut tuples = vec![Value::Tuple(Vec::with_capacity(inner_types.len())); rows];
    
    for column_values in column_data {
        for (i, value) in column_values.into_iter().enumerate() {
            if let Value::Tuple(tuple_values) = &mut tuples[i] {
                tuple_values.push(value);
            }
        }
    }
    
    tuples
}

impl Deserializer for TupleDeserializer {
    async fn read_prefix<R: ClickHouseRead>(
        type_: &Type,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<()> {
        let inner_types = type_.unwrap_tuple()?;
        for item in inner_types {
            item.deserialize_prefix_async(reader, state).await?;
        }
        Ok(())
    }

    async fn read<R: ClickHouseRead>(
        type_: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        let inner_types = type_.unwrap_tuple()?;
        let mut column_data = Vec::with_capacity(inner_types.len());
        
        for type_ in inner_types {
            column_data.push(type_.deserialize_column(reader, rows, state).await?);
        }
        
        Ok(build_tuples(rows, inner_types, column_data))
    }

    fn read_sync(
        type_: &Type,
        reader: &mut impl ClickHouseBytesRead,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        let inner_types = type_.unwrap_tuple()?;
        let mut column_data = Vec::with_capacity(inner_types.len());
        
        for type_ in inner_types {
            column_data.push(type_.deserialize_column_sync(reader, rows, state)?);
        }
        
        Ok(build_tuples(rows, inner_types, column_data))
    }
}
