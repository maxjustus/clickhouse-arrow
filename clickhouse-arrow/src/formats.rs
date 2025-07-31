mod arrow;
mod native;
pub(crate) mod protocol_data;

// Re-exports
pub use arrow::ArrowFormat;
pub use native::NativeFormat;

use crate::ArrowOptions;

/// Marker trait for various client formats.
///
/// Currently only two formats are in use: `ArrowFormat` and `NativeFormat`. This approach provides
/// a simple mechanism to introduce new formats to work with `ClickHouse` data without a lot of
/// overhead and a fullblown serde implementation.
#[expect(private_bounds)]
pub trait ClientFormat: sealed::ClientFormatImpl<Self::Data> + Send + Sync + 'static {
    type Data: std::fmt::Debug + Clone + Send + Sync + 'static;

    const FORMAT: &'static str;
}

pub(crate) mod sealed {
    use super::{DeserializerState, SerializerState};
    use crate::Type;
    use crate::client::connection::ClientMetadata;
    use crate::errors::Result;
    use crate::io::{ClickHouseRead, ClickHouseWrite};
    use crate::query::Qid;

    pub(crate) trait ClientFormatImpl<T>: std::fmt::Debug
    where
        T: std::fmt::Debug + Clone + Send + Sync + 'static,
    {
        type Schema: std::fmt::Debug + Clone + Send + Sync + 'static;
        type Deser: Default + Send + Sync + 'static;
        type Ser: Default + Send + Sync + 'static;

        #[expect(unused)]
        fn finish_ser(_state: &mut SerializerState<Self::Ser>) {}

        fn finish_deser(_state: &mut DeserializerState<Self::Deser>) {}

        fn write<'a, W: ClickHouseWrite>(
            writer: &'a mut W,
            data: T,
            qid: Qid,
            header: Option<&'a [(String, Type)]>,
            revision: u64,
            metadata: ClientMetadata,
        ) -> impl Future<Output = Result<()>> + Send + 'a;

        fn read<'a, R: ClickHouseRead + 'static>(
            reader: &'a mut R,
            revision: u64,
            metadata: ClientMetadata,
            state: &'a mut DeserializerState<Self::Deser>,
        ) -> impl Future<Output = Result<Option<T>>> + Send + 'a;
    }
}

/// Context maintained during deserialization
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct DeserializerState<T: Default = ()> {
    pub(crate) options:       Option<ArrowOptions>,
    pub(crate) deserializer:  T,
    pub(crate) type_specific: TypeSpecificState,
}

impl<T: Default> DeserializerState<T> {
    #[must_use]
    pub(crate) fn with_arrow_options(mut self, options: ArrowOptions) -> Self {
        self.options = Some(options);
        self
    }

    #[must_use]
    pub(crate) fn deserializer(&mut self) -> &mut T { &mut self.deserializer }
}

/// Context maintained during serialization
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct SerializerState<T: Default = ()> {
    pub(crate) options:        Option<ArrowOptions>,
    pub(crate) serializer:     T,
    pub(crate) server_version: Option<(u64, u64, u64)>,
    pub(crate) type_specific:  TypeSpecificState,
}

impl<T: Default> SerializerState<T> {
    #[must_use]
    pub(crate) fn with_arrow_options(mut self, options: ArrowOptions) -> Self {
        self.options = Some(options);
        self
    }

    #[must_use]
    pub(crate) fn with_server_version(mut self, version: (u64, u64, u64)) -> Self {
        self.server_version = Some(version);
        self
    }

    #[expect(unused)]
    #[must_use]
    pub(crate) fn serializer(&mut self) -> &mut T { &mut self.serializer }
}

use std::collections::{BTreeMap, HashMap};

use crate::Type;
use crate::native::values::Value;

/// Type alias for Dynamic type metadata used in JSON deserialization
pub(crate) type DynamicTypeData = Vec<(u64, Vec<(String, Type)>)>;

/// Metadata for Dynamic type
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DynamicState {
    pub(crate) version:     Option<u64>,
    pub(crate) total_types: u64,
    pub(crate) type_names:  Vec<String>,
    pub(crate) type_map:    HashMap<String, (usize, Type)>,
    pub(crate) types:       Vec<(String, Type)>,
}

/// Metadata for JSON type
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct JsonState {
    pub(crate) version:      Option<u64>,
    pub(crate) paths:        Vec<String>,
    pub(crate) path_columns: Option<BTreeMap<String, Vec<Value>>>,
    pub(crate) rows:         Option<usize>,
    pub(crate) dynamic_data: Option<DynamicTypeData>,
}

/// Enum to hold type-specific state
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum TypeSpecificState {
    #[default]
    None,
    Dynamic(DynamicState),
    Json(JsonState),
}
