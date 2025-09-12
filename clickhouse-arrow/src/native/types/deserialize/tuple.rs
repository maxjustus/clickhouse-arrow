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
        // Only delegate to children for non-sparse prefixes (LC, Variant, JSON, etc.).
        // Do NOT consume any sparse kind bytes here; they are parsed in block.rs into a plan.
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

        // Read each element column
        for (_idx, type_) in inner_types.iter().enumerate() {
            let data = type_.deserialize_column(reader, rows, state).await?;
            column_data.push(data);
        }

        Ok(build_tuples(rows, inner_types, column_data))
    }
}

pub(crate) async fn read_with_path<R: ClickHouseRead>(
    type_: &Type,
    reader: &mut R,
    rows: usize,
    state: &mut DeserializerState,
    path: &mut Vec<u16>,
) -> Result<Vec<Value>> {
    let inner_types = type_.unwrap_tuple()?;
    let mut column_data = Vec::with_capacity(inner_types.len());

    for (idx, type_) in inner_types.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        path.push(idx as u16);
        let data = type_
            .deserialize_column_with_path(reader, rows, state, path)
            .await?;
        let _ = path.pop();
        column_data.push(data);
    }

    Ok(build_tuples(rows, inner_types, column_data))
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
    async fn tuple_sparse_mixed() {
        // Plan: Tuple(UInt64, UInt64, Uuid)
        // kinds: []=DEFAULT, [0]=SPARSE, [1]=DEFAULT, [2]=SPARSE
        let types = vec![Type::UInt64, Type::UInt64, Type::Uuid];
        let tuple_ty = Type::Tuple(types);
        let rows = 6usize;

        // Build raw bytes for element 0 (UInt64, SPARSE): indices at 1 and 4
        let mut bytes = BytesMut::new();
        // Offsets groups: 1, 2, end-flag 1
        put_var_uint(&mut bytes, 1);
        put_var_uint(&mut bytes, 2);
        put_var_uint(&mut bytes, (1u64 << 62) | 1);
        // values: two u64 (10, 20)
        bytes.extend_from_slice(&10u64.to_le_bytes());
        bytes.extend_from_slice(&20u64.to_le_bytes());

        // Element 1 (UInt64, DEFAULT): 6 values 100..105
        for v in 100u64..106u64 {
            bytes.extend_from_slice(&v.to_le_bytes());
        }

        // Element 2 (Uuid, SPARSE): indices at 0 and 5
        // Offsets groups: 0, 4, end-flag 0
        put_var_uint(&mut bytes, 0);
        put_var_uint(&mut bytes, 4);
        put_var_uint(&mut bytes, (1u64 << 62) | 0);
        // values: two uuids as (u64,u64)
        let u0a: u64 = 0x11111111_22222222;
        let u0b: u64 = 0x33333333_44444444;
        let u1a: u64 = 0xaaaaaaaa_bbbbbbbb;
        let u1b: u64 = 0xcccccccc_dddddddd;
        bytes.extend_from_slice(&u0a.to_le_bytes());
        bytes.extend_from_slice(&u0b.to_le_bytes());
        bytes.extend_from_slice(&u1a.to_le_bytes());
        bytes.extend_from_slice(&u1b.to_le_bytes());

        let mut reader = BytesReader(bytes.freeze());

        // Prepare state with kind plan
        let mut state = DeserializerState::default();
        use crate::native::test_helpers::mk_kind_plan;
        state.kind_plan = Some(mk_kind_plan(&[
            (vec![], 0),          // tuple node default
            (vec![0], 1),         // e0 sparse
            (vec![1], 0),         // e1 default
            (vec![2], 1),         // e2 sparse
        ]));

        // Read tuple column
        let out = tuple_ty
            .deserialize_column(&mut reader, rows, &mut state)
            .await
            .expect("tuple read");

        assert_eq!(out.len(), rows);
        // Validate several rows
        // row 0: e0=default(0), e1=100, e2=uuid0 present
        if let Value::Tuple(v) = &out[0] {
            assert!(matches!(v[0], Value::UInt64(0)));
            assert!(matches!(v[1], Value::UInt64(100)));
            match v[2] {
                Value::Uuid(u) => {
                    assert_eq!(u.as_u128(), ((u128::from(u0a) << 64) | u128::from(u0b)));
                }
                _ => panic!("expected uuid"),
            }
        } else {
            panic!("expected tuple value");
        }

        // row 1: e0=10, e1=101, e2=default (all-zero uuid)
        if let Value::Tuple(v) = &out[1] {
            assert!(matches!(v[0], Value::UInt64(10)));
            assert!(matches!(v[1], Value::UInt64(101)));
            match v[2] {
                Value::Uuid(u) => {
                    assert_eq!(u.as_u128(), 0);
                }
                _ => panic!("expected uuid"),
            }
        }

        // row 4: e0=20, e1=104, e2=default
        if let Value::Tuple(v) = &out[4] {
            assert!(matches!(v[0], Value::UInt64(20)));
            assert!(matches!(v[1], Value::UInt64(104)));
            match v[2] {
                Value::Uuid(u) => {
                    assert_eq!(u.as_u128(), 0);
                }
                _ => panic!("expected uuid"),
            }
        }

        // row 5: e0=default(0), e1=105, e2=uuid1 present
        if let Value::Tuple(v) = &out[5] {
            assert!(matches!(v[0], Value::UInt64(0)));
            assert!(matches!(v[1], Value::UInt64(105)));
            match v[2] {
                Value::Uuid(u) => {
                    assert_eq!(u.as_u128(), ((u128::from(u1a) << 64) | u128::from(u1b)));
                }
                _ => panic!("expected uuid"),
            }
        }
    }

    #[tokio::test]
    async fn nested_tuple_sparse_elements() {
        // Type: Tuple(UInt64, Tuple(UInt64, UUID))
        // Kinds plan:
        //  []      = DEFAULT (tuple root)
        //  [0]     = SPARSE  (outer first element sparse)
        //  [1]     = DEFAULT (inner tuple)
        //  [1,0]   = DEFAULT (inner first element dense)
        //  [1,1]   = SPARSE  (inner second element sparse)
        let inner = Type::Tuple(vec![Type::UInt64, Type::Uuid]);
        let tuple_ty = Type::Tuple(vec![Type::UInt64, inner]);
        let rows = 5usize;

        let mut bytes = BytesMut::new();

        // Element [0] (UInt64, SPARSE): indices at 1 and 3
        // Groups: 1 (defaults before first), 1 (gap), end flag 1 (one trailing default)
        put_var_uint(&mut bytes, 1);
        put_var_uint(&mut bytes, 1);
        put_var_uint(&mut bytes, (1u64 << 62) | 1);
        // values: two u64 (11, 33)
        bytes.extend_from_slice(&11u64.to_le_bytes());
        bytes.extend_from_slice(&33u64.to_le_bytes());

        // Inner tuple [1]
        // Element [1,0] (UInt64, DEFAULT): 5 values
        for v in 100u64..105u64 {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        // Element [1,1] (UUID, SPARSE): indices at 0 and 4
        // Groups: 0, 3, end flag 0
        put_var_uint(&mut bytes, 0);
        put_var_uint(&mut bytes, 3);
        put_var_uint(&mut bytes, (1u64 << 62) | 0);
        // two UUIDs
        let a1: u64 = 0x01020304_05060708;
        let a2: u64 = 0x090a0b0c_0d0e0f10;
        let b1: u64 = 0x11223344_55667788;
        let b2: u64 = 0x99aabbcc_ddeeff00;
        bytes.extend_from_slice(&a1.to_le_bytes());
        bytes.extend_from_slice(&a2.to_le_bytes());
        bytes.extend_from_slice(&b1.to_le_bytes());
        bytes.extend_from_slice(&b2.to_le_bytes());

        let mut reader = BytesReader(bytes.freeze());
        let mut state = DeserializerState::default();
        use crate::native::test_helpers::mk_kind_plan;
        state.kind_plan = Some(mk_kind_plan(&[
            (vec![], 0),      // tuple root
            (vec![0], 1),     // outer first elem sparse
            (vec![1], 0),     // inner tuple default
            (vec![1, 0], 0),  // inner first elem dense
            (vec![1, 1], 1),  // inner second elem sparse
        ]));

        let out = tuple_ty
            .deserialize_column(&mut reader, rows, &mut state)
            .await
            .expect("nested tuple read");

        assert_eq!(out.len(), rows);
        // Row 0: [0]=default(0), inner=(100, uuid a)
        if let Value::Tuple(v) = &out[0] {
            assert!(matches!(v[0], Value::UInt64(0)));
            if let Value::Tuple(iv) = &v[1] {
                assert!(matches!(iv[0], Value::UInt64(100)));
                if let Value::Uuid(u) = iv[1] {
                    assert_eq!(u.as_u128(), ((u128::from(a1) << 64) | u128::from(a2)));
                } else { panic!("expected uuid"); }
            } else { panic!("expected inner tuple"); }
        } else { panic!("expected tuple"); }

        // Row 1: [0]=11 present, inner=(101, default uuid)
        if let Value::Tuple(v) = &out[1] {
            assert!(matches!(v[0], Value::UInt64(11)));
            if let Value::Tuple(iv) = &v[1] {
                assert!(matches!(iv[0], Value::UInt64(101)));
                if let Value::Uuid(u) = iv[1] { assert_eq!(u.as_u128(), 0); } else { panic!("uuid"); }
            } else { panic!("inner"); }
        } else { panic!("tuple"); }

        // Row 3: [0]=33 present, inner=(103, default)
        if let Value::Tuple(v) = &out[3] {
            assert!(matches!(v[0], Value::UInt64(33)));
            if let Value::Tuple(iv) = &v[1] {
                assert!(matches!(iv[0], Value::UInt64(103)));
                if let Value::Uuid(u) = iv[1] { assert_eq!(u.as_u128(), 0); } else { panic!("uuid"); }
            } else { panic!("inner"); }
        } else { panic!("tuple"); }

        // Row 4: [0]=default(0), inner=(104, uuid b)
        if let Value::Tuple(v) = &out[4] {
            assert!(matches!(v[0], Value::UInt64(0)));
            if let Value::Tuple(iv) = &v[1] {
                assert!(matches!(iv[0], Value::UInt64(104)));
                if let Value::Uuid(u) = iv[1] {
                    assert_eq!(u.as_u128(), ((u128::from(b1) << 64) | u128::from(b2)));
                } else { panic!("uuid"); }
            } else { panic!("inner"); }
        } else { panic!("tuple"); }
    }
}
