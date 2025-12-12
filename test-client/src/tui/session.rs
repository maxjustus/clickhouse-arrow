use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use tui_textarea::TextArea;

use crate::tui::query_store::QueryStoreEntry;
use crate::tui::widgets::table::{ResultsViewMode, SortOrder, SortableTable};

const SPARKLINE_SIZE: usize = 32;
const CHART_HISTORY_SIZE: usize = 200;

/// Number of rows to jump with { / } navigation
pub const ROW_JUMP_COUNT: usize = 25;

/// View mode for the metrics display
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MetricsViewMode {
    #[default]
    Table,
    Expanded {
        index: usize,
    },
}

/// Cached UI state for a query (preserved when switching between queries)
#[derive(Debug, Clone, Default)]
pub struct ViewState {
    // Results table
    pub results_selected_row:   usize,
    pub results_scroll_offset:  usize,
    pub results_col_offset:     usize,
    pub results_view_mode:      ResultsViewMode,
    pub results_header_focused: bool,
    pub results_focused_col:    usize,
    pub results_sort_column:    Option<usize>,
    pub results_sort_order:     SortOrder,

    // Stats
    pub stats_selected_row:  usize,
    pub stats_scroll_offset: usize,
    pub stats_view_mode:     MetricsViewMode,

    // Logs
    pub logs_selected_row:  usize,
    pub logs_scroll_offset: usize,
    pub logs_view_mode:     LogsViewMode,

    // SQL
    pub sql_scroll: u16,

    // Fullscreen state
    pub fullscreen: bool,
}

/// Aggregated metric data (grouped by name)
#[derive(Debug, Clone)]
pub struct AggregatedMetric {
    pub history:       VecDeque<i64>,   // For sparkline (last N values)
    pub chart_points:  Vec<(f64, f64)>, // For expanded chart (time_ms, value)
    pub current:       i64,
    pub min:           i64,
    pub max:           i64,
    pub sum:           i128,
    pub count:         u64,
    base_timestamp_us: Option<i64>,
}

impl AggregatedMetric {
    pub fn new() -> Self {
        Self {
            history:           VecDeque::with_capacity(SPARKLINE_SIZE),
            chart_points:      Vec::with_capacity(CHART_HISTORY_SIZE),
            current:           0,
            min:               i64::MAX,
            max:               i64::MIN,
            sum:               0,
            count:             0,
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

/// View mode for the logs display (4-level hierarchy: Sources > Threads > Entries > Detail)
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LogsViewMode {
    #[default]
    Sources,
    Threads {
        source: String,
    },
    Entries {
        source:    String,
        thread_id: u64,
    },
    EntryDetail {
        source:        String,
        thread_id:     u64,
        entry_index:   usize,
        scroll_offset: u16,
    },
}

/// Grouped log data for a single thread
#[derive(Debug, Clone)]
pub struct ThreadLogGroup {
    pub thread_id:   u64,
    pub entries:     Vec<LogEntry>, // All logs for this thread (newest first)
    pub latest_time: String,        // For sorting
    pub latest_text: String,        // For display
}

impl ThreadLogGroup {
    pub fn new(thread_id: u64) -> Self {
        Self {
            thread_id,
            entries: Vec::new(),
            latest_time: String::new(),
            latest_text: String::new(),
        }
    }

    pub fn add_entry(&mut self, entry: LogEntry) {
        self.latest_time = entry.time.clone();
        self.latest_text = entry.text.clone();
        // Insert at front for newest-first order
        self.entries.insert(0, entry);
    }
}

/// Grouped log data for a single source (contains threads)
#[derive(Debug, Clone)]
pub struct SourceLogGroup {
    pub source:         String,
    pub threads:        HashMap<u64, ThreadLogGroup>,
    pub sorted_threads: Vec<u64>,
    pub latest_time:    String,
    pub latest_text:    String,
}

impl SourceLogGroup {
    pub fn new(source: String) -> Self {
        Self {
            source,
            threads: HashMap::new(),
            sorted_threads: Vec::new(),
            latest_time: String::new(),
            latest_text: String::new(),
        }
    }

    pub fn add_entry(&mut self, entry: LogEntry) {
        let thread_id = entry.thread_id;
        self.latest_time = entry.time.clone();
        self.latest_text = entry.text.clone();

        let is_new = !self.threads.contains_key(&thread_id);
        self.threads
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
            let time_a = self.threads.get(a).map(|g| g.latest_time.as_str()).unwrap_or(&empty);
            let time_b = self.threads.get(b).map(|g| g.latest_time.as_str()).unwrap_or(&empty);
            time_b.cmp(time_a) // Descending - most recent first
        });
    }

    pub fn thread_count(&self) -> usize { self.sorted_threads.len() }

    pub fn entry_count(&self) -> usize { self.threads.values().map(|t| t.entries.len()).sum() }
}

/// Manages grouped logs with navigation state
#[derive(Debug)]
pub struct LogsData {
    pub sources:        HashMap<String, SourceLogGroup>,
    pub sorted_sources: Vec<String>, // Sorted by most recent time desc

    // Navigation state
    pub view_mode:      LogsViewMode,
    pub selected_row:   usize, // Current selection at each level
    pub scroll_offset:  usize, // Scroll offset at each level
    pub visible_height: usize,

    // Secondary navigation (threads/entries within expanded view)
    pub secondary_selected: usize,
    pub secondary_scroll:   usize,

    // Tertiary navigation (entries within thread)
    pub tertiary_selected: usize,
    pub tertiary_scroll:   usize,
}

impl Default for LogsData {
    fn default() -> Self {
        Self {
            sources:            HashMap::new(),
            sorted_sources:     Vec::new(),
            view_mode:          LogsViewMode::Sources,
            selected_row:       0,
            scroll_offset:      0,
            visible_height:     10,
            secondary_selected: 0,
            secondary_scroll:   0,
            tertiary_selected:  0,
            tertiary_scroll:    0,
        }
    }
}

impl LogsData {
    pub fn add_entry(&mut self, entry: LogEntry) {
        let source = entry.source.clone();
        let is_new = !self.sources.contains_key(&source);

        self.sources
            .entry(source.clone())
            .or_insert_with(|| SourceLogGroup::new(source.clone()))
            .add_entry(entry);

        if is_new {
            self.sorted_sources.push(source);
        }
        self.resort_sources();
    }

    fn resort_sources(&mut self) {
        let empty = String::new();
        self.sorted_sources.sort_by(|a, b| {
            let time_a = self.sources.get(a).map(|g| g.latest_time.as_str()).unwrap_or(&empty);
            let time_b = self.sources.get(b).map(|g| g.latest_time.as_str()).unwrap_or(&empty);
            time_b.cmp(time_a) // Descending - most recent first
        });
    }

    pub fn source_count(&self) -> usize { self.sorted_sources.len() }

    pub fn thread_count(&self) -> usize { self.sources.values().map(|s| s.thread_count()).sum() }

    pub fn total_log_count(&self) -> usize { self.sources.values().map(|s| s.entry_count()).sum() }

    /// Get thread group, navigating through source
    fn get_thread_group(&self, source: &str, thread_id: u64) -> Option<&ThreadLogGroup> {
        self.sources.get(source)?.threads.get(&thread_id)
    }

    pub fn nav_up(&mut self) {
        match &mut self.view_mode {
            LogsViewMode::Sources => {
                if self.selected_row > 0 {
                    self.selected_row -= 1;
                    if self.selected_row < self.scroll_offset {
                        self.scroll_offset = self.selected_row;
                    }
                }
            }
            LogsViewMode::Threads { .. } => {
                if self.secondary_selected > 0 {
                    self.secondary_selected -= 1;
                    if self.secondary_selected < self.secondary_scroll {
                        self.secondary_scroll = self.secondary_selected;
                    }
                }
            }
            LogsViewMode::Entries { .. } => {
                if self.tertiary_selected > 0 {
                    self.tertiary_selected -= 1;
                    if self.tertiary_selected < self.tertiary_scroll {
                        self.tertiary_scroll = self.tertiary_selected;
                    }
                }
            }
            LogsViewMode::EntryDetail { scroll_offset, .. } => {
                *scroll_offset = scroll_offset.saturating_sub(1);
            }
        }
    }

    pub fn nav_down(&mut self) {
        match &self.view_mode {
            LogsViewMode::Sources => {
                if !self.sorted_sources.is_empty()
                    && self.selected_row < self.sorted_sources.len() - 1
                {
                    self.selected_row += 1;
                    let max_visible = self.scroll_offset + self.visible_height.saturating_sub(1);
                    if self.selected_row > max_visible {
                        self.scroll_offset =
                            self.selected_row.saturating_sub(self.visible_height.saturating_sub(1));
                    }
                }
            }
            LogsViewMode::Threads { source } => {
                let source = source.clone();
                if let Some(src) = self.sources.get(&source) {
                    if self.secondary_selected < src.sorted_threads.len().saturating_sub(1) {
                        self.secondary_selected += 1;
                        let max_visible =
                            self.secondary_scroll + self.visible_height.saturating_sub(1);
                        if self.secondary_selected > max_visible {
                            self.secondary_scroll = self
                                .secondary_selected
                                .saturating_sub(self.visible_height.saturating_sub(1));
                        }
                    }
                }
            }
            LogsViewMode::Entries { source, thread_id } => {
                let source = source.clone();
                let thread_id = *thread_id;
                if let Some(thread) = self.get_thread_group(&source, thread_id) {
                    if self.tertiary_selected < thread.entries.len().saturating_sub(1) {
                        self.tertiary_selected += 1;
                        let max_visible =
                            self.tertiary_scroll + self.visible_height.saturating_sub(1);
                        if self.tertiary_selected > max_visible {
                            self.tertiary_scroll = self
                                .tertiary_selected
                                .saturating_sub(self.visible_height.saturating_sub(1));
                        }
                    }
                }
            }
            LogsViewMode::EntryDetail { .. } => {
                if let LogsViewMode::EntryDetail { scroll_offset, .. } = &mut self.view_mode {
                    *scroll_offset = scroll_offset.saturating_add(1);
                }
            }
        }
    }

    pub fn page_up(&mut self) {
        let page_size = self.visible_height.max(1);
        match &mut self.view_mode {
            LogsViewMode::Sources => {
                self.selected_row = self.selected_row.saturating_sub(page_size);
                self.scroll_offset = self.scroll_offset.saturating_sub(page_size);
            }
            LogsViewMode::Threads { .. } => {
                self.secondary_selected = self.secondary_selected.saturating_sub(page_size);
                self.secondary_scroll = self.secondary_scroll.saturating_sub(page_size);
            }
            LogsViewMode::Entries { .. } => {
                self.tertiary_selected = self.tertiary_selected.saturating_sub(page_size);
                self.tertiary_scroll = self.tertiary_scroll.saturating_sub(page_size);
            }
            LogsViewMode::EntryDetail { scroll_offset, .. } => {
                *scroll_offset = scroll_offset.saturating_sub(page_size as u16);
            }
        }
    }

    pub fn page_down(&mut self) {
        let page_size = self.visible_height.max(1);
        match &self.view_mode {
            LogsViewMode::Sources => {
                let max_row = self.sorted_sources.len().saturating_sub(1);
                self.selected_row = (self.selected_row + page_size).min(max_row);
                let max_visible = self.scroll_offset + self.visible_height.saturating_sub(1);
                if self.selected_row > max_visible {
                    self.scroll_offset =
                        self.selected_row.saturating_sub(self.visible_height.saturating_sub(1));
                }
            }
            LogsViewMode::Threads { source } => {
                let source = source.clone();
                if let Some(src) = self.sources.get(&source) {
                    let max_row = src.sorted_threads.len().saturating_sub(1);
                    self.secondary_selected = (self.secondary_selected + page_size).min(max_row);
                    let max_visible = self.secondary_scroll + self.visible_height.saturating_sub(1);
                    if self.secondary_selected > max_visible {
                        self.secondary_scroll = self
                            .secondary_selected
                            .saturating_sub(self.visible_height.saturating_sub(1));
                    }
                }
            }
            LogsViewMode::Entries { source, thread_id } => {
                let source = source.clone();
                let thread_id = *thread_id;
                if let Some(thread) = self.get_thread_group(&source, thread_id) {
                    let max_row = thread.entries.len().saturating_sub(1);
                    self.tertiary_selected = (self.tertiary_selected + page_size).min(max_row);
                    let max_visible = self.tertiary_scroll + self.visible_height.saturating_sub(1);
                    if self.tertiary_selected > max_visible {
                        self.tertiary_scroll = self
                            .tertiary_selected
                            .saturating_sub(self.visible_height.saturating_sub(1));
                    }
                }
            }
            LogsViewMode::EntryDetail { .. } => {
                if let LogsViewMode::EntryDetail { scroll_offset, .. } = &mut self.view_mode {
                    *scroll_offset = scroll_offset.saturating_add(page_size as u16);
                }
            }
        }
    }

    pub fn expand(&mut self) -> bool {
        match &self.view_mode {
            LogsViewMode::Sources => {
                if let Some(source) = self.sorted_sources.get(self.selected_row).cloned() {
                    self.view_mode = LogsViewMode::Threads { source };
                    self.secondary_scroll = 0;
                    self.secondary_selected = 0;
                    true
                } else {
                    false
                }
            }
            LogsViewMode::Threads { source } => {
                if let Some(src) = self.sources.get(source) {
                    if let Some(&thread_id) = src.sorted_threads.get(self.secondary_selected) {
                        self.view_mode =
                            LogsViewMode::Entries { source: source.clone(), thread_id };
                        self.tertiary_scroll = 0;
                        self.tertiary_selected = 0;
                        return true;
                    }
                }
                false
            }
            LogsViewMode::Entries { source, thread_id } => {
                self.view_mode = LogsViewMode::EntryDetail {
                    source:        source.clone(),
                    thread_id:     *thread_id,
                    entry_index:   self.tertiary_selected,
                    scroll_offset: 0,
                };
                true
            }
            LogsViewMode::EntryDetail { .. } => false, // Deepest level
        }
    }

    pub fn collapse(&mut self) -> bool {
        match &self.view_mode {
            LogsViewMode::Sources => false, // Signal to exit edit mode
            LogsViewMode::Threads { source } => {
                // Restore selection to the source we were viewing
                if let Some(idx) = self.sorted_sources.iter().position(|s| s == source) {
                    self.selected_row = idx;
                }
                self.view_mode = LogsViewMode::Sources;
                true
            }
            LogsViewMode::Entries { source, thread_id } => {
                // Restore selection to the thread we were viewing
                if let Some(src) = self.sources.get(source) {
                    if let Some(idx) = src.sorted_threads.iter().position(|&id| id == *thread_id) {
                        self.secondary_selected = idx;
                    }
                }
                self.view_mode = LogsViewMode::Threads { source: source.clone() };
                true
            }
            LogsViewMode::EntryDetail { source, thread_id, entry_index, .. } => {
                self.tertiary_selected = *entry_index;
                self.view_mode =
                    LogsViewMode::Entries { source: source.clone(), thread_id: *thread_id };
                true
            }
        }
    }

    /// Move to previous row/entry (works in all view modes)
    pub fn prev_detail_entry(&mut self) {
        match &mut self.view_mode {
            LogsViewMode::Sources => {
                if self.selected_row > 0 {
                    self.selected_row -= 1;
                    if self.selected_row < self.scroll_offset {
                        self.scroll_offset = self.selected_row;
                    }
                }
            }
            LogsViewMode::Threads { .. } => {
                if self.secondary_selected > 0 {
                    self.secondary_selected -= 1;
                    if self.secondary_selected < self.secondary_scroll {
                        self.secondary_scroll = self.secondary_selected;
                    }
                }
            }
            LogsViewMode::Entries { .. } => {
                if self.tertiary_selected > 0 {
                    self.tertiary_selected -= 1;
                    if self.tertiary_selected < self.tertiary_scroll {
                        self.tertiary_scroll = self.tertiary_selected;
                    }
                }
            }
            LogsViewMode::EntryDetail { entry_index, scroll_offset, .. } => {
                if *entry_index > 0 {
                    *entry_index -= 1;
                    *scroll_offset = 0;
                }
            }
        }
    }

    /// Move to next row/entry (works in all view modes)
    pub fn next_detail_entry(&mut self) {
        match &self.view_mode {
            LogsViewMode::Sources => {
                let max_row = self.sorted_sources.len().saturating_sub(1);
                if self.selected_row < max_row {
                    self.selected_row += 1;
                    let max_visible = self.scroll_offset + self.visible_height.saturating_sub(1);
                    if self.selected_row > max_visible {
                        self.scroll_offset =
                            self.selected_row.saturating_sub(self.visible_height.saturating_sub(1));
                    }
                }
            }
            LogsViewMode::Threads { source } => {
                let source = source.clone();
                if let Some(src) = self.sources.get(&source) {
                    let max_row = src.sorted_threads.len().saturating_sub(1);
                    if self.secondary_selected < max_row {
                        self.secondary_selected += 1;
                        let max_visible =
                            self.secondary_scroll + self.visible_height.saturating_sub(1);
                        if self.secondary_selected > max_visible {
                            self.secondary_scroll = self
                                .secondary_selected
                                .saturating_sub(self.visible_height.saturating_sub(1));
                        }
                    }
                }
            }
            LogsViewMode::Entries { source, thread_id } => {
                let source = source.clone();
                let thread_id = *thread_id;
                if let Some(thread) = self.get_thread_group(&source, thread_id) {
                    let max_row = thread.entries.len().saturating_sub(1);
                    if self.tertiary_selected < max_row {
                        self.tertiary_selected += 1;
                        let max_visible =
                            self.tertiary_scroll + self.visible_height.saturating_sub(1);
                        if self.tertiary_selected > max_visible {
                            self.tertiary_scroll = self
                                .tertiary_selected
                                .saturating_sub(self.visible_height.saturating_sub(1));
                        }
                    }
                }
            }
            LogsViewMode::EntryDetail { source, thread_id, entry_index, .. } => {
                let source = source.clone();
                let thread_id = *thread_id;
                let cur_entry = *entry_index;
                let max =
                    self.get_thread_group(&source, thread_id).map(|t| t.entries.len()).unwrap_or(0);
                if cur_entry < max.saturating_sub(1) {
                    if let LogsViewMode::EntryDetail { entry_index, scroll_offset, .. } =
                        &mut self.view_mode
                    {
                        *entry_index = cur_entry + 1;
                        *scroll_offset = 0;
                    }
                }
            }
        }
    }

    /// Jump backward by `count` rows/entries (works in all view modes)
    pub fn prev_detail_entry_jump(&mut self, count: usize) {
        match &self.view_mode {
            LogsViewMode::Sources => {
                self.selected_row = self.selected_row.saturating_sub(count);
                if self.selected_row < self.scroll_offset {
                    self.scroll_offset = self.selected_row;
                }
            }
            LogsViewMode::Threads { .. } => {
                self.secondary_selected = self.secondary_selected.saturating_sub(count);
                if self.secondary_selected < self.secondary_scroll {
                    self.secondary_scroll = self.secondary_selected;
                }
            }
            LogsViewMode::Entries { .. } => {
                self.tertiary_selected = self.tertiary_selected.saturating_sub(count);
                if self.tertiary_selected < self.tertiary_scroll {
                    self.tertiary_scroll = self.tertiary_selected;
                }
            }
            LogsViewMode::EntryDetail { source, thread_id, entry_index, .. } => {
                let source = source.clone();
                let thread_id = *thread_id;
                let cur_entry = *entry_index;
                let max = self
                    .get_thread_group(&source, thread_id)
                    .map(|t| t.entries.len().saturating_sub(1))
                    .unwrap_or(0);
                let new_entry = cur_entry.saturating_sub(count).min(max);
                if let LogsViewMode::EntryDetail { entry_index, scroll_offset, .. } =
                    &mut self.view_mode
                {
                    *entry_index = new_entry;
                    *scroll_offset = 0;
                }
            }
        }
    }

    /// Jump forward by `count` rows/entries (works in all view modes)
    pub fn next_detail_entry_jump(&mut self, count: usize) {
        match &self.view_mode {
            LogsViewMode::Sources => {
                let max_row = self.sorted_sources.len().saturating_sub(1);
                self.selected_row = (self.selected_row + count).min(max_row);
                let max_visible = self.scroll_offset + self.visible_height.saturating_sub(1);
                if self.selected_row > max_visible {
                    self.scroll_offset =
                        self.selected_row.saturating_sub(self.visible_height.saturating_sub(1));
                }
            }
            LogsViewMode::Threads { source } => {
                let source = source.clone();
                if let Some(src) = self.sources.get(&source) {
                    let max_row = src.sorted_threads.len().saturating_sub(1);
                    self.secondary_selected = (self.secondary_selected + count).min(max_row);
                    let max_visible = self.secondary_scroll + self.visible_height.saturating_sub(1);
                    if self.secondary_selected > max_visible {
                        self.secondary_scroll = self
                            .secondary_selected
                            .saturating_sub(self.visible_height.saturating_sub(1));
                    }
                }
            }
            LogsViewMode::Entries { source, thread_id } => {
                let source = source.clone();
                let thread_id = *thread_id;
                if let Some(thread) = self.get_thread_group(&source, thread_id) {
                    let max_row = thread.entries.len().saturating_sub(1);
                    self.tertiary_selected = (self.tertiary_selected + count).min(max_row);
                    let max_visible = self.tertiary_scroll + self.visible_height.saturating_sub(1);
                    if self.tertiary_selected > max_visible {
                        self.tertiary_scroll = self
                            .tertiary_selected
                            .saturating_sub(self.visible_height.saturating_sub(1));
                    }
                }
            }
            LogsViewMode::EntryDetail { source, thread_id, entry_index, .. } => {
                let source = source.clone();
                let thread_id = *thread_id;
                let cur_entry = *entry_index;
                let max = self
                    .get_thread_group(&source, thread_id)
                    .map(|t| t.entries.len().saturating_sub(1))
                    .unwrap_or(0);
                let new_entry = (cur_entry + count).min(max);
                if let LogsViewMode::EntryDetail { entry_index, scroll_offset, .. } =
                    &mut self.view_mode
                {
                    *entry_index = new_entry;
                    *scroll_offset = 0;
                }
            }
        }
    }
}

/// Status of a sub-query within a multi-query cell
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueryStatus {
    #[default]
    Pending,
    Running,
    Completed,
    Failed,
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

    // Max values during execution
    pub max_rows_read:  u64,
    pub max_bytes_read: u64,

    // Final values from ProfileInfo (set once at query completion)
    pub final_rows_read:  Option<u64>,
    pub final_bytes_read: Option<u64>,
    pub final_blocks:     Option<u64>,

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
            max_rows_read:      0,
            max_bytes_read:     0,
            final_rows_read:    None,
            final_bytes_read:   None,
            final_blocks:       None,
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

        // Track max values
        self.max_rows_read = self.max_rows_read.max(rows);
        self.max_bytes_read = self.max_bytes_read.max(bytes);

        if total_rows.is_some() {
            self.total_rows = total_rows;
        }
        self.rows_written = rows_written;
        self.bytes_written = bytes_written;
        self.elapsed_ns = elapsed_ns;
    }

    pub fn set_final_stats(&mut self, profile_info: serde_json::Value) {
        if let serde_json::Value::Object(ref map) = profile_info {
            self.final_rows_read = map.get("rows").and_then(|v| v.as_u64());
            self.final_bytes_read = map.get("bytes").and_then(|v| v.as_u64());
            self.final_blocks = map.get("blocks").and_then(|v| v.as_u64());
        }
    }

    pub fn add_profile_event(&mut self, event: serde_json::Value) {
        use chrono::DateTime;

        if let serde_json::Value::Object(ref map) = event
            && let (Some(name), Some(value), Some(time_str)) = (
                map.get("name").and_then(|n| n.as_str()),
                map.get("value").and_then(|v| v.as_i64()),
                map.get("current_time").and_then(|t| t.as_str()),
            )
        {
            // Parse timestamp for aggregation
            if let Ok(dt) = DateTime::parse_from_rfc3339(time_str) {
                let timestamp_us = dt.timestamp_micros();

                // Add to aggregated metrics
                if !self.metrics.contains_key(name) {
                    self.metric_names.push(name.to_string());
                    self.metric_names.sort();
                    self.metrics.insert(name.to_string(), AggregatedMetric::new());
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
            // Update Expanded mode index if active
            if let MetricsViewMode::Expanded { index } = &mut self.view_mode {
                *index = self.selected_row;
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
            // Update Expanded mode index if active
            if let MetricsViewMode::Expanded { index } = &mut self.view_mode {
                *index = self.selected_row;
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

    /// Move to previous row (works in both Table and Expanded modes)
    pub fn prev_detail_row(&mut self) {
        if self.selected_row > 0 {
            self.selected_row -= 1;
            // Adjust scroll if needed
            if self.selected_row < self.scroll_offset {
                self.scroll_offset = self.selected_row;
            }
            // Update Expanded mode index if active
            if let MetricsViewMode::Expanded { index } = &mut self.view_mode {
                *index = self.selected_row;
            }
        }
    }

    /// Move to next row (works in both Table and Expanded modes)
    pub fn next_detail_row(&mut self) {
        let max_row = self.metric_names.len().saturating_sub(1);
        if self.selected_row < max_row {
            self.selected_row += 1;
            // Adjust scroll if needed
            let max_visible = self.scroll_offset + self.visible_height.saturating_sub(1);
            if self.selected_row > max_visible {
                self.scroll_offset =
                    self.selected_row.saturating_sub(self.visible_height.saturating_sub(1));
            }
            // Update Expanded mode index if active
            if let MetricsViewMode::Expanded { index } = &mut self.view_mode {
                *index = self.selected_row;
            }
        }
    }

    /// Jump backward by `count` rows
    pub fn prev_detail_row_jump(&mut self, count: usize) {
        self.selected_row = self.selected_row.saturating_sub(count);
        if self.selected_row < self.scroll_offset {
            self.scroll_offset = self.selected_row;
        }
        if let MetricsViewMode::Expanded { index } = &mut self.view_mode {
            *index = self.selected_row;
        }
    }

    /// Jump forward by `count` rows
    pub fn next_detail_row_jump(&mut self, count: usize) {
        let max_row = self.metric_names.len().saturating_sub(1);
        self.selected_row = (self.selected_row + count).min(max_row);
        let max_visible = self.scroll_offset + self.visible_height.saturating_sub(1);
        if self.selected_row > max_visible {
            self.scroll_offset =
                self.selected_row.saturating_sub(self.visible_height.saturating_sub(1));
        }
        if let MetricsViewMode::Expanded { index } = &mut self.view_mode {
            *index = self.selected_row;
        }
    }
}

/// A single statement within a multi-query cell
#[derive(Debug)]
pub struct SubQueryBlock {
    pub sql:        String,
    pub sql_scroll: u16,
    pub results:    Option<SortableTable>,
    pub stats:      StatsData,
    pub error:      Option<String>,
    pub status:     QueryStatus,
}

impl SubQueryBlock {
    pub fn new(sql: String) -> Self {
        Self {
            sql,
            sql_scroll: 0,
            results: None,
            stats: StatsData::default(),
            error: None,
            status: QueryStatus::Pending,
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

    pub fn add_result_row(
        &mut self,
        row: serde_json::Value,
        pending_view_state: &mut Option<ViewState>,
    ) {
        if let serde_json::Value::Object(map) = row {
            let table_just_created = self.results.is_none();

            if table_just_created {
                let columns: Vec<String> = map.keys().cloned().collect();
                self.results = Some(SortableTable::new(columns));

                // Apply any pending view state now that table exists
                if let Some(state) = pending_view_state.take() {
                    if let Some(ref mut table) = self.results {
                        table.sort_column = state.results_sort_column;
                        table.sort_order = state.results_sort_order;
                        table.view_mode = state.results_view_mode;
                        table.header_focused = state.results_header_focused;
                        table.focused_col = state.results_focused_col;
                        table.col_offset = state.results_col_offset;
                    }
                }
            }

            if let Some(ref mut table) = self.results {
                let row_values: Vec<serde_json::Value> = table
                    .columns
                    .iter()
                    .map(|col| map.get(col).cloned().unwrap_or(serde_json::Value::Null))
                    .collect();
                table.add_row(row_values);
            }
        }
    }

    pub fn result_count(&self) -> usize { self.results.as_ref().map(|t| t.rows.len()).unwrap_or(0) }
}

/// A multi-query cell containing one or more statements
#[derive(Debug)]
pub struct QueryBlock {
    pub queries:            Vec<SubQueryBlock>,
    pub active_query:       usize,
    pub logs:               Vec<LogEntry>,
    pub logs_data:          LogsData,
    pub error_scroll:       u16,
    pub running:            bool,
    pub cancel_requested:   bool,
    pub cache_id:           Option<String>,
    pub pending_view_state: Option<ViewState>,
}

impl QueryBlock {
    /// Create a single-query block (backward compatible)
    pub fn new(sql: String) -> Self {
        Self {
            queries:            vec![SubQueryBlock::new(sql)],
            active_query:       0,
            logs:               Vec::new(),
            logs_data:          LogsData::default(),
            error_scroll:       0,
            running:            false,
            cancel_requested:   false,
            cache_id:           None,
            pending_view_state: None,
        }
    }

    /// Create a multi-query block from multiple SQL statements
    pub fn new_multi(sqls: Vec<String>) -> Self {
        let queries = sqls.into_iter().map(SubQueryBlock::new).collect();
        Self {
            queries,
            active_query: 0,
            logs: Vec::new(),
            logs_data: LogsData::default(),
            error_scroll: 0,
            running: false,
            cancel_requested: false,
            cache_id: None,
            pending_view_state: None,
        }
    }

    /// Get the active sub-query
    pub fn active(&self) -> &SubQueryBlock { &self.queries[self.active_query] }

    /// Get mutable reference to active sub-query
    pub fn active_mut(&mut self) -> &mut SubQueryBlock { &mut self.queries[self.active_query] }

    /// Check if this is a multi-query cell
    pub fn is_multi(&self) -> bool { self.queries.len() > 1 }

    /// Navigate to next query
    pub fn next_query(&mut self) -> bool {
        if self.active_query + 1 < self.queries.len() {
            self.active_query += 1;
            true
        } else {
            false
        }
    }

    /// Navigate to previous query
    pub fn prev_query(&mut self) -> bool {
        if self.active_query > 0 {
            self.active_query -= 1;
            true
        } else {
            false
        }
    }

    /// Select query by index (1-based for keybinding convenience)
    pub fn select_query(&mut self, n: usize) -> bool {
        let idx = n.saturating_sub(1);
        if idx < self.queries.len() {
            self.active_query = idx;
            true
        } else {
            false
        }
    }

    // Delegating methods for backward compatibility

    /// Get SQL of active query
    pub fn sql(&self) -> &str { &self.active().sql }

    /// Get results of active query
    pub fn results(&self) -> Option<&SortableTable> { self.active().results.as_ref() }

    /// Get mutable results of active query
    pub fn results_mut(&mut self) -> Option<&mut SortableTable> {
        self.active_mut().results.as_mut()
    }

    /// Get stats of active query
    pub fn stats(&self) -> &StatsData { &self.active().stats }

    /// Get mutable stats of active query
    pub fn stats_mut(&mut self) -> &mut StatsData { &mut self.active_mut().stats }

    /// Get error of active query
    pub fn error(&self) -> Option<&str> { self.active().error.as_deref() }

    /// Set error of a specific sub-query
    pub fn set_error(&mut self, sub_idx: usize, error: Option<String>) {
        if let Some(sq) = self.queries.get_mut(sub_idx) {
            sq.error = error;
            if sq.error.is_some() {
                sq.status = QueryStatus::Failed;
            }
        }
    }

    /// Get sql_scroll of active query
    pub fn sql_scroll(&self) -> u16 { self.active().sql_scroll }

    /// Set sql_scroll of active query
    pub fn set_sql_scroll(&mut self, scroll: u16) { self.active_mut().sql_scroll = scroll; }

    pub fn add_progress(&mut self, progress: serde_json::Value, sub_idx: usize) {
        if let Some(sq) = self.queries.get_mut(sub_idx) {
            sq.add_progress(progress);
        }
    }

    pub fn add_profile_event(&mut self, event: serde_json::Value, sub_idx: usize) {
        if let Some(sq) = self.queries.get_mut(sub_idx) {
            sq.add_profile_event(event);
        }
    }

    pub fn set_final_stats(&mut self, profile_info: serde_json::Value, sub_idx: usize) {
        if let Some(sq) = self.queries.get_mut(sub_idx) {
            sq.stats.set_final_stats(profile_info);
        }
    }

    pub fn add_result_row(&mut self, row: serde_json::Value, sub_idx: usize) {
        if let Some(sq) = self.queries.get_mut(sub_idx) {
            sq.add_result_row(row, &mut self.pending_view_state);
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

    /// Total result count across all queries
    pub fn result_count(&self) -> usize { self.queries.iter().map(|q| q.result_count()).sum() }

    /// Extract current UI state for caching
    pub fn extract_view_state(&self) -> ViewState {
        let sq = self.active();
        let (
            results_selected_row,
            results_scroll_offset,
            results_col_offset,
            results_view_mode,
            results_header_focused,
            results_focused_col,
            results_sort_column,
            results_sort_order,
        ) = if let Some(ref table) = sq.results {
            (
                table.selected_row,
                table.scroll_offset,
                table.col_offset,
                table.view_mode.clone(),
                table.header_focused,
                table.focused_col,
                table.sort_column,
                table.sort_order,
            )
        } else {
            Default::default()
        };

        ViewState {
            results_selected_row,
            results_scroll_offset,
            results_col_offset,
            results_view_mode,
            results_header_focused,
            results_focused_col,
            results_sort_column,
            results_sort_order,
            stats_selected_row: sq.stats.selected_row,
            stats_scroll_offset: sq.stats.scroll_offset,
            stats_view_mode: sq.stats.view_mode,
            logs_selected_row: self.logs_data.selected_row,
            logs_scroll_offset: self.logs_data.scroll_offset,
            logs_view_mode: self.logs_data.view_mode.clone(),
            sql_scroll: sq.sql_scroll,
            fullscreen: false,
        }
    }

    /// Apply cached UI state
    pub fn apply_view_state(&mut self, state: &ViewState) {
        // Apply to shared logs state
        self.logs_data.selected_row = state.logs_selected_row;
        self.logs_data.scroll_offset = state.logs_scroll_offset;
        self.logs_data.view_mode = state.logs_view_mode.clone();

        // Apply to active sub-query
        let sq = self.active_mut();
        sq.sql_scroll = state.sql_scroll;
        sq.stats.selected_row = state.stats_selected_row;
        sq.stats.scroll_offset = state.stats_scroll_offset;
        sq.stats.view_mode = state.stats_view_mode;

        if let Some(ref mut table) = sq.results {
            table.selected_row = state.results_selected_row.min(table.rows.len().saturating_sub(1));
            table.scroll_offset = state.results_scroll_offset;
            table.col_offset = state.results_col_offset;
            table.view_mode = state.results_view_mode.clone();
            table.header_focused = state.results_header_focused;
            table.focused_col = state.results_focused_col;
            table.sort_column = state.results_sort_column;
            table.sort_order = state.results_sort_order;
        } else {
            // Results table doesn't exist yet - store for later application
            self.pending_view_state = Some(state.clone());
        }
    }
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
    /// History view with query cards (query input is at bottom)
    HistoryView,
    /// Full-page query editor
    QueryEditor,
    /// Full results view for a selected query
    SubPane(SubPane),
}

/// Navigation vs Edit mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Navigation,
    Edit,
}

/// The entire session state
pub struct Session {
    // History of queries (shown as cards)
    pub history:                Vec<QueryStoreEntry>,
    pub selected_card:          Option<usize>, // None = empty history, Some(i) = card i selected
    pub history_scroll_offset:  usize,         // First visible card index in history view
    pub history_search_active:  bool,          // Search mode active in history view
    pub history_search_pattern: String,        // Current search filter pattern

    // Currently displayed query data (when viewing full results)
    pub current_block:    Option<QueryBlock>,
    pub loading_entry_id: Option<String>, // Entry ID being loaded async

    // In-progress queries (keyed by history index)
    pub running_queries: HashMap<usize, QueryBlock>,

    // UI state
    pub new_query:      TextArea<'static>,
    pub focus:          Focus,
    pub previous_focus: Option<Focus>,
    pub mode:           Mode,
    pub fullscreen:     bool, // True = current pane is fullscreened
    pub toast:          Option<(String, Instant)>,
    pub app_error:      Option<String>, // Non-query errors (archive, clipboard, etc.)
}

impl Session {
    pub fn new() -> Self {
        let mut new_query = TextArea::default();
        new_query.set_placeholder_text("Enter SQL query... (Cmd+Enter to execute)");

        Self {
            history: Vec::new(),
            selected_card: None,
            history_scroll_offset: 0,
            history_search_active: false,
            history_search_pattern: String::new(),
            current_block: None,
            loading_entry_id: None,
            running_queries: HashMap::new(),
            new_query,
            focus: Focus::HistoryView,
            previous_focus: None,
            mode: Mode::Navigation,
            fullscreen: false,
            toast: None,
            app_error: None,
        }
    }

    /// Load history from index at startup
    pub fn load_history(&mut self, entries: Vec<QueryStoreEntry>) {
        let mut entries = entries;
        entries.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        self.history = entries;
        // Select most recent entry (last one) if any exist
        if !self.history.is_empty() {
            self.selected_card = Some(self.history.len() - 1);
        }
    }

    /// Add a new entry to history (at the bottom, newest last)
    pub fn add_history_entry(&mut self, entry: QueryStoreEntry) -> usize {
        let idx = self.history.len();
        self.history.push(entry);
        idx // Return the index of the new entry
    }

    pub fn show_toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    pub fn clear_expired_toast(&mut self) {
        if let Some((_, created)) = &self.toast
            && created.elapsed() > std::time::Duration::from_secs(2)
        {
            self.toast = None;
        }
    }

    /// Get the currently displayed block (from running_queries or current_block)
    pub fn displayed_block(&self) -> Option<&QueryBlock> {
        if let Some(idx) = self.selected_card
            && let Some(block) = self.running_queries.get(&idx)
        {
            return Some(block);
        }
        self.current_block.as_ref()
    }

    /// Get mutable reference to displayed block
    pub fn displayed_block_mut(&mut self) -> Option<&mut QueryBlock> {
        if let Some(idx) = self.selected_card
            && self.running_queries.contains_key(&idx)
        {
            return self.running_queries.get_mut(&idx);
        }
        self.current_block.as_mut()
    }

    /// Get mutable running query by index
    pub fn get_running_mut(&mut self, idx: usize) -> Option<&mut QueryBlock> {
        self.running_queries.get_mut(&idx)
    }

    /// Get indices of history entries matching the current search pattern
    pub fn filtered_history_indices(&self) -> Vec<usize> {
        // Filter whenever pattern is non-empty, regardless of search_active state
        if self.history_search_pattern.is_empty() {
            return (0..self.history.len()).collect();
        }
        let pattern = self.history_search_pattern.to_lowercase();
        self.history
            .iter()
            .enumerate()
            .filter(|(_, e)| e.sql_preview.to_lowercase().contains(&pattern))
            .map(|(i, _)| i)
            .collect()
    }

    /// Move card selection up (previous card), returns true if selection changed
    /// Layout: [Card 0 (oldest)] [Card 1] ... [Card N (newest)]
    pub fn card_prev(&mut self) -> bool {
        let filtered = self.filtered_history_indices();
        if filtered.is_empty() {
            return false;
        }
        if let Some(idx) = self.selected_card {
            // Find current position in filtered list
            if let Some(pos) = filtered.iter().position(|&i| i == idx) {
                if pos > 0 {
                    self.selected_card = Some(filtered[pos - 1]);
                    return true;
                }
            } else if let Some(&last) = filtered.last() {
                // Current selection not in filtered list, jump to last
                self.selected_card = Some(last);
                return true;
            }
        } else if let Some(&last) = filtered.last() {
            // No selection, select last (newest)
            self.selected_card = Some(last);
            return true;
        }
        false
    }

    /// Move card selection down (next card), returns true if selection changed
    /// Layout: [Card 0 (oldest)] [Card 1] ... [Card N (newest)]
    pub fn card_next(&mut self) -> bool {
        let filtered = self.filtered_history_indices();
        if filtered.is_empty() {
            return false;
        }
        if let Some(idx) = self.selected_card {
            // Find current position in filtered list
            if let Some(pos) = filtered.iter().position(|&i| i == idx) {
                if pos + 1 < filtered.len() {
                    self.selected_card = Some(filtered[pos + 1]);
                    return true;
                }
            } else if let Some(&first) = filtered.first() {
                // Current selection not in filtered list, jump to first
                self.selected_card = Some(first);
                return true;
            }
        } else if let Some(&first) = filtered.first() {
            // No selection, select first (oldest)
            self.selected_card = Some(first);
            return true;
        }
        false
    }

    /// Select card by absolute index (0-based), returns true if selection changed
    pub fn select_card_by_index(&mut self, index: usize) -> bool {
        if index < self.history.len() {
            let changed = self.selected_card != Some(index);
            self.selected_card = Some(index);
            changed
        } else {
            false
        }
    }

    /// Check if selected entry is a running query
    pub fn selected_is_running(&self) -> bool {
        self.selected_card.map(|idx| self.running_queries.contains_key(&idx)).unwrap_or(false)
    }

    /// Navigate to next sub-pane
    pub fn subpane_next(&mut self) {
        if let Focus::SubPane(pane) = self.focus {
            self.focus = Focus::SubPane(match pane {
                SubPane::Sql => SubPane::Results,
                SubPane::Results => SubPane::Stats,
                SubPane::Stats => SubPane::Logs,
                SubPane::Logs => SubPane::Sql,
            });
        }
    }

    /// Navigate to previous sub-pane
    pub fn subpane_prev(&mut self) {
        if let Focus::SubPane(pane) = self.focus {
            self.focus = Focus::SubPane(match pane {
                SubPane::Sql => SubPane::Logs,
                SubPane::Results => SubPane::Sql,
                SubPane::Stats => SubPane::Results,
                SubPane::Logs => SubPane::Stats,
            });
        }
    }
}
