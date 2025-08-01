/// Serialization logic for `ClickHouse` `Array` types from Arrow list arrays.
///
/// This module provides functions to serialize Arrow `ListArray`, `ListViewArray`,
/// `LargeListArray`, `LargeListViewArray`, and `FixedSizeListArray` into `ClickHouse`'s native
/// format for the `Array` type. It is used by the `ClickHouseArrowSerializer` implementation
/// in `types.rs` to handle nested data structures.
///
/// The main `serialize` function handles four cases:
/// - `ListArray`: Writes variable-length offsets and serializes inner values.
/// - `ListViewArray`: Writes variable-length offsets and serializes inner values.
/// - `LargeListArray`: Writes variable-length offsets and serializes inner values.
/// - `LargeListViewArray`: Writes variable-length offsets and serializes inner values.
/// - `FixedSizeListArray`: Writes computed offsets based on fixed length and serializes inner
///   values.
///
/// # Examples
/// ```rust,ignore
/// use arrow::array::{Int32Array, ListArray};
/// use arrow::buffer::OffsetBuffer;
/// use arrow::datatypes::{ArrayRef, DataType, Field};
/// use clickhouse_arrow::types::{Type, list::serialize, SerializerState};
/// use std::sync::Arc;
/// use tokio::io::AsyncWriteExt;
///
/// let values = Arc::new(Int32Array::from(vec![1, 2, 3, 4])) as ArrayRef;
/// let offsets = OffsetBuffer::new(vec![0, 2, 4].into());
/// let column = Arc::new(ListArray::new(
///     Arc::new(Field::new("item", DataType::Int32, false)),
///     offsets,
///     values,
///     None,
/// )) as ArrayRef;
/// let field = Field::new(
///     "list",
///     DataType::List(Arc::new(Field::new("item", DataType::Int32, false))),
///     false,
/// );
/// let mut buffer = Vec::new();
/// let mut state = SerializerState::default();
/// serialize(&Type::Int32, &field, &column, &mut buffer, &mut state)
///     .await
///     .unwrap();
/// ```
use arrow::array::*;
use arrow::datatypes::DataType;
use tokio::io::AsyncWriteExt;

use super::ClickHouseArrowSerializer;
use crate::formats::SerializerState;
use crate::io::{ClickHouseBytesWrite, ClickHouseWrite};
use crate::{Error, Result, Type};

/// Extracts the inner `Field` from a `List`, `ListView`, `LargeList`, `LargeListView`, or
/// `FixedSizeList` data type.
///
/// # Arguments
/// - `field`: The Arrow `Field` to extract from.
///
/// # Returns
/// A `Result` containing the inner `Field` or a `Error` if the data type is not
/// `List`, `ListView`, `LargeList`, `LargeListView`, or `FixedSizeList`.
fn unwrap_array_data_type(dt: &DataType) -> Result<&DataType> {
    match dt {
        DataType::List(f)
        | DataType::ListView(f)
        | DataType::LargeList(f)
        | DataType::LargeListView(f)
        | DataType::FixedSizeList(f, _) => Ok(f.data_type()),
        _ => Err(Error::ArrowSerialize(format!("Expected List or FixedSizeList, got {dt:?}"))),
    }
}

/// Serializes an Arrow `ListArray`, `ListViewArray`, `LargeListArray`, `LargeListViewArray`, or
/// `FixedSizeListArray` to `ClickHouse`'s native format for `Array` types.
///
/// Writes offsets (variable-length for `ListArray`, computed for `FixedSizeListArray`) followed by
/// serialized inner values. The inner values are serialized using the provided `inner_type` and
/// `inner_field`.
///
/// # Arguments
/// - `type_hint`: The `ClickHouse` `Type` of the array.
/// - `field`: The Arrow `Field` describing the list's metadata.
/// - `values`: The `ListArray` or `FixedSizeListArray` containing the data.
/// - `writer`: The async writer to serialize to (e.g., a TCP stream).
/// - `state`: A mutable `SerializerState` for serialization context.
///
/// # Returns
/// A `Result` indicating success or a `Error` if serialization fails.
///
/// # Errors
/// - Returns `ArrowSerialize` if the `values` is not a `ListArray`, `ListViewArray`,
///   `LargeListArray`, `LargeListViewArray`, or `FixedSizeListArray`, or the field's data type is
///   invalid.
/// - Returns an error is the type is not an `Array`
/// - Returns `Io` if writing to the writer fails.
pub(super) async fn serialize_async<W: ClickHouseWrite>(
    type_hint: &Type,
    writer: &mut W,
    values: &ArrayRef,
    data_type: &DataType,
    state: &mut SerializerState,
) -> Result<()> {
    // Unwrap the inner type
    let inner_type = type_hint.strip_null().unwrap_array()?;

    macro_rules! write_list_array {
        ($( $array_ty:ty ),* $(,)?) => {{
            $(
            if let Some(array) = values.as_any().downcast_ref::<$array_ty>() {
                let inner_dt = unwrap_array_data_type(data_type)?;
                let offsets = array.value_offsets();
                let values = array.values();

                // Note: ClickHouse server accepts offsets starting from the second value
                // (e.g., [2, 3, 5] for [0, 2, 3, 5]), inferring the first 0 based on row count.
                //
                // Including the first offset breaks functionality.
                for offset in &offsets[1..] {
                    #[expect(clippy::cast_sign_loss)]
                    writer.write_u64_le(*offset as u64).await?;
                }

                // Write inner values
                inner_type.serialize_async(writer, values, inner_dt, state).await?;
                return Ok(());
            }
            )*
        }}
    }

    // ListArray, ListViewArray, LargeListArray, LargeListViewArray
    write_list_array!(ListArray, ListViewArray, LargeListArray, LargeListViewArray);

    // FixedSizeListArray
    if let Some(array) = values.as_any().downcast_ref::<FixedSizeListArray>() {
        let inner_dt = unwrap_array_data_type(data_type)?;

        #[expect(clippy::cast_sign_loss)]
        let value_len = array.value_length() as usize;
        let num_rows = array.len();

        // Note: ClickHouse server accepts offsets starting from the second value
        // (e.g., [2, 3, 5] for [0, 2, 3, 5]), inferring the first 0 based on row count.
        //
        // Including the first offset breaks functionality.
        for i in 1..=num_rows {
            writer.write_u64_le((value_len * i) as u64).await?;
        }

        // Write inner values
        let values = array.values();
        inner_type.serialize_async(writer, values, inner_dt, state).await?;
        return Ok(());
    }

    Err(Error::ArrowSerialize(format!(
        "Expected ListArray or FixedSizeListArray: type={inner_type:?}, data_type={data_type:?}"
    )))
}

pub(super) fn serialize<W: ClickHouseBytesWrite>(
    type_hint: &Type,
    writer: &mut W,
    values: &ArrayRef,
    data_type: &DataType,
    state: &mut SerializerState,
) -> Result<()> {
    // Unwrap the inner type
    let inner_type = type_hint.strip_null().unwrap_array()?;

    macro_rules! put_list_array {
        ($( $array_ty:ty ),* $(,)?) => {{
            $(
            if let Some(array) = values.as_any().downcast_ref::<$array_ty>() {
                // TODO: Should this fallback to a best effort conversion of type_hint -> arrow?
                let inner_dt = unwrap_array_data_type(data_type)?;

                let offsets = array.value_offsets();
                let values = array.values();

                // Note: ClickHouse server accepts offsets starting from the second value
                // (e.g., [2, 3, 5] for [0, 2, 3, 5]), inferring the first 0 based on row count.
                //
                // Including the first offset breaks functionality.
                for offset in &offsets[1..] {
                    #[expect(clippy::cast_sign_loss)]
                    writer.put_u64_le(*offset as u64);
                }

                // Write inner values
                inner_type.serialize(writer, values, inner_dt, state)?;
                return Ok(());
            }
            )*
        }}
    }

    // ListArray, ListViewArray, LargeListArray, LargeListViewArray
    put_list_array!(ListArray, ListViewArray, LargeListArray, LargeListViewArray);

    // FixedSizeListArray
    if let Some(array) = values.as_any().downcast_ref::<FixedSizeListArray>() {
        let inner_dt = unwrap_array_data_type(data_type)?;

        #[expect(clippy::cast_sign_loss)]
        let value_len = array.value_length() as usize;
        let num_rows = array.len();

        // Note: ClickHouse server accepts offsets starting from the second value
        // (e.g., [2, 3, 5] for [0, 2, 3, 5]), inferring the first 0 based on row count.
        //
        // Including the first offset breaks functionality.
        for i in 1..=num_rows {
            writer.put_u64_le((value_len * i) as u64);
        }

        // Write inner values
        let values = array.values();
        inner_type.serialize(writer, values, inner_dt, state)?;
        return Ok(());
    }

    Err(Error::ArrowSerialize(format!(
        "Expected ListArray or FixedSizeListArray: type={inner_type:?}, data_type={data_type:?}"
    )))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::*;
    use arrow::buffer::OffsetBuffer;
    use arrow::datatypes::*;

    use super::*;
    use crate::ArrowOptions;
    use crate::arrow::types::LIST_ITEM_FIELD_NAME;
    use crate::formats::SerializerState;
    use crate::native::types::Type;

    type MockWriter = Vec<u8>;

    fn wrap_array(typ: Type) -> Type { Type::Array(Box::new(typ)) }

    macro_rules! list_test {
        ($name:ident, $type_:expr, $field:expr, $array:expr, $expected:expr) => {
            #[tokio::test]
            async fn $name() {
                println!("Testing scenario: {}", stringify!($name));
                let type_ = $type_;
                let field = $field;
                let array = Arc::new($array) as ArrayRef;
                let expected = $expected;

                // Test async
                let mut async_writer = MockWriter::new();
                let mut async_state = SerializerState::default()
                    .with_arrow_options(ArrowOptions::default().with_strings_as_strings(true));
                serialize_async(
                    &type_,
                    &mut async_writer,
                    &array,
                    field.data_type(),
                    &mut async_state,
                )
                .await
                .unwrap();
                assert_eq!(*async_writer, expected);

                // Test sync
                let mut sync_writer = MockWriter::new();
                let mut sync_state = SerializerState::default()
                    .with_arrow_options(ArrowOptions::default().with_strings_as_strings(true));
                serialize(&type_, &mut sync_writer, &array, field.data_type(), &mut sync_state)
                    .unwrap();
                assert_eq!(*sync_writer, expected);
            }
        };
    }

    list_test!(
        test_serialize_list_int32,
        wrap_array(Type::Int32),
        {
            let inner_field = Arc::new(Field::new(LIST_ITEM_FIELD_NAME, DataType::Int32, false));
            Arc::new(Field::new("list", DataType::List(Arc::clone(&inner_field)), false))
        },
        ListArray::new(
            Arc::new(Field::new(LIST_ITEM_FIELD_NAME, DataType::Int32, false)),
            OffsetBuffer::new(vec![0, 2, 3, 5].into()),
            Arc::new(Int32Array::from(vec![1, 2, 3, 4, 5])) as ArrayRef,
            None,
        ),
        vec![
            // Offsets: [2, 3, 5] (u64, little-endian)
            2, 0, 0, 0, 0, 0, 0, 0, // 2
            3, 0, 0, 0, 0, 0, 0, 0, // 3
            5, 0, 0, 0, 0, 0, 0, 0, // 5
            // Values: [1, 2, 3, 4, 5] (i32, little-endian)
            1, 0, 0, 0, // 1
            2, 0, 0, 0, // 2
            3, 0, 0, 0, // 3
            4, 0, 0, 0, // 4
            5, 0, 0, 0, // 5
        ]
    );

    list_test!(
        test_serialize_list_nullable_int32,
        wrap_array(Type::Nullable(Box::new(Type::Int32))),
        {
            let inner_field = Arc::new(Field::new(LIST_ITEM_FIELD_NAME, DataType::Int32, true));
            Arc::new(Field::new("list", DataType::List(Arc::clone(&inner_field)), false))
        },
        ListArray::new(
            Arc::new(Field::new(LIST_ITEM_FIELD_NAME, DataType::Int32, true)),
            OffsetBuffer::new(vec![0, 2, 3, 5].into()),
            Arc::new(Int32Array::from(vec![Some(1), None, Some(3), None, Some(5)])) as ArrayRef,
            None,
        ),
        vec![
            // Offsets: [2, 3, 5] (u64, little-endian)
            2, 0, 0, 0, 0, 0, 0, 0, // 2
            3, 0, 0, 0, 0, 0, 0, 0, // 3
            5, 0, 0, 0, 0, 0, 0, 0, // 5
            // Null mask for values: [0, 1, 0, 1, 0] (5 bytes, 0=not null, 1=null)
            0, 1, 0, 1, 0, // Values: [1, 0, 3, 0, 5] (i32, little-endian, nulls as 0)
            1, 0, 0, 0, // 1
            0, 0, 0, 0, // 0 (null)
            3, 0, 0, 0, // 3
            0, 0, 0, 0, // 0 (null)
            5, 0, 0, 0, // 5
        ]
    );

    list_test!(
        test_serialize_list_nullable_string,
        wrap_array(Type::Nullable(Box::new(Type::String))),
        {
            let inner_field = Arc::new(Field::new(LIST_ITEM_FIELD_NAME, DataType::Utf8, true));
            Arc::new(Field::new("list", DataType::List(Arc::clone(&inner_field)), false))
        },
        ListArray::new(
            Arc::new(Field::new(LIST_ITEM_FIELD_NAME, DataType::Utf8, true)),
            OffsetBuffer::new(vec![0, 2, 3, 4].into()),
            Arc::new(StringArray::from(vec![Some("even"), Some("odd"), None, Some("odd")]))
                as ArrayRef,
            None,
        ),
        vec![
            // Offsets: [2, 3, 4] (u64, little-endian)
            2, 0, 0, 0, 0, 0, 0, 0, // 2
            3, 0, 0, 0, 0, 0, 0, 0, // 3
            4, 0, 0, 0, 0, 0, 0, 0, // 4
            // Null mask: [0, 0, 1, 0]
            0, 0, 1, 0,
            // Non-null values: ["even", "odd", "odd"] (var_uint length + string bytes)
            4, // var_uint length: 4 (1 byte)
            b'e', b'v', b'e', b'n', // "even" (4 bytes)
            3,    // var_uint length: 3 (1 byte)
            b'o', b'd', b'd', // "odd" (3 bytes)
            0,    // var_uint length: 0 (1 byte, null as empty string)
            3,    // var_uint length: 3 (1 byte)
            b'o', b'd', b'd', // "odd" (3 bytes)
        ]
    );

    list_test!(
        test_serialize_fixed_size_list_int32,
        wrap_array(Type::Int32),
        {
            let inner_field = Arc::new(Field::new(LIST_ITEM_FIELD_NAME, DataType::Int32, false));
            Arc::new(Field::new(
                "list",
                DataType::FixedSizeList(Arc::clone(&inner_field), 2),
                false,
            ))
        },
        FixedSizeListArray::new(
            Arc::new(Field::new(LIST_ITEM_FIELD_NAME, DataType::Int32, false)),
            2,
            Arc::new(Int32Array::from(vec![1, 2, 3, 4, 5, 6])) as ArrayRef,
            None,
        ),
        vec![
            // Offsets: [2, 4, 6] (u64, little-endian)
            2, 0, 0, 0, 0, 0, 0, 0, // 2
            4, 0, 0, 0, 0, 0, 0, 0, // 4
            6, 0, 0, 0, 0, 0, 0, 0, // 6
            // Values: [1, 2, 3, 4, 5, 6] (i32, little-endian)
            1, 0, 0, 0, // 1
            2, 0, 0, 0, // 2
            3, 0, 0, 0, // 3
            4, 0, 0, 0, // 4
            5, 0, 0, 0, // 5
            6, 0, 0, 0, // 6
        ]
    );

    list_test!(
        test_serialize_list_zero_rows,
        wrap_array(Type::Int32),
        {
            let inner_field = Arc::new(Field::new(LIST_ITEM_FIELD_NAME, DataType::Int32, false));
            Arc::new(Field::new("list", DataType::List(Arc::clone(&inner_field)), false))
        },
        ListArray::new(
            Arc::new(Field::new(LIST_ITEM_FIELD_NAME, DataType::Int32, false)),
            OffsetBuffer::new(vec![0].into()),
            Arc::new(Int32Array::from(Vec::<i32>::new())) as ArrayRef,
            None,
        ),
        Vec::<u8>::new()
    );

    list_test!(
        test_serialize_list_empty_inner,
        wrap_array(Type::Int32),
        {
            let inner_field = Arc::new(Field::new(LIST_ITEM_FIELD_NAME, DataType::Int32, false));
            Arc::new(Field::new("list", DataType::List(Arc::clone(&inner_field)), false))
        },
        ListArray::new(
            Arc::new(Field::new(LIST_ITEM_FIELD_NAME, DataType::Int32, false)),
            OffsetBuffer::new(vec![0, 0, 0].into()),
            Arc::new(Int32Array::from(Vec::<i32>::new())) as ArrayRef,
            None,
        ),
        vec![
            // Offsets: [0, 0] (u64, little-endian)
            0, 0, 0, 0, 0, 0, 0, 0, // 0
            0, 0, 0, 0, 0, 0, 0, 0, /* 0
                * Values: [] (empty) */
        ]
    );

    list_test!(
        test_serialize_nested_list_int32,
        wrap_array(Type::Array(Box::new(Type::Int32))),
        {
            let inner_field = Arc::new(Field::new(LIST_ITEM_FIELD_NAME, DataType::Int32, false));
            let nested_field = Arc::new(Field::new(
                LIST_ITEM_FIELD_NAME,
                DataType::List(Arc::clone(&inner_field)),
                false,
            ));
            Arc::new(Field::new("list", DataType::List(Arc::clone(&nested_field)), false))
        },
        {
            // Inner list: [[1, 2], [3]]
            let inner_values = Arc::new(Int32Array::from(vec![1, 2, 3])) as ArrayRef;
            let inner_offsets = OffsetBuffer::new(vec![0, 2, 3].into());
            let inner_field = Arc::new(Field::new(LIST_ITEM_FIELD_NAME, DataType::Int32, false));
            let inner_list =
                Arc::new(ListArray::new(inner_field, inner_offsets, inner_values, None))
                    as ArrayRef;

            // Outer list: [[[1, 2], [3]]]
            let outer_offsets = OffsetBuffer::new(vec![0, 2].into());
            let nested_field = Arc::new(Field::new(
                LIST_ITEM_FIELD_NAME,
                DataType::List(Arc::new(Field::new(LIST_ITEM_FIELD_NAME, DataType::Int32, false))),
                false,
            ));
            ListArray::new(nested_field, outer_offsets, inner_list, None)
        },
        vec![
            // Outer offsets: [2] (u64, little-endian)
            2, 0, 0, 0, 0, 0, 0, 0, // 2
            // Inner offsets: [2, 3] (u64, little-endian)
            2, 0, 0, 0, 0, 0, 0, 0, // 2
            3, 0, 0, 0, 0, 0, 0, 0, // 3
            // Values: [1, 2, 3] (i32, little-endian)
            1, 0, 0, 0, // 1
            2, 0, 0, 0, // 2
            3, 0, 0, 0, // 3
        ]
    );
}
