# ClickHouse Native Protocol: Substream Architecture and Implementation Plan

This document outlines the multi-stream serialization architecture used by the ClickHouse native protocol for complex types. It includes references to the canonical C++ implementation, a specification for the binary wire format, and a concrete plan for implementing this architecture in Rust.

## 1. Core Concept: The Multi-Stream Architecture

Instead of writing all column data into a single, sequential binary stream, the ClickHouse `serializeBinaryBulk` mechanism uses multiple, parallel, in-memory streams (buffers). Each distinct part of a complex type (e.g., a `LowCardinality` dictionary vs. its indexes) is written to its own dedicated, named substream.

At the end of the serialization process for a block, these named substreams are assembled into a single binary payload that is sent over the network. The server then deserializes this payload by first unpacking the substreams and then passing the correct buffer to the appropriate deserializer.

This approach is necessary for types whose data is not easily interleaved and is more efficiently processed in separate chunks.

## 2. Key C++ Source Code References

The canonical implementation is spread across these key files in the [ClickHouse repository](https://github.com/ClickHouse/ClickHouse):

-   **`src/IO/ISerialization.h`**: Defines the foundational `struct SerializeBinaryBulkSettings`, the `enum class Substream`, and the `SubstreamPath` type (`std::vector<Substream>`). This is the context object that carries the stream getter and path information.
-   **`src/Processors/Formats/Impl/NativeWriter.cpp`** (or `NativeBlockOutputStream.cpp`): The top-level class that orchestrates block serialization. It creates the map of substreams and defines the `getter` lambda that provides serializers with the correct buffer. It also assembles the final wire format.
-   **`src/DataTypes/Serializations/SerializationObject.cpp`**: Demonstrates how a container type (`Object`/`JSON`) pushes path components (e.g., `Substream::ObjectTypedPath`) to manage the serialization context for its sub-columns.
-   **`src/DataTypes/Serializations/SerializationLowCardinality.cpp`**: The primary example of a complex serializer. It requests distinct substreams (`DictionaryKeys`, `DictionaryIndexes`) and writes different parts of its data to each.
-   **`src/DataTypes/Serializations/SerializationVariant.cpp`**: Another example that uses `VariantDiscriminators` and `VariantElements` substreams.

## 3. Code Snippets and Explanation

#### The Context Object (`ISerialization.h`)
This struct is passed down the entire call stack. The `path` acts as a stack, and the `getter` is a function pointer to the stream manager.

```cpp
using SubstreamPath = std::vector<Substream>;
using SubstreamGetter = std::function<WriteBuffer *(const SubstreamPath &)>;

struct SerializeBinaryBulkSettings
{
    SubstreamGetter getter;
    SubstreamPath path;
    // ... other settings
};
```

#### The Stream Manager and "Getter" (`NativeWriter.cpp`)
The top-level writer owns the map of buffers and provides the `getter` function.

```cpp
// A map from a specific path to its own in-memory write buffer
using Substreams = std::map<SubstreamPath, WriteBufferPtr>;
Substreams substreams;

// The getter lambda, defined in the writer
settings.getter = [&](const SubstreamPath & path) -> WriteBuffer *
{
    auto it = substreams.find(path);
    if (it == substreams.end())
    {
        // If a stream for this path doesn't exist, create a new in-memory buffer for it.
        it = substreams.emplace(path, std::make_unique<WriteBufferFromOwnString>()).first;
    }
    return it->second.get();
};
```

#### A Serializer Using Substreams (`SerializationLowCardinality.cpp`)
A complex serializer manipulates the path to request the specific stream it needs.

```cpp
void SerializationLowCardinality::serializeBinaryBulkStatePrefix(...)
{
    // 1. PUSH: Add the desired substream to the path stack.
    settings.path.push_back(Substream::DictionaryKeys);

    // 2. GET: Call the getter to get the specific buffer for DictionaryKeys.
    auto * stream = settings.getter(settings.path);

    // 3. POP: Clean up the path stack for the next serializer.
    settings.path.pop_back();

    // 4. WRITE: Write data (e.g., a version number) into that specific buffer.
    writeBinaryLittleEndian(key_version, *stream);
}
```

## 4. Binary Wire Format Specification for Substreams

When a block is serialized using substreams, it is sent as a `Data` packet (type `0x02`). The payload of this packet is assembled as follows:

1.  **Temporary Block Info & Metadata** (standard for all data packets):
    -   `BlockInfo` (as needed)
    -   `[num_columns]` (VarUInt)
    -   `[num_rows]` (VarUInt)

2.  **Substream Payload**:
    -   `[substream_count]` (VarUInt): The number of distinct substreams that were written to.
    -   Then, for each of the `substream_count` streams:
        -   `[substream_name]` (String): The name of the substream, generated by serializing the `SubstreamPath`. Example: `"column_name.ObjectData.ObjectTypedPath.id.DictionaryKeys"`.
        -   `[compression_method]` (UInt8): e.g., `0x82` for LZ4.
        -   `[compressed_size]` (UInt64 LE): The size of the data block that follows.
        -   `[uncompressed_size]` (UInt64 LE): The size of the data after decompression.
        -   `[data]` (bytes): The actual (potentially compressed) content of the substream's buffer.

## 5. Rust Implementation Plan

This plan introduces a new I/O management layer without fundamentally rewriting the low-level serialization logic of simple types.

### Phase 1: I/O Abstraction Layer

1.  **Define `Substream` Enum**:
    -   Create a Rust `enum Substream` mirroring the C++ version, with variants like `ObjectData`, `ObjectTypedPath { name: String }`, `DictionaryKeys`, etc. It must be `Hash`, `Eq`, and `Clone`.

2.  **Define `SubstreamPath`**:
    -   Create a struct or type alias: `type SubstreamPath = Vec<Substream>`.

3.  **Create `MultiStreamWriter`**:
    -   This struct will be the central I/O manager.
    -   **Fields**: `streams: HashMap<SubstreamPath, Vec<u8>>`.
    -   **Methods**:
        -   `pub fn get_stream(&mut self, path: &SubstreamPath) -> &mut Vec<u8>`: Gets or creates the buffer for a given path.
        -   `pub fn assemble_block(&self) -> Result<Vec<u8>>`: Assembles the final wire format as specified in section 4. This will involve iterating the map, writing names, compressing data, and writing sizes and payloads.

### Phase 2: State Management Refactor

1.  **Update `SerializerState`**:
    -   Add `pub path: SubstreamPath` to the `SerializerState`. This will track the current serialization context.
    -   The `SerializerState` will no longer hold a writer itself.

### Phase 3: Refactor Serializers to Use Substreams

1.  **Update `ClickHouseNativeSerializer` Trait**:
    -   Change the method signatures to accept the new context:
        ```rust
        // Before
        fn serialize_prefix<W: ClickHouseWrite>(...);
        // After
        fn serialize_prefix(
            &self,
            type_: &Type,
            state: &mut SerializerState,
            writer: &mut MultiStreamWriter
        ) -> Result<()>;
        ```

2.  **Update Complex Serializers (`LowCardinality`, `Variant`, `Object`)**:
    -   These serializers will now actively manage the `state.path`.
    -   **Pattern**:
        ```rust
        // In LowCardinalitySerializer::serialize_prefix
        state.path.push(Substream::DictionaryKeys);
        let key_stream = writer.get_stream(&state.path);
        key_stream.put_u64_le(VERSION)?; // Using a custom extension trait for Write
        state.path.pop();
        // ... repeat for other substreams as needed
        ```

3.  **Update Simple Serializers (`Int`, `String`, etc.)**:
    -   These only require a signature change.
    -   Their logic remains the same. They will simply call `writer.get_stream(&state.path)` to get the "current" stream (which is typically the default data stream) and write their content to it. They don't need to push/pop.

### Phase 4: Top-Level Orchestration

1.  **Refactor `Block::write_internal`**:
    -   This function will now be the owner of the `MultiStreamWriter`.
    -   **Flow**:
        1.  Create a new `MultiStreamWriter`.
        2.  Create a new `SerializerState`.
        3.  Loop through each column in the block.
        4.  Set the initial path in `state.path` (e.g., `vec![Substream::ColumnData { name: "col_name" }]`).
        5.  Call `col.serialize_prefix(..., &mut writer)`.
        6.  Call `col.serialize_column(..., &mut writer)`.
        7.  After all columns are processed, call `writer.assemble_block()`.
        8.  Write the final, assembled `Vec<u8>` to the actual network/output stream.

### Phase 5: Deserialization (Mirror Architecture)

1.  **Create `MultiStreamReader`**:
    -   This struct will be initialized from an incoming `Data` packet. It will parse the substream payload into a `HashMap<SubstreamPath, &'a [u8]>`.
2.  **Refactor Deserializers**:
    -   Update signatures to accept `&mut MultiStreamReader`.
    -   Update logic to request the correct byte slice for the current `state.path` and deserialize from it.

### Phase 6: Testing

1.  **Unit Tests**: Create unit tests for `MultiStreamWriter::assemble_block` to ensure wire format correctness.
2.  **Integration Tests**: Adapt existing tests to the new serialization signatures.
3.  **Enable `e2e` Tests**: The ultimate goal is to enable and pass the `JSON` typed path tests involving `LowCardinality` and `Variant` against a live ClickHouse server.
