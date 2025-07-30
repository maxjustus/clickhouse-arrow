#!/usr/bin/env bash
# Simple multi-version test runner
# Uses testcontainers infrastructure - just sets CLICKHOUSE_VERSION env var

set -e

# Default versions to test
VERSIONS="${VERSIONS:-24.3 24.8 25.1 25.5 latest}"

# Colors for output
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m' # No Color

echo "🧪 Testing clickhouse-arrow against multiple ClickHouse versions"
echo "Versions: $VERSIONS"
echo

# Track results
declare -a RESULTS

for version in $VERSIONS; do
    echo -e "${YELLOW}Testing ClickHouse $version...${NC}"
    
    if CLICKHOUSE_VERSION="$version" cargo test --all-features --quiet; then
        echo -e "${GREEN}✓ $version: All tests passed${NC}"
        RESULTS+=("$version: ✓")
    else
        echo -e "${RED}✗ $version: Some tests failed${NC}"
        RESULTS+=("$version: ✗")
    fi
    echo
done

# Summary
echo "📊 Summary:"
for result in "${RESULTS[@]}"; do
    echo "  $result"
done