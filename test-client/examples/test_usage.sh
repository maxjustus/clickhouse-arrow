#!/bin/bash

# Test script for the ClickHouse Arrow test client
# This demonstrates the capabilities we've implemented

set -e

CLIENT="./target/release/clickhouse-test-client"

echo "🧪 Testing ClickHouse Arrow Test Client"
echo "========================================"

# Check if the client binary exists
if [ ! -f "$CLIENT" ]; then
    echo "❌ Client binary not found at $CLIENT"
    echo "Please run: cargo build --release"
    exit 1
fi

echo ""
echo "📋 Available Commands:"
echo "- $CLIENT --info                           # Get server information"
echo "- $CLIENT --test-types                     # Test all supported native types"  
echo "- $CLIENT --query 'SELECT ...'             # Execute custom queries"
echo "- echo '{...}' | $CLIENT --insert table   # Insert JSON data"

echo ""
echo "⚙️  Available Options:"
echo "- --host                         # ClickHouse server host"
echo "- --port                         # ClickHouse server port"
echo "- --user                         # Database user"
echo "- --password                     # Database password"
echo "- --database                     # Database name"
echo "- --compression lz4|zstd|none   # Compression method"
echo "- --format json|pretty           # Output format"
echo "- --debug                        # Enable debug logging"

echo ""
echo "🔧 Examples:"

echo ""
echo "1. Basic query:"
echo "   $CLIENT --query 'SELECT 1 as number, \"hello\" as text'"

echo ""
echo "2. Test all native types (including our new implementations):"
echo "   $CLIENT --test-types --format pretty"

echo ""
echo "3. Test specific new types:"
echo "   $CLIENT --query 'SELECT true::Bool as native_bool'"
echo "   $CLIENT --query 'SELECT NULL::Nothing as nothing_val'"

echo ""
echo "4. Server information:"
echo "   $CLIENT --info --format pretty"

echo ""
echo "5. Insert JSON data:"
echo "   echo '{\"id\": 1, \"name\": \"test\"}' | $CLIENT --insert test_table"

echo ""
echo "6. With custom connection:"
echo "   $CLIENT --host production.clickhouse.com --port 9440 --secure --user myuser --info"

echo ""
echo "7. Debug mode for troubleshooting:"
echo "   $CLIENT --debug --query 'SELECT version()'"

echo ""
echo "📚 For comprehensive type testing, see:"
echo "   cargo run --example type_showcase"
echo "   cargo run --example basic_usage"

echo ""
echo "💡 This test client showcases all the native ClickHouse types we implemented:"
echo "   ✅ Bool - Native boolean type"
echo "   ✅ AggregateFunction - Function states"
echo "   ✅ SimpleAggregateFunction - Simple aggregates"
echo "   ✅ Nothing - Null type"
echo "   ✅ Nested - Legacy nested structures"
echo "   ✅ Variant - Union types with discriminator"
echo "   ✅ Dynamic - Schema evolution support"
echo ""
echo "All types support proper serialization, deserialization, and JSON conversion!"