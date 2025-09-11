mod arrow;
mod native;
pub(crate) mod protocol_data;

// Re-exports
pub use arrow::ArrowFormat;
pub use native::NativeFormat;

use crate::ArrowOptions;
// BTreeMap is imported later with HashMap

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
    pub(crate) options:        Option<ArrowOptions>,
    pub(crate) deserializer:   T,
    pub(crate) type_specific:  TypeSpecificState,
    // Sparse/custom plan and traversal state
    // When present, maps a type-path (sequence of child indexes from column root)
    // to a kind byte (0 = DEFAULT, non-zero = SPARSE).
    pub(crate) kind_plan:      Option<BTreeMap<Vec<u16>, u8>>,
    // Current position in the type tree while deserializing values
    pub(crate) cur_path:       Vec<u16>,
    // Runtime sparse state per leaf path: (num_trailing_defaults, has_value_after_defaults)
    pub(crate) sparse_runtime: BTreeMap<Vec<u16>, (usize, bool)>,
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
pub struct DynamicState {
    pub version:     Option<u64>,
    pub total_types: u64,
    pub type_names:  Vec<String>,
    pub type_map:    HashMap<String, (usize, Type)>,
    pub types:       Vec<(String, Type)>,
}

/// Metadata for JSON type
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JsonState {
    pub version:                  Option<u64>,
    /// Dynamic paths that will use Dynamic serialization
    pub dynamic_paths:            Vec<String>,
    /// Typed paths with their declared types
    pub typed_paths:              Vec<(String, Type)>,
    /// Column data for dynamic paths
    pub dynamic_path_columns:     Option<BTreeMap<String, Vec<Value>>>,
    /// Column data for typed paths (path -> values)
    pub typed_path_columns:       Option<BTreeMap<String, Vec<Value>>>,
    pub rows:                     Option<usize>,
    pub dynamic_data:             Option<DynamicTypeData>,
    /// Dynamic states for each dynamic path (filled during `write_prefix`)
    pub path_dynamic_states:      BTreeMap<String, DynamicState>,
    /// Serialization states for each typed path (filled during `analyze_values`)
    pub(crate) typed_path_states: BTreeMap<String, SerializerState>,

    // Deprecated - kept for compatibility during migration
    #[deprecated(note = "Use dynamic_paths instead")]
    pub paths:        Vec<String>,
    #[deprecated(note = "Use dynamic_path_columns instead")]
    pub path_columns: Option<BTreeMap<String, Vec<Value>>>,
}

/// Enum to hold type-specific state
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum TypeSpecificState {
    #[default]
    None,
    Dynamic(DynamicState),
    Json(JsonState),
    // Indicates server-side custom/sparse serialization for current column
    Sparse(SparseState),
}

/// State for custom/sparse serialization
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SparseState {
    pub has_custom:               bool,
    pub use_custom:               Option<bool>,
    pub num_trailing_defaults:    usize,
    pub has_value_after_defaults: bool,
    // Future: we could add thresholds or stats here
}
