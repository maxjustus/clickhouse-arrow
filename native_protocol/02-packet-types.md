# ClickHouse Binary Protocol - Packet Type Specifications

This document provides byte-level specifications for every packet type in the ClickHouse native binary protocol.

## Packet Type Overview

All packets begin with a **VarInt packet type** followed by type-specific payload data. Packet types are defined in `src/Core/Protocol.h`.

---

## Server-to-Client Packets

### Hello (0x00)

Server capabilities and version information sent in response to Client Hello.

**Structure:**
```cpp
writeVarUInt(Protocol::Server::Hello, out);              // 0x00
writeStringBinary(server_name, out);                     // "ClickHouse"
writeVarUInt(server_version_major, out);                 // e.g., 24
writeVarUInt(server_version_minor, out);                 // e.g., 10
writeVarUInt(server_tcp_protocol_version, out);          // e.g., 54477
if (client_tcp_protocol_version >= DBMS_MIN_REVISION_WITH_SERVER_TIMEZONE)
    writeStringBinary(server_timezone, out);             // e.g., "UTC"
if (client_tcp_protocol_version >= DBMS_MIN_REVISION_WITH_SERVER_DISPLAY_NAME)
    writeStringBinary(server_display_name, out);         // e.g., "production"
if (client_tcp_protocol_version >= DBMS_MIN_REVISION_WITH_VERSION_PATCH)
    writeVarUInt(server_version_patch, out);             // e.g., 5
```

**Example Bytes:**
```
0x00                          // Packet Type: Server::Hello
0x0A "ClickHouse"             // Server name
0x18                          // Major version: 24
0x0A                          // Minor version: 10
0xCD 0xD4 0x03               // Protocol version: 54477
0x03 "UTC"                    // Timezone (if supported)
0x0A "production"             // Display name (if supported)
0x05                          // Patch version: 5 (if supported)
```

### Data (0x01)

Query result data block containing column data.

**Structure:**
```cpp
writeVarUInt(Protocol::Server::Data, out);               // 0x01
writeStringBinary(external_table_name, out);            // Usually empty ""
// Followed by Native format Block data
```

**Block Format** (simplified):
```cpp
// Block header
writeVarUInt(num_columns, out);
writeVarUInt(num_rows, out);

// Column data for each column
for (size_t i = 0; i < num_columns; ++i) {
    writeStringBinary(column_name, out);
    writeStringBinary(column_type, out);
    // Column data in type-specific format
}
```

**Example Bytes:**
```
0x01                          // Packet Type: Server::Data
0x00                          // External table name (empty)
0x02                          // Number of columns: 2
0x03                          // Number of rows: 3
0x02 "id"                     // Column 1 name
0x06 "UInt32"                 // Column 1 type
0x01 0x00 0x00 0x00          // Row 1: id=1
0x02 0x00 0x00 0x00          // Row 2: id=2  
0x03 0x00 0x00 0x00          // Row 3: id=3
0x04 "name"                   // Column 2 name
0x06 "String"                 // Column 2 type
0x03 "foo"                    // Row 1: name="foo"
0x03 "bar"                    // Row 2: name="bar"
0x03 "baz"                    // Row 3: name="baz"
```

### Exception (0x02)

Error information with code, message, and stack trace.

**Structure:**
```cpp
writeVarUInt(Protocol::Server::Exception, out);          // 0x02
writeException(exception, out, settings.send_logs_level);
```

**Exception Format:**
```cpp
writeVarInt(exception.code(), out);                      // Error code
writeStringBinary(exception.name(), out);               // Exception class name
writeStringBinary(exception.displayText(), out);        // Error message
writeStringBinary(exception.getStackTraceString(), out); // Stack trace
writeBool(exception.hasNested(), out);                  // Has nested exception?
if (exception.hasNested()) {
    writeException(exception.nested(), out, send_logs_level); // Recursive
}
```

**Example Bytes:**
```
0x02                          // Packet Type: Server::Exception
0x2F                          // Error code: 47 (UNKNOWN_IDENTIFIER)
0x1A "DB::Exception"          // Exception class name
0x1B "Unknown identifier 'foo'" // Error message
0x00                          // Stack trace (empty)
0x00                          // No nested exception
```

### Progress (0x03)

Query execution progress information.

**Structure:**
```cpp
writeVarUInt(Protocol::Server::Progress, out);           // 0x03
progress.writeProgress(out, client_tcp_protocol_version);
```

**Progress Format:**
```cpp
writeVarUInt(rows, out);                                 // Rows processed
writeVarUInt(bytes, out);                               // Bytes processed
writeVarUInt(total_rows, out);                          // Total rows estimate
if (client_tcp_protocol_version >= DBMS_MIN_REVISION_WITH_TOTAL_BYTES_IN_PROGRESS)
    writeVarUInt(total_bytes, out);                     // Total bytes estimate
if (client_tcp_protocol_version >= DBMS_MIN_REVISION_WITH_CLIENT_WRITE_INFO)
    writeVarUInt(written_rows, out);                    // Written rows
    writeVarUInt(written_bytes, out);                   // Written bytes
```

**Example Bytes:**
```
0x03                          // Packet Type: Server::Progress
0x64                          // Rows processed: 100
0xE8 0x07                     // Bytes processed: 1000
0xF4 0x01                     // Total rows: 500
0xA0 0x0F                     // Total bytes: 2000 (if supported)
0x00                          // Written rows: 0 (if supported)
0x00                          // Written bytes: 0 (if supported)
```

### Pong (0x04)

Response to Client Ping packet.

**Structure:**
```cpp
writeVarUInt(Protocol::Server::Pong, out);               // 0x04
// No payload
```

**Example Bytes:**
```
0x04                          // Packet Type: Server::Pong
```

### EndOfStream (0x05)

Indicates query execution completion.

**Structure:**
```cpp
writeVarUInt(Protocol::Server::EndOfStream, out);        // 0x05
// No payload
```

**Example Bytes:**
```
0x05                          // Packet Type: Server::EndOfStream
```

### ProfileInfo (0x06)

Query profiling and performance information.

**Structure:**
```cpp
writeVarUInt(Protocol::Server::ProfileInfo, out);        // 0x06
profile_info.write(out);
```

**ProfileInfo Format:**
```cpp
writeVarUInt(rows, out);                                 // Rows read
writeVarUInt(blocks, out);                              // Blocks read
writeVarUInt(bytes, out);                               // Bytes read
writeBool(applied_limit, out);                          // Was LIMIT applied?
writeVarUInt(rows_before_limit, out);                   // Rows before LIMIT
writeBool(calculated_rows_before_limit, out);           // Calculated total?
```

**Example Bytes:**
```
0x06                          // Packet Type: Server::ProfileInfo
0xC8                          // Rows read: 200
0x0A                          // Blocks read: 10
0xD0 0x07                     // Bytes read: 1000
0x01                          // LIMIT applied: true
0x90 0x01                     // Rows before LIMIT: 400
0x01                          // Calculated total: true
```

### Totals (0x07)

Aggregate totals when using GROUP BY with TOTALS.

**Structure:**
```cpp
writeVarUInt(Protocol::Server::Totals, out);             // 0x07
writeStringBinary("", out);                              // External table name
// Followed by Native format Block with totals
```

**Example Bytes:**
```
0x07                          // Packet Type: Server::Totals
0x00                          // External table name (empty)
// ... Native Block format with totals data
```

### Extremes (0x08)

Minimum and maximum values for result columns.

**Structure:**
```cpp
writeVarUInt(Protocol::Server::Extremes, out);           // 0x08
writeStringBinary("", out);                              // External table name
// Followed by Native format Block with min/max rows
```

**Example Bytes:**
```
0x08                          // Packet Type: Server::Extremes
0x00                          // External table name (empty)
// ... Native Block format with extremes data
```

### TablesStatusResponse (0x09)

Response to TablesStatusRequest with table metadata.

**Structure:**
```cpp
writeVarUInt(Protocol::Server::TablesStatusResponse, out); // 0x09
writeVarUInt(tables_data.size(), out);                   // Number of tables
for (const auto & table_data : tables_data) {
    writeStringBinary(table_data.database, out);        // Database name
    writeStringBinary(table_data.table, out);           // Table name  
    writeBool(table_data.is_replicated, out);           // Is replicated?
    if (table_data.is_replicated) {
        writeVarUInt(table_data.absolute_delay, out);   // Replication delay
    }
}
```

**Example Bytes:**
```
0x09                          // Packet Type: Server::TablesStatusResponse
0x02                          // Number of tables: 2
0x07 "default"                // Database 1
0x05 "users"                  // Table 1
0x00                          // Not replicated
0x07 "default"                // Database 2
0x06 "events"                 // Table 2
0x01                          // Is replicated
0x0A                          // Replication delay: 10
```

### Log (0x0A)

Server log messages sent to client.

**Structure:**
```cpp
writeVarUInt(Protocol::Server::Log, out);                // 0x0A
writeStringBinary("", out);                              // External table name
// Followed by Native format Block with log entries
```

**Log Block Columns:**
- `event_date` (Date)
- `event_time` (DateTime) 
- `event_time_microseconds` (DateTime64)
- `microseconds` (UInt32)
- `thread_name` (String)
- `thread_id` (UInt64)
- `level` (Enum8)
- `query_id` (String)
- `logger_name` (String)
- `message` (String)
- `revision` (UInt32)
- `source_file` (String)
- `source_line` (UInt64)

### TableColumns (0x0B)

Table schema information with column definitions.

**Structure:**
```cpp
writeVarUInt(Protocol::Server::TableColumns, out);       // 0x0B
writeStringBinary(external_table_name, out);            // Table name
writeStringBinary("", out);                              // External table name
// Followed by Native format Block with column info
```

**Column Info Block Columns:**
- `name` (String) - Column name
- `type` (String) - Column type  
- `default_type` (String) - Default value type
- `default_expression` (String) - Default expression
- `comment` (String) - Column comment
- `codec_expression` (String) - Compression codec
- `ttl_expression` (String) - TTL expression

### PartUUIDs (0x0C)

MergeTree part identifiers for distributed queries.

**Structure:**
```cpp
writeVarUInt(Protocol::Server::PartUUIDs, out);          // 0x0C
writeVarUInt(part_uuids.size(), out);                    // Number of UUIDs
for (const auto & part_uuid : part_uuids) {
    writeUUIDText(part_uuid, out);                       // UUID as string
}
```

**Example Bytes:**
```
0x0C                          // Packet Type: Server::PartUUIDs
0x02                          // Number of UUIDs: 2
0x24 "550e8400-e29b-41d4-a716-446655440000" // UUID 1
0x24 "550e8400-e29b-41d4-a716-446655440001" // UUID 2
```

### ReadTaskRequest (0x0D)

Request for distributed read task coordination.

**Structure:**
```cpp
writeVarUInt(Protocol::Server::ReadTaskRequest, out);    // 0x0D
// No payload - simple request
```

### ProfileEvents (0x0E)

Real-time performance metrics and profiling data.

**Structure:**
```cpp
writeVarUInt(Protocol::Server::ProfileEvents, out);      // 0x0E
writeStringBinary("", out);                              // External table name
// Followed by Native format Block with profile events
```

**ProfileEvents Block Columns:**
- `host_name` (String) - Host where event occurred
- `current_time` (DateTime) - Event timestamp
- `thread_id` (UInt64) - Thread ID
- `type` (Int8) - Event type (INCREMENT=1, GAUGE=2)
- `name` (String) - Event name
- `value` (Int64) - Event value

**Example ProfileEvents:**
```
CompressedReadBufferBytes = 179030000000    // Bytes read from compressed buffers
SelectedBytes = 229470000000                // Bytes selected from tables  
OSReadBytes = 12080000000                   // Bytes read from OS
RowsReadByMainReader = 3130000000           // Rows read by main reader
```

### MergeTreeAllRangesAnnouncement (0x0F)

Distributed MergeTree query range coordination.

**Structure:**
```cpp
writeVarUInt(Protocol::Server::MergeTreeAllRangesAnnouncement, out); // 0x0F
InitialAllRangesAnnouncement announcement;
announcement.serialize(out);
```

### MergeTreeReadTaskRequest (0x10)

MergeTree read task request in distributed queries.

**Structure:**
```cpp
writeVarUInt(Protocol::Server::MergeTreeReadTaskRequest, out); // 0x10
// Payload contains serialized read task request
```

### TimezoneUpdate (0x11)

Timezone change notification during query execution.

**Structure:**
```cpp
writeVarUInt(Protocol::Server::TimezoneUpdate, out);     // 0x11
writeStringBinary(timezone_name, out);                   // New timezone
```

**Example Bytes:**
```
0x11                          // Packet Type: Server::TimezoneUpdate
0x13 "America/New_York"       // New timezone
```

### SSHChallenge (0x12)

SSH authentication challenge for SSH-based authentication.

**Structure:**
```cpp
writeVarUInt(Protocol::Server::SSHChallenge, out);       // 0x12
writeStringBinary(challenge_data, out);                  // Random challenge bytes
```

---

## Client-to-Server Packets

### Hello (0x00)

Client capabilities, version, and authentication credentials.

**Structure:**
```cpp
writeVarUInt(Protocol::Client::Hello, out);              // 0x00
writeStringBinary(client_name, out);                     // "ClickHouse client"
writeVarUInt(client_version_major, out);                 // e.g., 24
writeVarUInt(client_version_minor, out);                 // e.g., 10
writeVarUInt(client_tcp_protocol_version, out);          // e.g., 54477
writeStringBinary(default_database, out);               // "default"
writeStringBinary(user, out);                           // "default"
writeStringBinary(password, out);                       // Password or token
if (client_tcp_protocol_version >= DBMS_MIN_REVISION_WITH_QUOTA_KEY_IN_CLIENT_INFO)
    writeStringBinary(quota_key, out);                   // Quota key
if (client_tcp_protocol_version >= DBMS_MIN_REVISION_WITH_DISTRIBUTED_DEPTH)
    writeVarUInt(distributed_depth, out);               // Query depth
if (client_tcp_protocol_version >= DBMS_MIN_REVISION_WITH_VERSION_PATCH)
    writeVarUInt(client_version_patch, out);            // Patch version
if (client_tcp_protocol_version >= DBMS_MIN_REVISION_WITH_OPENTELEMETRY)
    client_trace_context.serialize(out);                // OpenTelemetry context
```

**Example Bytes:**
```
0x00                          // Packet Type: Client::Hello
0x10 "ClickHouse client"      // Client name
0x18                          // Major version: 24
0x0A                          // Minor version: 10
0xCD 0xD4 0x03               // Protocol version: 54477
0x07 "default"                // Database
0x07 "default"                // User
0x08 "password"               // Password
0x00                          // Quota key (empty, if supported)
0x00                          // Distributed depth: 0 (if supported)
0x05                          // Patch version: 5 (if supported)
```

### Query (0x01)

SQL query with settings and execution parameters.

**Structure:**
```cpp
writeVarUInt(Protocol::Client::Query, out);              // 0x01
writeStringBinary(query_id, out);                        // Unique query ID
if (client_tcp_protocol_version >= DBMS_MIN_REVISION_WITH_CLIENT_INFO)
    client_info.write(out);                             // Client info
writeStringBinary(query, out);                          // SQL query text
// Settings and compression info
```

**ClientInfo Structure:**
```cpp
writeVarUInt(client_info.query_kind, out);              // INITIAL_QUERY=1
writeStringBinary(client_info.initial_user, out);       // Initial user
writeStringBinary(client_info.initial_query_id, out);   // Initial query ID  
writeStringBinary(client_info.initial_address.toString(), out); // Client IP
writeVarUInt(client_info.interface, out);               // Interface type
writeStringBinary(client_info.os_user, out);            // OS user
writeStringBinary(client_info.client_hostname, out);    // Client hostname
writeStringBinary(client_info.client_name, out);        // Client name
writeVarUInt(client_info.client_version_major, out);    // Client version
writeVarUInt(client_info.client_version_minor, out);
writeVarUInt(client_info.client_tcp_protocol_version, out);
// ... additional fields based on protocol version
```

**Example Bytes:**
```
0x01                          // Packet Type: Client::Query
0x24 "550e8400-e29b-41d4-a716-446655440000" // Query ID (UUID)
0x01                          // Query kind: INITIAL_QUERY
0x07 "default"                // Initial user
0x24 "550e8400-e29b-41d4-a716-446655440000" // Initial query ID
0x09 "127.0.0.1"              // Client address
0x01                          // Interface: TCP
0x04 "user"                   // OS user
0x07 "client1"                // Client hostname
0x10 "ClickHouse client"      // Client name
0x18                          // Client version major: 24
0x0A                          // Client version minor: 10
0xCD 0xD4 0x03               // Protocol version: 54477
0x0D "SELECT 1"               // SQL query
```

### Data (0x02)

INSERT data blocks for bulk data insertion.

**Structure:**
```cpp
writeVarUInt(Protocol::Client::Data, out);               // 0x02
writeStringBinary(external_table_name, out);            // Usually empty
// Followed by Native format Block data
```

**Example Bytes:**
```
0x02                          // Packet Type: Client::Data
0x00                          // External table name (empty)
// ... Native Block format with INSERT data
```

### Cancel (0x03)

Request to cancel currently running query.

**Structure:**
```cpp
writeVarUInt(Protocol::Client::Cancel, out);             // 0x03
// No payload
```

**Example Bytes:**
```
0x03                          // Packet Type: Client::Cancel
```

### Ping (0x04)

Connection keepalive heartbeat.

**Structure:**
```cpp
writeVarUInt(Protocol::Client::Ping, out);               // 0x04
// No payload  
```

**Example Bytes:**
```
0x04                          // Packet Type: Client::Ping
```

### TablesStatusRequest (0x05)

Request table status and metadata information.

**Structure:**
```cpp
writeVarUInt(Protocol::Client::TablesStatusRequest, out); // 0x05
writeVarUInt(table_names.size(), out);                   // Number of tables
for (const auto & table_name : table_names) {
    writeStringBinary(table_name.database, out);        // Database name
    writeStringBinary(table_name.table, out);           // Table name
}
```

**Example Bytes:**
```
0x05                          // Packet Type: Client::TablesStatusRequest
0x02                          // Number of tables: 2
0x07 "default"                // Database 1
0x05 "users"                  // Table 1
0x07 "default"                // Database 2
0x06 "events"                 // Table 2
```

---

## Version-Specific Behavior

### Key Protocol Version Constants

| Version | Constant | Feature |
|---------|----------|---------|
| 54226 | `DBMS_MIN_REVISION_WITH_CLIENT_INFO` | ClientInfo in Query packet |
| 54227 | `DBMS_MIN_REVISION_WITH_SERVER_TIMEZONE` | Timezone in Server Hello |
| 54372 | `DBMS_MIN_REVISION_WITH_QUOTA_KEY_IN_CLIENT_INFO` | Quota key support |
| 54405 | `DBMS_MIN_REVISION_WITH_TOTAL_BYTES_IN_PROGRESS` | Total bytes in Progress |
| 54451 | `DBMS_MIN_PROTOCOL_VERSION_WITH_INCREMENTAL_PROFILE_EVENTS` | ProfileEvents streaming |
| 54477 | `DBMS_TCP_PROTOCOL_VERSION` | Current protocol version |

### Conditional Field Handling

Many packet fields are **conditionally included** based on the negotiated protocol version:

```cpp
if (client_tcp_protocol_version >= DBMS_MIN_REVISION_WITH_FEATURE) {
    writeField(out);  // Only included if both sides support it
}
```

### Backward Compatibility

- **Newer clients** connecting to **older servers**: Advanced fields omitted
- **Older clients** connecting to **newer servers**: Extra fields ignored  
- **Version negotiation** ensures compatibility at connection establishment

---

## Implementation Notes

### Packet Parsing Strategy
1. **Read packet type** (VarInt)
2. **Switch on packet type** to determine structure
3. **Check protocol version** for conditional fields
4. **Parse payload** according to specification

### Error Handling
- **Unknown packet types**: Close connection or ignore based on implementation
- **Malformed packets**: Send Exception packet or close connection
- **Version mismatches**: Graceful degradation or connection rejection

### Performance Considerations
- **Packet buffering**: Buffer small packets to reduce system calls
- **Large packet streaming**: Use chunked encoding for >64KB payloads
- **Compression**: Apply selectively based on packet type and size
- **Connection pooling**: Reuse authenticated connections when possible

### Security Considerations
- **Input validation**: Validate all VarInt lengths and string sizes
- **Authentication**: Verify credentials before processing any other packets
- **Resource limits**: Impose limits on packet sizes and frequencies
- **Logging**: Log authentication attempts and suspicious packet patterns