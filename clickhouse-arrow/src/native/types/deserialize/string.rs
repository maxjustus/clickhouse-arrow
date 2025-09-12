use tokio::io::AsyncReadExt;
use std::future::Future;

use super::{Deserializer, DeserializerState, Type};
use crate::Result;
use crate::io::ClickHouseRead;
use crate::native::values::Value;

pub(crate) struct StringDeserializer;

impl Deserializer for StringDeserializer {
    fn read_prefix<R: ClickHouseRead>(
        _type_: &Type,
        _reader: &mut R,
        _state: &mut DeserializerState,
    ) -> impl Future<Output = Result<()>> {
        async move { Ok(()) }
    }
    
    async fn read<R: ClickHouseRead>(
        type_: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        let mut path = Vec::new();
        read_with_path(type_, reader, rows, state, &mut path).await
    }

    // sync string deserialization removed
}

pub(crate) async fn read_with_path<R: ClickHouseRead>(
    type_: &Type,
    reader: &mut R,
    rows: usize,
    state: &mut DeserializerState,
    path: &mut Vec<u16>,
) -> Result<Vec<Value>> {
    // Decide sparse by plan for current path (non-zero = SPARSE)
    let sparse_enabled = state
        .kind_plan
        .as_ref()
        .and_then(|p| p.get(path))
        .map(|&k| k != 0)
        .unwrap_or(false);

    if sparse_enabled {
        return crate::native::types::deserialize::sparse::read_sparse_with_path(
            type_, reader, rows, state, path,
        )
        .await;
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
