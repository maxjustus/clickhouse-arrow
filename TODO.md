# Test Refactoring TODO

Based on analysis of commit `02194e7` which applied macro-based test consolidation to `primitive.rs`, the following files would benefit from similar refactoring patterns.

## High Priority (Major Impact)

### 1. `clickhouse-arrow/src/arrow/serialize/binary.rs`
- **Pattern**: Complete duplication - has both sync and async serialize functions but tests are duplicated between `tests_async` and `tests_sync` modules
- **Opportunity**: Apply exact same `primitive_test!` and `test_both_serialize!` macro patterns
- **Impact**: Eliminate ~300+ lines of duplicated test code across 16+ test functions
- **Status**: Ready for refactoring - identical pattern to what was fixed in primitive.rs

### 2. `clickhouse-arrow/src/arrow/serialize/list.rs` 
- **Pattern**: Two separate test modules with identical test logic, just calling different helper functions (`test_type_serializer` async vs sync)
- **Opportunity**: Create unified macros similar to primitive.rs approach
- **Impact**: Cut test code in half while ensuring both sync/async paths tested consistently
- **Status**: Ready for refactoring

### 3. `clickhouse-arrow/src/native/types/serialize/dynamic.rs`
- **Pattern**: Missing coverage - 14 sync tests but only 1 async test
- **Opportunity**: Add missing async test coverage using macro approach from primitive.rs
- **Impact**: Better test coverage + consistent patterns across codebase
- **Status**: Needs analysis of which functions have async variants

## Medium Priority (Good ROI)

### 4. `clickhouse-arrow/src/arrow/serialize/enums.rs`
- **Pattern**: 16 tests that likely follow similar duplication patterns
- **Opportunity**: Apply same macro-based consolidation
- **Status**: Needs investigation of test structure

### 5. `clickhouse-arrow/src/arrow/serialize/map.rs`
- **Pattern**: 7 tests that may benefit from consolidation
- **Opportunity**: Apply same refactoring patterns
- **Status**: Needs investigation of test structure

## Refactoring Patterns to Apply

Based on the successful refactoring in `primitive.rs`:

1. **Macro-based test generation**
   ```rust
   macro_rules! primitive_test {
       ($name:ident, $hint:expr, $array:expr, $dt:expr, $expected:expr) => {
           #[tokio::test]
           async fn $name() {
               // Test both sync and async paths
           }
       };
   }
   ```

2. **Dual sync/async testing helper**
   ```rust
   macro_rules! test_both_serialize {
       ($type:expr, $column:expr, $data_type:expr) => {{
           // Returns (sync_result, async_result, sync_writer, async_writer)
       }};
   }
   ```

3. **Code formatting improvements**
   - Break long lines for better readability
   - Consistent formatting across test modules
   - Better organization of test data

4. **Consistent test coverage**
   - Ensure both sync and async code paths are tested
   - Eliminate gaps in test coverage
   - Use unified approach across all serialize modules

## Benefits of This Refactoring

1. **DRY Principle**: Eliminate hundreds of lines of duplicated test code
2. **Maintainability**: Single source of truth for test logic  
3. **Coverage**: Ensure both sync and async paths are tested consistently
4. **Consistency**: Apply uniform testing patterns across the codebase
5. **Readability**: Better formatted, more organized test code

## Implementation Notes

- Start with `binary.rs` as it's most similar to the completed `primitive.rs` refactoring
- Use the same macro patterns established in the recent commit
- Ensure all existing test behavior is preserved
- Run `cargo test --features test-utils` to verify no regressions
- Follow existing code style and formatting conventions

## Files Already Refactored

- ✅ `clickhouse-arrow/src/arrow/serialize/primitive.rs` (commit `02194e7`)
- ✅ `clickhouse-arrow/src/native/types/tests.rs` (formatting improvements)
- ✅ `clickhouse-arrow/src/native/values/tests.rs` (formatting improvements)