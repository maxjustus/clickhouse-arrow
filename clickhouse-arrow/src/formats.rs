mod arrow;
mod native;
pub(crate) mod protocol_data;

// Re-exports
pub use arrow::ArrowFormat;
pub use native::NativeFormat;

use crate::ArrowOptions;
// no futures needed after refactor
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
    pub(crate) options: Option<ArrowOptions>,
    pub(crate) deserializer: T,
    // TODO: just wondering out loud. We do kind_plan for all paths in one pass but we have a
    // single type_specific state. Are these sort of inconsistent patterns or does it make sense?
    // Type specific is sort of a container for state as we deserialize so maybe it makes sense?
    pub(crate) type_specific: TypeSpecificState,
    // Sparse/custom plan and traversal state
    // When present, maps a type-path (sequence of child indexes from column root)
    // to a kind byte (0 = DEFAULT, non-zero = SPARSE).
    // TODO: kind is too general. Should this serialization_type_by_path?
    pub(crate) kind_plan: Option<BTreeMap<Vec<u16>, u8>>,
    // TODO: this feels like a bad name. Maybe `sparse_format_state`?
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
    pub(crate) fn deserializer(&mut self) -> &mut T {
        &mut self.deserializer
    }

    /// Look up the custom/sparse kind byte for a given path, falling back to parent paths.
    #[must_use]
    pub(crate) fn kind_for_path(&self, path: &[u16]) -> Option<u8> {
        let plan = self.kind_plan.as_ref()?;
        let mut len = path.len();
        loop {
            if let Some(kind) = plan.get(&path[..len]) {
                return Some(*kind);
            }
            if len == 0 {
                break;
            }
            len -= 1;
        }
        plan.get(&[][..]).copied()
    }

    /// Whether the given path should use sparse decoding.
    #[must_use]
    pub(crate) fn is_sparse_path(&self, path: &[u16]) -> bool {
        self.kind_for_path(path).map(|kind| kind != 0).unwrap_or(false)
    }
}

/// Context maintained during serialization
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct SerializerState<T: Default = ()> {
    pub(crate) options: Option<ArrowOptions>,
    pub(crate) serializer: T,
    pub(crate) server_version: Option<(u64, u64, u64)>,
    pub(crate) type_specific: TypeSpecificState,
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
    pub(crate) fn serializer(&mut self) -> &mut T {
        &mut self.serializer
    }
}

use std::collections::{BTreeMap, HashMap};

use crate::Type;
use crate::native::values::Value;

/// Type alias for Dynamic type metadata used in JSON deserialization
pub(crate) type DynamicTypeData = Vec<(u64, Vec<(String, Type)>)>;

/// Metadata for Dynamic type
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DynamicState {
    pub version: Option<u64>,
    pub total_types: u64,
    pub type_names: Vec<String>,
    pub type_map: HashMap<String, (usize, Type)>,
    pub types: Vec<(String, Type)>,
}

/// Metadata for JSON type
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JsonState {
    pub version: Option<u64>,
    /// Dynamic paths that will use Dynamic serialization
    pub dynamic_paths: Vec<String>,
    /// Typed paths with their declared types
    pub typed_paths: Vec<(String, Type)>,
    /// Column data for dynamic paths
    pub dynamic_path_columns: Option<BTreeMap<String, Vec<Value>>>,
    /// Column data for typed paths (path -> values)
    pub typed_path_columns: Option<BTreeMap<String, Vec<Value>>>,
    pub rows: Option<usize>,
    pub dynamic_data: Option<DynamicTypeData>,
    /// Dynamic states for each dynamic path (filled during `write_prefix`)
    pub path_dynamic_states: BTreeMap<String, DynamicState>,
    /// Serialization states for each typed path (filled during `analyze_values`)
    pub(crate) typed_path_states: BTreeMap<String, SerializerState>,
    /// Cached path segmentation for faster nested JSON assembly (path -> segments)
    pub path_segments: BTreeMap<String, Vec<String>>,

    // Deprecated - kept for compatibility during migration
    #[deprecated(note = "Use dynamic_paths instead")]
    pub paths: Vec<String>,
    #[deprecated(note = "Use dynamic_path_columns instead")]
    pub path_columns: Option<BTreeMap<String, Vec<Value>>>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::DeserializerState;

    #[test]
    fn sparse_kind_falls_back_to_parent() {
        let mut state: DeserializerState<()> = DeserializerState::default();
        let mut plan = BTreeMap::new();
        let _ = plan.insert(Vec::<u16>::new(), 1);
        state.kind_plan = Some(plan);

        assert!(state.is_sparse_path(&[]));
        assert!(state.is_sparse_path(&[0]));
        assert!(state.is_sparse_path(&[0, 1]));
    }

    #[test]
    fn sparse_kind_defaults_to_dense() {
        let state: DeserializerState<()> = DeserializerState::default();
        assert!(!state.is_sparse_path(&[]));
        assert!(!state.is_sparse_path(&[1, 2]));
    }
}

/// Enum to hold type-specific state
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum TypeSpecificState {
    #[default]
    None,
    Dynamic(DynamicState),
    Json(JsonState),
}
