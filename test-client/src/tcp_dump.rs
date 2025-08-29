use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::SystemTime;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Debug, Clone)]
pub struct TcpDumpConfig {
    pub enabled:   bool,
    pub format:    DumpFormat,
    pub file_path: Option<String>,
    pub verbose:   bool,
}

#[derive(Debug, Clone)]
pub enum DumpFormat {
    Hex,
    Binary,
    Json,
    Pcap,
}

impl Default for TcpDumpConfig {
    fn default() -> Self {
        Self { enabled: false, format: DumpFormat::Hex, file_path: None, verbose: false }
    }
}

#[derive(Debug, Clone)]
struct PacketCapture {
    timestamp: SystemTime,
    direction: Direction,
    data:      Vec<u8>,
    sequence:  u64,
}

#[derive(Debug, Clone, Copy)]
enum Direction {
    ClientToServer,
    ServerToClient,
}

impl Direction {
    fn as_str(&self) -> &'static str {
        match self {
            Direction::ClientToServer => "CLIENT->SERVER",
            Direction::ServerToClient => "SERVER->CLIENT",
        }
    }
}

#[derive(Debug)]
struct CaptureState {
    packets:          Vec<PacketCapture>,
    sequence_counter: u64,
    config:           TcpDumpConfig,
}

impl CaptureState {
    fn new(config: TcpDumpConfig) -> Self {
        Self { packets: Vec::new(), sequence_counter: 0, config }
    }

    fn capture_packet(&mut self, direction: Direction, data: &[u8]) {
        if !self.config.enabled || data.is_empty() {
            return;
        }

        let packet = PacketCapture {
            timestamp: SystemTime::now(),
            direction,
            data: data.to_vec(),
            sequence: self.sequence_counter,
        };

        self.sequence_counter += 1;

        match self.config.format {
            DumpFormat::Hex => self.print_hex_dump(&packet),
            DumpFormat::Json => self.print_json_dump(&packet),
            DumpFormat::Binary => self.print_binary_dump(&packet),
            DumpFormat::Pcap => {} // TODO: Implement PCAP output
        }

        self.packets.push(packet);
    }

    fn print_hex_dump(&self, packet: &PacketCapture) {
        let timestamp =
            packet.timestamp.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_millis();

        println!();
        println!(
            "=== {} Packet #{} ({} bytes) @ {} ===",
            packet.direction.as_str(),
            packet.sequence,
            packet.data.len(),
            timestamp
        );

        // Print hex dump with ASCII sidebar
        for (i, chunk) in packet.data.chunks(16).enumerate() {
            print!("{:08x}  ", i * 16);

            // Print hex bytes
            for (j, byte) in chunk.iter().enumerate() {
                if j == 8 {
                    print!(" ");
                }
                print!("{:02x} ", byte);
            }

            // Pad if chunk is less than 16 bytes
            if chunk.len() < 16 {
                for j in chunk.len()..16 {
                    if j == 8 {
                        print!(" ");
                    }
                    print!("   ");
                }
            }

            print!(" |");

            // Print ASCII representation
            for &byte in chunk {
                if byte >= 32 && byte <= 126 {
                    print!("{}", byte as char);
                } else {
                    print!(".");
                }
            }

            println!("|");
        }

        if self.config.verbose {
            self.print_protocol_analysis(packet);
        }
    }

    fn print_json_dump(&self, packet: &PacketCapture) {
        let json_packet = serde_json::json!({
            "timestamp": packet.timestamp.duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default().as_millis(),
            "sequence": packet.sequence,
            "direction": packet.direction.as_str(),
            "size": packet.data.len(),
            "data_hex": hex::encode(&packet.data),
            "protocol_info": if self.config.verbose {
                Some(self.analyze_protocol(&packet.data, packet.direction))
            } else {
                None
            }
        });

        println!("{}", serde_json::to_string(&json_packet).unwrap());
    }

    fn print_binary_dump(&self, packet: &PacketCapture) {
        // For binary mode, just write the raw bytes to stdout
        // This is useful for piping to other tools
        use std::io::Write;
        let _ = std::io::stdout().write_all(&packet.data);
    }

    fn print_protocol_analysis(&self, packet: &PacketCapture) {
        let analysis = self.analyze_protocol(&packet.data, packet.direction);
        if !analysis.is_empty() {
            println!("Protocol Analysis:");
            for field in analysis {
                println!(
                    "  {} @ offset {}: {} ({})",
                    field["name"], field["offset"], field["value"], field["hex"]
                );
            }
        }
    }

    fn analyze_protocol(&self, data: &[u8], direction: Direction) -> Vec<serde_json::Value> {
        let mut fields = Vec::new();

        if data.len() < 2 {
            return fields;
        }

        match direction {
            Direction::ClientToServer => {
                // Check for client handshake pattern
                if data.len() >= 34 && data[0] == 0x00 {
                    fields.push(serde_json::json!({
                        "offset": 0,
                        "name": "packet_type",
                        "value": "client_handshake",
                        "hex": format!("{:02x}", data[0])
                    }));

                    if data.len() > 1 {
                        let str_len = data[1] as usize;
                        if data.len() > 2 + str_len {
                            let client_name = String::from_utf8_lossy(&data[2..2 + str_len]);
                            fields.push(serde_json::json!({
                                "offset": 2,
                                "name": "client_name",
                                "value": client_name,
                                "hex": hex::encode(&data[2..2+str_len])
                            }));
                        }
                    }
                }
                // Check for query patterns
                else if data.len() > 10 && self.contains_sql_keywords(data) {
                    fields.push(serde_json::json!({
                        "offset": 0,
                        "name": "packet_type",
                        "value": "query_packet",
                        "hex": hex::encode(&data[..std::cmp::min(8, data.len())])
                    }));
                }
            }
            Direction::ServerToClient => {
                // Check for server handshake
                if data.len() > 10
                    && data[0] == 0x00
                    && data.windows(10).any(|w| w == b"ClickHouse")
                {
                    fields.push(serde_json::json!({
                        "offset": 0,
                        "name": "packet_type",
                        "value": "server_handshake",
                        "hex": hex::encode(&data[..std::cmp::min(4, data.len())])
                    }));
                }
            }
        }

        // Look for printable strings
        let strings = self.find_strings(data);
        for string_info in strings {
            fields.push(serde_json::json!({
                "offset": string_info.offset,
                "name": "string_data",
                "value": string_info.value,
                "hex": string_info.hex
            }));
        }

        fields
    }

    fn contains_sql_keywords(&self, data: &[u8]) -> bool {
        let data_upper = String::from_utf8_lossy(data).to_uppercase();
        data_upper.contains("SELECT")
            || data_upper.contains("INSERT")
            || data_upper.contains("UPDATE")
            || data_upper.contains("DELETE")
            || data_upper.contains("SHOW")
            || data_upper.contains("DESCRIBE")
    }

    fn find_strings(&self, data: &[u8]) -> Vec<StringInfo> {
        let mut strings = Vec::new();
        let mut current_string = Vec::new();
        let mut start_offset = 0;

        for (i, &byte) in data.iter().enumerate() {
            if byte >= 32 && byte <= 126 {
                if current_string.is_empty() {
                    start_offset = i;
                }
                current_string.push(byte as char);
            } else {
                if current_string.len() >= 4 {
                    let value: String = current_string.iter().collect();
                    strings.push(StringInfo {
                        offset: start_offset,
                        value,
                        hex: hex::encode(&data[start_offset..start_offset + current_string.len()]),
                    });
                }
                current_string.clear();
            }
        }

        // Handle string at end of data
        if current_string.len() >= 4 {
            let value: String = current_string.iter().collect();
            strings.push(StringInfo {
                offset: start_offset,
                value,
                hex: hex::encode(&data[start_offset..start_offset + current_string.len()]),
            });
        }

        strings
    }
}

#[derive(Debug)]
struct StringInfo {
    offset: usize,
    value:  String,
    hex:    String,
}

pub struct TcpDumpStream<S> {
    inner: S,
    state: Arc<Mutex<CaptureState>>,
}

impl<S> TcpDumpStream<S> {
    pub fn new(inner: S, config: TcpDumpConfig) -> Self {
        Self { inner, state: Arc::new(Mutex::new(CaptureState::new(config))) }
    }

    pub fn captured_packets(&self) -> Vec<PacketCapture> {
        self.state.lock().unwrap().packets.clone()
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for TcpDumpStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before_len = buf.filled().len();

        match Pin::new(&mut self.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let after_len = buf.filled().len();
                let bytes_read = after_len - before_len;

                if bytes_read > 0 {
                    let new_data = &buf.filled()[before_len..after_len];
                    self.state.lock().unwrap().capture_packet(Direction::ServerToClient, new_data);
                }

                Poll::Ready(Ok(()))
            }
            poll => poll,
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for TcpDumpStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(bytes_written)) => {
                if bytes_written > 0 {
                    self.state
                        .lock()
                        .unwrap()
                        .capture_packet(Direction::ClientToServer, &buf[..bytes_written]);
                }
                Poll::Ready(Ok(bytes_written))
            }
            poll => poll,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
