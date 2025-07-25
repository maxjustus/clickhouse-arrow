# ClickHouse Binary Protocol - Advanced Features

This document provides detailed specifications for advanced features of the ClickHouse native binary protocol including distributed query processing, real-time streaming, bulk operations, and sophisticated connection management.

## Advanced Features Overview

ClickHouse's protocol supports enterprise-grade features for high-performance distributed analytics:

- **Distributed Query Coordination** across multiple shards with sophisticated load balancing
- **Real-time Streaming** with ProfileEvents and progress reporting  
- **Bulk Data Operations** with backpressure control and flow management
- **Advanced Connection Management** including multiplexing, hedging, and failover
- **Parallel Replica Processing** with consistent coordination
- **Streaming Compression** and adaptive network optimization

---

## 1. Distributed Query Processing

### 1.1 Multi-Shard Query Architecture

The distributed query system uses a **coordinator-shard model** implemented through several key components:

**Core Components:**
- **RemoteQueryExecutor**: Central coordinator for distributed execution
- **MultiplexedConnections**: Manages simultaneous connections to multiple shards
- **HedgedConnections**: Implements fault-tolerant hedged request patterns
- **ParallelReplicasReadingCoordinator**: Coordinates parallel replica reads

### 1.2 Distributed Query Flow

**Phase 1: Query Planning**
```cpp
// Query decomposition and distribution
DistributedCreateLocalPlan plan_builder;
auto local_plan = plan_builder.build(query, cluster_info);

// Connection establishment to shards
for (auto & shard : shards) {
    connections.emplace_back(connection_pool->get(shard.address));
}
```

**Phase 2: Query Execution**
```
Coordinator → Shard[1..N]: Query packets
Coordinator → Shard[1..N]: QueryPlan packets (if supported)
Shard[1..N] → Coordinator: Data packets (streaming results)
Shard[1..N] → Coordinator: Progress packets (execution status)
Shard[1..N] → Coordinator: ProfileEvents packets (performance metrics)
```

**Example Distributed Query Packet Flow:**
```
// Query distribution phase
Client -> Shard1: Query("SELECT sum(revenue) FROM sales WHERE date >= '2024-01-01'")
Client -> Shard2: Query("SELECT sum(revenue) FROM sales WHERE date >= '2024-01-01'")
Client -> Shard3: Query("SELECT sum(revenue) FROM sales WHERE date >= '2024-01-01'")

// Parallel execution phase  
Shard1 -> Client: Progress(rows=1000000, bytes=100MB)
Shard2 -> Client: Progress(rows=800000, bytes=80MB)
Shard3 -> Client: Progress(rows=1200000, bytes=120MB)

// Result aggregation phase
Shard1 -> Client: Data([sum=5000000])
Shard2 -> Client: Data([sum=4000000])  
Shard3 -> Client: Data([sum=6000000])

// Final aggregation locally: sum=15000000
```

### 1.3 Shard-to-Shard Communication

**Direct Shard Communication Packets:**
```cpp
// MergeTree range coordination
Protocol::Server::MergeTreeAllRangesAnnouncement = 15  
Protocol::Server::MergeTreeReadTaskRequest = 16

// Part UUID coordination for deduplication
Protocol::Server::PartUUIDs = 12

// Generic task coordination
Protocol::Server::ReadTaskRequest = 13
```

**MergeTree Range Distribution Example:**
```cpp
// Coordinator announces available ranges
MergeTreeAllRangesAnnouncement {
    coordinator_id: "node-1",
    available_ranges: [
        {part: "202401_1_1_0", range: [0, 1000000]},
        {part: "202401_2_2_0", range: [0, 800000]},
        {part: "202401_3_3_0", range: [0, 1200000]}
    ]
}

// Replicas request specific ranges
MergeTreeReadTaskRequest {
    replica_id: "replica-1",
    requested_ranges: [{part: "202401_1_1_0", range: [0, 500000]}]
}
```

### 1.4 Result Aggregation Strategies

**Streaming Result Aggregation:**
```cpp
class RemoteQueryExecutor {
    ReadResult read() override {
        while (true) {
            auto packet = connections->receivePacket();
            
            switch (packet.type) {
            case Protocol::Server::Data:
                return ReadResult(adaptBlockStructure(packet.block, header));
            case Protocol::Server::Progress:
                if (progress_callback) progress_callback(packet.progress);
                break;
            case Protocol::Server::EndOfStream:
                return ReadResult();
            }
        }
    }
};
```

**Block Structure Adaptation:**
```cpp
// Adapt remote block structure to local expectations
Block adaptBlockStructure(const Block & block, const Block & header) {
    if (blocksHaveEqualStructure(block, header))
        return block;
        
    auto converting_actions = ActionsDAG::makeConvertingActions(
        block.cloneEmpty().getColumnsWithTypeAndName(),
        header.cloneEmpty().getColumnsWithTypeAndName(),
        ActionsDAG::MatchColumnsMode::Name);
        
    return converting_actions->execute(block);
}
```

### 1.5 Distributed JOIN Operations

**Cross-Shard JOIN Coordination:**
```cpp
// Local JOIN with remote data fetching
class DistributedJoinPipeline {
    // 1. Send JOIN keys to remote shards
    void distributeJoinKeys(const Block & join_keys);
    
    // 2. Receive matching records from shards  
    Block receiveJoinResults();
    
    // 3. Perform local JOIN with aggregated remote data
    Block executeLocalJoin(const Block & local_data, const Block & remote_data);
};
```

### 1.6 Scalar Subquery Optimization

**Scalar Value Distribution:**
```cpp
// Distribute scalar subquery results to all shards
void RemoteQueryExecutor::sendScalars(const Scalars & scalars) {
    for (const auto & [name, value] : scalars) {
        writeVarUInt(Protocol::Client::Scalar, *out);
        writeStringBinary(name, *out);
        value.serialize(*out);  // Serialize scalar value
    }
}
```

---

## 2. Real-time Features

### 2.1 ProfileEvents Streaming Implementation

**ProfileEvents Collection Architecture:**
```cpp
// Real-time event collection and transmission
class ProfileEventsHandler {
    ThreadGroupPtr thread_group;
    ProfileEvents::Counters last_sent_snapshots;
    
    void sendProfileEvents(QueryState & state) {
        Block block = ProfileEvents::getProfileEvents(
            host_name, 
            state.profile_queue, 
            state.last_sent_snapshots
        );
        
        if (block.rows() > 0) {
            writeVarUInt(Protocol::Server::ProfileEvents, *out);
            writeStringBinary("", *out);  // External table name
            state.profile_events_block_out->write(block);
        }
    }
};
```

**ProfileEvents Block Structure:**
```cpp
// ProfileEvents block columns (Native format)
Block createProfileEventsBlock() {
    return Block({
        {"host_name", std::make_shared<DataTypeString>()},
        {"current_time", std::make_shared<DataTypeDateTime>()},
        {"thread_id", std::make_shared<DataTypeUInt64>()},
        {"type", std::make_shared<DataTypeInt8>()},        // INCREMENT=1, GAUGE=2
        {"name", std::make_shared<DataTypeString>()},      // Event name
        {"value", std::make_shared<DataTypeInt64>()}       // Event value
    });
}
```

**Example ProfileEvents Streaming:**
```
// Real-time ProfileEvents during query execution
ProfileEvents {
    host_name: "shard-1",
    thread_id: 12345,
    events: [
        {name: "CompressedReadBufferBytes", type: INCREMENT, value: 179030000000},
        {name: "SelectedBytes", type: INCREMENT, value: 229470000000},  
        {name: "OSReadBytes", type: INCREMENT, value: 12080000000},
        {name: "RowsReadByMainReader", type: INCREMENT, value: 3130000000},
        {name: "QueryMemoryLimitExceeded", type: GAUGE, value: 0}
    ]
}
```

### 2.2 Progress Reporting Mechanisms

**Progress Packet Structure and Timing:**
```cpp
struct Progress {
    std::atomic<UInt64> read_rows{0};         // Rows processed
    std::atomic<UInt64> read_bytes{0};        // Bytes processed
    std::atomic<UInt64> read_raw_bytes{0};    // Raw bytes (before compression)
    std::atomic<UInt64> total_rows_to_read{0}; // Estimated total rows
    std::atomic<UInt64> total_bytes_to_read{0}; // Estimated total bytes
    std::atomic<UInt64> written_rows{0};       // Written rows (for INSERTs)
    std::atomic<UInt64> written_bytes{0};      // Written bytes (for INSERTs)
    
    void writeProgress(WriteBuffer & out, UInt64 client_revision) const;
    void readProgress(ReadBuffer & in, UInt64 server_revision);
};
```

**Adaptive Progress Reporting:**
```cpp
// Send progress based on time intervals and data thresholds
void TCPHandler::sendProgress() {
    auto elapsed_ns = watch.elapsedNanoseconds();
    
    if (elapsed_ns - state.prev_elapsed_ns >= interactive_delay * 1000000ULL) {
        state.prev_elapsed_ns = elapsed_ns;
        
        Progress current_progress = getProgress();
        writeVarUInt(Protocol::Server::Progress, *out);
        current_progress.writeProgress(*out, client_tcp_protocol_version);
        out->next();
    }
}
```

### 2.3 Query Profiling Data Collection

**ProfileInfo Structure:**
```cpp
struct ProfileInfo {
    UInt64 rows = 0;                    // Total rows read
    UInt64 blocks = 0;                  // Total blocks read  
    UInt64 bytes = 0;                   // Total bytes read
    bool applied_limit = false;         // Whether LIMIT was applied
    UInt64 rows_before_limit = 0;       // Rows before LIMIT
    bool calculated_rows_before_limit = false;
    
    void write(WriteBuffer & out) const;
    void read(ReadBuffer & in);
};
```

**Memory and CPU Profiling:**
```cpp
// Detailed profiling information
class QueryProfiler {
    std::atomic<UInt64> memory_usage{0};
    std::atomic<UInt64> peak_memory_usage{0};
    std::chrono::steady_clock::time_point start_time;
    std::atomic<UInt64> cpu_time_ns{0};
    
    ProfileInfo getProfileInfo() const;
    void updateMemoryUsage(UInt64 usage);
    void recordCPUTime(UInt64 cpu_ns);
};
```

### 2.4 Performance Metric Aggregation

**Client-Side Metric Aggregation:**
```cpp
class ProgressTable {
    struct HostData {
        String host_name;
        ProfileEvents::Counters last_values;
        ProfileEvents::Counters current_values;
        std::chrono::steady_clock::time_point last_update;
    };
    
    std::vector<HostData> hosts_data;
    
    void updateTable(const Block & block);      // Update from ProfileEvents
    void writeTable(WriteBuffer & out) const;  // Display formatted table
};
```

**Real-time Monitoring Integration:**
```cpp
// Integration with external monitoring systems
void ClientBase::onProfileEvents(Block & block) {
    progress_table.updateTable(block);
    
    // Send to external monitoring (Prometheus, etc.)
    if (metrics_exporter) {
        metrics_exporter->export_metrics(block);
    }
    
    // Display progress table if enabled
    if (progress_table_toggle_on) {
        progress_table.writeTable();
    }
}
```

---

## 3. Streaming and Bulk Operations

### 3.1 Large Result Set Streaming Protocol

**Chunked Data Transmission:**
```cpp
class RemoteQueryExecutor : public IRemoteQueryExecutor {
    // Streaming result reading with adaptive block sizes
    ReadResult read() override {
        if (!sent_query) {
            sendQuery();
        }
        
        while (true) {
            auto packet = connections->receivePacket();
            
            switch (packet.type) {
            case Protocol::Server::Data:
                if (packet.block) {
                    // Adapt block structure and return
                    return ReadResult(adaptBlockStructure(packet.block, header));
                }
                break;
                
            case Protocol::Server::Progress:
                if (progress_callback) {
                    progress_callback(packet.progress);
                }
                break;
                
            case Protocol::Server::EndOfStream:
                finished = true;
                return ReadResult();
            }
        }
    }
};
```

**Adaptive Block Size Management:**
```cpp
// Dynamic block size adjustment based on network conditions
class AdaptiveBlockSizer {
    size_t current_block_size = DEFAULT_BLOCK_SIZE;
    size_t min_block_size = MIN_BLOCK_SIZE;
    size_t max_block_size = MAX_BLOCK_SIZE;
    
    size_t adjustBlockSize(UInt64 network_latency, UInt64 bandwidth) {
        if (network_latency > HIGH_LATENCY_THRESHOLD) {
            current_block_size = std::min(current_block_size * 2, max_block_size);
        } else if (bandwidth > HIGH_BANDWIDTH_THRESHOLD) {
            current_block_size = std::max(current_block_size / 2, min_block_size);
        }
        return current_block_size;
    }
};
```

### 3.2 Bulk INSERT Operation Handling

**Distributed Bulk INSERT:**
```cpp
class DistributedSink : public SinkToStorage {
    // Synchronous bulk write
    void writeSync(const Block & block) {
        for (auto & connection : connections) {
            writeVarUInt(Protocol::Client::Data, *connection->out);
            writeStringBinary("", *connection->out);  // External table
            block_out->write(block);
            connection->out->next();
        }
    }
    
    // Asynchronous bulk write with connection multiplexing
    void writeAsync(const Block & block) {
        ThreadPool thread_pool(connections.size());
        
        for (size_t i = 0; i < connections.size(); ++i) {
            thread_pool.scheduleOrThrowOnError([&, i] {
                auto & connection = connections[i];
                Block shard_block = getShardBlock(block, i);
                writeSingleShard(connection, shard_block);
            });
        }
        
        thread_pool.wait();
    }
    
    // Sharded bulk write with consistent hashing
    void writeSplitAsync(const Block & block) {
        auto sharded_blocks = splitBlockByShard(block, sharding_key);
        
        ThreadPool thread_pool(sharded_blocks.size());
        for (auto & [shard_id, shard_block] : sharded_blocks) {
            thread_pool.scheduleOrThrowOnError([&] {
                writeSingleShard(connections[shard_id], shard_block);
            });
        }
        thread_pool.wait();
    }
};
```

### 3.3 Backpressure and Flow Control

**Network Throttling Implementation:**
```cpp
class NetworkThrottler {
    std::shared_ptr<Throttler> bandwidth_throttler;
    std::shared_ptr<Throttler> bytes_throttler;
    
    NetworkThrottler(const Settings & settings) {
        if (settings.max_network_bandwidth) {
            bandwidth_throttler = std::make_shared<Throttler>(
                settings.max_network_bandwidth
            );
        }
        
        if (settings.max_network_bytes) {
            bytes_throttler = std::make_shared<Throttler>(
                0, settings.max_network_bytes, 
                std::chrono::seconds(settings.max_network_bytes_reset_period)
            );
        }
    }
    
    void throttle(UInt64 bytes_transferred) {
        if (bandwidth_throttler) {
            bandwidth_throttler->add(bytes_transferred);
        }
        if (bytes_throttler) {
            bytes_throttler->add(bytes_transferred);
        }
    }
};
```

**Connection-Level Flow Control:**
```cpp
// Apply throttling to connection streams
void Connection::setThrottler(const ThrottlerPtr & throttler_) {
    throttler = throttler_;
    
    // Apply to input/output streams
    if (throttler && maybe_compressed_out) {
        maybe_compressed_out = std::make_shared<ThrottlingWriteBuffer>(
            maybe_compressed_out, throttler
        );
    }
}
```

### 3.4 Memory Management for Large Operations

**Memory Pressure Handling:**
```cpp
class LargeOperationManager {
    std::atomic<UInt64> current_memory_usage{0};
    UInt64 max_memory_limit;
    std::condition_variable memory_condition;
    std::mutex memory_mutex;
    
    void waitForMemoryAvailable(UInt64 required_bytes) {
        std::unique_lock<std::mutex> lock(memory_mutex);
        memory_condition.wait(lock, [&] {
            return current_memory_usage + required_bytes <= max_memory_limit;
        });
        current_memory_usage += required_bytes;
    }
    
    void releaseMemory(UInt64 bytes) {
        {
            std::lock_guard<std::mutex> lock(memory_mutex);
            current_memory_usage -= bytes;
        }
        memory_condition.notify_all();
    }
};
```

**Streaming Compression for Network Efficiency:**
```cpp
// Adaptive compression based on data characteristics
class AdaptiveCompressor {
    CompressionCodecPtr selectCodec(const Block & block) {
        // Analyze block characteristics
        double compression_ratio = estimateCompressionRatio(block);
        size_t block_size = block.bytes();
        
        if (compression_ratio < 1.5 && block_size < SMALL_BLOCK_THRESHOLD) {
            return nullptr;  // No compression for small, incompressible blocks
        } else if (compression_ratio > 5.0) {
            return CompressionCodecFactory::instance().get("ZSTD", 3);  // High ratio
        } else {
            return CompressionCodecFactory::instance().get("LZ4");      // Fast compression
        }
    }
};
```

---

## 4. Advanced Connection Management

### 4.1 Connection Multiplexing Architecture

**MultiplexedConnections Implementation:**
```cpp
class MultiplexedConnections : public IConnections {
private:
    struct ReplicaState {
        ConnectionPool::Entry connection;
        UInt64 parallel_replica_offset = 0;
        bool active = true;
        
        void reset() {
            parallel_replica_offset = 0; 
            active = true;
        }
    };
    
    std::vector<ReplicaState> replica_states;
    size_t active_connection_count = 0;
    
public:
    void sendQuery(const ConnectionTimeouts & timeouts,
                   const String & query,
                   const String & query_id,
                   UInt64 stage,
                   ClientInfo & client_info) override {
        
        for (auto & state : replica_states) {
            if (state.active) {
                state.connection->sendQuery(timeouts, query, query_id, stage, client_info);
            }
        }
    }
    
    Packet receivePacket() override {
        ReplicaState & state = getReplicaForReading();
        return state.connection->receivePacket();
    }
    
private:
    ReplicaState & getReplicaForReading() {
        // Prioritize connections with pending data
        for (auto & state : replica_states) {
            if (state.active && state.connection->hasReadPendingData()) {
                return state;
            }
        }
        
        // Poll connections for availability
        return selectAvailableConnection();
    }
};
```

### 4.2 Hedged Requests and Failover

**HedgedConnections Advanced Features:**
```cpp
class HedgedConnections : public IConnections {
private:
    struct ReplicaLocation {
        ConnectionPoolPtr connection_pool;
        std::vector<size_t> indices;        // Replica indices
        bool is_local = false;
    };
    
    struct OffsetState {
        enum State { 
            INACTIVE,           // No active connections
            ACTIVE,             // Actively processing
            CANCELLED           // Cancelled due to faster response
        };
        
        State state = INACTIVE;
        size_t active_connection_count = 0;
        size_t next_replica_index_to_start = 0;
    };
    
    std::vector<ReplicaLocation> replicas;
    std::vector<OffsetState> offset_states;
    HedgedConnectionsFactory hedged_connections_factory;
    
    // Epoll for efficient connection monitoring
    Epoll epoll;
    
public:
    void sendQuery(/* ... */) override {
        // Start hedged requests to multiple replicas
        for (size_t offset = 0; offset < offset_states.size(); ++offset) {
            startHedgedRequestsForOffset(offset);
        }
    }
    
    Packet receivePacket() override {
        // Wait for first available response
        int ready_fd = epoll.wait();
        
        // Cancel slower connections once fast one responds
        for (auto & offset_state : offset_states) {
            if (offset_state.state == OffsetState::ACTIVE) {
                cancelSlowConnections(offset_state);
                break;
            }
        }
        
        return getPacketFromFastestConnection(ready_fd);
    }
    
private:
    void startHedgedRequestsForOffset(size_t offset) {
        // Start connections with staggered timing
        for (size_t replica_idx = 0; 
             replica_idx < replicas.size() && 
             offset_states[offset].active_connection_count < max_parallel_replicas; 
             ++replica_idx) {
            
            scheduleConnectionStart(offset, replica_idx, 
                                  replica_idx * hedged_connection_delay_ms);
        }
    }
};
```

### 4.3 Parallel Replica Coordination

**ParallelReplicasReadingCoordinator:**
```cpp
class ParallelReplicasReadingCoordinator {
private:
    using PartitionReadRequest = std::vector<PartitionReadRange>;
    
    struct ReplicaState {
        bool is_unavailable = false;
        size_t number_of_requests = 0;
        std::set<size_t> active_partition_requests;
    };
    
    std::vector<ReplicaState> replicas;
    std::queue<PartitionReadRequest> pending_requests;
    ParallelReadingExtension extension;
    
public:
    ParallelReadResponse handleRequest(ParallelReadRequest request) {
        auto replica_num = request.replica_num;
        auto & replica_state = replicas[replica_num];
        
        if (replica_state.is_unavailable) {
            return ParallelReadResponse{.finish = true};
        }
        
        // Distribute read ranges across available replicas
        if (!pending_requests.empty()) {
            auto read_task = pending_requests.front();
            pending_requests.pop();
            
            replica_state.active_partition_requests.insert(read_task.partition_id);
            return ParallelReadResponse{
                .description = std::move(read_task),
                .replica_num = replica_num
            };
        }
        
        return ParallelReadResponse{.finish = true};
    }
    
    void handleInitialAllRangesAnnouncement(InitialAllRangesAnnouncement announcement) {
        // Distribute announced ranges across replicas
        for (const auto & range : announcement.description) {
            pending_requests.emplace(createPartitionReadRequest(range));
        }
    }
    
    void markReplicaAsUnavailable(size_t replica_number) {
        replicas[replica_number].is_unavailable = true;
        
        // Redistribute active requests from failed replica
        redistributeActiveRequests(replica_number);
    }
};
```

### 4.4 Load Balancing Strategies

**Consistent Hash Load Balancing:**
```cpp
class ConsistentHashBalancer {
private:
    std::map<UInt64, size_t> hash_ring;  // Hash ring for consistent hashing
    std::vector<ConnectionPoolPtr> pools;
    
public:
    ConnectionPoolPtr selectPool(const String & key) {
        UInt64 hash = sipHash64(key);
        
        // Find first pool with hash >= key hash
        auto it = hash_ring.lower_bound(hash);
        if (it == hash_ring.end()) {
            it = hash_ring.begin();  // Wrap around
        }
        
        return pools[it->second];
    }
    
    void addPool(ConnectionPoolPtr pool, size_t virtual_nodes = 100) {
        size_t pool_index = pools.size();
        pools.push_back(pool);
        
        // Add virtual nodes to hash ring
        for (size_t i = 0; i < virtual_nodes; ++i) {
            String virtual_key = pool->getHost() + ":" + std::to_string(i);
            UInt64 hash = sipHash64(virtual_key);
            hash_ring[hash] = pool_index;
        }
    }
};
```

**Performance-Based Load Balancing:**
```cpp
class PerformanceLoadBalancer {
private:
    struct PoolMetrics {
        std::atomic<UInt64> total_requests{0};
        std::atomic<UInt64> total_response_time_ms{0};
        std::atomic<UInt64> active_connections{0};
        std::atomic<UInt64> failed_requests{0};
        
        double getAverageResponseTime() const {
            auto requests = total_requests.load();
            return requests > 0 ? total_response_time_ms.load() / double(requests) : 0.0;
        }
        
        double getSuccessRate() const {
            auto total = total_requests.load();
            auto failed = failed_requests.load();
            return total > 0 ? (total - failed) / double(total) : 1.0;
        }
    };
    
    std::vector<std::pair<ConnectionPoolPtr, PoolMetrics>> pools_with_metrics;
    
public:
    ConnectionPoolPtr selectOptimalPool() {
        size_t best_index = 0;
        double best_score = calculateScore(0);
        
        for (size_t i = 1; i < pools_with_metrics.size(); ++i) {
            double score = calculateScore(i);
            if (score > best_score) {
                best_score = score;
                best_index = i;
            }
        }
        
        return pools_with_metrics[best_index].first;
    }
    
private:
    double calculateScore(size_t pool_index) const {
        const auto & metrics = pools_with_metrics[pool_index].second;
        
        // Combine response time, success rate, and connection availability
        double response_time_score = 1.0 / (1.0 + metrics.getAverageResponseTime() / 1000.0);
        double success_rate_score = metrics.getSuccessRate();
        double availability_score = 1.0 / (1.0 + metrics.active_connections.load());
        
        return (response_time_score * 0.4 + 
                success_rate_score * 0.4 + 
                availability_score * 0.2);
    }
};
```

---

## 5. Performance Optimization Techniques

### 5.1 Async I/O and Non-blocking Operations

**Linux-Specific PacketReceiver (Fiber-based):**
```cpp
#if defined(OS_LINUX)
class PacketReceiver {
private:
    Epoll epoll;
    TimerDescriptor timer_fd;
    std::vector<AsyncCallback> async_callbacks;
    
public:
    bool receivePacket(AsyncCallback async_callback) {
        epoll.add(connection_fd, connection.get());
        
        if (timeout.totalMicroseconds()) {
            timer_fd.reset(timeout);
            epoll.add(timer_fd.getDescriptor(), &timer_fd);
        }
        
        // Non-blocking wait for I/O events
        int ready_fd = epoll.wait(0);  // Non-blocking
        
        if (ready_fd == timer_fd.getDescriptor()) {
            throw Exception(ErrorCodes::SOCKET_TIMEOUT, "Timeout exceeded");
        }
        
        if (ready_fd != connection_fd) {
            return false;  // No data ready
        }
        
        // Process packet asynchronously via fiber/callback
        async_callbacks.emplace_back(async_callback);
        return true;
    }
};
#endif
```

### 5.2 Memory-Mapped I/O for Large Transfers

**Memory-Mapped Buffer Implementation:**
```cpp
class MMapReadBuffer : public ReadBuffer {
private:
    int fd;
    size_t file_size;
    void * mapped_data;
    
public:
    MMapReadBuffer(const String & filename) {
        fd = open(filename.c_str(), O_RDONLY);
        file_size = lseek(fd, 0, SEEK_END);
        
        mapped_data = mmap(nullptr, file_size, PROT_READ, MAP_SHARED, fd, 0);
        if (mapped_data == MAP_FAILED) {
            throw Exception(ErrorCodes::SYSTEM_ERROR, "Cannot mmap file");
        }
        
        // Advise kernel about access patterns
        madvise(mapped_data, file_size, MADV_SEQUENTIAL);
        
        // Set buffer pointers for zero-copy access
        working_buffer = Buffer(
            static_cast<char*>(mapped_data),
            static_cast<char*>(mapped_data) + file_size
        );
        pos = working_buffer.begin();
    }
    
    ~MMapReadBuffer() {
        if (mapped_data != MAP_FAILED) {
            munmap(mapped_data, file_size);
        }
        if (fd >= 0) {
            close(fd);
        }
    }
};
```

### 5.3 Connection Pool Optimization

**Smart Connection Pool Management:**
```cpp
class OptimizedConnectionPool {
private:
    struct ConnectionEntry {
        ConnectionPtr connection;
        std::chrono::steady_clock::time_point last_used;
        std::atomic<bool> in_use{false};
        UInt64 total_queries{0};
        UInt64 total_errors{0};
    };
    
    std::vector<ConnectionEntry> connections;
    std::mutex pool_mutex;
    std::condition_variable pool_condition;
    
    // Pool configuration
    size_t max_connections;
    std::chrono::seconds connection_ttl{300};
    std::chrono::seconds cleanup_interval{60};
    
public:
    ConnectionPtr getConnection(const ConnectionTimeouts & timeouts) {
        std::unique_lock<std::mutex> lock(pool_mutex);
        
        // Find available healthy connection
        for (auto & entry : connections) {
            if (!entry.in_use && isConnectionHealthy(entry)) {
                entry.in_use = true;
                entry.last_used = std::chrono::steady_clock::now();
                return entry.connection;
            }
        }
        
        // Create new connection if pool not full
        if (connections.size() < max_connections) {
            ConnectionEntry new_entry{
                .connection = createNewConnection(timeouts),
                .last_used = std::chrono::steady_clock::now(),
                .in_use = true
            };
            connections.emplace_back(std::move(new_entry));
            return connections.back().connection;
        }
        
        // Wait for available connection
        pool_condition.wait(lock, [this] { 
            return hasAvailableConnection(); 
        });
        
        return getConnection(timeouts);  // Retry
    }
    
    void returnConnection(ConnectionPtr connection) {
        std::lock_guard<std::mutex> lock(pool_mutex);
        
        for (auto & entry : connections) {
            if (entry.connection == connection) {
                entry.in_use = false;
                entry.total_queries++;
                break;
            }
        }
        
        pool_condition.notify_one();
    }
    
private:
    bool isConnectionHealthy(const ConnectionEntry & entry) {
        // Check connection age
        auto age = std::chrono::steady_clock::now() - entry.last_used;
        if (age > connection_ttl) {
            return false;
        }
        
        // Check error rate
        if (entry.total_queries > 10) {
            double error_rate = double(entry.total_errors) / entry.total_queries;
            if (error_rate > 0.1) {  // 10% error threshold
                return false;
            }
        }
        
        // Ping connection to verify it's alive
        return entry.connection->ping();
    }
};
```

This comprehensive documentation covers all advanced features of the ClickHouse protocol, providing implementation-ready specifications for distributed query processing, real-time streaming, bulk operations, and sophisticated connection management strategies.