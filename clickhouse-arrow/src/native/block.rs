use std::str::FromStr;

use indexmap::IndexMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::block_info::BlockInfo;
use super::protocol::DBMS_MIN_PROTOCOL_VERSION_WITH_CUSTOM_SERIALIZATION;
use crate::deserialize::ClickHouseNativeDeserializer;
use crate::formats::protocol_data::ProtocolData;
use crate::formats::{DeserializerState, SerializerState, TypeSpecificState};
use crate::io::{ClickHouseRead, ClickHouseWrite};
use crate::native::types::serialize::dynamic::DynamicSerializer;
use crate::native::types::serialize::json::JsonSerializer;
use crate::native::values::Value;
use crate::prelude::*;
use crate::serialize::ClickHouseNativeSerializer;
use crate::{Error, Result, Row, Type};

#[derive(Debug, Clone, Default)]
/// A chunk of data in columnar form.
pub struct Block {
    /// Metadata about the block
    pub info:         BlockInfo,
    /// The number of rows contained in the block
    pub rows:         u64,
    /// The type of each column by name, in order.
    pub column_types: Vec<(String, Type)>,
    /// The data of each column by name, in order. All `Value` should correspond to the associated
    /// type in `column_types`.
    pub column_data:  Vec<Value>,
    /// Optional JSON/Dynamic serialization version override (for testing)
    /// When set, forces the specified version instead of auto-detecting.
    /// Values: 0 = V1, 2 = V2, 3 = V3/FLATTENED
    pub json_version: Option<u64>,
}

// Iterator type for `take_iter_rows`
pub struct BlockRowValueIter<'a, I>
where
    I: Iterator<Item = Value>,
{
    column_data: Vec<(&'a str, &'a Type, I)>,
}

impl<'a, I> Iterator for BlockRowValueIter<'a, I>
where
    I: Iterator<Item = Value>,
{
    type Item = Vec<(&'a str, &'a Type, Value)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.column_data.is_empty() {
            return None;
        }
        let mut out = Vec::new();
        for (name, type_, pop) in &mut self.column_data {
            out.push((*name, *type_, pop.next()?));
        }
        Some(out)
    }
}

impl Block {
    /// Iterate over all rows with owned values.
    pub fn take_iter_rows(&mut self) -> BlockRowValueIter<'_, impl Iterator<Item = Value>> {
        #[allow(clippy::cast_possible_truncation)]
        let rows = self.rows as usize;
        let mut column_data = std::mem::take(&mut self.column_data);
        let mut out = Vec::with_capacity(rows);
        for (name, type_) in &self.column_types {
            let mut column = Vec::with_capacity(rows);
            let column_slice = column_data.drain(..rows);
            column.extend(column_slice);
            out.push((&**name, type_.strip_low_cardinality(), column.into_iter()));
        }
        BlockRowValueIter { column_data: out }
    }

    /// Estimate the serialized size of this block for buffer allocation
    pub fn estimate_size(&self) -> usize {
        let mut size = 16; // BlockInfo + columns count + rows count

        #[allow(clippy::cast_possible_truncation)]
        let rows = self.rows as usize;

        for (name, type_) in &self.column_types {
            // Column name + type string
            size += name.len() + type_.to_string().len() + 10; // +10 for length prefixes and overhead

            // Estimate data size
            size += rows * type_.estimate_capacity();
        }

        // Add 20% buffer for overhead
        size * 6 / 5
    }

    /// Create a block from a vector of rows and a schema.
    ///
    /// # Errors
    ///
    /// Returns an error if the number of rows does not match the number of columns, serializing
    /// fails, or the field cannot be found in the schema.
    pub fn from_rows<T: Row>(rows: Vec<T>, schema: Vec<(String, Type)>) -> Result<Self> {
        let row_len = rows.len();
        let row_col_len = schema.len() * rows.len();

        let mut columns = schema
            .iter()
            .map(|(name, _)| (name.clone(), Vec::with_capacity(rows.len())))
            .collect::<IndexMap<String, Vec<_>>>();

        rows.into_iter()
            .enumerate()
            .map(|(i, x)| {
                x.serialize_row(&schema)
                    .inspect_err(|error| error!(?error, "serialize error during insert (ROW {i})"))
                    .map(|r| (i, r))
            })
            .try_for_each(|result| -> Result<()> {
                let (i, x) = result?;
                for (key, value) in x {
                    let type_ = &schema
                        .iter()
                        .find(|(n, _)| n == &*key)
                        .ok_or_else(|| {
                            Error::Protocol(format!(
                                "missing type for data in row {i}, column: {key}"
                            ))
                        })?
                        .1;
                    type_.validate_value(&value).inspect_err(|error| {
                        tracing::error!(
                            ?error,
                            ?value,
                            ?key,
                            ?type_,
                            "Value validation failed for row {i}"
                        );
                    })?;
                    let column = columns.get_mut(key.as_ref()).ok_or(Error::Protocol(format!(
                        "missing column for data in row {i}, column: {key}"
                    )))?;
                    column.push(value);
                }
                Ok(())
            })?;

        let mut column_data = Vec::with_capacity(row_col_len);

        // Move the values into a flattened vector
        for (_, mut values) in columns.drain(..) {
            column_data.append(&mut values);
        }

        Ok(Block {
            info: BlockInfo::default(),
            rows: row_len as u64,
            column_types: schema,
            column_data,
            json_version: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::connection::ClientMetadata;
    use crate::formats::sealed::ClientFormatImpl;
    use crate::native::protocol::{CompressionMethod, DBMS_TCP_PROTOCOL_VERSION};
    use crate::{ArrowOptions, NativeFormat};

    #[tokio::test]
    async fn compressed_roundtrip_dynamic_json_map() {
        let rows = 2u64;
        let column_types = vec![
            ("m".to_string(), Type::Map(Box::new(Type::String), Box::new(Type::Int32))),
            ("d".to_string(), Type::Dynamic { max_types: None }),
            ("j".to_string(), Type::JSON {
                max_dynamic_paths: None,
                max_dynamic_types: None,
                typed_paths:       vec![],
                skip_exact:        vec![],
                skip_regex:        vec![],
            }),
        ];

        let map_row0 =
            Value::Map(vec![Value::String(b"k1".to_vec()), Value::String(b"k2".to_vec())], vec![
                Value::Int32(10),
                Value::Int32(20),
            ]);
        let map_row1 = Value::Map(vec![Value::String(b"a".to_vec())], vec![Value::Int32(-1)]);

        #[cfg(feature = "serde")]
        let json_row0 = Value::Json(serde_json::json!({"a": 1, "b": "z"}));
        #[cfg(not(feature = "serde"))]
        let json_row0 = Value::Object(br#"{"a":1,"b":"z"}"#.to_vec());

        #[cfg(feature = "serde")]
        let json_row1 = Value::Json(serde_json::json!({"c": [1,2,3]}));
        #[cfg(not(feature = "serde"))]
        let json_row1 = Value::Object(br#"{"c":[1,2,3]}"#.to_vec());

        // Column-major order flattened rows
        // Column-major order: m[0..rows], d[0..rows], j[0..rows]
        let d_row0 = Value::UInt64(42);
        let d_row1 = Value::String(b"x".to_vec());
        let column_data = vec![
            map_row0.clone(),
            map_row1.clone(),
            d_row0,
            d_row1,
            json_row0.clone(),
            json_row1.clone(),
        ];

        let block = Block {
            info: BlockInfo::default(),
            rows,
            column_types: column_types.clone(),
            column_data,
            ..Default::default()
        };

        // Write compressed
        let metadata = ClientMetadata {
            client_id:      1,
            compression:    CompressionMethod::LZ4,
            arrow_options:  ArrowOptions::default(),
            server_version: None,
        };
        let mut buffer = Vec::new();
        NativeFormat::write(
            &mut buffer,
            block.clone(),
            Qid::default(),
            Some(&block.column_types),
            DBMS_TCP_PROTOCOL_VERSION,
            metadata,
        )
        .await
        .expect("write compressed block");

        // Read back
        let mut cursor = std::io::Cursor::new(buffer);
        let mut state = DeserializerState::default();
        let read_block =
            NativeFormat::read(&mut cursor, DBMS_TCP_PROTOCOL_VERSION, metadata, &mut state)
                .await
                .expect("read compressed block")
                .expect("some block");

        assert_eq!(read_block.rows, rows);
        assert_eq!(read_block.column_types, column_types);
        assert_eq!(read_block.column_data.len(), block.column_data.len());
    }
}

impl ProtocolData<Self, ()> for Block {
    type Options = Option<crate::client::connection::ClientMetadata>;

    // this code is insanely duplicative..
    async fn write_async<W: ClickHouseWrite>(
        mut self,
        writer: &mut W,
        revision: u64,
        _header: Option<&[(String, Type)]>,
        options: Self::Options,
    ) -> Result<()> {
        if revision > 0 {
            self.info.write_async(writer).await?;
        }

        let columns = self.column_types.len();

        #[allow(clippy::cast_possible_truncation)]
        let rows = self.rows as usize;

        writer.write_var_uint(columns as u64).await?;
        writer.write_var_uint(self.rows).await?;
        tracing::trace!(columns, rows=%self.rows, "block.write_async: dims");

        for (name, col_type) in self.column_types {
            let mut values = Vec::with_capacity(rows);
            values.extend(self.column_data.drain(..rows));

            if values.len() != rows {
                return Err(Error::Protocol(format!(
                    "row and column length mismatch. {} != {}",
                    values.len(),
                    rows
                )));
            }

            // EncodeStart
            // Compute type string, allowing conditional Object('json') for older servers TODO:
            // just use col_type.to_string()? directly. This helper does the same thing that the
            // col_type.to_string() does for Object
            let ty_str = format_type_for_header(&col_type, &options);
            tracing::trace!(col=%name, ty=%ty_str, "block.write_async: column header");
            writer.write_string(&name).await?;
            writer.write_string(ty_str).await?;

            if self.rows > 0 {
                // We do not write sparse/custom columns on client side; let the server choose.
                // NOTE: We currently do NOT implement client-side sparse/custom serialization
                // for sized primitives. Always emit 0 (no custom serialization) for writes.
                // The server may still choose sparse on read responses; our reader supports it.
                if revision >= DBMS_MIN_PROTOCOL_VERSION_WITH_CUSTOM_SERIALIZATION {
                    writer.write_u8(0).await?;
                }

                let mut state = SerializerState::default();
                if let Some(metadata) = options
                    && let Some(version) = metadata.server_version
                {
                    state = state.with_server_version(version);
                }

                // For Dynamic/JSON types, analyze values before writing prefix
                // Use json_version override if set (for testing V1/V2 serialization)
                state.type_specific = if matches!(col_type, Type::Dynamic { .. }) {
                    DynamicSerializer::analyze_values_with_version(&values, self.json_version)
                } else if matches!(col_type, Type::JSON { .. }) {
                    JsonSerializer::analyze_values_with_version(
                        &values,
                        &col_type,
                        self.json_version,
                    )?
                } else {
                    TypeSpecificState::None
                };

                col_type.serialize_prefix_async(writer, &mut state).await?;
                col_type.serialize_column(values, writer, &mut state).await?;
            }
        }
        Ok(())
    }

    async fn read_async<R: ClickHouseRead>(
        reader: &mut R,
        revision: u64,
        _options: Self::Options,
        state: &mut DeserializerState,
    ) -> Result<Self> {
        let info =
            if revision > 0 { BlockInfo::read_async(reader).await? } else { BlockInfo::default() };

        #[allow(clippy::cast_possible_truncation)]
        let columns = reader.read_var_uint().await? as usize;
        let rows = reader.read_var_uint().await?;

        let mut block = Block {
            info,
            rows,
            column_types: Vec::with_capacity(columns),
            column_data: Vec::with_capacity(columns),
            json_version: None,
        };

        for i in 0..columns {
            let name_bytes = reader
                .read_string()
                .await
                .inspect_err(|e| error!("reading column name bytes (index {i}): {e}"))?;
            let name = match String::from_utf8(name_bytes.clone()) {
                Ok(s) => s,
                Err(e) => {
                    let hex: String = name_bytes
                        .iter()
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join("");
                    error!(?e, hex = %hex, len = name_bytes.len(), index = i, "reading column name (invalid utf-8)");
                    return Err(crate::Error::from(e));
                }
            };

            let type_bytes = reader
                .read_string()
                .await
                .inspect_err(|e| error!("reading column type bytes (name {name}): {e}"))?;
            let type_name = match String::from_utf8(type_bytes.clone()) {
                Ok(s) => s,
                Err(e) => {
                    let hex: String = type_bytes
                        .iter()
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join("");
                    error!(?e, hex = %hex, len = type_bytes.len(), name = %name, "reading column type (invalid utf-8)");
                    return Err(crate::Error::from(e));
                }
            };

            let type_ = Type::from_str(&type_name).inspect_err(|error| {
                error!(?error, "Type deserialize failed: name={name}, type={type_name}");
            })?;

            // Custom/Sparse serialization plan (server-side flag + kinds plan)
            let mut _has_custom_serialization = false;
            if revision >= DBMS_MIN_PROTOCOL_VERSION_WITH_CUSTOM_SERIALIZATION {
                _has_custom_serialization = reader.read_u8().await? != 0;
            }

            let mut row_data = if rows > 0 {
                // Build a kind plan by consuming kind bytes for this column's type tree.
                // We parse one kind per node; for Tuple we also parse element kinds recursively.
                state.kind_plan = None;
                state.sparse_runtime.clear();
                if _has_custom_serialization {
                    let mut plan = std::collections::BTreeMap::<Vec<u16>, u8>::new();

                    // Iterative DFS: node first, then children
                    // Tuple emits a kind for itself and all children; for Array/Map/Nullable,
                    // servers may include child kinds as well — handle them recursively.
                    let mut stack: Vec<(Vec<u16>, &Type)> = vec![(Vec::new(), &type_)];
                    while let Some((path, ty)) = stack.pop() {
                        let kind = reader.read_u8().await?;
                        let _ = plan.insert(path.clone(), kind);
                        match ty {
                            Type::Tuple(children) => {
                                for (idx, child) in children.iter().enumerate().rev() {
                                    let mut next = path.clone();
                                    #[allow(clippy::cast_possible_truncation)]
                                    next.push(idx as u16);
                                    stack.push((next, child));
                                }
                            }
                            Type::TupleNamed(fields) => {
                                for (idx, (_, child)) in fields.iter().enumerate().rev() {
                                    let mut next = path.clone();
                                    #[allow(clippy::cast_possible_truncation)]
                                    next.push(idx as u16);
                                    stack.push((next, child));
                                }
                            }
                            Type::Array(inner) => {
                                let mut next = path.clone();
                                next.push(0);
                                stack.push((next, inner));
                            }
                            Type::Map(key, value) => {
                                let mut kpath = path.clone();
                                kpath.push(0);
                                stack.push((kpath, key));
                                let mut vpath = path.clone();
                                vpath.push(1);
                                stack.push((vpath, value));
                            }
                            Type::Nullable(inner) => {
                                let mut next = path.clone();
                                next.push(0);
                                stack.push((next, inner));
                            }
                            // Do not descend into LowCardinality/Variant/JSON/Dynamic/Object
                            _ => {}
                        }
                    }
                    state.kind_plan = Some(plan);
                }

                // Let types read any non-sparse prefixes as usual (e.g., LC, variant, dynamic,
                // json)
                type_.deserialize_prefix_async(reader, state).await?;

                #[allow(clippy::cast_possible_truncation)]
                type_
                    .deserialize_column(reader, rows as usize, state)
                    .await
                    .inspect_err(|e| error!("deserialize (name {name}): {e}"))?
            } else {
                vec![]
            };

            block.column_types.push((name, type_));
            block.column_data.append(&mut row_data);

            // Clear per-column plan/state before the next column
            state.kind_plan = None;
            state.sparse_runtime.clear();
        }

        Ok(block)
    }
}

fn format_type_for_header(
    ty: &Type,
    _options: &Option<crate::client::connection::ClientMetadata>,
) -> String {
    match ty {
        // Safe, backward-compatible emission for legacy Object JSON columns
        Type::Object => "Object('json')".to_string(),
        _ => ty.to_string(),
    }
}
