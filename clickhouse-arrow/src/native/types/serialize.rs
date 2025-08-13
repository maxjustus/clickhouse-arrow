pub(crate) mod array;
pub(crate) mod dynamic;
pub(crate) mod geo;
pub(crate) mod json;
pub(crate) mod low_cardinality;
pub(crate) mod map;
pub(crate) mod nullable;
pub(crate) mod object;
pub(crate) mod sized;
pub(crate) mod string;
pub(crate) mod tuple;
pub(crate) mod variant;

use super::*;
use crate::io::{ClickHouseBytesWrite, ClickHouseWrite};

pub(crate) trait ClickHouseNativeSerializer {
    fn serialize_prefix_async<'a, W: ClickHouseWrite>(
        &'a self,
        writer: &'a mut W,
        state: &'a mut SerializerState,
    ) -> impl Future<Output = Result<()>> + Send + 'a;

    fn serialize_prefix<W: ClickHouseBytesWrite>(
        &self,
        writer: &mut W,
        _state: &mut SerializerState,
    );
}

impl ClickHouseNativeSerializer for Type {
    fn serialize_prefix_async<'a, W: ClickHouseWrite>(
        &'a self,
        writer: &'a mut W,
        state: &'a mut SerializerState,
    ) -> impl Future<Output = Result<()>> + Send + 'a {
        use serialize::*;
        async move {
            let type_ = match self {
                Type::Nullable(inner) | Type::Array(inner) => inner,
                Type::Map(key, value) => &super::map::normalize_map_type(key, value),
                Type::Tuple(inner) => {
                    for item in inner {
                        item.serialize_prefix_async(writer, state).await?;
                    }
                    return Ok(());
                }
                Type::Point => {
                    for _ in 0..2 {
                        Type::Float64.serialize_prefix_async(writer, state).await?;
                    }
                    return Ok(());
                }
                Type::LowCardinality(_) => {
                    low_cardinality::LowCardinalitySerializer::write_prefix(self, writer, state)
                        .await?;
                    return Ok(());
                }
                Type::Object => {
                    object::ObjectSerializer::write_prefix(self, writer, state).await?;
                    return Ok(());
                }
                Type::Variant(_) => {
                    variant::VariantSerializer::write_prefix(self, writer, state).await?;
                    return Ok(());
                }
                Type::Dynamic { .. } => {
                    dynamic::DynamicSerializer::write_prefix(self, writer, state).await?;
                    return Ok(());
                }
                Type::JSON { .. } => {
                    json::JsonSerializer::write_prefix(self, writer, state).await?;
                    return Ok(());
                }
                _ => return Ok(()), // All primitive types do nothing
            };

            type_.serialize_prefix_async(writer, state).await
        }
        .boxed()
    }

    fn serialize_prefix<W: ClickHouseBytesWrite>(
        &self,
        writer: &mut W,
        state: &mut SerializerState,
    ) {
        let type_ = match self {
            Type::Nullable(inner) | Type::Array(inner) => inner,
            Type::Map(key, value) => &super::map::normalize_map_type(key, value),
            Type::Tuple(inner) => {
                for item in inner {
                    item.serialize_prefix(writer, state);
                }
                return;
            }
            Type::Point => {
                for _ in 0..2 {
                    Type::Float64.serialize_prefix(writer, state);
                }
                return;
            }
            Type::LowCardinality(_) => {
                low_cardinality::LowCardinalitySerializer::write_prefix_sync(self, writer, state)
                    .expect("LowCardinality prefix serialization failed");
                return;
            }
            Type::Object => {
                object::ObjectSerializer::write_prefix_sync(self, writer, state)
                    .expect("Object prefix serialization failed");
                return;
            }
            Type::Variant(_) => {
                variant::VariantSerializer::write_prefix_sync(self, writer, state)
                    .expect("Variant prefix serialization failed");
                return;
            }
            Type::Dynamic { .. } => {
                dynamic::DynamicSerializer::write_prefix_sync(self, writer, state)
                    .expect("Dynamic prefix serialization failed");
                return;
            }
            Type::JSON { .. } => {
                json::JsonSerializer::write_prefix_sync(self, writer, state)
                    .expect("JSON prefix serialization failed");
                return;
            }
            _ => return,
        };

        type_.serialize_prefix(writer, state);
    }
}
