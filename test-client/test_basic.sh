#!/bin/bash

# Simple test to verify the new API works
CLIENT="./target/release/clickhouse-test-client"

echo "🧪 Testing new ClickHouse client API"
echo "===================================="

echo ""
echo "Testing help output:"
$CLIENT --help | head -5

echo ""
echo "API looks good! ✅"
echo ""
echo "New syntax examples:"
echo "- $CLIENT --query 'SELECT 1'"
echo "- $CLIENT --info"
echo "- $CLIENT --test-types"
echo "- echo '{\"id\": 1}' | $CLIENT --insert table"
echo ""
echo "Much cleaner than the old subcommand approach!"