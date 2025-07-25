# ClickHouse Binary Protocol Implementation - Source File Reference

This document lists all source files relevant to the ClickHouse native binary protocol implementation. The goal is to provide comprehensive context for understanding and implementing the protocol.

## Overview

The ClickHouse binary protocol is a packet-based TCP protocol that enables efficient communication between clients and servers. It supports compression, authentication, distributed queries, and streaming data transfer using the Native binary format.

**Current Protocol Version**: 54477  
**ProfileEvents Support**: Minimum version 54451

---

## 1. Core Protocol Definitions

### `src/Core/Protocol.h`
**Role**: Central protocol specification and packet type definitions
- Defines Server packet types (Hello=0, Data=1, Exception=2, Progress=3, ProfileEvents=14, etc.)
- Defines Client packet types (Hello=0, Query=1, Data=2, Cancel=3, etc.)
- Contains comprehensive protocol flow documentation
- Authentication type markers (INTER_SERVER_SECRET, SSH_CHALLENGE, JWT)
- Protocol constants and magic numbers

### `src/Core/ProtocolDefines.h`
**Role**: Protocol version constants and feature compatibility
- Current TCP protocol version (DBMS_TCP_PROTOCOL_VERSION = 54477)
- Minimum version requirements for features (DBMS_MIN_PROTOCOL_VERSION_WITH_*)
- Protocol revision history and compatibility matrix
- Feature flags for incremental capabilities

---

## 2. Server-Side Protocol Implementation

### `src/Server/TCPHandler.h` / `src/Server/TCPHandler.cpp`
**Role**: Main server-side protocol handler implementation
- Implements complete packet processing pipeline
- Handles authentication (password, SSH, JWT, inter-server)
- Query execution coordination and result streaming
- Compression management and profile events transmission
- Connection state management and error handling

### `src/Server/TCPServer.h` / `src/Server/TCPServer.cpp`
**Role**: TCP server infrastructure for accepting connections
- Socket listening and connection acceptance
- Connection lifecycle management
- Thread pool coordination for concurrent connections

### `src/Server/TCPProtocolStackHandler.h`
**Role**: Protocol stack abstraction layer
- Layered protocol handling architecture
- Support for different protocol variants

### `src/Server/TCPProtocolStackFactory.h`
**Role**: Factory for creating protocol stack handlers
- Protocol detection and handler selection
- Configuration-based protocol customization

### `src/Server/TCPProtocolStackData.h`
**Role**: Data structures for protocol stack management
- Protocol state and configuration data

### `src/Server/TCPHandlerFactory.h`
**Role**: Factory for creating TCP handlers
- Handler instantiation and configuration
- Resource management for handlers

### `src/Server/TCPServerConnectionFactory.h`
**Role**: Factory for server connection creation
- Connection object instantiation
- Server-side connection configuration

### `src/Server/ProtocolServerAdapter.h` / `src/Server/ProtocolServerAdapter.cpp`
**Role**: Adapter pattern for protocol server implementations
- Interface abstraction for different server types
- Protocol adaptation layer

---

## 3. Client-Side Protocol Implementation

### `src/Client/Connection.h` / `src/Client/Connection.cpp`
**Role**: Main client-side connection implementation
- Implements IServerConnection interface
- Handles client-to-server communication and authentication
- Manages query sending, result receiving, and compression
- Protocol packet serialization/deserialization
- Connection establishment and teardown

### `src/Client/IServerConnection.h`
**Role**: Abstract interface for server connections
- Defines contract for client-server communication
- Standard API for different connection implementations
- Query execution and data transfer interface

### `src/Client/ConnectionParameters.h` / `src/Client/ConnectionParameters.cpp`
**Role**: Connection configuration and parameter management
- Host, port, authentication credentials
- Timeout and retry configuration
- Protocol-specific settings

### `src/Client/ConnectionEstablisher.h` / `src/Client/ConnectionEstablisher.cpp`
**Role**: Connection establishment logic and retry mechanisms
- Connection attempt coordination
- Failover and retry strategies
- Network error handling

### `src/Client/ConnectionPool.h` / `src/Client/ConnectionPool.cpp`
**Role**: Connection pooling for efficient resource management
- Connection reuse and lifecycle management
- Resource optimization for frequent connections
- Thread-safe connection sharing

### `src/Client/ConnectionPoolWithFailover.h` / `src/Client/ConnectionPoolWithFailover.cpp`
**Role**: Failover-enabled connection pooling
- Automatic failover between multiple servers
- Health monitoring and recovery
- Load balancing across replicas

### `src/Client/LocalConnection.h` / `src/Client/LocalConnection.cpp`
**Role**: Local connection implementation for embedded usage
- In-process connection without network layer
- Same interface as network connections
- Used for local queries and testing

### `src/Client/PacketReceiver.h` / `src/Client/PacketReceiver.cpp`
**Role**: Packet receiving and processing on client side
- Asynchronous packet reception
- Packet type dispatch and handling
- Progress and profile events processing

---

## 4. Multiple Connection Management

### `src/Client/MultiplexedConnections.h` / `src/Client/MultiplexedConnections.cpp`
**Role**: Managing multiple parallel connections
- Distributed query execution coordination
- Result aggregation from multiple sources
- Parallel data streaming

### `src/Client/HedgedConnections.h` / `src/Client/HedgedConnections.cpp`
**Role**: Hedged connection strategy for improved latency
- Multiple connection attempts to reduce tail latency
- Winner-take-all connection selection
- Network performance optimization

### `src/Client/IConnections.h`
**Role**: Abstract interface for connection management
- Common interface for single/multiple connection handling
- Unified API for different connection strategies

---

## 5. Data Serialization/Deserialization

### `src/Formats/NativeReader.h` / `src/Formats/NativeReader.cpp`
**Role**: Reading data in ClickHouse native binary format
- Block deserialization from protocol streams
- Column data type handling
- Efficient binary data parsing

### `src/Formats/NativeWriter.h` / `src/Formats/NativeWriter.cpp`
**Role**: Writing data in ClickHouse native binary format  
- Block serialization for protocol streams
- Column data type serialization
- Efficient binary data encoding

### `src/IO/VarInt.h`
**Role**: Variable-length integer encoding/decoding utilities
- writeVarUInt/readVarUInt functions used throughout protocol
- Efficient integer compression for protocol overhead reduction
- Cross-platform integer serialization

### `src/IO/WriteHelpers.h` / `src/IO/ReadHelpers.h`
**Role**: Low-level serialization helpers
- Protocol data type serialization utilities
- String, binary, and numeric type handling
- Endianness and encoding management

---

## 6. Network Buffer Classes

### `src/IO/ReadBufferFromPocoSocket.h` / `src/IO/ReadBufferFromPocoSocket.cpp`
**Role**: Reading from TCP sockets using Poco networking
- Socket-based data input with buffering
- Network error handling and recovery
- Integration with ClickHouse I/O system

### `src/IO/WriteBufferFromPocoSocket.h` / `src/IO/WriteBufferFromPocoSocket.cpp`  
**Role**: Writing to TCP sockets using Poco networking
- Socket-based data output with buffering
- Efficient network data transmission
- Integration with ClickHouse I/O system

### `src/IO/ReadBufferFromPocoSocketChunked.h` / `src/IO/ReadBufferFromPocoSocketChunked.cpp`
**Role**: Chunked reading for protocol packet handling
- Protocol packet boundary management
- Chunked data reception and reassembly
- Streaming data processing

### `src/IO/WriteBufferFromPocoSocketChunked.h` / `src/IO/WriteBufferFromPocoSocketChunked.cpp`
**Role**: Chunked writing for protocol packet handling
- Protocol packet boundary management  
- Chunked data transmission
- Streaming data output

### `src/IO/ConnectionTimeouts.h` / `src/IO/ConnectionTimeouts.cpp`
**Role**: Network timeout configuration for protocol connections
- Connection establishment timeouts
- Read/write operation timeouts
- Keepalive and idle timeout management

---

## 7. Compression Handling

### `src/Compression/CompressedReadBuffer.h` / `src/Compression/CompressedReadBuffer.cpp`
**Role**: Reading compressed data streams in protocol
- Automatic decompression of protocol data
- Multiple compression algorithm support
- Streaming decompression for large datasets

### `src/Compression/CompressedWriteBuffer.h` / `src/Compression/CompressedWriteBuffer.cpp`
**Role**: Writing compressed data streams in protocol  
- Automatic compression of protocol data
- Configurable compression levels and algorithms
- Streaming compression for large datasets

### `src/Compression/CompressedReadBufferBase.h` / `src/Compression/CompressedReadBufferBase.cpp`
**Role**: Base class for compressed reading operations
- Common compression handling infrastructure
- Algorithm-specific decompression logic
- Checksum validation and error handling

---

## 8. Remote Query Execution

### `src/QueryPipeline/RemoteQueryExecutor.h` / `src/QueryPipeline/RemoteQueryExecutor.cpp`
**Role**: Executing queries on remote servers via protocol
- Distributed query processing coordination
- Result streaming and aggregation
- Error handling across multiple servers

### `src/QueryPipeline/RemoteQueryExecutorReadContext.h` / `src/QueryPipeline/RemoteQueryExecutorReadContext.cpp`
**Role**: Context management for remote query execution
- Query execution state tracking
- Resource management for distributed queries
- Progress and profiling information coordination

### `src/QueryPipeline/RemoteInserter.cpp`
**Role**: Remote data insertion via protocol
- Efficient bulk data insertion over network
- Transaction coordination for distributed inserts
- Error handling and recovery for failed inserts

---

## 9. Supporting Protocol Classes

### `src/Interpreters/ClientInfo.h` / `src/Interpreters/ClientInfo.cpp`
**Role**: Client information transmitted in protocol
- Client version, user info, and capabilities
- Query metadata and execution context
- Authentication and authorization information

### `src/Interpreters/TablesStatus.cpp`
**Role**: Table status information for protocol responses
- Table metadata and statistics
- Health and availability information
- Distributed table coordination

### `src/IO/Progress.cpp`
**Role**: Query progress information transmitted via protocol
- Real-time query execution progress
- Performance metrics and statistics
- Progress reporting for long-running queries

### `src/Core/BlockInfo.cpp`
**Role**: Block metadata serialization for protocol
- Data block header information
- Column metadata and type information
- Block-level statistics and checksums

---

## 10. Data Type Serialization

### `src/DataTypes/DataTypesBinaryEncoding.cpp`
**Role**: Binary encoding of data types for protocol transmission
- Data type serialization format definitions
- Cross-version compatibility handling
- Efficient type encoding for network transfer

### `src/DataTypes/Native.h` / `src/DataTypes/Native.cpp`
**Role**: Native format handling for data types
- ClickHouse native binary format implementation
- Type-specific serialization optimizations
- Format version management

---

## 11. Protocol Testing and Examples

### `src/Client/examples/test_connect.cpp`
**Role**: Example/test code for protocol connections
- Protocol usage examples and testing
- Connection establishment verification
- Basic protocol operation demonstrations

---

## Protocol Architecture Summary

The ClickHouse binary protocol is designed as a stateful, packet-based system with the following key characteristics:

1. **Bidirectional Communication**: Both client and server send packets
2. **Compression Support**: Automatic compression/decompression of data
3. **Authentication**: Multiple authentication methods (password, SSH, JWT)
4. **Streaming**: Supports streaming of large datasets
5. **Progress Reporting**: Real-time query progress via ProfileEvents packets
6. **Distributed Queries**: Coordination across multiple servers
7. **Version Negotiation**: Backward/forward compatibility management

**Key Packet Types**:
- **Server → Client**: Hello, Data, Exception, Progress, ProfileEvents, EndOfStream
- **Client → Server**: Hello, Query, Data, Cancel, Ping

**Protocol Flow**:
1. Connection establishment and Hello packet exchange
2. Authentication (if required)
3. Query transmission and execution
4. Streaming data/progress/profile events
5. EndOfStream or Exception termination

This reference provides the complete context needed to understand, implement, or verify ClickHouse's native binary protocol.