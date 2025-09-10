// no extra imports

use super::{DeserializerState, Type};
use crate::formats::{SparseState, TypeSpecificState};
use crate::io::ClickHouseRead;
use crate::native::values::Value;
use crate::Result;

const END_OF_GRANULE_FLAG: u64 = 1u64 << 62;

pub(crate) async fn read_sparse_async<R: ClickHouseRead>(
    type_: &Type,
    reader: &mut R,
    rows: usize,
    state: &mut DeserializerState,
) -> Result<Vec<Value>> {
    // Parse offsets
    let mut indices: Vec<usize> = Vec::new();
    let mut total_rows: usize;
    let mut tmp_offset: usize = 0;
    let mut skipped_values_rows: usize = 0;
    let mut first = true;

    let (mut trailing_defaults, mut has_value_after_defaults) = match &state.type_specific {
        TypeSpecificState::Sparse(SparseState { num_trailing_defaults, has_value_after_defaults, .. }) => {
            (*num_trailing_defaults, *has_value_after_defaults)
        }
        _ => (0, false),
    };

    total_rows = trailing_defaults;

    if has_value_after_defaults {
        if trailing_defaults >= tmp_offset {
            let start_of_group = 0;
            indices.push(start_of_group + trailing_defaults - tmp_offset);
            tmp_offset = 0;
            first = false;
        } else {
            skipped_values_rows += 1;
            tmp_offset = tmp_offset.saturating_sub(trailing_defaults + 1);
        }
        has_value_after_defaults = false;
        trailing_defaults = 0;
        total_rows += 1;
    }

    loop {
        let mut v = reader.read_var_uint().await?;
        let end = (v & END_OF_GRANULE_FLAG) != 0;
        if end { v &= !END_OF_GRANULE_FLAG; }
        let mut group_size = v as usize;

        let mut next_total_rows = total_rows + group_size;
        group_size += trailing_defaults;

        if next_total_rows >= rows {
            trailing_defaults = next_total_rows - rows;
            has_value_after_defaults = !end;
            break;
        }

        if end {
            has_value_after_defaults = false;
            trailing_defaults = group_size;
            break;
        } else {
            let start_of_group = if !first && !indices.is_empty() { indices[indices.len()-1] + 1 } else { 0 };
            if group_size >= tmp_offset {
                indices.push(start_of_group + group_size - tmp_offset);
                tmp_offset = 0;
                first = false;
            } else {
                skipped_values_rows += 1;
                tmp_offset = tmp_offset.saturating_sub(group_size + 1);
            }
            trailing_defaults = 0;
            has_value_after_defaults = false;
            next_total_rows += 1;
        }
        total_rows = next_total_rows;
    }

    // Update state for any potential continued reads within this column (rare)
    if let TypeSpecificState::Sparse(s) = &mut state.type_specific {
        s.num_trailing_defaults = trailing_defaults;
        s.has_value_after_defaults = has_value_after_defaults;
    }

    // Temporarily disable sparse while reading elements
    let saved = std::mem::replace(&mut state.type_specific, TypeSpecificState::None);

    // Skip prior values
    if skipped_values_rows > 0 {
        let _ = type_
            .deserialize_column(reader, skipped_values_rows, state)
            .await?;
    }
    // Read current values
    let values = if !indices.is_empty() {
        type_.deserialize_column(reader, indices.len(), state).await?
    } else {
        Vec::new()
    };

    // Restore sparse state with updated fields
    state.type_specific = saved;
    if let TypeSpecificState::Sparse(s) = &mut state.type_specific {
        s.num_trailing_defaults = trailing_defaults;
        s.has_value_after_defaults = has_value_after_defaults;
    }

    // Reconstruct dense
    let mut out = vec![type_.default_value(); rows];
    for (i, v) in indices.into_iter().zip(values.into_iter()) {
        if i < rows { out[i] = v; }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use tokio::io::{AsyncRead, ReadBuf};

    // Minimal AsyncRead over Bytes
    struct BytesReader(bytes::Bytes);
    impl AsyncRead for BytesReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let to_copy = std::cmp::min(buf.remaining(), self.0.len());
            let chunk = self.0.split_to(to_copy);
            buf.put_slice(&chunk);
            std::task::Poll::Ready(Ok(()))
        }
    }

    fn put_var_uint(buf: &mut BytesMut, mut value: u64) {
        let mut tmp = [0u8; 9];
        let mut pos = 0;
        while pos < 9 {
            let mut byte = (value & 0x7F) as u8;
            value >>= 7;
            if value > 0 { byte |= 0x80; }
            tmp[pos] = byte;
            pos += 1;
            if value == 0 { break; }
        }
        buf.extend_from_slice(&tmp[..pos]);
    }

    #[tokio::test]
    async fn test_sparse_string_generic_async() {
        // Build offsets for 10 rows: non-defaults at 1 and 5
        let mut bytes = BytesMut::new();
        put_var_uint(&mut bytes, 1); // defaults before first value
        put_var_uint(&mut bytes, 3); // gap to next value
        put_var_uint(&mut bytes, (1u64 << 62) | 4); // end-of-granule with trailing defaults

        // Elements: two strings "a", "bbb"
        // String layout: varUInt length + bytes
        put_var_uint(&mut bytes, 1);
        bytes.extend_from_slice(b"a");
        put_var_uint(&mut bytes, 3);
        bytes.extend_from_slice(b"bbb");

        let mut reader = BytesReader(bytes.freeze());
        let ty = Type::String;
        let mut state = DeserializerState::default();
        state.type_specific = TypeSpecificState::Sparse(SparseState {
            has_custom: true,
            use_custom: None,
            num_trailing_defaults: 0,
            has_value_after_defaults: false,
        });

        let out = read_sparse_async(&ty, &mut reader, 10, &mut state).await.unwrap();
        assert_eq!(out.len(), 10);
        let get_str = |v: &Value| match v { Value::String(s) => String::from_utf8_lossy(s).to_string(), _ => panic!("expected String") };
        assert_eq!(get_str(&out[0]), "");
        assert_eq!(get_str(&out[1]), "a");
        assert_eq!(get_str(&out[5]), "bbb");
        assert_eq!(get_str(&out[9]), "");
    }
}
