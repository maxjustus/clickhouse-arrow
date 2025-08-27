use tokio::io::AsyncWriteExt;

use super::{ClickHouseNativeSerializer, Serializer, SerializerState, Type};
use crate::io::{ClickHouseBytesWrite, ClickHouseWrite};
use crate::prelude::*;
use crate::{Result, Value};

/// Transform array elements to Variant when type expects Array(Variant(...)) but values are raw
fn wrap_heterogeneous_elements(elements: &[Value], variant_types: &[Type]) -> Result<Vec<Value>> {
    // Create discriminator mapping from variant types (sorted alphabetically)
    let mut type_map = std::collections::HashMap::new();
    for (idx, typ) in variant_types.iter().enumerate() {
        let _ = type_map.insert(typ.to_string(), idx);
    }

    let mut wrapped = Vec::with_capacity(elements.len());
    for element in elements {
        if matches!(element, Value::Null) {
            // Handle NULL case - use max discriminator without wrapping in Nullable
            wrapped.push(Value::Variant(0xFF, Box::new(Value::Null)));
        } else {
            let elem_type = element.guess_type();
            let type_string = elem_type.to_string();

            if let Some(&discriminator) = type_map.get(&type_string) {
                // Wrap in Variant with correct discriminator
                let disc_u8 = u8::try_from(discriminator).map_err(|_| {
                    Error::SerializeError(format!("Discriminator {discriminator} too large for u8"))
                })?;
                wrapped.push(Value::Variant(disc_u8, Box::new(element.clone())));
            } else {
                return Err(Error::SerializeError(format!(
                    "Element type {type_string} not found in Variant types {variant_types:?}"
                )));
            }
        }
    }

    Ok(wrapped)
}

/// Check if we need to transform values for type mismatch
fn needs_heterogeneous_transformation(array_type: &Type, values: &[Value]) -> Option<Vec<Type>> {
    if let Type::Variant(variant_types) = array_type {
        // Check if any values are not already Variant
        if values.iter().any(|v| !matches!(v, Value::Variant(_, _) | Value::Null)) {
            return Some(variant_types.clone());
        }
    }
    None
}

// Trait to allow serializing [Values] wrapping an array of items.
pub(crate) trait ArraySerializerGeneric {
    fn inner_type(type_: &Type) -> Result<&Type>;
    fn value_len(value: &Value) -> Result<usize>;
    fn values(value: Value) -> Result<Vec<Value>>;
}

pub(crate) struct ArraySerializer;
impl ArraySerializerGeneric for ArraySerializer {
    fn value_len(value: &Value) -> Result<usize> { value.unwrap_array_ref().map(<[Value]>::len) }

    fn inner_type(type_: &Type) -> Result<&Type> { type_.unwrap_array() }

    fn values(value: Value) -> Result<Vec<Value>> { value.unwrap_array() }
}

impl<T: ArraySerializerGeneric + 'static> Serializer for T {
    async fn write_prefix<W: ClickHouseWrite>(
        type_: &Type,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        T::inner_type(type_)?.serialize_prefix_async(writer, state).await
    }

    async fn write<W: ClickHouseWrite>(
        type_: &Type,
        values: Vec<Value>,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        let type_ = T::inner_type(type_)?;

        let mut offset = 0usize;
        for value in &values {
            offset += Self::value_len(value)?;
            writer.write_u64_le(offset as u64).await?;
        }
        let mut all_values: Vec<Value> = Vec::with_capacity(offset);
        for value in values {
            all_values.append(&mut Self::values(value)?);
        }

        // Check if we need to transform values for heterogeneous arrays
        let final_values =
            if let Some(variant_types) = needs_heterogeneous_transformation(type_, &all_values) {
                wrap_heterogeneous_elements(&all_values, &variant_types)?
            } else {
                all_values
            };

        match type_.serialize_column(final_values, writer, state).await {
            Ok(()) => {}
            Err(e) => {
                error!("error serializing column in array type={type_:?}: {}", e);
                return Err(e);
            }
        }

        Ok(())
    }

    fn write_sync(
        type_: &Type,
        values: Vec<Value>,
        writer: &mut impl ClickHouseBytesWrite,
        state: &mut SerializerState,
    ) -> Result<()> {
        let type_ = T::inner_type(type_)?;

        let mut offset = 0usize;
        for value in &values {
            offset += Self::value_len(value)?;
            writer.put_u64_le(offset as u64);
        }
        let mut all_values: Vec<Value> = Vec::with_capacity(offset);
        for value in values {
            all_values.append(&mut Self::values(value)?);
        }

        // Check if we need to transform values for heterogeneous arrays
        let final_values =
            if let Some(variant_types) = needs_heterogeneous_transformation(type_, &all_values) {
                wrap_heterogeneous_elements(&all_values, &variant_types)?
            } else {
                all_values
            };

        match type_.serialize_column_sync(final_values, writer, state) {
            Ok(()) => {}
            Err(e) => {
                error!("error serializing column in array type={type_:?}: {}", e);
                return Err(e);
            }
        }

        Ok(())
    }
}
