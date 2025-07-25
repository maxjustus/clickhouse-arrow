#!/usr/bin/env python3
"""
ClickHouse Native Protocol Integration Test Runner

Compares output between official ClickHouse client and our clickhouse-test-client
to ensure compatibility and correctness.
"""

import os
import sys
import json
import yaml
import subprocess
import argparse
import difflib
from typing import Dict, List, Any, Tuple, Optional
from pathlib import Path
import tempfile
import shutil

class TestRunner:
    def __init__(self, verbose: bool = False, save_diffs: bool = False):
        self.verbose = verbose
        self.save_diffs = save_diffs
        self.results_dir = Path("results")
        self.results_dir.mkdir(exist_ok=True)
        
        # Check if clients are available
        self.check_clients()
    
    def check_clients(self):
        """Verify both clients are available"""
        try:
            subprocess.run(["clickhouse", "client", "--version"], 
                          capture_output=True, check=True)
        except (subprocess.CalledProcessError, FileNotFoundError):
            print("❌ ClickHouse client not found. Please install and add to PATH.")
            sys.exit(1)
        
        # Build our test client if needed
        test_client_path = Path("../target/debug/clickhouse-test-client")
        if not test_client_path.exists():
            print("📦 Building test client...")
            subprocess.run(["cargo", "build"], 
                          cwd="../test-client", check=True)
        
        self.test_client_path = str(test_client_path.resolve())
    
    def run_clickhouse_client(self, query: str) -> Tuple[str, bool]:
        """Run query with official ClickHouse client"""
        try:
            cmd = [
                "clickhouse", "client",
                "--query", query,
                "--format", "JSONEachRow",
                "--host", "localhost",
                "--port", "9000"
            ]
            
            result = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
            
            if result.returncode != 0:
                return f"Error: {result.stderr}", False
            
            return result.stdout.strip(), True
            
        except subprocess.TimeoutExpired:
            return "Error: Query timeout", False
        except Exception as e:
            return f"Error: {str(e)}", False
    
    def run_test_client(self, query: str) -> Tuple[str, bool]:
        """Run query with our test client"""
        try:
            cmd = [
                self.test_client_path,
                "--query", query,
                "--format", "pretty",
                "--host", "localhost",
                "--port", "9000"
            ]
            
            result = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
            
            if result.returncode != 0:
                return f"Error: {result.stderr}", False
            
            return result.stdout.strip(), True
            
        except subprocess.TimeoutExpired:
            return "Error: Query timeout", False
        except Exception as e:
            return f"Error: {str(e)}", False
    
    def normalize_json_output(self, output: str) -> Optional[Dict]:
        """Normalize JSON output for comparison"""
        if not output or output.startswith("Error:"):
            return None
        
        try:
            # Handle JSONEachRow format (one JSON object per line)
            lines = output.strip().split('\n')
            if len(lines) == 1:
                return json.loads(lines[0])
            else:
                return [json.loads(line) for line in lines if line.strip()]
        except json.JSONDecodeError:
            return None
    
    def compare_outputs(self, ch_output: str, test_output: str, test_name: str) -> bool:
        """Compare outputs from both clients"""
        ch_json = self.normalize_json_output(ch_output)
        test_json = self.normalize_json_output(test_output)
        
        if ch_json is None and test_json is None:
            # Both failed to parse, compare raw strings
            return ch_output == test_output
        
        if ch_json is None or test_json is None:
            # One parsed, one didn't
            if self.save_diffs:
                self.save_diff(ch_output, test_output, test_name, "parse_mismatch")
            return False
        
        # Compare JSON structures
        if ch_json == test_json:
            return True
        
        if self.save_diffs:
            self.save_diff(
                json.dumps(ch_json, indent=2, sort_keys=True),
                json.dumps(test_json, indent=2, sort_keys=True),
                test_name,
                "json_diff"
            )
        
        return False
    
    def save_diff(self, expected: str, actual: str, test_name: str, diff_type: str):
        """Save diff to file"""
        diff_file = self.results_dir / f"{test_name}_{diff_type}.diff"
        
        diff = difflib.unified_diff(
            expected.splitlines(keepends=True),
            actual.splitlines(keepends=True),
            fromfile="ClickHouse Client",
            tofile="Test Client",
            lineterm=""
        )
        
        with open(diff_file, 'w') as f:
            f.writelines(diff)
    
    def run_test_case(self, test_case: Dict, category: str) -> Dict[str, Any]:
        """Run a single test case"""
        name = test_case.get('name', 'unnamed')
        query = test_case['query']
        
        if self.verbose:
            print(f"  🔍 Running: {name}")
            print(f"     Query: {query}")
        
        # Run with ClickHouse client
        ch_output, ch_success = self.run_clickhouse_client(query)
        
        # Run with test client
        test_output, test_success = self.run_test_client(query)
        
        # Compare results
        if not ch_success or not test_success:
            status = "ERROR"
            passed = False
            details = {
                "clickhouse_error": not ch_success,
                "test_client_error": not test_success,
                "clickhouse_output": ch_output if not ch_success else None,
                "test_client_output": test_output if not test_success else None
            }
        else:
            passed = self.compare_outputs(ch_output, test_output, f"{category}_{name}")
            status = "PASS" if passed else "FAIL"
            details = {
                "clickhouse_output": ch_output if self.verbose or not passed else None,
                "test_client_output": test_output if self.verbose or not passed else None
            }
        
        return {
            "name": name,
            "query": query,
            "status": status,
            "passed": passed,
            "details": details
        }
    
    def load_test_cases(self, test_dir: Path) -> List[Dict]:
        """Load all test case files from directory"""
        test_cases = []
        
        for yaml_file in test_dir.glob("*.yaml"):
            try:
                with open(yaml_file, 'r') as f:
                    data = yaml.safe_load(f)
                    
                category_name = data.get('name', yaml_file.stem)
                tests = data.get('tests', [])
                
                for test in tests:
                    test['category'] = category_name
                    test['file'] = yaml_file.name
                    test_cases.append(test)
                    
            except Exception as e:
                print(f"⚠️  Failed to load {yaml_file}: {e}")
        
        return test_cases
    
    def run_category(self, category_path: Path) -> Dict[str, Any]:
        """Run all tests in a category"""
        category_name = category_path.name
        print(f"\n📂 Running category: {category_name}")
        
        test_cases = self.load_test_cases(category_path)
        if not test_cases:
            print(f"   No test cases found in {category_path}")
            return {"name": category_name, "tests": [], "summary": {"total": 0, "passed": 0, "failed": 0, "errors": 0}}
        
        results = []
        for test_case in test_cases:
            result = self.run_test_case(test_case, category_name)
            results.append(result)
            
            # Print status
            status_emoji = {"PASS": "✅", "FAIL": "❌", "ERROR": "💥"}
            print(f"   {status_emoji.get(result['status'], '❓')} {result['name']}")
            
            if result['status'] != "PASS" and self.verbose:
                if result['details'].get('clickhouse_output'):
                    print(f"      CH: {result['details']['clickhouse_output']}")
                if result['details'].get('test_client_output'):
                    print(f"      TC: {result['details']['test_client_output']}")
        
        # Calculate summary
        total = len(results)
        passed = sum(1 for r in results if r['status'] == 'PASS')
        failed = sum(1 for r in results if r['status'] == 'FAIL')
        errors = sum(1 for r in results if r['status'] == 'ERROR')
        
        summary = {"total": total, "passed": passed, "failed": failed, "errors": errors}
        
        return {
            "name": category_name,
            "tests": results,
            "summary": summary
        }
    
    def run_all_tests(self, categories: Optional[List[str]] = None) -> Dict[str, Any]:
        """Run all test categories"""
        test_cases_dir = Path("test_cases")
        
        if not test_cases_dir.exists():
            print(f"❌ Test cases directory not found: {test_cases_dir}")
            sys.exit(1)
        
        category_dirs = []
        if categories:
            for cat in categories:
                cat_dir = test_cases_dir / cat
                if cat_dir.exists():
                    category_dirs.append(cat_dir)
                else:
                    print(f"⚠️  Category not found: {cat}")
        else:
            category_dirs = [d for d in test_cases_dir.iterdir() if d.is_dir()]
        
        if not category_dirs:
            print("❌ No test categories found")
            sys.exit(1)
        
        print(f"🚀 Running integration tests...")
        print(f"📍 Test categories: {[d.name for d in category_dirs]}")
        
        all_results = []
        overall_summary = {"total": 0, "passed": 0, "failed": 0, "errors": 0}
        
        for category_dir in sorted(category_dirs):
            category_result = self.run_category(category_dir)
            all_results.append(category_result)
            
            # Update overall summary
            summary = category_result["summary"]
            overall_summary["total"] += summary["total"]
            overall_summary["passed"] += summary["passed"]
            overall_summary["failed"] += summary["failed"]
            overall_summary["errors"] += summary["errors"]
        
        return {
            "categories": all_results,
            "summary": overall_summary
        }

def main():
    parser = argparse.ArgumentParser(description="ClickHouse integration test runner")
    parser.add_argument("categories", nargs="*", help="Specific categories to run")
    parser.add_argument("--verbose", "-v", action="store_true", help="Verbose output")
    parser.add_argument("--save-diffs", action="store_true", help="Save diff files for failed tests")
    
    args = parser.parse_args()
    
    runner = TestRunner(verbose=args.verbose, save_diffs=args.save_diffs)
    results = runner.run_all_tests(args.categories)
    
    # Print final summary
    summary = results["summary"]
    print(f"\n📊 Test Summary:")
    print(f"   Total: {summary['total']}")
    print(f"   ✅ Passed: {summary['passed']}")
    print(f"   ❌ Failed: {summary['failed']}")
    print(f"   💥 Errors: {summary['errors']}")
    
    # Save results
    results_file = Path("results/test_results.json")
    with open(results_file, 'w') as f:
        json.dump(results, f, indent=2)
    
    print(f"\n📄 Detailed results saved to: {results_file}")
    
    # Exit with error code if any tests failed
    if summary['failed'] > 0 or summary['errors'] > 0:
        sys.exit(1)

if __name__ == "__main__":
    main()