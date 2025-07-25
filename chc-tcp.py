#!/usr/bin/env -S uv run --script
# /// script
# dependencies = []
# ///

import socket
import threading
import subprocess
import sys
import time
import select
import json
import argparse
import struct
from datetime import datetime

class ClickHouseProtocolParser:
    """Parse ClickHouse native protocol messages"""
    
    @staticmethod
    def parse_packet(data, direction):
        """Attempt to parse known ClickHouse protocol structures"""
        parsed = {
            'raw_hex': data.hex(),
            'size': len(data),
            'direction': direction,
            'parsed_fields': []
        }
        
        if len(data) < 2:
            return parsed
            
        # Try to parse based on direction and packet patterns
        if direction == 'CLIENT->SERVER':
            if len(data) == 34 and data[0] == 0x00:
                # Client handshake
                parsed['packet_type'] = 'client_handshake'
                parsed['parsed_fields'] = [
                    {'offset': 0, 'size': 1, 'name': 'packet_type', 'value': data[0], 'hex': f'{data[0]:02x}'},
                    {'offset': 1, 'size': 1, 'name': 'string_length', 'value': data[1], 'hex': f'{data[1]:02x}'},
                    {'offset': 2, 'size': data[1], 'name': 'client_name', 'value': data[2:2+data[1]].decode('utf-8', errors='replace'), 'hex': data[2:2+data[1]].hex()},
                ]
            elif len(data) > 100 and b'SELECT' in data:
                # Query packet
                parsed['packet_type'] = 'query'
                select_idx = data.find(b'SELECT')
                if select_idx > 0:
                    query_len_idx = select_idx - 1
                    query_len = data[query_len_idx]
                    parsed['parsed_fields'].append({
                        'offset': select_idx,
                        'size': query_len,
                        'name': 'query',
                        'value': data[select_idx:select_idx+query_len].decode('utf-8'),
                        'hex': data[select_idx:select_idx+query_len].hex()
                    })
                    
        elif direction == 'SERVER->CLIENT':
            if len(data) > 10 and data[0:2] == b'\x00\x0a' and b'ClickHouse' in data:
                # Server handshake response
                parsed['packet_type'] = 'server_handshake'
                parsed['parsed_fields'].append({
                    'offset': 0,
                    'size': 2,
                    'name': 'header',
                    'value': 'server_hello',
                    'hex': data[0:2].hex()
                })
                
        # Add common binary patterns
        parsed['binary_patterns'] = {
            'nulls': [(i, i+len(run)) for i, run in ClickHouseProtocolParser._find_runs(data, b'\x00')],
            'printable_strings': ClickHouseProtocolParser._find_strings(data),
            'potential_integers': ClickHouseProtocolParser._find_integers(data)
        }
        
        return parsed
    
    @staticmethod
    def _find_runs(data, byte_value):
        """Find runs of a specific byte value"""
        runs = []
        i = 0
        while i < len(data):
            if data[i:i+1] == byte_value:
                start = i
                while i < len(data) and data[i:i+1] == byte_value:
                    i += 1
                if i - start >= 3:  # Only report runs of 3+ bytes
                    runs.append((start, data[start:i]))
            else:
                i += 1
        return runs
    
    @staticmethod
    def _find_strings(data, min_length=4):
        """Find printable ASCII strings"""
        strings = []
        current_string = []
        start_offset = 0
        
        for i, byte in enumerate(data):
            if 32 <= byte < 127:
                if not current_string:
                    start_offset = i
                current_string.append(chr(byte))
            else:
                if len(current_string) >= min_length:
                    string_value = ''.join(current_string)
                    strings.append({
                        'offset': start_offset,
                        'length': len(current_string),
                        'value': string_value,
                        'hex': data[start_offset:start_offset+len(current_string)].hex()
                    })
                current_string = []
                
        return strings
    
    @staticmethod
    def _find_integers(data):
        """Find potential integer values at aligned positions"""
        integers = []
        
        # Check for common integer positions (every 4/8 bytes)
        for i in range(0, len(data)-8, 4):
            if i % 4 == 0:  # Aligned position
                # Try to parse as different integer types
                if i + 4 <= len(data):
                    try:
                        val32 = struct.unpack('<I', data[i:i+4])[0]  # Little-endian 32-bit
                        if 1 <= val32 <= 1000000:  # Reasonable range
                            integers.append({
                                'offset': i,
                                'type': 'uint32_le',
                                'value': val32,
                                'hex': data[i:i+4].hex()
                            })
                    except:
                        pass
                        
                if i + 8 <= len(data):
                    try:
                        val64 = struct.unpack('<Q', data[i:i+8])[0]  # Little-endian 64-bit
                        if 1 <= val64 <= 1000000:  # Reasonable range
                            integers.append({
                                'offset': i,
                                'type': 'uint64_le',
                                'value': val64,
                                'hex': data[i:i+8].hex()
                            })
                    except:
                        pass
                        
        return integers

class ClickHouseTCPCapture:
    def __init__(self, clickhouse_host='localhost', clickhouse_port=9000, proxy_port=9001, output_format='construct', verbose=False):
        self.clickhouse_host = clickhouse_host
        self.clickhouse_port = clickhouse_port
        self.proxy_port = proxy_port
        self.output_format = output_format
        self.verbose = verbose
        self.parser = ClickHouseProtocolParser()
        
    def log_packet(self, direction, data):
        timestamp = datetime.now().strftime('%Y-%m-%d %H:%M:%S.%f')[:-3]
        
        packet_info = {
            'timestamp': timestamp,
            'direction': direction,
            'size': len(data),
            'data': data,
            'parsed': self.parser.parse_packet(data, direction)
        }
        
        # Stream JSON output immediately
        if self.output_format == 'minimal':
            # Minimal output - just the essentials
            output_packet = {
                'timestamp': timestamp,
                'direction': direction,
                'size': len(data),
                'hex': ' '.join(f'{b:02x}' for b in data)
            }
            # Add packet type if we detected it
            parsed = packet_info['parsed']
            if parsed.get('packet_type'):
                output_packet['type'] = parsed['packet_type']
            print(json.dumps(output_packet))
        elif self.output_format == 'parsed':
            # Full parsing with all details
            output_packet = {
                'timestamp': timestamp,
                'direction': direction,
                'size': len(data),
                'hex': ' '.join(f'{b:02x}' for b in data),
                'ascii': ''.join(chr(b) if 32 <= b < 127 else '.' for b in data),
                'parsed': packet_info['parsed']
            }
            print(json.dumps(output_packet))
        elif self.output_format == 'construct':
            # Format optimized for Construct parsing - byte array
            output_packet = {
                'timestamp': timestamp,
                'direction': direction,
                'size': len(data),
                'bytes': list(data)
            }
            if packet_info['parsed'].get('packet_type'):
                output_packet['type'] = packet_info['parsed']['packet_type']
            print(json.dumps(output_packet))
        elif self.output_format == 'compact':
            # Ultra-minimal - just type and key info
            parsed = packet_info['parsed']
            output_packet = {
                't': timestamp[-12:],  # Just time portion
                'd': direction[0] + '->' + direction[-1],  # C->S or S->C
                's': len(data)
            }
            if parsed.get('packet_type'):
                output_packet['type'] = parsed['packet_type']
            # Add query text if found
            for field in parsed.get('parsed_fields', []):
                if field['name'] == 'query':
                    output_packet['query'] = field['value']
            print(json.dumps(output_packet))
        elif self.output_format == 'hex':
            # Hex dump format
            print(f"\n[{timestamp}] {direction} ({len(data)} bytes)")
            print("-" * 80)
            for i in range(0, len(data), 16):
                hex_str = ' '.join(f'{b:02x}' for b in data[i:i+16])
                ascii_str = ''.join(chr(b) if 32 <= b < 127 else '.' for b in data[i:i+16])
                print(f"{i:04x}  {hex_str:<48}  |{ascii_str}|")
            print()
        
        if self.verbose:
            print(f"\n[{timestamp}] {direction} ({len(data)} bytes)", file=sys.stderr)
            print("-" * 80, file=sys.stderr)
            
            # Print hex dump to stderr for debugging
            for i in range(0, len(data), 16):
                hex_str = ' '.join(f'{b:02x}' for b in data[i:i+16])
                ascii_str = ''.join(chr(b) if 32 <= b < 127 else '.' for b in data[i:i+16])
                print(f"{i:04x}  {hex_str:<48}  |{ascii_str}|", file=sys.stderr)
            
    def handle_connection(self, client_socket, client_addr):
        if self.verbose:
            print(f"New connection from {client_addr}", file=sys.stderr)
        
        # Connect to actual ClickHouse server
        server_socket = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            server_socket.connect((self.clickhouse_host, self.clickhouse_port))
        except Exception as e:
            print(f"Failed to connect to ClickHouse: {e}", file=sys.stderr)
            client_socket.close()
            return
            
        # Relay data between client and server
        sockets = [client_socket, server_socket]
        
        try:
            while True:
                readable, _, exceptional = select.select(sockets, [], sockets, 1.0)
                
                if exceptional:
                    break
                    
                for sock in readable:
                    if sock is client_socket:
                        # Data from client to server
                        data = sock.recv(65536)
                        if not data:
                            return
                        self.log_packet("CLIENT->SERVER", data)
                        server_socket.sendall(data)
                        
                    elif sock is server_socket:
                        # Data from server to client
                        data = sock.recv(65536)
                        if not data:
                            return
                        self.log_packet("SERVER->CLIENT", data)
                        client_socket.sendall(data)
                        
        except Exception as e:
            print(f"Connection error: {e}", file=sys.stderr)
        finally:
            client_socket.close()
            server_socket.close()
            if self.verbose:
                print("Connection closed", file=sys.stderr)
            
    def start_proxy(self):
        proxy_socket = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        proxy_socket.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        proxy_socket.bind(('localhost', self.proxy_port))
        proxy_socket.listen(5)
        
        if self.verbose:
            print(f"TCP proxy listening on localhost:{self.proxy_port}", file=sys.stderr)
            print(f"Forwarding to {self.clickhouse_host}:{self.clickhouse_port}", file=sys.stderr)
        
        try:
            while True:
                client_socket, client_addr = proxy_socket.accept()
                thread = threading.Thread(
                    target=self.handle_connection,
                    args=(client_socket, client_addr)
                )
                thread.daemon = True
                thread.start()
        except KeyboardInterrupt:
            if self.verbose:
                print("\nProxy stopped", file=sys.stderr)
        finally:
            proxy_socket.close()
            
    def run_clickhouse_client(self, query):
        # Start proxy in background
        proxy_thread = threading.Thread(target=self.start_proxy)
        proxy_thread.daemon = True
        proxy_thread.start()
        
        # Give proxy time to start
        time.sleep(0.5)
        
        # Run ClickHouse client connecting to our proxy
        cmd = [
            'clickhouse', 'client',
            '--host', 'localhost',
            '--port', str(self.proxy_port),
            '--compression=false',
            '--format=JSONEachRow',
            '--proto_caps=notchunked',
            '--query', query
        ]
        
        if self.verbose:
            print(f"\nRunning command: {' '.join(cmd)}", file=sys.stderr)
            print("=" * 80, file=sys.stderr)
        
        try:
            result = subprocess.run(cmd, capture_output=True, text=True)
            # Don't print query result since it will interfere with JSON output
            return result
        except Exception as e:
            print(f"Error running ClickHouse client: {e}", file=sys.stderr)
            return None

def main():
    parser = argparse.ArgumentParser(
        description='Capture TCP interactions for ClickHouse client queries and output as JSON',
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog='''Examples:
  # Output byte arrays for Construct parsing (default)
  uv run clickhouse_tcp_capture.py 'SELECT 1'
  
  # Save to file and parse with Python
  uv run clickhouse_tcp_capture.py 'SELECT 1' > capture.jsonl
  # Then: data = bytes(json.loads(line)['bytes'])
  
  # Process streaming JSON with jq
  uv run clickhouse_tcp_capture.py 'SELECT 1' | jq -c 'select(.direction == "SERVER->CLIENT")'
  
  # Different formats
  uv run clickhouse_tcp_capture.py 'SELECT 1'                    # Default: byte array
  uv run clickhouse_tcp_capture.py 'SELECT 1' --format minimal   # Hex string with spaces  
  uv run clickhouse_tcp_capture.py 'SELECT 1' --format compact   # Ultra-compact format
  uv run clickhouse_tcp_capture.py 'SELECT 1' --format parsed    # Full parsing details
  uv run clickhouse_tcp_capture.py 'SELECT 1' --format hex       # Traditional hex dump
  
  # Verbose mode (show debug output to stderr)
  uv run clickhouse_tcp_capture.py 'SELECT 1' --verbose
  
  # Custom server
  uv run clickhouse_tcp_capture.py 'SELECT version()' --host myserver.com --port 9000'''
    )
    
    parser.add_argument('query', help='SQL query to execute')
    parser.add_argument('--host', default='localhost', help='ClickHouse server host (default: localhost)')
    parser.add_argument('--port', type=int, default=9000, help='ClickHouse server port (default: 9000)')
    parser.add_argument('--format', choices=['construct', 'minimal', 'compact', 'parsed', 'hex'], default='construct',
                        help='Output format: construct (byte array, default), minimal (hex string), compact (ultra-minimal), parsed (full details), hex (hex dump)')
    parser.add_argument('--verbose', '-v', action='store_true', help='Show debug output to stderr')
    
    args = parser.parse_args()
    
    capturer = ClickHouseTCPCapture(
        args.host, 
        args.port, 
        output_format=args.format,
        verbose=args.verbose
    )
    capturer.run_clickhouse_client(args.query)
    
    # Keep proxy running for a bit to capture any trailing packets
    time.sleep(2)

if __name__ == '__main__':
    main()
