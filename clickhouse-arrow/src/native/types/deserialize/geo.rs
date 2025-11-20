use super::{ClickHouseNativeDeserializer, Deserializer, DeserializerState, Type};
use crate::io::ClickHouseRead;
use crate::native::sync::{ParseStatus, SyncReader};
use crate::{Point, Result, Value};

pub(crate) struct PointDeserializer;

impl Deserializer for PointDeserializer {
    async fn read_prefix<R: ClickHouseRead>(
        _type_: &Type,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<()> {
        for _ in 0..2 {
            Type::Float64.deserialize_prefix_async(reader, state).await?;
        }
        Ok(())
    }
}
macro_rules! array_deser {
    ($name:ident, $item:ty) => {
        paste::paste! {
            pub(crate) struct [<$name Deserializer>];
            impl super::array::ArrayDeserializerGeneric for [<$name Deserializer>] {
                type Item = $crate::native::values::$item;
                fn inner_type(_type_: &Type) -> Result<&Type> {
                    Ok(&Type::$item)
                }
                fn inner_value(items: Vec<Self::Item>) -> Value {
                    Value::$name($crate::native::values::$name(items))
                }
                fn item_mapping(value: Value) -> Self::Item {
                    let Value::$item(point) = value else {
                        unreachable!()
                    };
                    point
                }
            }
        }
    };
}

// DEV TODO: Are these infinite loops?
array_deser!(Ring, Point);
array_deser!(Polygon, Ring);
array_deser!(MultiPolygon, Polygon);

pub(crate) fn parse_point_with_path(
    rows: usize,
    state: &mut DeserializerState,
    _path: &mut Vec<u16>,
    reader: &mut SyncReader<'_>,
) -> Result<ParseStatus<Vec<Value>>> {
    let mut points = vec![Point::default(); rows];
    for col in 0..2 {
        let values = match Type::Float64.parse_column_sync_with_path(rows, state, _path, reader)? {
            ParseStatus::Complete { value, .. } => value,
            ParseStatus::NeedMore { needed } => return Ok(ParseStatus::NeedMore { needed }),
        };
        for (row_idx, value) in values.into_iter().enumerate() {
            let Value::Float64(v) = value else { unreachable!() };
            points[row_idx].0[col] = v;
        }
    }
    Ok(ParseStatus::Complete {
        value:    points.into_iter().map(Value::Point).collect(),
        consumed: reader.consumed(),
    })
}

pub(crate) fn parse_geo_array_with_path<T: super::array::ArrayDeserializerGeneric>(
    type_: &Type,
    rows: usize,
    state: &mut DeserializerState,
    path: &mut Vec<u16>,
    reader: &mut SyncReader<'_>,
) -> Result<ParseStatus<Vec<Value>>> {
    if rows == 0 {
        return Ok(ParseStatus::Complete { value: Vec::new(), consumed: reader.consumed() });
    }

    // Parse array offsets
    let offsets = match crate::native::sync::parse_array_offsets(rows, reader.remaining())? {
        ParseStatus::Complete { value, consumed } => {
            reader.advance(consumed)?;
            value
        }
        ParseStatus::NeedMore { needed } => return Ok(ParseStatus::NeedMore { needed }),
    };

    let total_items = *offsets.last().unwrap_or(&0) as usize;
    let inner_type = T::inner_type(type_)?;

    path.push(0);
    let items = match inner_type.parse_column_sync_with_path(total_items, state, path, reader)? {
        ParseStatus::Complete { value, .. } => value,
        ParseStatus::NeedMore { needed } => {
            let _ = path.pop();
            return Ok(ParseStatus::NeedMore { needed });
        }
    };
    let _ = path.pop();

    let mut out = Vec::with_capacity(rows);
    let mut iter = items.into_iter();
    let mut prev = 0u64;
    for offset in offsets {
        let len = offset - prev;
        prev = offset;
        #[allow(clippy::cast_possible_truncation)]
        out.push(T::inner_value((&mut iter).take(len as usize).map(T::item_mapping).collect()));
    }

    Ok(ParseStatus::Complete { value: out, consumed: reader.consumed() })
}
