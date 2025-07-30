# Refactoring Thread-Local State to Explicit State Passing

## Current State

The Dynamic and JSON type serializers/deserializers currently use `thread_local!` storage to pass metadata between different phases of serialization:

1. **Dynamic Type**:
   - `serialize/dynamic.rs`: Uses `DYNAMIC_CACHE` thread-local
   - `deserialize/dynamic.rs`: Uses `METADATA` thread-local

2. **JSON Type**:
   - `serialize/json.rs`: Uses `JSON_CACHE` thread-local
   - `deserialize/json.rs`: Uses `JSON_STATE` thread-local

## The Problem

Thread-local storage introduces several issues:
- Makes functions non-reentrant
- Potential issues if async tasks are moved between threads
- Implicit data flow that's harder to reason about
- Testing complexity

## Proposed Solution

### 1. Extend State Structures

Add a type-specific state storage mechanism to `SerializerState` and `DeserializerState`:

```rust
// In formats.rs
pub(crate) struct SerializerState<T: Default = ()> {
    pub(crate) options: Option<ArrowOptions>,
    pub(crate) serializer: T,
    pub(crate) server_version: Option<(u64, u64, u64)>,
    pub(crate) type_state: HashMap<TypeId, Box<dyn Any + Send + Sync>>, // NEW
}
```

### 2. Create Type-Specific State Structures

```rust
// In types/state.rs
pub struct DynamicSerializerCache {
    pub type_names: Vec<String>,
    pub type_map: HashMap<String, (usize, Type)>,
    pub total_types: usize,
}

pub struct JsonSerializerCache {
    pub paths: Vec<String>,
    pub path_columns: BTreeMap<String, Vec<Value>>,
    pub rows: usize,
}
// ... similar for deserializer caches
```

### 3. Update Method Signatures

Change `analyze_values` to accept state:
```rust
// Before
pub(crate) fn analyze_values(values: &[Value])

// After
pub(crate) fn analyze_values(values: &[Value], state: &mut SerializerState) -> Result<()>
```

### 4. Update Call Sites

Update all places where these methods are called:
- `native/block.rs`: Pass state to `analyze_values`
- Type serialization methods: Use state instead of thread_local

## Implementation Steps

1. **Phase 1**: Add state storage to SerializerState/DeserializerState
   - Add the `type_state` field
   - Implement helper methods for type-safe access

2. **Phase 2**: Create parallel implementations
   - Keep existing thread_local versions temporarily
   - Create new versions that use explicit state
   - Mark old versions as deprecated

3. **Phase 3**: Migrate call sites
   - Update block.rs to pass state
   - Update tests to use new API

4. **Phase 4**: Remove thread_local usage
   - Delete old implementations
   - Remove thread_local declarations

## Benefits

1. **Thread Safety**: No reliance on thread-local storage
2. **Testability**: Easier to test with explicit state
3. **Clarity**: Data flow is explicit and visible
4. **Async Safety**: No concerns about task migration

## Challenges

1. **API Changes**: Requires updating method signatures throughout the codebase
2. **Backward Compatibility**: May need a migration period
3. **Performance**: Need to ensure no performance regression

## Alternative Approaches Considered

1. **Using the generic parameter T in State**: Would require changes throughout the codebase where `SerializerState<()>` is used
2. **Creating separate state types**: Would complicate the API significantly
3. **Using Arc<Mutex<>>**: Would add unnecessary synchronization overhead

## Conclusion

While this refactoring would improve the architecture, it's a significant change that touches many parts of the codebase. It should be done carefully with proper testing and possibly in phases to minimize disruption.