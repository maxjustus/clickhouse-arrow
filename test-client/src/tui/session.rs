use std::collections::{HashMap, VecDeque};

use tui_textarea::TextArea;

use crate::tui::widgets::table::SortableTable;

const SPARKLINE_SIZE: usize = 16;
const CHART_HISTORY_SIZE: usize = 200;

/// View mode for the metrics display
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricsViewMode {
    Table,
    Expanded { index: usize },
}

/// Aggregated metric data (grouped by name)
#[derive(Debug, Clone)]
pub struct AggregatedMetric {
    pub name:          String,
    pub history:       VecDeque<i64>, // For sparkline (last N values)
    pub chart_points:  Vec<(f64, f64)>, // For expanded chart (time_ms, value)
    pub current:       i64,
    pub min:           i64,
    pub max:           i64,
    pub sum:           i128,
    pub count:         u64,
    base_timestamp_us: Option<i64>,
}

impl AggregatedMetric {
    pub fn new(name: String) -> Self {
        Self {
            name,
            history: VecDeque::with_capacity(SPARKLINE_SIZE),
            chart_points: Vec::with_capacity(CHART_HISTORY_SIZE),
            current: 0,
            min: i64::MAX,
            max: i64::MIN,
            sum: 0,
            count: 0,
            base_timestamp_us: None,
        }
    }

    pub fn add_value(&mut self, value: i64, timestamp_us: i64) {
        // Set base timestamp on first value
        if self.base_timestamp_us.is_none() {
            self.base_timestamp_us = Some(timestamp_us);
        }

        self.current = value;
        self.count += 1;
        self.sum += value as i128;
        self.min = self.min.min(value);
        self.max = self.max.max(value);

        // Sparkline history (most recent N values)
        self.history.push_back(value);
        if self.history.len() > SPARKLINE_SIZE {
            self.history.pop_front();
        }

        // Chart history (relative time in ms)
        let relative_time_ms = (timestamp_us - self.base_timestamp_us.unwrap()) as f64 / 1000.0;
        self.chart_points.push((relative_time_ms, value as f64));

        // Limit chart history
        if self.chart_points.len() > CHART_HISTORY_SIZE {
            self.chart_points.remove(0);
        }
    }

    pub fn avg(&self) -> f64 {
        if self.count == 0 { 0.0 } else { (self.sum as f64) / (self.count as f64) }
    }
}

/// A single log entry from ClickHouse
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub time:      String,
    pub thread_id: u64,
    pub source:    String,
    pub text:      String,
}

/// View mode for the logs display
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogsViewMode {
    Grouped,
    Expanded { thread_id: u64 },
}

impl Default for LogsViewMode {
    fn default() -> Self { Self::Grouped }
}

/// Grouped log data for a single thread
#[derive(Debug, Clone)]
pub struct ThreadLogGroup {
    pub thread_id:     u64,
    pub entries:       Vec<LogEntry>, // All logs for this thread (newest first)
    pub latest_time:   String,        // For sorting
    pub latest_source: String,        // For display
    pub latest_text:   String,        // For display
}

impl ThreadLogGroup {
    pub fn new(thread_id: u64) -> Self {
        Self {
            thread_id,
            entries: Vec::new(),
            latest_time: String::new(),
            latest_source: String::new(),
            latest_text: String::new(),
        }
    }

    pub fn add_entry(&mut self, entry: LogEntry) {
        self.latest_time = entry.time.clone();
        self.latest_source = entry.source.clone();
        self.latest_text = entry.text.clone();
        // Insert at front for newest-first order
        self.entries.insert(0, entry);
    }
}

/// Manages grouped logs with navigation state
#[derive(Debug)]
pub struct LogsData {
    pub groups:         HashMap<u64, ThreadLogGroup>,
    pub sorted_threads: Vec<u64>, // Sorted by most recent time desc

    // Navigation (grouped view)
    pub view_mode:      LogsViewMode,
    pub selected_row:   usize,
    pub scroll_offset:  usize,
    pub visible_height: usize,

    // Navigation (expanded view)
    pub expanded_scroll:   usize,
    pub expanded_selected: usize,
}

impl Default for LogsData {
    fn default() -> Self {
        Self {
            groups:            HashMap::new(),
            sorted_threads:    Vec::new(),
            view_mode:         LogsViewMode::Grouped,
            selected_row:      0,
            scroll_offset:     0,
            visible_height:    10,
            expanded_scroll:   0,
            expanded_selected: 0,
        }
    }
}

impl LogsData {
    pub fn add_entry(&mut self, entry: LogEntry) {
        let thread_id = entry.thread_id;
        let is_new = !self.groups.contains_key(&thread_id);

        self.groups
            .entry(thread_id)
            .or_insert_with(|| ThreadLogGroup::new(thread_id))
            .add_entry(entry);

        if is_new {
            self.sorted_threads.push(thread_id);
        }
        self.resort_threads();
    }

    fn resort_threads(&mut self) {
        let empty = String::new();
        self.sorted_threads.sort_by(|a, b| {
            let time_a = self.groups.get(a).map(|g| g.latest_time.as_str()).unwrap_or(&empty);
            let time_b = self.groups.get(b).map(|g| g.latest_time.as_str()).unwrap_or(&empty);
            time_b.cmp(time_a) // Descending - most recent first
        });
    }

    pub fn thread_count(&self) -> usize { self.sorted_threads.len() }

    pub fn total_log_count(&self) -> usize { self.groups.values().map(|g| g.entries.len()).sum() }

    pub fn selected_group(&self) -> Option<&ThreadLogGroup> {
        self.sorted_threads.get(self.selected_row).and_then(|id| self.groups.get(id))
    }

    pub fn nav_up(&mut self) {
        match self.view_mode {
            LogsViewMode::Grouped => {
                if self.selected_row > 0 {
                    self.selected_row -= 1;
                    if self.selected_row < self.scroll_offset {
                        self.scroll_offset = self.selected_row;
                    }
                }
            }
            LogsViewMode::Expanded { .. } => {
                if self.expanded_selected > 0 {
                    self.expanded_selected -= 1;
                    if self.expanded_selected < self.expanded_scroll {
                        self.expanded_scroll = self.expanded_selected;
                    }
                }
            }
        }
    }

    pub fn nav_down(&mut self) {
        match self.view_mode {
            LogsViewMode::Grouped => {
                if !self.sorted_threads.is_empty()
                    && self.selected_row < self.sorted_threads.len() - 1
                {
                    self.selected_row += 1;
                    let max_visible = self.scroll_offset + self.visible_height.saturating_sub(1);
                    if self.selected_row > max_visible {
                        self.scroll_offset =
                            self.selected_row.saturating_sub(self.visible_height.saturating_sub(1));
                    }
                }
            }
            LogsViewMode::Expanded { thread_id } => {
                if let Some(group) = self.groups.get(&thread_id) {
                    if self.expanded_selected < group.entries.len().saturating_sub(1) {
                        self.expanded_selected += 1;
                        let max_visible =
                            self.expanded_scroll + self.visible_height.saturating_sub(1);
                        if self.expanded_selected > max_visible {
                            self.expanded_scroll = self
                                .expanded_selected
                                .saturating_sub(self.visible_height.saturating_sub(1));
                        }
                    }
                }
            }
        }
    }

    pub fn page_up(&mut self) {
        let page_size = self.visible_height.max(1);
        match self.view_mode {
            LogsViewMode::Grouped => {
                self.selected_row = self.selected_row.saturating_sub(page_size);
                self.scroll_offset = self.scroll_offset.saturating_sub(page_size);
            }
            LogsViewMode::Expanded { .. } => {
                self.expanded_selected = self.expanded_selected.saturating_sub(page_size);
                self.expanded_scroll = self.expanded_scroll.saturating_sub(page_size);
            }
        }
    }

    pub fn page_down(&mut self) {
        let page_size = self.visible_height.max(1);
        match self.view_mode {
            LogsViewMode::Grouped => {
                let max_row = self.sorted_threads.len().saturating_sub(1);
                self.selected_row = (self.selected_row + page_size).min(max_row);
                let max_visible = self.scroll_offset + self.visible_height.saturating_sub(1);
                if self.selected_row > max_visible {
                    self.scroll_offset =
                        self.selected_row.saturating_sub(self.visible_height.saturating_sub(1));
                }
            }
            LogsViewMode::Expanded { thread_id } => {
                if let Some(group) = self.groups.get(&thread_id) {
                    let max_row = group.entries.len().saturating_sub(1);
                    self.expanded_selected = (self.expanded_selected + page_size).min(max_row);
                    let max_visible = self.expanded_scroll + self.visible_height.saturating_sub(1);
                    if self.expanded_selected > max_visible {
                        self.expanded_scroll = self
                            .expanded_selected
                            .saturating_sub(self.visible_height.saturating_sub(1));
                    }
                }
            }
        }
    }

    pub fn expand(&mut self) -> bool {
        match self.view_mode {
            LogsViewMode::Grouped => {
                if let Some(&thread_id) = self.sorted_threads.get(self.selected_row) {
                    self.view_mode = LogsViewMode::Expanded { thread_id };
                    self.expanded_scroll = 0;
                    self.expanded_selected = 0;
                    true
                } else {
                    false
                }
            }
            LogsViewMode::Expanded { .. } => false, // Already at deepest level
        }
    }

    pub fn collapse(&mut self) -> bool {
        match self.view_mode {
            LogsViewMode::Grouped => false, // Signal to exit edit mode
            LogsViewMode::Expanded { thread_id } => {
                self.view_mode = LogsViewMode::Grouped;
                // Restore selection to the thread we were viewing
                if let Some(idx) = self.sorted_threads.iter().position(|&id| id == thread_id) {
                    self.selected_row = idx;
                }
                true
            }
        }
    }
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
    pub cpu_history:    VecDeque<u64>,
    pub cpu_current:    u64,
    prev_cpu_us:        Option<u64>,
    prev_user_cpu_us:   Option<u64>,
    prev_system_cpu_us: Option<u64>,
    prev_timestamp_us:  Option<i64>,

    // RAM sparkline (bytes)
    pub ram_history:      VecDeque<u64>,
    pub ram_current:      u64,
    pub peak_ram_history: VecDeque<u64>,
    pub peak_ram_current: u64,

    // Aggregated profile metrics (grouped by name)
    pub metrics:        HashMap<String, AggregatedMetric>,
    pub metric_names:   Vec<String>, // Kept sorted alphabetically
    pub view_mode:      MetricsViewMode,
    pub selected_row:   usize,
    pub scroll_offset:  usize,
    pub visible_height: usize,
}

impl Default for StatsData {
    fn default() -> Self {
        Self {
            rows_read:          0,
            bytes_read:         0,
            total_rows:         None,
            rows_written:       0,
            bytes_written:      0,
            elapsed_ns:         0,
            cpu_history:        VecDeque::with_capacity(SPARKLINE_SIZE),
            cpu_current:        0,
            prev_cpu_us:        None,
            prev_user_cpu_us:   None,
            prev_system_cpu_us: None,
            prev_timestamp_us:  None,
            ram_history:        VecDeque::with_capacity(SPARKLINE_SIZE),
            ram_current:        0,
            peak_ram_history:   VecDeque::with_capacity(SPARKLINE_SIZE),
            peak_ram_current:   0,
            metrics:            HashMap::new(),
            metric_names:       Vec::new(),
            view_mode:          MetricsViewMode::Table,
            selected_row:       0,
            scroll_offset:      0,
            visible_height:     10, // Default, will be updated by UI
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
                // Parse timestamp for aggregation
                if let Ok(dt) = DateTime::parse_from_rfc3339(time_str) {
                    let timestamp_us = dt.timestamp_micros();

                    // Add to aggregated metrics
                    if !self.metrics.contains_key(name) {
                        self.metric_names.push(name.to_string());
                        self.metric_names.sort();
                        self.metrics
                            .insert(name.to_string(), AggregatedMetric::new(name.to_string()));
                    }
                    if let Some(metric) = self.metrics.get_mut(name) {
                        metric.add_value(value, timestamp_us);
                    }

                    // Also extract CPU and RAM metrics for header display
                    match name {
                        "UserTimeMicroseconds" => {
                            let value_u64 = value as u64;
                            self.prev_user_cpu_us = Some(value_u64);
                            self.prev_timestamp_us = Some(timestamp_us);
                            self.update_cpu_percentage(timestamp_us);
                        }
                        "SystemTimeMicroseconds" => {
                            let value_u64 = value as u64;
                            self.prev_system_cpu_us = Some(value_u64);
                            self.prev_timestamp_us = Some(timestamp_us);
                            self.update_cpu_percentage(timestamp_us);
                        }
                        "MemoryTrackerUsage" => {
                            self.ram_current = value as u64;
                            self.ram_history.push_back(self.ram_current);
                            if self.ram_history.len() > SPARKLINE_SIZE {
                                self.ram_history.pop_front();
                            }
                        }
                        "MemoryTrackerPeakUsage" => {
                            self.peak_ram_current = value as u64;
                            self.peak_ram_history.push_back(self.peak_ram_current);
                            if self.peak_ram_history.len() > SPARKLINE_SIZE {
                                self.peak_ram_history.pop_front();
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn update_cpu_percentage(&mut self, timestamp_us: i64) {
        // Only calculate when we have both user and system times
        let (prev_user, prev_system, prev_ts) =
            match (self.prev_user_cpu_us, self.prev_system_cpu_us, self.prev_timestamp_us) {
                (Some(u), Some(s), Some(ts)) if ts > 0 => (u, s, ts),
                _ => return, // Don't have both metrics yet
            };

        // Sum user + system to get total CPU time
        let total_cpu_us = prev_user + prev_system;

        // Calculate delta from previous reading
        if let Some(prev_total) = self.prev_cpu_us {
            let delta_cpu = total_cpu_us.saturating_sub(prev_total);
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

        // Store for next delta calculation
        self.prev_cpu_us = Some(total_cpu_us);
    }

    pub fn metric_count(&self) -> usize { self.metric_names.len() }

    /// Navigate up in the metrics table
    pub fn nav_up(&mut self) {
        if self.selected_row > 0 {
            self.selected_row -= 1;
            // Scroll up if selection goes above visible area
            if self.selected_row < self.scroll_offset {
                self.scroll_offset = self.selected_row;
            }
        }
    }

    /// Navigate down in the metrics table
    pub fn nav_down(&mut self) {
        if !self.metric_names.is_empty() && self.selected_row < self.metric_names.len() - 1 {
            self.selected_row += 1;
            // Scroll down if selection goes below visible area
            let max_visible = self.scroll_offset + self.visible_height.saturating_sub(1);
            if self.selected_row > max_visible {
                self.scroll_offset =
                    self.selected_row.saturating_sub(self.visible_height.saturating_sub(1));
            }
        }
    }

    /// Page up in the metrics table
    pub fn page_up(&mut self) {
        let page_size = self.visible_height.max(1);
        self.selected_row = self.selected_row.saturating_sub(page_size);
        self.scroll_offset = self.scroll_offset.saturating_sub(page_size);
    }

    /// Page down in the metrics table
    pub fn page_down(&mut self) {
        if self.metric_names.is_empty() {
            return;
        }
        let page_size = self.visible_height.max(1);
        let max_row = self.metric_names.len().saturating_sub(1);
        self.selected_row = (self.selected_row + page_size).min(max_row);
        // Adjust scroll to keep selection visible
        let max_visible = self.scroll_offset + self.visible_height.saturating_sub(1);
        if self.selected_row > max_visible {
            self.scroll_offset =
                self.selected_row.saturating_sub(self.visible_height.saturating_sub(1));
        }
    }

    /// Expand the selected metric to show detail view
    pub fn expand(&mut self) -> bool {
        if self.metric_names.is_empty() {
            return false;
        }
        self.view_mode = MetricsViewMode::Expanded { index: self.selected_row };
        true
    }

    /// Collapse from detail view back to table view
    pub fn collapse(&mut self) -> bool {
        match self.view_mode {
            MetricsViewMode::Table => false,
            MetricsViewMode::Expanded { index } => {
                self.view_mode = MetricsViewMode::Table;
                self.selected_row = index;
                true
            }
        }
    }

    /// Get the currently selected metric name
    pub fn selected_metric(&self) -> Option<&AggregatedMetric> {
        self.metric_names.get(self.selected_row).and_then(|name| self.metrics.get(name))
    }
}

/// A single query and all its associated data
#[derive(Debug)]
pub struct QueryBlock {
    pub id:               usize,
    pub sql:              String,
    pub sql_scroll:       u16,
    pub results:          Option<SortableTable>,
    pub stats:            StatsData,
    pub logs:             Vec<LogEntry>,
    pub logs_data:        LogsData,
    pub error:            Option<String>,
    pub running:          bool,
    pub cancel_requested: bool,
}

impl QueryBlock {
    pub fn new(id: usize, sql: String) -> Self {
        Self {
            id,
            sql,
            sql_scroll: 0,
            results: None,
            stats: StatsData::default(),
            logs: Vec::new(),
            logs_data: LogsData::default(),
            error: None,
            running: true,
            cancel_requested: false,
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
            self.stats.add_progress(rows, bytes, total, rows_written, bytes_written, elapsed_ns);
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
                // Store JSON values directly to enable nested navigation
                let row_values: Vec<serde_json::Value> = table
                    .columns
                    .iter()
                    .map(|col| map.get(col).cloned().unwrap_or(serde_json::Value::Null))
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

            self.logs_data.add_entry(entry.clone());
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
