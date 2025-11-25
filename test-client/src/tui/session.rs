use std::collections::VecDeque;

use tui_textarea::TextArea;

use crate::tui::widgets::table::SortableTable;

const SPARKLINE_SIZE: usize = 16;

/// A single log entry from ClickHouse
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub time:      String,
    pub thread_id: u64,
    pub source:    String,
    pub text:      String,
}

/// Combined stats data (progress + profile metrics)
#[derive(Debug)]
pub struct StatsData {
    // Read progress
    pub rows_read:  u64,
    pub bytes_read: u64,
    pub total_rows: Option<u64>,

    // Write progress
    pub rows_written:  u64,
    pub bytes_written: u64,

    // Elapsed time for rate calculation (nanoseconds)
    pub elapsed_ns: u64,

    // CPU sparkline (percentage values)
    pub cpu_history:   VecDeque<u64>,
    pub cpu_current:   u64,
    prev_cpu_us:       Option<u64>,
    prev_timestamp_us: Option<i64>,

    // RAM sparkline (bytes)
    pub ram_history: VecDeque<u64>,
    pub ram_current: u64,

    // Profile events table
    pub events: Vec<serde_json::Value>,
    pub table:  Option<SortableTable>,
}

impl Default for StatsData {
    fn default() -> Self {
        Self {
            rows_read:         0,
            bytes_read:        0,
            total_rows:        None,
            rows_written:      0,
            bytes_written:     0,
            elapsed_ns:        0,
            cpu_history:       VecDeque::with_capacity(SPARKLINE_SIZE),
            cpu_current:       0,
            prev_cpu_us:       None,
            prev_timestamp_us: None,
            ram_history:       VecDeque::with_capacity(SPARKLINE_SIZE),
            ram_current:       0,
            events:            Vec::new(),
            table:             None,
        }
    }
}

impl StatsData {
    pub fn add_progress(
        &mut self,
        rows: u64,
        bytes: u64,
        total_rows: Option<u64>,
        rows_written: u64,
        bytes_written: u64,
        elapsed_ns: u64,
    ) {
        self.rows_read = rows;
        self.bytes_read = bytes;
        if total_rows.is_some() {
            self.total_rows = total_rows;
        }
        self.rows_written = rows_written;
        self.bytes_written = bytes_written;
        self.elapsed_ns = elapsed_ns;
    }

    pub fn add_profile_event(&mut self, event: serde_json::Value) {
        use chrono::DateTime;

        if let serde_json::Value::Object(ref map) = event {
            if let (Some(name), Some(value), Some(time_str)) = (
                map.get("name").and_then(|n| n.as_str()),
                map.get("value").and_then(|v| v.as_i64()),
                map.get("current_time").and_then(|t| t.as_str()),
            ) {
                let thread_id = map.get("thread_id").and_then(|t| t.as_u64()).unwrap_or(0);

                // Initialize table if needed
                if self.table.is_none() {
                    self.table = Some(SortableTable::new(vec![
                        "Time".to_string(),
                        "Thread".to_string(),
                        "Metric".to_string(),
                        "Value".to_string(),
                    ]));
                }

                if let Some(ref mut table) = self.table {
                    table.add_row(vec![
                        time_str.to_string(),
                        thread_id.to_string(),
                        name.to_string(),
                        value.to_string(),
                    ]);
                }

                // Extract CPU and RAM metrics
                match name {
                    "OSCPUVirtualTimeMicroseconds" => {
                        // Parse timestamp for CPU % calculation
                        if let Ok(dt) = DateTime::parse_from_rfc3339(time_str) {
                            let timestamp_us = dt.timestamp_micros();
                            let value_u64 = value as u64;

                            if let (Some(prev_cpu), Some(prev_ts)) =
                                (self.prev_cpu_us, self.prev_timestamp_us)
                            {
                                let delta_cpu = value_u64.saturating_sub(prev_cpu);
                                let delta_wall = (timestamp_us - prev_ts).unsigned_abs();
                                if delta_wall > 0 {
                                    let cpu_pct = (delta_cpu * 100) / delta_wall;
                                    self.cpu_current = cpu_pct.min(999);
                                    self.cpu_history.push_back(self.cpu_current);
                                    if self.cpu_history.len() > SPARKLINE_SIZE {
                                        self.cpu_history.pop_front();
                                    }
                                }
                            }
                            self.prev_cpu_us = Some(value_u64);
                            self.prev_timestamp_us = Some(timestamp_us);
                        }
                    }
                    "MemoryUsage" => {
                        self.ram_current = value as u64;
                        self.ram_history.push_back(self.ram_current);
                        if self.ram_history.len() > SPARKLINE_SIZE {
                            self.ram_history.pop_front();
                        }
                    }
                    _ => {}
                }
            }
        }

        self.events.push(event);
    }

    pub fn event_count(&self) -> usize { self.events.len() }
}

/// A single query and all its associated data
#[derive(Debug)]
pub struct QueryBlock {
    pub id:        usize,
    pub sql:       String,
    pub results:   Option<SortableTable>,
    pub stats:     StatsData,
    pub logs:      Vec<LogEntry>,
    pub log_table: Option<SortableTable>,
    pub error:     Option<String>,
    pub running:   bool,
}

impl QueryBlock {
    pub fn new(id: usize, sql: String) -> Self {
        Self {
            id,
            sql,
            results: None,
            stats: StatsData::default(),
            logs: Vec::new(),
            log_table: None,
            error: None,
            running: true,
        }
    }

    pub fn add_progress(&mut self, progress: serde_json::Value) {
        if let serde_json::Value::Object(map) = progress {
            let rows = map.get("read_rows").and_then(|v| v.as_u64()).unwrap_or(0);
            let bytes = map.get("read_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
            let total = map.get("total_rows_to_read").and_then(|v| v.as_u64());
            let rows_written = map.get("written_rows").and_then(|v| v.as_u64()).unwrap_or(0);
            let bytes_written = map.get("written_bytes").and_then(|v| v.as_u64()).unwrap_or(0);
            let elapsed_ns = map.get("elapsed_ns").and_then(|v| v.as_u64()).unwrap_or(0);
            self.stats
                .add_progress(rows, bytes, total, rows_written, bytes_written, elapsed_ns);
        }
    }

    pub fn add_profile_event(&mut self, event: serde_json::Value) {
        self.stats.add_profile_event(event);
    }

    pub fn add_result_row(&mut self, row: serde_json::Value) {
        if let serde_json::Value::Object(map) = row {
            if self.results.is_none() {
                let columns: Vec<String> = map.keys().cloned().collect();
                self.results = Some(SortableTable::new(columns));
            }

            if let Some(ref mut table) = self.results {
                let row_values: Vec<String> = table
                    .columns
                    .iter()
                    .map(|col| {
                        map.get(col)
                            .map(|v| match v {
                                serde_json::Value::String(s) => s.clone(),
                                serde_json::Value::Null => "NULL".to_string(),
                                other => other.to_string(),
                            })
                            .unwrap_or_default()
                    })
                    .collect();
                table.add_row(row_values);
            }
        }
    }

    pub fn add_log(&mut self, log: serde_json::Value) {
        if let serde_json::Value::Object(map) = log {
            let entry = LogEntry {
                time:      map.get("time").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                thread_id: map.get("thread_id").and_then(|v| v.as_u64()).unwrap_or(0),
                source:    map.get("source").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                text:      map.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            };

            // Initialize table if needed
            if self.log_table.is_none() {
                self.log_table = Some(SortableTable::new(vec![
                    "Time".to_string(),
                    "Thread".to_string(),
                    "Source".to_string(),
                    "Text".to_string(),
                ]));
            }

            if let Some(ref mut table) = self.log_table {
                table.add_row(vec![
                    entry.time.clone(),
                    entry.thread_id.to_string(),
                    entry.source.clone(),
                    entry.text.clone(),
                ]);
            }

            self.logs.push(entry);
        }
    }

    pub fn result_count(&self) -> usize { self.results.as_ref().map(|t| t.rows.len()).unwrap_or(0) }
}

/// Which sub-pane type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubPane {
    Sql,
    Results,
    Stats,
    Logs,
}

/// What is currently focused
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Focus {
    NewQuery,
    Sidebar,
    SubPane(SubPane), // Always refers to selected_query
}

/// Navigation vs Edit mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Navigation,
    Edit,
}

/// The entire session state
pub struct Session {
    pub blocks:         Vec<QueryBlock>,
    pub new_query:      TextArea<'static>,
    pub focus:          Focus,
    pub mode:           Mode,
    pub selected_query: Option<usize>, // ID of query shown in main area
    pub sidebar_scroll: usize,         // Scroll offset for sidebar list
    next_id:            usize,
}

impl Session {
    pub fn new() -> Self {
        let mut new_query = TextArea::default();
        new_query.set_placeholder_text("Enter SQL query... (Ctrl+Enter to execute)");

        Self {
            blocks: Vec::new(),
            new_query,
            focus: Focus::NewQuery,
            mode: Mode::Edit, // Start in edit mode in the new query pane
            selected_query: None,
            sidebar_scroll: 0,
            next_id: 0,
        }
    }

    /// Create a new query block from the current new_query text
    pub fn execute_new_query(&mut self) -> Option<(usize, String)> {
        let sql = self.new_query.lines().join("\n");
        if sql.trim().is_empty() {
            return None;
        }

        let id = self.next_id;
        self.next_id += 1;

        let block = QueryBlock::new(id, sql.clone());
        self.blocks.push(block);

        // Clear the new query editor
        self.new_query = TextArea::default();
        self.new_query.set_placeholder_text("Enter SQL query... (Ctrl+Enter to execute)");

        // Select this query and focus its results
        self.selected_query = Some(id);
        self.focus = Focus::SubPane(SubPane::Results);
        self.mode = Mode::Navigation;

        Some((id, sql))
    }

    /// Get a query block by ID
    pub fn get_block(&self, id: usize) -> Option<&QueryBlock> {
        self.blocks.iter().find(|b| b.id == id)
    }

    /// Get a mutable query block by ID
    pub fn get_block_mut(&mut self, id: usize) -> Option<&mut QueryBlock> {
        self.blocks.iter_mut().find(|b| b.id == id)
    }

    /// Get the currently selected query block
    pub fn selected_block(&self) -> Option<&QueryBlock> {
        self.selected_query.and_then(|id| self.get_block(id))
    }

    /// Get the currently selected query block mutably
    pub fn selected_block_mut(&mut self) -> Option<&mut QueryBlock> {
        self.selected_query.and_then(|id| self.blocks.iter_mut().find(|b| b.id == id))
    }

    /// Move sidebar selection up
    pub fn sidebar_prev(&mut self) {
        if let Some(current_id) = self.selected_query {
            let idx = self.blocks.iter().position(|b| b.id == current_id);
            if let Some(i) = idx {
                if i > 0 {
                    self.selected_query = Some(self.blocks[i - 1].id);
                }
            }
        } else if !self.blocks.is_empty() {
            self.selected_query = Some(self.blocks.last().unwrap().id);
        }
    }

    /// Move sidebar selection down
    pub fn sidebar_next(&mut self) {
        if let Some(current_id) = self.selected_query {
            let idx = self.blocks.iter().position(|b| b.id == current_id);
            if let Some(i) = idx {
                if i + 1 < self.blocks.len() {
                    self.selected_query = Some(self.blocks[i + 1].id);
                }
            }
        } else if !self.blocks.is_empty() {
            self.selected_query = Some(self.blocks.first().unwrap().id);
        }
    }

    /// Navigate to next sub-pane within selected query
    pub fn subpane_next(&mut self) {
        if let Focus::SubPane(pane) = self.focus {
            self.focus = Focus::SubPane(match pane {
                SubPane::Sql => SubPane::Results,
                SubPane::Results => SubPane::Stats,
                SubPane::Stats => SubPane::Logs,
                SubPane::Logs => SubPane::Sql, // wrap
            });
        }
    }

    /// Navigate to previous sub-pane within selected query
    pub fn subpane_prev(&mut self) {
        if let Focus::SubPane(pane) = self.focus {
            self.focus = Focus::SubPane(match pane {
                SubPane::Sql => SubPane::Logs, // wrap
                SubPane::Results => SubPane::Sql,
                SubPane::Stats => SubPane::Results,
                SubPane::Logs => SubPane::Stats,
            });
        }
    }
}
