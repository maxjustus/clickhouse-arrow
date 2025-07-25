# ClickHouse Binary Protocol - Core Protocol Structure

This document provides byte-level specifications for the foundational elements of the ClickHouse native binary protocol.

## Protocol Overview

The ClickHouse binary protocol is a stateful, packet-based TCP protocol with the following characteristics:

- **Transport**: TCP (typically port 9000)
- **Byte Order**: Little-endian for all multi-byte integers
- **Encoding**: Variable-length integers (VarInt) for length prefixes and packet types
- **Strings**: UTF-8 with VarInt length prefixes
- **Compression**: Optional, negotiated during handshake
- **Chunking**: Optional transfer encoding for large payloads

---

## 1. Packet Format Documentation

### 1.1 Base Packet Structure

Every protocol packet follows this structure:

```
[Optional Chunk Header][VarInt Packet Type][Packet Payload]
```

- **No fixed packet length headers** - packets are self-delimited by their content structure
- **Packet Type**: VarInt encoding of packet type enum
- **Payload**: Type-specific data (strings, blocks, metadata)

### 1.2 Variable-Length Integer Encoding (VarInt)

ClickHouse uses a **7-bit continuation encoding** for variable-length integers:

#### Encoding Algorithm
```cpp
while (value > 0x7F) {
    output_byte(0x80 | (value & 0x7F));  // Set continuation bit + 7 data bits
    value >>= 7;
}
output_byte(value);  // Final byte without continuation bit
```

#### Decoding Algorithm
```cpp
result = 0;
for (int shift = 0; shift < 70; shift += 7) {  // Max 10 bytes for 64-bit
    byte = input_byte();
    result |= (byte & 0x7F) << shift;
    if (!(byte & 0x80)) break;  // No continuation bit = end
}
```

#### VarInt Examples

| Value | Hex Bytes | Binary Representation | Description |
|-------|-----------|----------------------|-------------|
| `0` | `0x00` | `00000000` | Single byte, no continuation |
| `127` | `0x7F` | `01111111` | Maximum single-byte value |
| `128` | `0x80 0x01` | `10000000 00000001` | First multi-byte value |
| `255` | `0xFF 0x01` | `11111111 00000001` | |
| `16384` | `0x80 0x80 0x01` | `10000000 10000000 00000001` | 3-byte encoding |
| `2147483647` | `0xFF 0xFF 0xFF 0xFF 0x07` | | Maximum 32-bit signed int |

#### Signed VarInt (ZigZag Encoding)

For signed integers, ClickHouse uses **ZigZag encoding** to efficiently handle negative numbers:

```cpp
// Encoding: Map signed to unsigned
encoded = (value << 1) ^ (value >> 63);  // For 64-bit signed

// Decoding: Map unsigned back to signed  
decoded = (encoded >> 1) ^ -(encoded & 1);
```

**ZigZag Examples:**
- `-1` → `1` → `0x01`
- `-2` → `3` → `0x03`  
- `1` → `2` → `0x02`
- `2` → `4` → `0x04`

### 1.3 String Encoding Format

All strings use **UTF-8 encoding** with **VarInt length prefixes**:

```
[VarInt length][UTF-8 string data]
```

#### String Examples

| String | Hex Bytes | Breakdown |
|--------|-----------|-----------|
| `""` (empty) | `0x00` | Length=0, no data |
| `"Hello"` | `0x05 0x48 0x65 0x6C 0x6C 0x6F` | Length=5, UTF-8 data |
| `"ClickHouse"` | `0x0A 0x43 0x6C 0x69 0x63 0x6B 0x48 0x6F 0x75 0x73 0x65` | Length=10 |
| `"Тест"` (Cyrillic) | `0x08 0xD0 0xA2 0xD0 0xB5 0xD1 0x81 0xD1 0x82` | Length=8 (UTF-8 bytes) |

**Note**: Length represents **byte count**, not character count for multi-byte UTF-8.

### 1.4 Binary Data Encoding

Binary data (byte arrays) follows the same pattern as strings:

```
[VarInt length][raw binary data]
```

### 1.5 Endianness Handling

- **VarInt**: Byte-order independent by design
- **Fixed integers** (UInt32, UInt64, etc.): **Little-endian**
- **Floating point**: IEEE 754 little-endian

#### Fixed Integer Examples

| Value | Type | Hex Bytes (Little-Endian) |
|-------|------|---------------------------|
| `305419896` | UInt32 | `0x78 0x56 0x34 0x12` |
| `1311768467463790320` | UInt64 | `0xF0 0xDE 0xBC 0x9A 0x78 0x56 0x34 0x12` |

---

## 2. Connection Handshake Flow

### 2.1 Hello Packet Exchange

The protocol begins with a **bidirectional Hello packet exchange**:

```
Client → Server: Hello (Client capabilities)
Server → Client: Hello (Server capabilities)  
[Optional: Authentication challenge/response]
```

### 2.2 Client Hello Packet

**Structure:**
```cpp
writeVarUInt(Protocol::Client::Hello, out);        // 0x00
writeStringBinary(client_name, out);               // e.g., "ClickHouse client"
writeVarUInt(client_version_major, out);           
writeVarUInt(client_version_minor, out);
writeVarUInt(client_tcp_protocol_version, out);    // e.g., 54477
writeStringBinary(default_database, out);          // e.g., "default"
writeStringBinary(user, out);                      // e.g., "default"
writeStringBinary(password, out);                  // Plain text or empty
```

**Example Client Hello Bytes:**
```
0x00                                    // Packet Type: Client::Hello
0x10 "ClickHouse client"                // Client name (length=16)
0x18                                    // Major version: 24
0x0A                                    // Minor version: 10  
0xCD 0xD4 0x03                         // Protocol version: 54477
0x07 "default"                          // Database (length=7)
0x07 "default"                          // User (length=7)
0x00                                    // Password (empty)
```

### 2.3 Server Hello Packet

**Structure:**
```cpp
writeVarUInt(Protocol::Server::Hello, out);        // 0x00
writeStringBinary(server_name, out);               // e.g., "ClickHouse"
writeVarUInt(server_version_major, out);
writeVarUInt(server_version_minor, out);
writeVarUInt(server_tcp_protocol_version, out);
writeStringBinary(server_timezone, out);           // e.g., "UTC"
writeStringBinary(server_display_name, out);       // e.g., "production"
writeVarUInt(server_version_patch, out);
```

**Example Server Hello Bytes:**
```
0x00                                    // Packet Type: Server::Hello  
0x0A "ClickHouse"                       // Server name (length=10)
0x18                                    // Major version: 24
0x0A                                    // Minor version: 10
0xCD 0xD4 0x03                         // Protocol version: 54477  
0x03 "UTC"                              // Timezone (length=3)
0x0A "production"                       // Display name (length=10)
0x05                                    // Patch version: 5
```

### 2.4 Protocol Version Negotiation

**Version Compatibility Rules:**
1. Client and server **MUST** support the same major protocol version
2. **Minimum version** determines available features
3. **Newer features** are enabled only if both sides support them

**Key Protocol Versions:**
- `54451`: Incremental ProfileEvents support
- `54477`: Current version (as of 2024)

---

## 3. Authentication Mechanisms

### 3.1 Password Authentication

**Basic Flow:**
1. Client sends plaintext password in Hello packet
2. Server validates and responds with Hello or Exception

**Security Note**: Password is sent in **plaintext**. Use TLS/SSL for production.

### 3.2 SSH Challenge-Response Authentication

**Packet Type**: `Protocol::Server::SSHChallenge` (18)

**Flow:**
1. Client Hello with SSH authentication marker
2. Server sends SSH challenge
3. Client responds with signed challenge
4. Server validates signature

### 3.3 JWT Token Authentication

**Flow:**
1. Client Hello with JWT token in password field
2. Server validates JWT signature and claims
3. Server responds with Hello or Exception

### 3.4 Inter-Server Secret Authentication

Used for **cluster communication** between ClickHouse servers:
1. Shared secret configured on all cluster nodes
2. Secret sent in Hello packet for authentication
3. Bypasses user-based authentication

---

## 4. Compression Handling

### 4.1 Compression Negotiation

**Compression Support** is indicated in Hello packet:
```cpp
writeVarUInt(compression_enabled ? 1 : 0, out);    // Compression flag
```

### 4.2 Supported Compression Codecs

- **LZ4**: Fast compression/decompression
- **ZSTD**: Better compression ratio
- **None**: No compression

### 4.3 Compressed Packet Format

When compression is enabled:
```
[VarInt Packet Type][Compressed Payload]
```

**Notes:**
- **Packet type** remains uncompressed for routing
- **Entire payload** is compressed as single block
- **Decompression** required before payload parsing

---

## 5. Chunked Transfer Encoding

### 5.1 Chunk Format

For large payloads, ClickHouse supports **chunked transfer**:

```
[UInt32 chunk_size (little-endian)][chunk_data]
[UInt32 chunk_size (little-endian)][chunk_data]
...
[UInt32 0x00000000]                    // End marker
```

### 5.2 Chunk Header Structure

- **Size**: 32-bit unsigned integer, little-endian
- **Includes**: Payload bytes only (excludes 4-byte size header)
- **Zero size**: Indicates end of chunked sequence

### 5.3 Chunked Transfer Example

**Sending "Hello World" in 2 chunks:**
```
0x06 0x00 0x00 0x00          // Chunk 1: 6 bytes
0x48 0x65 0x6C 0x6C 0x6F 0x20 // "Hello "
0x05 0x00 0x00 0x00          // Chunk 2: 5 bytes  
0x57 0x6F 0x72 0x6C 0x64     // "World"
0x00 0x00 0x00 0x00          // End marker
```

---

## 6. Packet Types Overview

### 6.1 Server-to-Client Packets

| Type | Value | Name | Description |
|------|-------|------|-------------|
| 0 | `0x00` | Hello | Server capabilities and version |
| 1 | `0x01` | Data | Query result data blocks |
| 2 | `0x02` | Exception | Error information |
| 3 | `0x03` | Progress | Query execution progress |
| 4 | `0x04` | Pong | Ping response |
| 5 | `0x05` | EndOfStream | Query completion |
| 6 | `0x06` | ProfileInfo | Query profiling data |
| 7 | `0x07` | Totals | Aggregate totals |
| 8 | `0x08` | Extremes | Min/max values |
| 14 | `0x0E` | ProfileEvents | Real-time performance metrics |
| 18 | `0x12` | SSHChallenge | SSH authentication challenge |

### 6.2 Client-to-Server Packets

| Type | Value | Name | Description |
|------|-------|------|-------------|
| 0 | `0x00` | Hello | Client capabilities and credentials |
| 1 | `0x01` | Query | SQL query and settings |
| 2 | `0x02` | Data | INSERT data blocks |
| 3 | `0x03` | Cancel | Cancel running query |
| 4 | `0x04` | Ping | Connection keepalive |

---

## 7. Connection State Machine

### 7.1 Connection States

```
DISCONNECTED → CONNECTING → AUTHENTICATED → READY → QUERYING → READY
                                                   ↓
                                               DISCONNECTED
```

### 7.2 State Transitions

1. **CONNECTING**: TCP connection established, Hello exchange in progress
2. **AUTHENTICATED**: Hello exchange complete, authentication successful
3. **READY**: Connection ready for queries
4. **QUERYING**: Query in progress, receiving data/progress/profile events
5. **DISCONNECTED**: Connection closed or error occurred

---

## Implementation Notes

### Error Handling
- **Invalid VarInt**: If continuation bits indicate >10 bytes for 64-bit values
- **String Length Validation**: Ensure length doesn't exceed reasonable limits  
- **Endianness**: Always use little-endian for fixed-width integers
- **UTF-8 Validation**: Validate string data is properly encoded UTF-8

### Performance Considerations
- **VarInt Efficiency**: Most packet types and small values use single bytes
- **String Interning**: Consider caching commonly used strings
- **Chunking**: Use for payloads >64KB to improve streaming
- **Compression**: Enable for large data transfers, disable for low-latency queries

### Security Considerations
- **Password Transmission**: Use TLS for production deployments
- **Input Validation**: Validate all VarInt and string lengths
- **Resource Limits**: Impose limits on packet sizes and string lengths
- **Authentication**: Implement proper timeout and retry limits