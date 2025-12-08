use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use clickhouse_arrow::file_stream::FileStreamWriter;
use clickhouse_arrow::native::block::Block;
use clickhouse_arrow::{CompressionMethod, NativeFormat};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::BufWriter;

const MAX_QUERIES: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QueryStoreIndex {
    pub version: u32,
    pub entries: Vec<QueryStoreEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryStoreEntry {
    pub id:          String, // "{timestamp}-{hash}" e.g. "1732645123-a3f2b1c9"
    pub hash:        String, // Just the hash
    pub sql_preview: String, // First 80 chars
    pub timestamp:   u64,    // Unix timestamp
    pub duration_ms: Option<u64>, // Execution time
    pub row_count:   u64,    // Number of rows returned
    pub error:       Option<String>, // Error message if failed
    #[serde(default)]
    pub rows_read:   Option<u64>, // Rows read from storage
    #[serde(default)]
    pub bytes_read:  Option<u64>, // Bytes read from storage
    #[serde(default)]
    pub peak_memory: Option<u64>, // Peak memory usage
}

pub struct QueryStore {
    pub index:     QueryStoreIndex,
    pub base_path: PathBuf,
}

impl QueryStore {
    pub fn find_chc_dir() -> PathBuf {
        // Walk up from cwd looking for .chc/
        let mut dir = std::env::current_dir().ok();
        while let Some(d) = dir {
            let chc = d.join(".chc");
            if chc.is_dir() {
                return chc;
            }
            dir = d.parent().map(|p| p.to_path_buf());
        }
        // Fallback to home directory
        dirs::home_dir().unwrap_or_default().join(".chc")
    }

    pub async fn load() -> Result<Self> {
        let base_path = Self::find_chc_dir();
        fs::create_dir_all(&base_path).await?;

        let index_path = base_path.join("index.json");
        let index = if index_path.exists() {
            let content = fs::read_to_string(&index_path).await?;
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            QueryStoreIndex { version: 1, entries: Vec::new() }
        };

        Ok(Self { index, base_path })
    }

    pub fn entries(&self) -> Vec<QueryStoreEntry> { self.index.entries.clone() }

    pub fn hash_sql(sql: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(sql.trim().as_bytes());
        let result = hasher.finalize();
        format!("{:x}", result)[..8].to_string()
    }

    pub async fn start_query(&self, sql: &str) -> Result<QueryCacheWriter> {
        let hash = Self::hash_sql(sql);
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let id = format!("{}-{}", timestamp, hash);
        let archive_path = self.base_path.join(format!("{}.chc", id));

        QueryCacheWriter::new(id, hash, archive_path, sql).await
    }

    pub async fn finish_query(&mut self, entry: QueryStoreEntry) -> Result<()> {
        // Each run is kept separately (unique timestamp-hash id)
        self.index.entries.push(entry);

        // Evict oldest if over limit
        self.evict_oldest().await?;
        self.save_index().await
    }

    async fn evict_oldest(&mut self) -> Result<()> {
        if self.index.entries.len() <= MAX_QUERIES {
            return Ok(());
        }

        // Sort by timestamp (oldest first)
        self.index.entries.sort_by_key(|e| e.timestamp);

        while self.index.entries.len() > MAX_QUERIES {
            if let Some(oldest) = self.index.entries.first() {
                let archive = self.base_path.join(format!("{}.chc", oldest.id));
                let _ = fs::remove_file(&archive).await;
                self.index.entries.remove(0);
            }
        }
        Ok(())
    }

    async fn save_index(&self) -> Result<()> {
        let index_path = self.base_path.join("index.json");
        let tmp_path = self.base_path.join("index.json.tmp");
        let content = serde_json::to_string_pretty(&self.index)?;
        fs::write(&tmp_path, &content).await?;
        fs::rename(&tmp_path, &index_path).await?;
        Ok(())
    }
}

pub struct QueryCacheWriter {
    pub id:           String,  // "{timestamp}-{hash}"
    pub hash:         String,  // Just hash
    archive_path:     PathBuf, // Final .chc file path
    temp_dir:         PathBuf, // Temp dir for writing files before zip
    native_writer:    FileStreamWriter<NativeFormat, BufWriter<tokio::fs::File>>,
    sql:              String,
    profile_buf:      Vec<u8>,
    profile_info_buf: Vec<u8>,
    logs_buf:         Vec<u8>,
    row_count:        u64,
    // Stats tracking
    rows_read:        u64,
    bytes_read:       u64,
    peak_memory:      u64,
}

impl QueryCacheWriter {
    pub async fn new(id: String, hash: String, archive_path: PathBuf, sql: &str) -> Result<Self> {
        // Create temp directory for building archive contents
        let temp_dir = std::env::temp_dir().join(format!("chc-{}", id));
        fs::create_dir_all(&temp_dir).await?;

        // Open native results writer to temp file
        let results_file = tokio::fs::File::create(temp_dir.join("results.native")).await?;
        let native_writer = FileStreamWriter::<NativeFormat, _>::new(
            BufWriter::new(results_file),
            CompressionMethod::LZ4,
            Default::default(),
            None,
        );

        Ok(Self {
            id,
            hash,
            archive_path,
            temp_dir,
            native_writer,
            sql: sql.to_string(),
            profile_buf: Vec::new(),
            profile_info_buf: Vec::new(),
            logs_buf: Vec::new(),
            row_count: 0,
            rows_read: 0,
            bytes_read: 0,
            peak_memory: 0,
        })
    }

    pub async fn write_block(&mut self, block: Block) -> Result<()> {
        self.row_count += block.rows;
        self.native_writer.write(block).await?;
        Ok(())
    }

    pub fn write_profile_event(&mut self, event: &serde_json::Value) {
        // Track peak memory from profile events
        if event.get("name").and_then(|v| v.as_str()) == Some("MemoryTrackerPeakUsage") {
            if let Some(value) = event.get("value").and_then(|v| v.as_i64()) {
                self.peak_memory = self.peak_memory.max(value.unsigned_abs());
            }
        }

        if let Ok(line) = serde_json::to_string(event) {
            self.profile_buf.extend_from_slice(line.as_bytes());
            self.profile_buf.push(b'\n');
        }
    }

    pub fn write_log(&mut self, log: &serde_json::Value) {
        if let Ok(line) = serde_json::to_string(log) {
            self.logs_buf.extend_from_slice(line.as_bytes());
            self.logs_buf.push(b'\n');
        }
    }

    pub fn write_profile_info(&mut self, profile_info: &serde_json::Value) {
        // Extract rows/bytes from profile info
        if let Some(rows) = profile_info.get("rows").and_then(|v| v.as_u64()) {
            self.rows_read = rows;
        }
        if let Some(bytes) = profile_info.get("bytes").and_then(|v| v.as_u64()) {
            self.bytes_read = bytes;
        }

        // ProfileInfo is sent once at query end, write only if buffer is empty
        if self.profile_info_buf.is_empty() {
            if let Ok(line) = serde_json::to_string(profile_info) {
                self.profile_info_buf.extend_from_slice(line.as_bytes());
                self.profile_info_buf.push(b'\n');
            }
        }
    }

    pub async fn finish(
        mut self,
        duration_ms: Option<u64>,
        error: Option<String>,
    ) -> Result<QueryStoreEntry> {
        use zip::write::SimpleFileOptions;

        // Finish native writer
        self.native_writer.finish().await?;

        // Write SQL to temp
        fs::write(self.temp_dir.join("query.sql"), &self.sql).await?;

        // Compress and write profile events to temp
        if !self.profile_buf.is_empty() {
            let compressed = lz4_flex::compress_prepend_size(&self.profile_buf);
            fs::write(self.temp_dir.join("profile.lz4"), &compressed).await?;
        }

        // Compress and write logs to temp
        if !self.logs_buf.is_empty() {
            let compressed = lz4_flex::compress_prepend_size(&self.logs_buf);
            fs::write(self.temp_dir.join("logs.lz4"), &compressed).await?;
        }

        // Compress and write profile info to temp
        if !self.profile_info_buf.is_empty() {
            let compressed = lz4_flex::compress_prepend_size(&self.profile_info_buf);
            fs::write(self.temp_dir.join("profile_info.lz4"), &compressed).await?;
        }

        // Create zip archive (sync operation)
        let archive_path = self.archive_path.clone();
        let temp_dir = self.temp_dir.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let file = std::fs::File::create(&archive_path)?;
            let mut zip = zip::ZipWriter::new(file);
            let options =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

            // Add files to zip (already compressed, use Stored)
            for entry in std::fs::read_dir(&temp_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_file() {
                    let name = path.file_name().unwrap().to_string_lossy();
                    zip.start_file(name.as_ref(), options)?;
                    let content = std::fs::read(&path)?;
                    zip.write_all(&content)?;
                }
            }
            zip.finish()?;

            // Cleanup temp dir
            let _ = std::fs::remove_dir_all(&temp_dir);
            Ok(())
        })
        .await??;

        let sql_preview: String = self.sql.chars().take(500).collect::<String>().replace('\n', " ");
        let timestamp = self.id.split('-').next().and_then(|s| s.parse().ok()).unwrap_or(0);

        Ok(QueryStoreEntry {
            id: self.id,
            hash: self.hash,
            sql_preview,
            timestamp,
            duration_ms,
            row_count: self.row_count,
            error,
            rows_read: if self.rows_read > 0 { Some(self.rows_read) } else { None },
            bytes_read: if self.bytes_read > 0 { Some(self.bytes_read) } else { None },
            peak_memory: if self.peak_memory > 0 { Some(self.peak_memory) } else { None },
        })
    }
}

/// Load a .chc archive and extract its contents
pub struct QueryArchiveReader {
    pub sql:          String,
    pub results:      Vec<u8>, // Raw .native bytes
    pub profile:      Vec<u8>, // Decompressed JSONL
    pub profile_info: Vec<u8>, // Decompressed JSONL
    pub logs:         Vec<u8>, // Decompressed JSONL
}

impl QueryArchiveReader {
    pub fn open(path: &PathBuf) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        let mut archive = zip::ZipArchive::new(file)?;

        let mut sql = String::new();
        let mut results = Vec::new();
        let mut profile = Vec::new();
        let mut profile_info = Vec::new();
        let mut logs = Vec::new();

        // Read SQL
        if let Ok(mut file) = archive.by_name("query.sql") {
            file.read_to_string(&mut sql)?;
        }

        // Read results.native
        if let Ok(mut file) = archive.by_name("results.native") {
            file.read_to_end(&mut results)?;
        }

        // Read and decompress profile
        if let Ok(mut file) = archive.by_name("profile.lz4") {
            let mut compressed = Vec::new();
            file.read_to_end(&mut compressed)?;
            if let Ok(decompressed) = lz4_flex::decompress_size_prepended(&compressed) {
                profile = decompressed;
            }
        }

        // Read and decompress logs
        if let Ok(mut file) = archive.by_name("logs.lz4") {
            let mut compressed = Vec::new();
            file.read_to_end(&mut compressed)?;
            if let Ok(decompressed) = lz4_flex::decompress_size_prepended(&compressed) {
                logs = decompressed;
            }
        }

        // Read and decompress profile_info (optional, for backwards compatibility)
        if let Ok(mut file) = archive.by_name("profile_info.lz4") {
            let mut compressed = Vec::new();
            file.read_to_end(&mut compressed)?;
            if let Ok(decompressed) = lz4_flex::decompress_size_prepended(&compressed) {
                profile_info = decompressed;
            }
        }

        Ok(Self { sql, results, profile, profile_info, logs })
    }
}
