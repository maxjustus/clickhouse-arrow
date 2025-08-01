# Test Consolidation TODO List

## Completed Targets ✅

1. ✅ **Remove debug println! statements from JSON serialization tests** (medium)
2. ✅ **Analyze test verbosity and create consolidation plan** (medium)  
3. ✅ **Create test consolidation implementation plan** (high)
4. ✅ **MEGA TARGET: arrow/serialize/primitive.rs** (2727 lines, 183 tests) - consolidate dual async/sync test suites **[-799 lines]**
5. ✅ **MAJOR TARGET: native/types/tests.rs** (969 lines, 46 tests) - consolidate roundtrip patterns **[-196 lines]**
6. ✅ **MAJOR TARGET: native/values/tests.rs** (875 lines, 54 tests) - consolidate FromSql/ToSql patterns **[-180 lines]**
7. ✅ **Consolidate JSON serialization test duplication** (8 test functions with identical 45-line patterns) **[-246 lines]**

**TOTAL COMPLETED: -1,940 lines eliminated!** 🎉

## New Targets (Branch-Specific Test Additions) 🎯

8. ✅ **NEW MEGA TARGET: native/types/deserialize/json.rs** (+649 lines) - consolidate JSON deserialization tests **[-178 lines]** 
9. ✅ **NEW MAJOR TARGET: native/types/serialize/dynamic.rs** (+551 lines) - consolidate Dynamic serialization tests **[-115 lines]**
10. ✅ **NEW MAJOR TARGET: native/types/deserialize/variant.rs** (+497 lines) - consolidate Variant deserialization tests **[-157 lines]**
11. ⏳ **NEW BIG TARGET: tests/tests/native.rs** (+421 lines) - consolidate native integration tests (MEDIUM PRIORITY)
12. ⏳ **NEW BIG TARGET: native/types/serialize/variant.rs** (+370 lines) - consolidate Variant serialization tests (MEDIUM PRIORITY)
13. ⏳ **NEW GOOD TARGET: native/types/deserialize/dynamic.rs** (+312 lines) - consolidate Dynamic deserialization tests (MEDIUM PRIORITY)

## Strategy

Focus on **new test code added in this branch vs main** rather than existing legacy tests. These files contain substantial test additions that can be consolidated using the proven macro-based approach.

**Estimated potential additional reduction: 1,500+ lines**

**Target: 3,000+ total lines eliminated** 🚀

## Status Legend
- ✅ Completed
- 🔄 In Progress  
- ⏳ Pending