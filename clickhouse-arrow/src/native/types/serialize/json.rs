use std::collections::BTreeMap;

use tokio::io::AsyncWriteExt;

use super::dynamic::DynamicSerializer;
use super::{ClickHouseNativeSerializer, Serializer, SerializerState, Type};
use crate::formats::{JsonState, TypeSpecificState};
use crate::io::ClickHouseWrite;
// unused imports removed
use crate::{Error, Result, Value};

pub(crate) struct JsonSerializer;

// JSON serialization versions from ClickHouse
const JSON_OBJECT_VERSION_V1: u64 = 0; // Legacy with max_dynamic_paths field
const JSON_OBJECT_VERSION_V2: u64 = 2; // Modern with shared data
const JSON_OBJECT_SERIALIZATION_VERSION_FLATTENED: u64 = 3;

// Dynamic serialization versions (same as JSON versions for V1/V2/V3)
// For compatibility with pre-25.6 ClickHouse: V1=0, V2=2, V3=3
const DYNAMIC_VERSION_V1: u64 = 0;
const DYNAMIC_VERSION_V2: u64 = 2;
const DYNAMIC_VERSION_V3: u64 = 3;

/// Map JSON serialization version to Dynamic serialization version
/// Currently JSON and Dynamic versions align (V1=0, V2=2, V3=3)
#[inline]
fn json_version_to_dynamic_version(json_version: Option<u64>) -> Option<u64> {
    json_version.map(|v| match v {
        JSON_OBJECT_VERSION_V1 => DYNAMIC_VERSION_V1, // JSON V1 (0) → Dynamic V1 (0)
        JSON_OBJECT_VERSION_V2 => DYNAMIC_VERSION_V2, // JSON V2 (2) → Dynamic V2 (2)
        JSON_OBJECT_SERIALIZATION_VERSION_FLATTENED => DYNAMIC_VERSION_V3, /* JSON V3 (3) → */
        // Dynamic V3 (3)
        _ => v, // Unknown version, pass through
    })
}

/// Parsed JSON data organized by typed and dynamic paths
#[derive(Debug, Clone)]
struct JsonData {
    /// Map from path to values for dynamic paths
    dynamic_path_columns: BTreeMap<String, Vec<Value>>,
    /// Map from path to values for typed paths
    typed_path_columns:   BTreeMap<String, Vec<Value>>,
    /// Path frequency: how many rows contain each dynamic path (non-null count)
    path_frequency:       BTreeMap<String, usize>,
    /// Number of rows
    rows:                 usize,
}

impl JsonData {
    #[inline]
    fn is_effectively_nullable(t: &Type) -> bool {
        match t {
            Type::Nullable(_) => true,
            Type::LowCardinality(inner) => inner.is_nullable(),
            _ => false,
        }
    }

    /// Build skip matchers from exact strings and regex patterns
    fn compile_skip_matchers(
        skip_exact: &[String],
        skip_regex: &[String],
    ) -> Result<(std::collections::HashSet<String>, Vec<regex::Regex>)> {
        let skip_set: std::collections::HashSet<String> = skip_exact.iter().cloned().collect();

        let patterns: Result<Vec<regex::Regex>> = skip_regex
            .iter()
            .map(|p| {
                regex::Regex::new(p).map_err(|e| {
                    Error::SerializeError(format!("Invalid skip_path pattern '{p}': {e}"))
                })
            })
            .collect();

        Ok((skip_set, patterns?))
    }

    /// Pre-fill typed columns with appropriate defaults
    fn init_typed_columns(
        typed_paths: &[(String, Type)],
        rows: usize,
    ) -> BTreeMap<String, Vec<Value>> {
        let mut columns = BTreeMap::new();
        for (path, ty) in typed_paths {
            let fill = match ty {
                Type::Variant(_) => Value::Variant(0xFF, Box::new(Value::Null)),
                _ if Self::is_effectively_nullable(ty) => Value::Null,
                _ => ty.default_value(),
            };
            drop(columns.insert(path.clone(), vec![fill; rows]));
        }
        columns
    }

    /// Parse a Value into serde_json::Value (returns None for Null)
    fn parse_json_value(value: Value) -> Result<Option<serde_json::Value>> {
        match value {
            Value::Object(bytes) => {
                let json = serde_json::from_slice(&bytes)
                    .map_err(|e| Error::SerializeError(format!("Invalid JSON bytes: {e}")))?;
                Ok(Some(json))
            }
            #[cfg(feature = "serde")]
            Value::Json(json) => Ok(Some(json)),
            Value::String(bytes) => {
                let s = String::from_utf8(bytes).map_err(|e| {
                    Error::SerializeError(format!("Invalid UTF-8 in JSON string: {e}"))
                })?;
                let json = serde_json::from_str(&s)
                    .map_err(|e| Error::SerializeError(format!("Invalid JSON string: {e}")))?;
                Ok(Some(json))
            }
            Value::Null => Ok(None),
            _ => Err(Error::SerializeError(format!(
                "JSON serialization expects Object or String, got: {value:?}"
            ))),
        }
    }

    /// Pad all columns to exactly `rows` entries with Null
    fn pad_columns(columns: &mut BTreeMap<String, Vec<Value>>, rows: usize) {
        for column in columns.values_mut() {
            column.resize(rows, Value::Null);
        }
    }

    /// Count non-null values per path
    fn compute_frequency(columns: &BTreeMap<String, Vec<Value>>) -> BTreeMap<String, usize> {
        columns
            .iter()
            .map(|(path, col)| {
                let count = col.iter().filter(|v| !matches!(v, Value::Null)).count();
                (path.clone(), count)
            })
            .collect()
    }

    /// Parse JSON values into path-organized structure
    fn from_values(
        values: Vec<Value>,
        typed_paths: &[(String, Type)],
        skip_exact: &[String],
        skip_regex: &[String],
    ) -> Result<Self> {
        let rows = values.len();
        let (skip_set, skip_patterns) = Self::compile_skip_matchers(skip_exact, skip_regex)?;
        let mut typed_path_columns = Self::init_typed_columns(typed_paths, rows);
        let mut dynamic_path_columns: BTreeMap<String, Vec<Value>> = BTreeMap::new();

        for (row_idx, value) in values.into_iter().enumerate() {
            if let Some(json) = Self::parse_json_value(value)? {
                Self::extract_paths_from_json(
                    &json,
                    "",
                    &mut dynamic_path_columns,
                    &mut typed_path_columns,
                    typed_paths,
                    &skip_set,
                    &skip_patterns,
                    row_idx,
                    rows,
                )?;
            }
        }

        Self::pad_columns(&mut dynamic_path_columns, rows);
        let path_frequency = Self::compute_frequency(&dynamic_path_columns);

        Ok(JsonData { dynamic_path_columns, typed_path_columns, path_frequency, rows })
    }

    /// Recursively extract paths from JSON value
    fn extract_paths_from_json(
        json_value: &serde_json::Value,
        current_path: &str,
        dynamic_path_columns: &mut BTreeMap<String, Vec<Value>>,
        typed_path_columns: &mut BTreeMap<String, Vec<Value>>,
        typed_paths: &[(String, Type)],
        skip_exact: &std::collections::HashSet<String>,
        skip_patterns: &[regex::Regex],
        row_idx: usize,
        total_rows: usize,
    ) -> Result<()> {
        if let serde_json::Value::Object(map) = json_value {
            for (key, value) in map {
                let path = if current_path.is_empty() {
                    key.clone()
                } else {
                    format!("{current_path}.{key}")
                };

                Self::extract_paths_from_json(
                    value,
                    &path,
                    dynamic_path_columns,
                    typed_path_columns,
                    typed_paths,
                    skip_exact,
                    skip_patterns,
                    row_idx,
                    total_rows,
                )?;
            }
        } else {
            // Determine if this is typed
            let typed_path_type =
                typed_paths.iter().find(|(path, _)| path == current_path).map(|(_, typ)| typ);

            // Apply skip only for dynamic (non-typed) paths
            if typed_path_type.is_none() {
                if skip_exact.contains(current_path) {
                    return Ok(());
                }
                for pattern in skip_patterns {
                    if pattern.is_match(current_path) {
                        return Ok(());
                    }
                }
            }

            // Leaf value - convert to ClickHouse Value and store
            let ch_value = Self::json_value_to_clickhouse_value(json_value)?;

            if let Some(expected_type) = typed_path_type {
                // Convert the value to the expected type
                let converted_value =
                    Self::convert_to_type_with_path(ch_value, expected_type, current_path)?;

                // Store in typed path columns
                if let Some(column) = typed_path_columns.get_mut(current_path) {
                    if row_idx < column.len() {
                        column[row_idx] = converted_value;
                    }
                }
            } else {
                // Store in dynamic path columns
                let column = dynamic_path_columns
                    .entry(current_path.to_string())
                    .or_insert_with(|| vec![Value::Null; total_rows]);

                if row_idx < column.len() {
                    column[row_idx] = ch_value;
                }
            }
        }
        Ok(())
    }

    /// Convert a value to a specific type with path context for better error messages
    fn convert_to_type_with_path(value: Value, expected_type: &Type, path: &str) -> Result<Value> {
        Self::convert_to_type(value.clone(), expected_type).map_err(|e| {
            Error::SerializeError(format!(
                "Failed to convert value for path '{path}' to type {expected_type:?}: {e}"
            ))
        })
    }

    // Note: exact-match helper removed; discriminator uses shared coerce::discriminate

    /// Convert a value to a specific type if needed (delegates to shared coercion)
    fn convert_to_type(value: Value, expected_type: &Type) -> Result<Value> {
        crate::native::coerce::convert_to_type(value, expected_type)
    }

    /// Convert `serde_json::Value` to `ClickHouse` Value
    fn json_value_to_clickhouse_value(json_value: &serde_json::Value) -> Result<Value> {
        let value = match json_value {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => {
                // ClickHouse doesn't have a native Bool, use UInt8
                Value::UInt8(u8::from(*b))
            }
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Int64(i)
                } else if let Some(u) = n.as_u64() {
                    Value::UInt64(u)
                } else if let Some(f) = n.as_f64() {
                    Value::Float64(f)
                } else {
                    return Err(Error::SerializeError(format!(
                        "Unsupported JSON number format: {n}"
                    )));
                }
            }
            serde_json::Value::String(s) => Value::String(s.as_bytes().to_vec()),
            serde_json::Value::Array(arr) => {
                // Convert array elements recursively to preserve structure
                let elements: Result<Vec<Value>> =
                    arr.iter().map(Self::json_value_to_clickhouse_value).collect();
                Value::Array(elements?)
            }
            serde_json::Value::Object(_) => {
                // For objects at leaf positions, serialize back to JSON string
                // (Objects should have been recursed through already)
                let json_str = serde_json::to_string(json_value).map_err(|e| {
                    Error::SerializeError(format!("Failed to serialize JSON value: {e}"))
                })?;
                Value::String(json_str.into_bytes())
            }
        };
        Ok(value)
    }
}

impl JsonSerializer {
    /// Check if server supports JSON v3
    fn check_server_version(state: &SerializerState, version: u64) -> Result<()> {
        // V1/V2 supported on older servers, V3 requires >= 25.6
        if version == JSON_OBJECT_SERIALIZATION_VERSION_FLATTENED {
            if let Some((major, minor, _)) = state.server_version
                && (major < 25 || (major == 25 && minor < 6))
            {
                return Err(Error::SerializeError(format!(
                    "JSON V3 requires ClickHouse server version >= 25.6, got {major}.{minor}"
                )));
            }
        }
        Ok(())
    }
}

impl JsonSerializer {
    /// Analyze JSON values and return metadata for use in `write_prefix`
    #[allow(dead_code)] // Used by tests
    pub(crate) fn analyze_values(values: &[Value], type_: &Type) -> Result<TypeSpecificState> {
        Self::analyze_values_with_version(values, type_, None)
    }

    pub(crate) fn analyze_values_with_version(
        values: &[Value],
        type_: &Type,
        version: Option<u64>,
    ) -> Result<TypeSpecificState> {
        // Extract typed_paths and SKIP rules from the Type
        let (typed_paths, skip_exact, skip_regex, max_dyn_paths, _max_dyn_types) = match type_ {
            Type::JSON {
                typed_paths,
                skip_exact,
                skip_regex,
                max_dynamic_paths,
                max_dynamic_types,
            } => {
                let typed_list: Vec<(String, Type)> = typed_paths
                    .iter()
                    .map(|(path, boxed_type)| (path.clone(), *boxed_type.clone()))
                    .collect();
                (
                    typed_list,
                    skip_exact.clone(),
                    skip_regex.clone(),
                    *max_dynamic_paths,
                    *max_dynamic_types,
                )
            }
            _ => return Err(Error::SerializeError("Expected JSON type".to_string())),
        };

        // Parse JSON values into path-organized structure (filtering skipped paths)
        let json_data =
            JsonData::from_values(values.to_vec(), &typed_paths, &skip_exact, &skip_regex)?;

        // Determine if we need to enforce max_dynamic_paths (V1/V2 only)
        let is_v1_v2 =
            version == Some(JSON_OBJECT_VERSION_V1) || version == Some(JSON_OBJECT_VERSION_V2);
        let max_paths = max_dyn_paths.unwrap_or(1024); // Default: 1024 for V1/V2

        // Build path list sorted by frequency (descending), then alphabetically for ties
        let mut all_paths: Vec<String> = json_data.dynamic_path_columns.keys().cloned().collect();
        all_paths.sort_by(|a, b| {
            let freq_a = json_data.path_frequency.get(a).copied().unwrap_or(0);
            let freq_b = json_data.path_frequency.get(b).copied().unwrap_or(0);
            // Higher frequency first, then alphabetically for ties
            freq_b.cmp(&freq_a).then_with(|| a.cmp(b))
        });

        // Split paths: V3 sends all paths, V1/V2 enforces limit
        let (dynamic_paths, shared_paths): (Vec<String>, Vec<String>) = if is_v1_v2 {
            let limit = all_paths.len().min(max_paths as usize);
            let (dynamic, shared) = all_paths.split_at(limit);
            // Re-sort dynamic paths alphabetically for wire format consistency
            let mut dynamic_sorted = dynamic.to_vec();
            dynamic_sorted.sort();
            (dynamic_sorted, shared.to_vec())
        } else {
            // V3: all paths are dynamic, none shared
            all_paths.sort(); // Alphabetical for V3
            (all_paths, vec![])
        };

        // Split column data between dynamic and shared
        let mut dynamic_path_columns = json_data.dynamic_path_columns;
        let shared_path_columns: Option<BTreeMap<String, Vec<Value>>> = if shared_paths.is_empty() {
            None
        } else {
            let mut shared_cols = BTreeMap::new();
            for path in &shared_paths {
                if let Some(col) = dynamic_path_columns.remove(path) {
                    drop(shared_cols.insert(path.clone(), col));
                }
            }
            Some(shared_cols)
        };

        // Build states for typed paths
        // This is crucial for types like LowCardinality that need to build dictionaries
        let mut typed_path_states = BTreeMap::new();
        for (path, _type_) in &typed_paths {
            // Get the column values for this typed path
            let _column_values = json_data
                .typed_path_columns
                .get(path)
                .cloned()
                .unwrap_or_else(|| vec![Value::Null; json_data.rows]);

            // For now, we'll handle this in the write method where we can properly
            // build the state. Just store a placeholder.
            drop(typed_path_states.insert(path.clone(), SerializerState::default()));
        }

        // Precompute Dynamic states per dynamic path only (not shared paths).
        // Shared paths are binary-encoded, not via Dynamic serialization.
        let mut path_dynamic_states = BTreeMap::new();
        for path in &dynamic_paths {
            if let Some(col) = dynamic_path_columns.get(path) {
                let analyzed = DynamicSerializer::analyze_values(col);
                if let TypeSpecificState::Dynamic(dyn_state) = analyzed.clone() {
                    drop(path_dynamic_states.insert(path.clone(), dyn_state));
                }
            }
        }

        // For V1/V2, also set version on Dynamic states
        // Note: JSON version differs from Dynamic version (JSON V1=0 → Dynamic V1=1)
        if is_v1_v2 {
            let dynamic_version = json_version_to_dynamic_version(version);
            for dyn_state in path_dynamic_states.values_mut() {
                dyn_state.version = dynamic_version;
            }
        }

        let state = JsonState {
            version, // Will be used in write_prefix if set, otherwise defaults to V3
            dynamic_paths,
            typed_paths,
            dynamic_path_columns: Some(dynamic_path_columns),
            typed_path_columns: Some(json_data.typed_path_columns),
            shared_path_columns,
            rows: Some(json_data.rows),
            dynamic_data: None,
            path_dynamic_states,
            typed_path_states,
            path_segments: BTreeMap::new(),
        };

        Ok(TypeSpecificState::Json(state))
    }

    /// Get serialization version based on server support
    fn get_serialization_version(state: &SerializerState) -> u64 {
        if let TypeSpecificState::Json(json_state) = &state.type_specific {
            json_state.version.unwrap_or(JSON_OBJECT_SERIALIZATION_VERSION_FLATTENED)
        } else {
            JSON_OBJECT_SERIALIZATION_VERSION_FLATTENED
        }
    }
}

impl Serializer for JsonSerializer {
    async fn write_prefix<W: ClickHouseWrite>(
        _type_: &Type,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        let version = Self::get_serialization_version(state);

        // Check server version support
        Self::check_server_version(state, version)?;

        // Write version
        writer.write_u64_le(version).await?;

        // Update the version in state
        if let TypeSpecificState::Json(json_state) = &mut state.type_specific {
            json_state.version = Some(version);
        }

        // Clone metadata from state (required due to borrow checker - state is passed mutably to
        // nested calls)
        let (typed_paths, dynamic_paths, path_dynamic_states) =
            if let TypeSpecificState::Json(json_state) = &state.type_specific {
                (
                    json_state.typed_paths.clone(),
                    json_state.dynamic_paths.clone(),
                    json_state.path_dynamic_states.clone(),
                )
            } else {
                return Err(Error::SerializeError(
                    "JSON serialization state not found. `analyze_values` must be called before \
                     `write_prefix`."
                        .to_string(),
                ));
            };

        match version {
            JSON_OBJECT_VERSION_V1 | JSON_OBJECT_VERSION_V2 => {
                Self::write_prefix_v1_v2_async(
                    writer,
                    state,
                    version,
                    &typed_paths,
                    &dynamic_paths,
                    &path_dynamic_states,
                )
                .await
            }
            JSON_OBJECT_SERIALIZATION_VERSION_FLATTENED => {
                Self::write_prefix_v3_async(
                    writer,
                    state,
                    &typed_paths,
                    &dynamic_paths,
                    &path_dynamic_states,
                )
                .await
            }
            _ => {
                Err(Error::SerializeError(format!("Unsupported JSON version for write: {version}")))
            }
        }
    }

    async fn write<W: ClickHouseWrite>(
        _type_: &Type,
        _values: Vec<Value>,
        writer: &mut W,
        state: &mut SerializerState,
    ) -> Result<()> {
        // Take metadata from state (avoids cloning column data)
        let (
            version,
            typed_paths,
            typed_columns,
            dynamic_paths,
            dynamic_columns,
            shared_columns,
            path_dynamic_states,
            rows,
        ) = if let TypeSpecificState::Json(json_state) = &mut state.type_specific {
            let rows = json_state.rows.ok_or_else(|| {
                Error::SerializeError("JSON rows count not found in state".to_string())
            })?;

            // Take ownership of column data (consumed during write)
            let typed_columns = json_state.typed_path_columns.take().unwrap_or_default();
            let dynamic_columns = json_state.dynamic_path_columns.take().unwrap_or_default();
            let shared_columns = json_state.shared_path_columns.take();
            let path_dynamic_states = std::mem::take(&mut json_state.path_dynamic_states);
            let version = json_state.version.unwrap_or(JSON_OBJECT_SERIALIZATION_VERSION_FLATTENED);

            // Clone small Vec<String> and Vec<(String, Type)> (cheap)
            (
                version,
                json_state.typed_paths.clone(),
                typed_columns,
                json_state.dynamic_paths.clone(),
                dynamic_columns,
                shared_columns,
                path_dynamic_states,
                rows,
            )
        } else {
            return Err(Error::SerializeError(
                "JSON serialization state not found. `analyze_values` must be called before \
                 `write`."
                    .to_string(),
            ));
        };

        // First write typed path columns (same for all versions)
        for (path, type_) in &typed_paths {
            // Use a clean state for typed columns but preserve server version
            let mut typed_state = SerializerState::default();
            if let Some(server_version) = state.server_version {
                typed_state = typed_state.with_server_version(server_version);
            }

            // Get the column values or use nulls
            let column_values = if let Some(values) = typed_columns.get(path) {
                values.clone()
            } else {
                vec![Value::Null; rows]
            };

            type_.serialize_column(column_values, writer, &mut typed_state).await?;
        }

        // Write dynamic path columns
        // For V1/V2, paths must be sorted alphabetically
        let sorted_paths: Vec<String> =
            if version == JSON_OBJECT_VERSION_V1 || version == JSON_OBJECT_VERSION_V2 {
                let mut paths = dynamic_paths;
                paths.sort();
                paths
            } else {
                dynamic_paths
            };

        for path in &sorted_paths {
            if let Some(column_values) = dynamic_columns.get(path) {
                if let Some(dynamic_state) = path_dynamic_states.get(path) {
                    // For V1/V2, set the Dynamic version (mapped from JSON version)
                    let mut dyn_state = dynamic_state.clone();
                    if version == JSON_OBJECT_VERSION_V1 || version == JSON_OBJECT_VERSION_V2 {
                        dyn_state.version = json_version_to_dynamic_version(Some(version));
                    }

                    DynamicSerializer::write_dynamic_data_async(
                        column_values,
                        writer,
                        state,
                        TypeSpecificState::Dynamic(dyn_state),
                    )
                    .await?;
                } else {
                    return Err(Error::SerializeError(format!(
                        "Dynamic state not found for path: {path}"
                    )));
                }
            }
        }

        // V1/V2 format needs shared data Map(String, String)
        if version == JSON_OBJECT_VERSION_V1 || version == JSON_OBJECT_VERSION_V2 {
            Self::write_shared_data_map(writer, &shared_columns, rows).await?;
        }

        Ok(())
    }
}

// Helper methods for JSON serialization
impl JsonSerializer {
    /// Write V3/FLATTENED format prefix
    async fn write_prefix_v3_async<W: ClickHouseWrite>(
        writer: &mut W,
        state: &mut SerializerState,
        typed_paths: &[(String, Type)],
        dynamic_paths: &[String],
        path_dynamic_states: &BTreeMap<String, crate::formats::DynamicState>,
    ) -> Result<()> {
        writer.write_var_uint(dynamic_paths.len() as u64).await?;

        for path in dynamic_paths {
            writer.write_string(path.as_bytes().to_vec()).await?;
        }

        // Write typed path prefixes (sorted for deterministic order)
        let mut typed_entries: Vec<(&String, &Type)> =
            typed_paths.iter().map(|(n, t)| (n, t)).collect();
        typed_entries.sort_by(|(a, _), (b, _)| a.cmp(b));

        for (_path_name, path_type) in typed_entries {
            let mut typed_prefix_state = SerializerState::default();
            if let Some(version) = state.server_version {
                typed_prefix_state = typed_prefix_state.with_server_version(version);
            }
            path_type.serialize_prefix_async(writer, &mut typed_prefix_state).await?;
        }

        // Write Dynamic prefix for each path (path_dynamic_states has entries for all paths with
        // data)
        for path in dynamic_paths {
            if let Some(dyn_state) = path_dynamic_states.get(path) {
                DynamicSerializer::write_prefix_with_state(dyn_state, writer, state).await?;
            }
        }

        Ok(())
    }

    /// Write V1/V2 format prefix (shared data format)
    async fn write_prefix_v1_v2_async<W: ClickHouseWrite>(
        writer: &mut W,
        state: &mut SerializerState,
        version: u64,
        typed_paths: &[(String, Type)],
        dynamic_paths: &[String],
        path_dynamic_states: &BTreeMap<String, crate::formats::DynamicState>,
    ) -> Result<()> {
        // V1 has extra max_dynamic_paths parameter
        if version == JSON_OBJECT_VERSION_V1 {
            // Default max_dynamic_paths to 1024
            writer.write_var_uint(1024).await?;
        }

        // Write num_dynamic_paths
        writer.write_var_uint(dynamic_paths.len() as u64).await?;

        // Write dynamic path names (sorted)
        let mut sorted_paths: Vec<&String> = dynamic_paths.iter().collect();
        sorted_paths.sort();
        for path in &sorted_paths {
            writer.write_string(path.as_bytes().to_vec()).await?;
        }

        // Write typed path prefixes (alphabetically sorted)
        let mut typed_entries: Vec<(&String, &Type)> =
            typed_paths.iter().map(|(n, t)| (n, t)).collect();
        typed_entries.sort_by(|(a, _), (b, _)| a.cmp(b));

        for (_path_name, path_type) in &typed_entries {
            let mut typed_prefix_state = SerializerState::default();
            if let Some(server_version) = state.server_version {
                typed_prefix_state = typed_prefix_state.with_server_version(server_version);
            }
            path_type.serialize_prefix_async(writer, &mut typed_prefix_state).await?;
        }

        // Write Dynamic V1/V2 prefix for each dynamic path
        for path in &sorted_paths {
            if let Some(dyn_state) = path_dynamic_states.get(*path) {
                // Set version (mapped from JSON version)
                let mut dyn_state_v1v2 = dyn_state.clone();
                dyn_state_v1v2.version = json_version_to_dynamic_version(Some(version));

                DynamicSerializer::write_prefix_with_state(&dyn_state_v1v2, writer, state).await?;
            }
        }

        // Write Map(String, String) prefix for shared data
        let map_type = Type::Map(Box::new(Type::String), Box::new(Type::String));
        map_type.serialize_prefix_async(writer, state).await?;

        Ok(())
    }

    /// Write Map(String, String) for shared data (V1/V2)
    /// Map is serialized as: offsets (u64 per row), keys column, values column
    async fn write_shared_data_map<W: ClickHouseWrite>(
        writer: &mut W,
        shared_columns: &Option<BTreeMap<String, Vec<Value>>>,
        rows: usize,
    ) -> Result<()> {
        use crate::native::types::serialize::binary_value::serialize_binary_value;

        // If no shared columns, write empty map
        let Some(shared_cols) = shared_columns else {
            for _ in 0..rows {
                writer.write_u64_le(0).await?;
            }
            return Ok(());
        };

        // Build per-row entries: Vec<(path, binary_value_bytes)>
        let mut all_entries: Vec<(String, Vec<u8>)> = Vec::new();
        let mut offsets: Vec<u64> = Vec::with_capacity(rows);
        let mut cumulative_offset: u64 = 0;

        for row_idx in 0..rows {
            for (path, column) in shared_cols {
                if let Some(value) = column.get(row_idx) {
                    // Skip null values - they don't need to be in shared data
                    if matches!(value, Value::Null) {
                        continue;
                    }

                    // Binary encode the value
                    let mut binary_bytes = Vec::new();
                    serialize_binary_value(&mut binary_bytes, value)?;
                    all_entries.push((path.clone(), binary_bytes));
                    cumulative_offset += 1;
                }
            }
            offsets.push(cumulative_offset);
        }

        // Stream 1: ArraySizes (cumulative offsets)
        for offset in &offsets {
            writer.write_u64_le(*offset).await?;
        }

        // Stream 2: Keys (path names as strings)
        for (path, _) in &all_entries {
            writer.write_string(path.as_bytes().to_vec()).await?;
        }

        // Stream 3: Values (binary-encoded values as strings/bytes)
        for (_, binary_bytes) in &all_entries {
            writer.write_string(binary_bytes.clone()).await?;
        }

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::formats::{DeserializerState, SerializerState};
    use crate::native::types::deserialize::ClickHouseNativeDeserializer;
    use crate::native::types::serialize::ClickHouseNativeSerializer;

    // ========== Test Helpers ==========

    fn default_json_type() -> Type {
        Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![],
            skip_exact:        vec![],
            skip_regex:        vec![],
        }
    }

    fn json_type_with_config(
        typed_paths: Vec<(String, Box<Type>)>,
        skip_exact: Vec<String>,
        skip_regex: Vec<String>,
        max_dynamic_paths: Option<u32>,
    ) -> Type {
        Type::JSON {
            max_dynamic_paths,
            max_dynamic_types: None,
            typed_paths,
            skip_exact,
            skip_regex,
        }
    }

    fn parse_json_value(v: &Value) -> Result<serde_json::Value> {
        match v {
            #[cfg(feature = "serde")]
            Value::Json(json) => Ok(json.clone()),
            Value::Object(bytes) | Value::String(bytes) => serde_json::from_slice(bytes)
                .map_err(|e| Error::DeserializeError(format!("JSON parse error: {e}"))),
            other => Err(Error::DeserializeError(format!("Unexpected value type: {other:?}"))),
        }
    }

    async fn roundtrip(values: Vec<Value>, type_: &Type) -> Result<Vec<Value>> {
        let values_len = values.len();
        let mut output = vec![];
        let mut state = SerializerState::default();

        state.type_specific = JsonSerializer::analyze_values(&values, type_)?;
        type_.serialize_prefix_async(&mut output, &mut state).await?;
        type_.serialize_column(values, &mut output, &mut state).await?;

        let mut input = Cursor::new(output);
        let mut deser_state = DeserializerState::default();
        type_.deserialize_prefix_async(&mut input, &mut deser_state).await?;
        type_.deserialize_column(&mut input, values_len, &mut deser_state).await
    }

    async fn roundtrip_with_version(
        values: Vec<Value>,
        type_: &Type,
        version: Option<u64>,
    ) -> Result<Vec<Value>> {
        let values_len = values.len();
        let mut output = vec![];
        let mut state = SerializerState::default();

        state.type_specific = JsonSerializer::analyze_values_with_version(&values, type_, version)?;
        type_.serialize_prefix_async(&mut output, &mut state).await?;
        type_.serialize_column(values, &mut output, &mut state).await?;

        let mut input = Cursor::new(output);
        let mut deser_state = DeserializerState::default();
        type_.deserialize_prefix_async(&mut input, &mut deser_state).await?;
        type_.deserialize_column(&mut input, values_len, &mut deser_state).await
    }

    // ========== Basic Roundtrip Tests ==========

    #[tokio::test]
    async fn test_json_roundtrip_various_shapes() -> Result<()> {
        // Test multiple JSON shapes in one test
        let test_cases: Vec<(&str, Vec<Value>)> = vec![
            ("simple objects", vec![
                Value::String(br#"{"name": "Alice", "age": 30}"#.to_vec()),
                Value::String(br#"{"name": "Bob", "age": 25}"#.to_vec()),
            ]),
            ("nested objects", vec![
                Value::String(br#"{"user": {"name": "Alice"}, "active": true}"#.to_vec()),
                Value::String(br#"{"user": {"name": "Bob"}, "score": 95.5}"#.to_vec()),
            ]),
            ("mixed types", vec![
                Value::String(br#"{"id": 1, "name": "test", "active": true}"#.to_vec()),
                Value::String(br#"{"id": 2, "score": 88.1, "metadata": "extra"}"#.to_vec()),
            ]),
            ("with nulls", vec![
                Value::String(br#"{"name": "Alice"}"#.to_vec()),
                Value::Null,
                Value::String(br#"{"name": "Bob"}"#.to_vec()),
            ]),
            ("empty objects", vec![
                Value::String(br#"{}"#.to_vec()),
                Value::String(br#"{"name": "test"}"#.to_vec()),
            ]),
        ];

        let type_ = default_json_type();
        for (name, values) in test_cases {
            let result = roundtrip(values.clone(), &type_).await;
            assert!(result.is_ok(), "Failed on case '{name}': {:?}", result.err());
            assert_eq!(result.unwrap().len(), values.len(), "Length mismatch for '{name}'");
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_json_wire_format_uses_v3() -> Result<()> {
        use std::io::Read;

        let values = vec![
            Value::String(br#"{"name": "Alice", "age": 30}"#.to_vec()),
            Value::String(br#"{"name": "Bob", "score": 95.5}"#.to_vec()),
        ];

        let type_ = default_json_type();
        let mut output = vec![];
        let mut state = SerializerState::default();
        state.type_specific = JsonSerializer::analyze_values(&values, &type_)?;
        type_.serialize_prefix_async(&mut output, &mut state).await?;
        type_.serialize_column(values, &mut output, &mut state).await?;

        let mut cursor = Cursor::new(&output);
        let mut version_bytes = [0u8; 8];
        cursor.read_exact(&mut version_bytes)?;
        let version = u64::from_le_bytes(version_bytes);

        assert_eq!(version, JSON_OBJECT_SERIALIZATION_VERSION_FLATTENED);
        assert!(output[8] > 0, "Should have dynamic paths");
        Ok(())
    }

    // ========== Typed Paths Tests ==========

    #[tokio::test]
    async fn test_json_typed_paths_extraction_and_roundtrip() -> Result<()> {
        let values = vec![
            Value::String(br#"{"id": 123, "name": "Alice", "score": 95.5}"#.to_vec()),
            Value::String(br#"{"id": 456, "name": "Bob", "active": true}"#.to_vec()),
            Value::String(br#"{"id": 789, "name": "Charlie", "tags": ["a", "b"]}"#.to_vec()),
        ];

        let type_ = json_type_with_config(
            vec![
                ("id".to_string(), Box::new(Type::UInt32)),
                ("name".to_string(), Box::new(Type::String)),
            ],
            vec![],
            vec![],
            None,
        );

        // Verify extraction
        let state = JsonSerializer::analyze_values(&values, &type_)?;
        if let TypeSpecificState::Json(json_state) = &state {
            assert_eq!(json_state.typed_paths.len(), 2);
            assert!(!json_state.dynamic_paths.contains(&"id".to_string()));
            assert!(!json_state.dynamic_paths.contains(&"name".to_string()));

            let typed_cols = json_state.typed_path_columns.as_ref().unwrap();
            assert!(typed_cols.contains_key("id"));
            assert!(typed_cols.contains_key("name"));
        } else {
            panic!("Expected JSON state");
        }

        // Verify roundtrip
        let result = roundtrip(values.clone(), &type_).await?;
        assert_eq!(result.len(), values.len());

        for (orig, deser) in values.iter().zip(result.iter()) {
            let orig_json = parse_json_value(orig)?;
            let deser_json = parse_json_value(deser)?;
            assert_eq!(orig_json["id"], deser_json["id"]);
            assert_eq!(orig_json["name"], deser_json["name"]);
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_json_typed_paths_defaults_behavior() -> Result<()> {
        // Non-nullable UInt32 defaults to 0
        let values_uint = vec![
            Value::String(br#"{"name": "Alice"}"#.to_vec()),
            Value::String(br#"{"name": "Bob", "id": 42}"#.to_vec()),
        ];
        let type_uint = json_type_with_config(
            vec![("id".to_string(), Box::new(Type::UInt32))],
            vec![],
            vec![],
            None,
        );
        let result = roundtrip(values_uint, &type_uint).await?;
        let json0 = parse_json_value(&result[0])?;
        let json1 = parse_json_value(&result[1])?;
        assert_eq!(json0["id"], 0);
        assert_eq!(json1["id"], 42);

        // LowCardinality(String) non-nullable defaults to ""
        let values_lc = vec![
            Value::String(br#"{"name": "Alice"}"#.to_vec()),
            Value::String(br#"{"name": "Bob", "status": "ok"}"#.to_vec()),
        ];
        let type_lc = json_type_with_config(
            vec![("status".to_string(), Box::new(Type::LowCardinality(Box::new(Type::String))))],
            vec![],
            vec![],
            None,
        );
        let result = roundtrip(values_lc, &type_lc).await?;
        let json0 = parse_json_value(&result[0])?;
        let json1 = parse_json_value(&result[1])?;
        assert_eq!(json0["status"], "");
        assert_eq!(json1["status"], "ok");

        // Variant with missing value maps to null
        let values_var = vec![
            Value::String(br#"{"name": "Alice"}"#.to_vec()),
            Value::String(br#"{"name": "Bob", "value": "x"}"#.to_vec()),
            Value::String(br#"{"name": "Carol", "value": 7}"#.to_vec()),
        ];
        let type_var = json_type_with_config(
            vec![("value".to_string(), Box::new(Type::variant(vec![Type::String, Type::UInt64])))],
            vec![],
            vec![],
            None,
        );
        let result = roundtrip(values_var, &type_var).await?;
        let json0 = parse_json_value(&result[0])?;
        let json1 = parse_json_value(&result[1])?;
        let json2 = parse_json_value(&result[2])?;
        assert!(json0["value"].is_null());
        assert_eq!(json1["value"], "x");
        assert_eq!(json2["value"], 7);

        Ok(())
    }

    #[tokio::test]
    async fn test_json_typed_paths_type_conversions() -> Result<()> {
        let values = vec![
            Value::String(br#"{"i8": 127, "u8": 255, "f32": 3.14, "f64": 2.71828}"#.to_vec()),
            Value::String(br#"{"i8": -128, "u8": 0, "f32": -1.23, "f64": -9.876}"#.to_vec()),
        ];

        let type_ = json_type_with_config(
            vec![
                ("i8".to_string(), Box::new(Type::Int8)),
                ("u8".to_string(), Box::new(Type::UInt8)),
                ("f32".to_string(), Box::new(Type::Float32)),
                ("f64".to_string(), Box::new(Type::Float64)),
            ],
            vec![],
            vec![],
            None,
        );

        let result = roundtrip(values, &type_).await?;
        for v in &result {
            let json = parse_json_value(v)?;
            assert!(json["i8"].is_number());
            assert!(json["u8"].is_number());
            assert!(json["f32"].is_number());
            assert!(json["f64"].is_number());
        }
        let json0 = parse_json_value(&result[0])?;
        assert_eq!(json0["i8"], 127);
        assert_eq!(json0["u8"], 255);

        Ok(())
    }

    #[tokio::test]
    async fn test_json_typed_paths_overflow_wrapping() -> Result<()> {
        let values = vec![Value::String(br#"{"overflow_u8": 256, "negative_to_u8": -1}"#.to_vec())];

        let type_ = json_type_with_config(
            vec![
                ("overflow_u8".to_string(), Box::new(Type::UInt8)),
                ("negative_to_u8".to_string(), Box::new(Type::UInt8)),
            ],
            vec![],
            vec![],
            None,
        );

        let result = roundtrip(values, &type_).await?;
        let json = parse_json_value(&result[0])?;
        assert_eq!(json["overflow_u8"], 0); // 256 wraps to 0
        assert_eq!(json["negative_to_u8"], 255); // -1 wraps to 255

        Ok(())
    }

    #[tokio::test]
    async fn test_json_typed_paths_collections() -> Result<()> {
        let values = vec![Value::String(
            br#"{"int_arr": [1, 2, 3], "nested": [[1, 2], [3, 4]], "tuple": [100, 3.14, "hi"]}"#
                .to_vec(),
        )];

        let type_ = json_type_with_config(
            vec![
                ("int_arr".to_string(), Box::new(Type::Array(Box::new(Type::Int32)))),
                (
                    "nested".to_string(),
                    Box::new(Type::Array(Box::new(Type::Array(Box::new(Type::Int32))))),
                ),
                (
                    "tuple".to_string(),
                    Box::new(Type::Tuple(vec![Type::UInt32, Type::Float32, Type::String])),
                ),
            ],
            vec![],
            vec![],
            None,
        );

        let result = roundtrip(values, &type_).await?;
        let json = parse_json_value(&result[0])?;

        assert_eq!(json["int_arr"][0], 1);
        assert_eq!(json["nested"][0][0], 1);
        assert_eq!(json["tuple"][0], 100);
        assert_eq!(json["tuple"][2], "hi");

        Ok(())
    }

    #[tokio::test]
    async fn test_json_typed_paths_variant_in_array() -> Result<()> {
        let values = vec![
            Value::String(br#"{"items": [1, "text", 3.14]}"#.to_vec()),
            Value::String(br#"{"items": ["hello", 42]}"#.to_vec()),
        ];

        let type_ = json_type_with_config(
            vec![(
                "items".to_string(),
                Box::new(Type::Array(Box::new(Type::variant(vec![
                    Type::String,
                    Type::Int64,
                    Type::Float64,
                ])))),
            )],
            vec![],
            vec![],
            None,
        );

        // Verify analysis produces correct structure
        let state = JsonSerializer::analyze_values(&values, &type_)?;
        if let TypeSpecificState::Json(json_state) = &state {
            let typed_cols = json_state.typed_path_columns.as_ref().unwrap();
            let items = typed_cols.get("items").unwrap();
            assert_eq!(items.len(), 2);

            for array_val in items {
                if let Value::Array(elements) = array_val {
                    for elem in elements {
                        assert!(matches!(elem, Value::Variant(_, _)));
                    }
                }
            }
        }

        // Verify prefix serialization works
        let mut output = vec![];
        let mut ser_state = SerializerState::default();
        ser_state.type_specific = state;
        type_.serialize_prefix_async(&mut output, &mut ser_state).await?;
        assert!(!output.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn test_json_typed_paths_variant_type_mismatch_error() -> Result<()> {
        let values = vec![Value::String(br#"{"strict_int": "not_a_number"}"#.to_vec())];

        let type_ = json_type_with_config(
            vec![(
                "strict_int".to_string(),
                Box::new(Type::variant(vec![Type::UInt32, Type::Int32])),
            )],
            vec![],
            vec![],
            None,
        );

        let result = JsonSerializer::analyze_values(&values, &type_);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Cannot find matching variant type"));

        Ok(())
    }

    // ========== Skip Paths Tests ==========

    #[tokio::test]
    async fn test_json_skip_paths() -> Result<()> {
        let values = vec![
            Value::String(
                br#"{"public": "data", "password": "secret", "api_key": "xyz"}"#.to_vec(),
            ),
            Value::String(br#"{"public": "info", "secret_token": "abc"}"#.to_vec()),
        ];

        let type_ = json_type_with_config(
            vec![],
            vec!["password".to_string()],
            vec![".*_key".to_string(), "secret.*".to_string()],
            None,
        );

        // Verify analysis excludes skipped paths
        let state = JsonSerializer::analyze_values(&values, &type_)?;
        if let TypeSpecificState::Json(json_state) = &state {
            assert_eq!(json_state.dynamic_paths.len(), 1);
            assert!(json_state.dynamic_paths.contains(&"public".to_string()));
            assert!(!json_state.dynamic_paths.contains(&"password".to_string()));
            assert!(!json_state.dynamic_paths.contains(&"api_key".to_string()));
            assert!(!json_state.dynamic_paths.contains(&"secret_token".to_string()));
        }

        // Verify roundtrip excludes skipped paths
        let result = roundtrip(values, &type_).await?;
        for v in &result {
            let json = parse_json_value(v)?;
            assert!(!json["public"].is_null());
            assert!(json["password"].is_null());
            assert!(json["api_key"].is_null());
            assert!(json["secret_token"].is_null());
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_json_typed_and_skip_paths_combined() -> Result<()> {
        let values = vec![Value::String(
            br#"{"id": 1, "name": "Alice", "password": "secret", "score": 95}"#.to_vec(),
        )];

        let type_ = json_type_with_config(
            vec![
                ("id".to_string(), Box::new(Type::UInt32)),
                ("name".to_string(), Box::new(Type::String)),
            ],
            vec!["password".to_string()],
            vec![],
            None,
        );

        let state = JsonSerializer::analyze_values(&values, &type_)?;
        if let TypeSpecificState::Json(json_state) = &state {
            assert_eq!(json_state.typed_paths.len(), 2);
            assert!(json_state.dynamic_paths.contains(&"score".to_string()));
            assert!(!json_state.dynamic_paths.contains(&"password".to_string()));
            assert!(!json_state.dynamic_paths.contains(&"id".to_string()));
        }

        Ok(())
    }

    // ========== V1/V2/V3 Format Tests ==========

    #[tokio::test]
    async fn test_json_v1_format() -> Result<()> {
        use bytes::Buf;

        let values = vec![
            Value::String(br#"{"a": 1, "b": "hello"}"#.to_vec()),
            Value::String(br#"{"a": 2, "c": true}"#.to_vec()),
        ];
        let type_ = default_json_type();

        // Verify prefix format
        let mut output = vec![];
        let mut state = SerializerState::default();
        state.type_specific = JsonSerializer::analyze_values_with_version(
            &values,
            &type_,
            Some(JSON_OBJECT_VERSION_V1),
        )?;
        type_.serialize_prefix_async(&mut output, &mut state).await?;

        let mut reader = &output[..];
        let version = reader.get_u64_le();
        assert_eq!(version, JSON_OBJECT_VERSION_V1);
        // V1 has max_dynamic_paths first (1024 as varuint = 0x80 0x08)
        assert_eq!(reader[0], 0x80);
        assert_eq!(reader[1], 0x08);

        // Verify roundtrip
        let result =
            roundtrip_with_version(values.clone(), &type_, Some(JSON_OBJECT_VERSION_V1)).await?;
        assert_eq!(result.len(), values.len());

        Ok(())
    }

    #[tokio::test]
    async fn test_json_v2_format() -> Result<()> {
        use bytes::Buf;

        let values = vec![
            Value::String(br#"{"name": "Alice", "age": 30}"#.to_vec()),
            Value::String(br#"{"name": "Bob", "score": 95.5}"#.to_vec()),
        ];
        let type_ = default_json_type();

        // Verify prefix format
        let mut output = vec![];
        let mut state = SerializerState::default();
        state.type_specific = JsonSerializer::analyze_values_with_version(
            &values,
            &type_,
            Some(JSON_OBJECT_VERSION_V2),
        )?;
        type_.serialize_prefix_async(&mut output, &mut state).await?;

        let mut reader = &output[..];
        let version = reader.get_u64_le();
        assert_eq!(version, JSON_OBJECT_VERSION_V2);
        // V2 has no max_dynamic_paths, just num_paths
        assert_eq!(reader[0], 3); // 3 paths: age, name, score

        // Verify roundtrip
        let result =
            roundtrip_with_version(values.clone(), &type_, Some(JSON_OBJECT_VERSION_V2)).await?;
        assert_eq!(result.len(), values.len());

        Ok(())
    }

    #[tokio::test]
    async fn test_json_v2_path_frequency_selection() -> Result<()> {
        // With max_dynamic_paths=2, only most frequent paths become dynamic
        let values = vec![
            Value::String(br#"{"frequent": 1, "common": 10, "rare1": 100}"#.to_vec()),
            Value::String(br#"{"frequent": 2, "common": 20, "rare2": 200}"#.to_vec()),
            Value::String(br#"{"frequent": 3, "rare3": 300}"#.to_vec()),
        ];

        let type_ = json_type_with_config(vec![], vec![], vec![], Some(2));
        let state = JsonSerializer::analyze_values_with_version(
            &values,
            &type_,
            Some(JSON_OBJECT_VERSION_V2),
        )?;

        if let TypeSpecificState::Json(json_state) = &state {
            assert_eq!(json_state.dynamic_paths.len(), 2);
            assert!(json_state.dynamic_paths.contains(&"frequent".to_string()));
            assert!(json_state.dynamic_paths.contains(&"common".to_string()));

            let shared = json_state.shared_path_columns.as_ref().unwrap();
            assert_eq!(shared.len(), 3);
            assert!(shared.contains_key("rare1"));
            assert!(shared.contains_key("rare2"));
            assert!(shared.contains_key("rare3"));
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_json_v2_shared_data_roundtrip() -> Result<()> {
        let values = vec![
            Value::String(br#"{"main": 1, "overflow": 100}"#.to_vec()),
            Value::String(br#"{"main": 2, "overflow": 200}"#.to_vec()),
            Value::String(br#"{"main": 3}"#.to_vec()),
        ];

        let type_ = json_type_with_config(vec![], vec![], vec![], Some(1));
        let result =
            roundtrip_with_version(values.clone(), &type_, Some(JSON_OBJECT_VERSION_V2)).await?;
        assert_eq!(result.len(), values.len());

        for v in &result {
            match v {
                Value::Object(_) | Value::String(_) | Value::Dynamic(_, _) => {}
                #[cfg(feature = "serde")]
                Value::Json(_) => {}
                other => panic!("Unexpected value type: {other:?}"),
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_json_v2_no_overflow_when_under_limit() -> Result<()> {
        let values = vec![Value::String(br#"{"a": 1, "b": 2}"#.to_vec())];

        let type_ = json_type_with_config(vec![], vec![], vec![], Some(10)); // limit > paths
        let state = JsonSerializer::analyze_values_with_version(
            &values,
            &type_,
            Some(JSON_OBJECT_VERSION_V2),
        )?;

        if let TypeSpecificState::Json(json_state) = &state {
            assert_eq!(json_state.dynamic_paths.len(), 2);
            assert!(json_state.shared_path_columns.is_none());
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_json_v3_ignores_max_dynamic_paths() -> Result<()> {
        let values = vec![Value::String(br#"{"a": 1, "b": 2, "c": 3, "d": 4, "e": 5}"#.to_vec())];

        let type_ = json_type_with_config(vec![], vec![], vec![], Some(2)); // would limit V1/V2
        let state = JsonSerializer::analyze_values_with_version(&values, &type_, None)?; // V3

        if let TypeSpecificState::Json(json_state) = &state {
            assert_eq!(json_state.dynamic_paths.len(), 5); // V3 sends all
            assert!(json_state.shared_path_columns.is_none());
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_json_analyze_values_returns_json_state() -> Result<()> {
        let values = vec![
            Value::String(br#"{"name": "Alice"}"#.to_vec()),
            Value::String(br#"{"name": "Bob", "score": 95}"#.to_vec()),
        ];

        let type_ = default_json_type();
        let state = JsonSerializer::analyze_values(&values, &type_)?;

        assert!(matches!(state, TypeSpecificState::Json(_)));
        Ok(())
    }

    #[tokio::test]
    async fn test_json_string_to_numeric_parsing() -> Result<()> {
        let values = vec![Value::String(
            br#"{"str_int": "123", "str_float": "3.14", "digit": "0"}"#.to_vec(),
        )];

        let type_ = json_type_with_config(
            vec![
                ("str_int".to_string(), Box::new(Type::Int32)),
                ("str_float".to_string(), Box::new(Type::Float64)),
                ("digit".to_string(), Box::new(Type::UInt8)),
            ],
            vec![],
            vec![],
            None,
        );

        let result = roundtrip(values, &type_).await?;
        let json = parse_json_value(&result[0])?;

        assert_eq!(json["str_int"], 123);
        assert_eq!(json["str_float"], 3.14);
        assert_eq!(json["digit"], 0);

        Ok(())
    }

    // ========== ClickHouse Discriminator Ordering ==========

    #[tokio::test]
    async fn test_json_typed_paths_variant_discriminator_ordering() -> Result<()> {
        // ClickHouse sorts variant types alphabetically: Int64 (disc=0), String (disc=1)
        let values = vec![
            Value::String(br#"{"a": ["b0"]}"#.to_vec()),
            Value::String(br#"{"a": ["b1"]}"#.to_vec()),
        ];

        let type_ = json_type_with_config(
            vec![(
                "a".to_string(),
                Box::new(Type::Array(Box::new(Type::variant(vec![Type::String, Type::Int64])))),
            )],
            vec![],
            vec![],
            None,
        );

        let state = JsonSerializer::analyze_values(&values, &type_)?;
        if let TypeSpecificState::Json(json_state) = &state {
            let typed_cols = json_state.typed_path_columns.as_ref().unwrap();
            let a_column = typed_cols.get("a").unwrap();

            for array_val in a_column {
                if let Value::Array(arr) = array_val {
                    if let Some(Value::Variant(discriminator, _)) = arr.first() {
                        // String values get discriminator 1 (Int64=0, String=1 alphabetically)
                        assert_eq!(*discriminator, 1);
                    }
                }
            }
        }

        Ok(())
    }
}
