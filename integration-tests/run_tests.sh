#!/bin/bash

# ClickHouse Native Protocol Integration Test Runner
# Usage: ./run_tests.sh [category1 category2...] [--verbose] [--save-diffs]

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Check Python requirements
if ! python3 -c "import yaml" 2>/dev/null; then
    echo "📦 Installing PyYAML..."
    pip3 install PyYAML
fi

# Ensure ClickHouse server is running
if ! nc -z localhost 9000 2>/dev/null; then
    echo "❌ ClickHouse server is not running on localhost:9000"
    echo "   Please start ClickHouse server first:"
    echo "   docker run -d --name clickhouse-server -p 9000:9000 clickhouse/clickhouse-server"
    exit 1
fi

echo "🔍 ClickHouse server detected on localhost:9000"

# Run the Python test runner
python3 runner.py "$@"