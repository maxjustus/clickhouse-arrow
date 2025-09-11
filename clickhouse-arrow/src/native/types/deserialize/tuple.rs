use tokio::io::AsyncReadExt;

use super::{ClickHouseNativeDeserializer, Deserializer, DeserializerState, Type};
use crate::Result;
use crate::io::ClickHouseRead;
use crate::native::values::Value;

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

        // Per C++ SerializationInfoTuple.cpp, when a tuple has custom serialization, the
        // stream contains a kind for the tuple itself (always DEFAULT), followed by a kind
        // for each element. We must consume one byte for the tuple and one for each element,
        // and crucially, maintain per-element deserializer state so sparse flags and trailing
        // defaults don't leak across elements.

        // Prepare child states to persist element-specific info read during prefix.
        let mut child_states: Vec<DeserializerState> = Vec::with_capacity(inner_types.len());

        // If sparse/custom detected at column level, discard tuple-level kind byte and
        // initialize each child with its own sparse state; otherwise just recurse normally.
        let is_sparse_column = matches!(
            state.type_specific,
            crate::formats::TypeSpecificState::Sparse(_)
        );

        if is_sparse_column {
            // 1. Read and discard tuple's own kind byte.
            let _ = reader.read_u8().await?;

            // 2. For each child, create a fresh sub-state (with Sparse marker) and delegate.
            for item in inner_types {
                let mut sub_state = DeserializerState::default();
                sub_state.type_specific = crate::formats::TypeSpecificState::Sparse(
                    crate::formats::SparseState {
                        has_custom: true,
                        use_custom: None,
                        num_trailing_defaults: 0,
                        has_value_after_defaults: false,
                    },
                );
                item.deserialize_prefix_async(reader, &mut sub_state).await?;
                child_states.push(sub_state);
            }

            // Stash child states in the parent to be used during value reads.
            state.type_specific = crate::formats::TypeSpecificState::Composite(child_states);
        } else {
            // Not sparse: still recurse so children can consume any non-sparse prefixes
            // (e.g. LowCardinality headers). Maintain separate states for symmetry.
            for item in inner_types {
                let mut sub_state = DeserializerState::default();
                item.deserialize_prefix_async(reader, &mut sub_state).await?;
                child_states.push(sub_state);
            }
            state.type_specific = crate::formats::TypeSpecificState::Composite(child_states);
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

        // Retrieve per-element states captured during prefix, or create defaults.
        let mut default_states = Vec::new();
        let child_states: &mut [DeserializerState] = match &mut state.type_specific {
            crate::formats::TypeSpecificState::Composite(states) => states.as_mut_slice(),
            _ => {
                // Fallback: build default sub-states if none were captured.
                default_states = vec![DeserializerState::default(); inner_types.len()];
                default_states.as_mut_slice()
            }
        };

        // Read each element column with its own state.
        for (idx, type_) in inner_types.iter().enumerate() {
            let data = type_
                .deserialize_column(reader, rows, &mut child_states[idx])
                .await?;
            column_data.push(data);
        }

        Ok(build_tuples(rows, inner_types, column_data))
    }
}
