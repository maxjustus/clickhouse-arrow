use super::{DeserializerState, Type};
use crate::Result;
#[cfg(test)]
use crate::io::ClickHouseRead;
#[cfg(test)]
use crate::native::sync::AsyncParseAdapter;
use crate::native::sync::{ParseStatus, SyncReader, parse_var_uint};
use crate::native::values::Value;

const END_OF_GRANULE_FLAG: u64 = 1u64 << 62;

pub(crate) fn parse_sparse_with_path(
    type_: &Type,
    reader: &mut SyncReader<'_>,
    rows: usize,
    state: &mut DeserializerState,
    path: &mut Vec<u16>,
) -> Result<ParseStatus<Vec<Value>>> {
    // Parse offsets
    let mut indices: Vec<usize> = Vec::new();
    let mut total_rows: usize;
    let mut tmp_offset: usize = 0;
    let mut skipped_values_rows: usize = 0;
    let mut first = true;

    let key = path.clone();
    let (mut trailing_defaults, mut has_value_after_defaults) =
        state.sparse_runtime.get(&key).copied().unwrap_or((0, false));

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
        trailing_defaults = 0;
        total_rows += 1;
    }

    loop {
        let v = match parse_var_uint(reader.remaining())? {
            ParseStatus::Complete { value, consumed } => {
                reader.advance(consumed)?;
                value.0
            }
            ParseStatus::NeedMore { needed } => return Ok(ParseStatus::NeedMore { needed }),
        };
        let end = (v & END_OF_GRANULE_FLAG) != 0;
        let mut v = v;
        if end {
            v &= !END_OF_GRANULE_FLAG;
        }
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
            let start_of_group =
                if !first && !indices.is_empty() { indices[indices.len() - 1] + 1 } else { 0 };
            if group_size >= tmp_offset {
                indices.push(start_of_group + group_size - tmp_offset);
                tmp_offset = 0;
                first = false;
            } else {
                skipped_values_rows += 1;
                tmp_offset = tmp_offset.saturating_sub(group_size + 1);
            }
            trailing_defaults = 0;
            next_total_rows += 1;
        }
        total_rows = next_total_rows;
    }

    let _ = state.sparse_runtime.insert(key.clone(), (trailing_defaults, has_value_after_defaults));

    if skipped_values_rows > 0 {
        match parse_dense_for_sparse(type_, skipped_values_rows, state, path, reader)? {
            ParseStatus::Complete { .. } => {}
            ParseStatus::NeedMore { needed } => return Ok(ParseStatus::NeedMore { needed }),
        }
    }
    let values = if !indices.is_empty() {
        match parse_dense_for_sparse(type_, indices.len(), state, path, reader)? {
            ParseStatus::Complete { value, .. } => value,
            ParseStatus::NeedMore { needed } => return Ok(ParseStatus::NeedMore { needed }),
        }
    } else {
        Vec::new()
    };

    let _ = state.sparse_runtime.insert(key, (trailing_defaults, has_value_after_defaults));

    let mut out = vec![type_.default_value(); rows];
    for (i, v) in indices.into_iter().zip(values.into_iter()) {
        if i < rows {
            out[i] = v;
        }
    }
    Ok(ParseStatus::Complete { value: out, consumed: reader.consumed() })
}

#[cfg(test)]
pub(crate) async fn read_sparse_with_path<R>(
    type_: &Type,
    reader: &mut R,
    rows: usize,
    state: &mut DeserializerState,
    path: &mut Vec<u16>,
) -> Result<Vec<Value>>
where
    R: ClickHouseRead + crate::native::sync::ReadAheadBuffer,
{
    let mut scratch = std::mem::take(&mut state.sync_buffer);
    let read_ahead = state.sync_read_ahead_bytes;
    let mut adapter = AsyncParseAdapter::new(reader, &mut scratch, read_ahead);
    let out = adapter
        .parse(|buf| {
            let mut sr = SyncReader::new(buf);
            parse_sparse_with_path(type_, &mut sr, rows, state, path)
        })
        .await;
    state.sync_buffer = scratch;
    out
}

fn parse_dense_for_sparse(
    type_: &Type,
    rows: usize,
    state: &mut DeserializerState,
    path: &mut Vec<u16>,
    reader: &mut SyncReader<'_>,
) -> Result<ParseStatus<Vec<Value>>> {
    // Temporarily remove sparse marking for this path so dense parse doesn't recurse into sparse.
    let removed = state.kind_plan.as_mut().and_then(|plan| plan.remove(path));
    let res = type_.parse_column_sync_with_path(rows, state, path, reader);
    if let Some(plan) = state.kind_plan.as_mut() {
        if let Some(val) = removed {
            let _ = plan.insert(path.clone(), val);
        }
    }
    res
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use tokio::io::{AsyncRead, ReadBuf};

    use super::*;
    use crate::native::sync::ReadAheadReader;

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
            if value > 0 {
                byte |= 0x80;
            }
            tmp[pos] = byte;
            pos += 1;
            if value == 0 {
                break;
            }
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

        let mut reader = ReadAheadReader::new(BytesReader(bytes.freeze()));
        let ty = Type::String;
        let mut state = DeserializerState::default();
        // No plan needed when calling read_sparse_async directly; runtime state starts empty

        let mut path = Vec::new();
        let out = read_sparse_with_path(&ty, &mut reader, 10, &mut state, &mut path).await.unwrap();
        assert_eq!(out.len(), 10);
        let get_str = |v: &Value| match v {
            Value::String(s) => String::from_utf8_lossy(s).to_string(),
            _ => panic!("expected String"),
        };
        assert_eq!(get_str(&out[0]), "");
        assert_eq!(get_str(&out[1]), "a");
        assert_eq!(get_str(&out[5]), "bbb");
        assert_eq!(get_str(&out[9]), "");
    }
}
