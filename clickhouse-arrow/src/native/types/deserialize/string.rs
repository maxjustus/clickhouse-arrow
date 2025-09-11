use tokio::io::AsyncReadExt;
use std::future::Future;

use super::{Deserializer, DeserializerState, Type, sparse};
use crate::Result;
use crate::io::ClickHouseRead;
use crate::native::values::Value;
use crate::formats::{TypeSpecificState, SparseState};

pub(crate) struct StringDeserializer;

impl Deserializer for StringDeserializer {
    fn read_prefix<R: ClickHouseRead>(
        _type_: &Type,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> impl Future<Output = Result<()>> {
        async move {
            // Check if sparse/custom serialization is indicated for this column
            if let TypeSpecificState::Sparse(SparseState { has_custom: true, use_custom, .. }) =
                &mut state.type_specific
            {
                // Only read the toggle if it hasn't been provided at the column-level
                if use_custom.is_none() {
                    let toggle = reader.read_u8().await?;
                    let use_flag = toggle != 0;
                    *use_custom = Some(use_flag);
                    tracing::debug!(toggle, use_custom = use_flag, ty = ?_type_, "string sparse prefix toggle (async)");
                } else {
                    tracing::trace!(ty = ?_type_, "string sparse toggle provided at column-level; skipping read");
                }
            }
            Ok(())
        }
    }
    
    async fn read<R: ClickHouseRead>(
        type_: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        // Check if sparse is enabled for this column
        let sparse_enabled = matches!(
            state.type_specific,
            TypeSpecificState::Sparse(SparseState { has_custom: true, use_custom: Some(true), .. })
        );
        
        if sparse_enabled {
            // Use the general sparse reader for String columns
            return sparse::read_sparse_async(type_, reader, rows, state).await;
        }
        match type_ {
            Type::String | Type::Binary => {
                let mut out = Vec::with_capacity(rows);
                for _ in 0..rows {
                    out.push(Value::String(reader.read_string().await?));
                }
                Ok(out)
            }
            Type::FixedSizedString(n) | Type::FixedSizedBinary(n) => {
                let mut out = Vec::with_capacity(rows);
                #[expect(clippy::uninit_vec)]
                for _ in 0..rows {
                    let mut buf = Vec::with_capacity(*n);
                    unsafe { buf.set_len(*n) };
                    let _ = reader.read_exact(&mut buf[..]).await?;
                    let first_null = buf.iter().position(|x| *x == 0).unwrap_or(buf.len());
                    buf.truncate(first_null);
                    out.push(Value::String(buf));
                }
                Ok(out)
            }
            _ => Err(crate::Error::DeserializeError(
                "StringDeserializer called with non-string type".to_string(),
            )),
        }
    }

    // sync string deserialization removed
}
