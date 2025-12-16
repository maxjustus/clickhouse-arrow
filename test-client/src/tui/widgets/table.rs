use std::cell::{Cell as StdCell, RefCell};
use std::cmp::Ordering;
use std::collections::HashMap;

use ratatui::layout::Constraint;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, List, ListItem, Paragraph, Row, Table, Widget, Wrap};
use serde_json::Value;
use tokio::sync::oneshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortOrder {
    #[default]
    Ascending,
    Descending,
}

/// A segment in a navigation path into nested JSON values
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PathSegment {
    Index(usize), // Array index: [0], [1], ...
    Key(String),  // Object key or map key
}

/// Full navigation path into a nested value
pub type ValuePath = Vec<PathSegment>;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ResultsViewMode {
    #[default]
    Table,
    FieldValue {
        row:            usize,
        field:          usize,
        path:           ValuePath,
        selected_index: usize,
        scroll_offset:  usize,
    },
}

// === Path Statistics ===

/// Type of values found at a path
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PathValueType {
    #[default]
    Unknown,
    Numeric,
    String,
    Array,
    Object,
    Boolean,
    Mixed,
    AllNull,
}

/// Numeric statistics for a path
#[derive(Debug, Clone, Default)]
pub struct NumericStats {
    pub min:    f64,
    pub max:    f64,
    pub sum:    f64,
    pub count:  usize,
    pub values: Vec<f64>, // For sparkline + percentiles (bounded)
}

impl NumericStats {
    const MAX_VALUES: usize = 200;

    pub fn add(&mut self, v: f64) {
        if self.count == 0 {
            self.min = v;
            self.max = v;
        } else {
            self.min = self.min.min(v);
            self.max = self.max.max(v);
        }
        self.sum += v;
        self.count += 1;
        if self.values.len() < Self::MAX_VALUES {
            self.values.push(v);
        }
    }

    pub fn avg(&self) -> f64 { if self.count == 0 { 0.0 } else { self.sum / self.count as f64 } }

    pub fn percentile(&self, p: f64) -> Option<f64> {
        if self.values.is_empty() {
            return None;
        }
        let mut sorted = self.values.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
        Some(sorted[idx.min(sorted.len() - 1)])
    }
}

/// Sample of unique values with counts
#[derive(Debug, Clone, Default)]
pub struct UniqueSample {
    pub values:       Vec<(String, usize)>, // (value, count) sorted by count desc
    pub total_unique: usize,
    pub truncated:    bool,
}

/// Computed statistics for a path
#[derive(Debug, Clone, Default)]
pub struct PathStats {
    pub total_rows:    usize,
    pub null_count:    usize,
    pub value_type:    PathValueType,
    pub numeric:       Option<NumericStats>,
    pub unique_sample: Option<UniqueSample>,
}

impl PathStats {
    const MAX_UNIQUE: usize = 1000;

    fn add_value(&mut self, value: &Value, unique_counts: &mut HashMap<String, usize>) {
        match value {
            Value::Null => self.null_count += 1,
            Value::Number(n) => {
                if let Some(f) = n.as_f64() {
                    self.update_type(PathValueType::Numeric);
                    self.numeric.get_or_insert_with(NumericStats::default).add(f);
                }
                self.track_unique(value, unique_counts);
            }
            Value::String(_) => {
                self.update_type(PathValueType::String);
                self.track_unique(value, unique_counts);
            }
            Value::Bool(_) => {
                self.update_type(PathValueType::Boolean);
                self.track_unique(value, unique_counts);
            }
            Value::Array(_) => {
                self.update_type(PathValueType::Array);
            }
            Value::Object(_) => {
                self.update_type(PathValueType::Object);
            }
        }
    }

    fn update_type(&mut self, new_type: PathValueType) {
        if self.value_type == PathValueType::Unknown {
            self.value_type = new_type;
        } else if self.value_type != new_type {
            self.value_type = PathValueType::Mixed;
        }
    }

    fn track_unique(&mut self, value: &Value, unique_counts: &mut HashMap<String, usize>) {
        if unique_counts.len() >= Self::MAX_UNIQUE {
            return; // Cap reached
        }
        let key = match value {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => return,
        };
        *unique_counts.entry(key).or_insert(0) += 1;
    }

    fn finalize(&mut self, unique_counts: HashMap<String, usize>) {
        if self.null_count == self.total_rows {
            self.value_type = PathValueType::AllNull;
        }

        if !unique_counts.is_empty() {
            let mut values: Vec<_> = unique_counts.into_iter().collect();
            values.sort_by(|a, b| b.1.cmp(&a.1)); // Sort by count desc
            let truncated = values.len() >= Self::MAX_UNIQUE;
            let total_unique = values.len();
            values.truncate(20); // Keep top 20 for display
            self.unique_sample = Some(UniqueSample { values, total_unique, truncated });
        }
    }
}

/// State of stats for a path
pub enum PathStatsState<'a> {
    Ready(&'a PathStats),
    Computing,
    NotStarted,
}

// === Helper functions for JSON value handling ===

/// Format a JSON value for display (truncated for table cells)
fn format_cell_value(value: &Value, max_len: usize) -> String {
    match value {
        Value::Null => "NULL".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => {
            if s.len() > max_len {
                // Use char_indices to find safe UTF-8 truncation point
                let truncate_at = max_len.saturating_sub(3);
                let end = s
                    .char_indices()
                    .take_while(|(i, _)| *i < truncate_at)
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0);
                format!("{}...", &s[..end])
            } else {
                s.clone()
            }
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                "[]".into()
            } else if is_map_like(arr) {
                format_map_preview(arr, max_len)
            } else {
                format_array_preview(arr, max_len)
            }
        }
        Value::Object(obj) => {
            if obj.is_empty() {
                "{}".into()
            } else {
                format_object_preview(obj, max_len)
            }
        }
    }
}

/// Format array preview: [1, 2, "foo"...]
fn format_array_preview(arr: &[Value], max_len: usize) -> String {
    let mut result = String::from("[");
    let mut first = true;
    let item_max = (max_len / 4).max(10);

    for item in arr {
        let item_str = format_cell_value(item, item_max);
        let separator = if first { "" } else { ", " };

        // Check if adding this item would exceed limit (+4 for "...]")
        if result.len() + separator.len() + item_str.len() + 4 > max_len {
            result.push_str("...");
            break;
        }

        result.push_str(separator);
        result.push_str(&item_str);
        first = false;
    }

    result.push(']');
    result
}

/// Format map-like array preview: {k: v, k2: v2...}
fn format_map_preview(arr: &[Value], max_len: usize) -> String {
    let mut result = String::from("{");
    let mut first = true;
    let key_max = (max_len / 6).max(8);
    let val_max = (max_len / 4).max(10);

    for item in arr {
        if let Value::Array(pair) = item
            && pair.len() == 2
        {
            let key_str = format_cell_value(&pair[0], key_max);
            let val_str = format_cell_value(&pair[1], val_max);
            let entry = format!("{}: {}", key_str, val_str);
            let separator = if first { "" } else { ", " };

            if result.len() + separator.len() + entry.len() + 4 > max_len {
                result.push_str("...");
                break;
            }

            result.push_str(separator);
            result.push_str(&entry);
            first = false;
        }
    }

    result.push('}');
    result
}

/// Format object preview: {a: 1, b: 2...}
fn format_object_preview(obj: &serde_json::Map<String, Value>, max_len: usize) -> String {
    let mut result = String::from("{");
    let mut first = true;
    let val_max = (max_len / 4).max(10);

    for (key, val) in obj {
        let val_str = format_cell_value(val, val_max);
        let entry = format!("{}: {}", key, val_str);
        let separator = if first { "" } else { ", " };

        if result.len() + separator.len() + entry.len() + 4 > max_len {
            result.push_str("...");
            break;
        }

        result.push_str(separator);
        result.push_str(&entry);
        first = false;
    }

    result.push('}');
    result
}

/// Check if value is expandable (array or object)
fn is_expandable(value: &Value) -> bool {
    matches!(value, Value::Array(a) if !a.is_empty())
        || matches!(value, Value::Object(o) if !o.is_empty())
}

/// Check if an array looks like a map (array of 2-element arrays)
fn is_map_like(arr: &[Value]) -> bool {
    !arr.is_empty() && arr.iter().all(|v| matches!(v, Value::Array(pair) if pair.len() == 2))
}

/// Resolve a path into a nested JSON value
fn resolve_path<'a>(value: &'a Value, path: &ValuePath) -> Option<&'a Value> {
    let mut current = value;
    for segment in path {
        current = match segment {
            PathSegment::Index(i) => current.as_array()?.get(*i)?,
            PathSegment::Key(k) => current.as_object()?.get(k)?,
        };
    }
    Some(current)
}

/// Format a breadcrumb string from field name and path
fn format_breadcrumb(field_name: &str, path: &ValuePath) -> String {
    if path.is_empty() {
        return field_name.to_string();
    }
    let mut parts = vec![field_name.to_string()];
    for seg in path {
        match seg {
            PathSegment::Index(i) => parts.push(format!("[{}]", i)),
            PathSegment::Key(k) => parts.push(k.clone()),
        }
    }
    parts.join(" > ")
}

/// Count items in a collection value
fn collection_len(value: &Value) -> usize {
    match value {
        Value::Array(arr) => arr.len(),
        Value::Object(obj) => obj.len(),
        _ => 0,
    }
}

#[derive(Debug)]
pub struct SortableTable {
    pub columns:        Vec<String>,
    pub rows:           Vec<Vec<Value>>,
    pub sort_column:    Option<usize>,
    pub sort_order:     SortOrder,
    pub selected_row:   usize,
    pub scroll_offset:  usize,
    col_widths:         Vec<usize>,
    pub col_offset:     usize,
    pub view_mode:      ResultsViewMode,
    pub selected_field: usize,
    pub value_scroll:   usize,
    pub visible_height: usize,

    // Detail panel visible height (set during render, used for scroll calculations)
    detail_visible_height: StdCell<usize>,

    // Detail panel field positions (start line of each field, set during render)
    detail_field_positions: RefCell<Vec<usize>>,

    // Detail panel total content height (for scroll clamping)
    detail_total_height: StdCell<usize>,

    // Detail panel focus (for single-row view in Table mode)
    pub detail_focused: bool,

    // Index into visible headers vector (for detail panel navigation)
    selected_visible_index: usize,

    // Header focus for column sorting
    pub header_focused: bool,
    pub focused_col:    usize,
    visible_cols:       usize, // Updated during render

    // Path stats computation
    stats_cache:           HashMap<(usize, ValuePath), PathStats>,
    stats_pending:         Option<(usize, ValuePath, oneshot::Receiver<PathStats>)>,
    stats_cache_row_count: usize,

    // Saved scroll state for restoring after FieldValue zoom
    saved_value_scroll:           Option<usize>,
    saved_selected_field:         Option<usize>,
    saved_selected_visible_index: Option<usize>,
}

impl SortableTable {
    pub fn new(columns: Vec<String>) -> Self {
        // Initialize widths from header lengths (+2 for sort arrow)
        let col_widths: Vec<usize> = columns.iter().map(|h| h.len() + 2).collect();
        Self {
            columns,
            rows: Vec::new(),
            sort_column: None,
            sort_order: SortOrder::Ascending,
            selected_row: 0,
            scroll_offset: 0,
            col_widths,
            col_offset: 0,
            view_mode: ResultsViewMode::Table,
            selected_field: 0,
            value_scroll: 0,
            visible_height: 20,
            detail_visible_height: StdCell::new(20),
            detail_field_positions: RefCell::new(Vec::new()),
            detail_total_height: StdCell::new(0),
            detail_focused: false,
            selected_visible_index: 0,
            header_focused: false,
            focused_col: 0,
            visible_cols: 10,
            stats_cache: HashMap::new(),
            stats_pending: None,
            stats_cache_row_count: 0,
            saved_value_scroll: None,
            saved_selected_field: None,
            saved_selected_visible_index: None,
        }
    }

    /// Update visible dimensions based on render area. Call before navigation operations.
    pub fn set_visible_height(&mut self, height: u16) {
        // Account for borders (2) and header row (1)
        self.visible_height = height.saturating_sub(3) as usize;
    }

    /// Update detail panel visible height. Uses Cell for interior mutability during render.
    pub fn set_detail_visible_height(&self, height: u16) {
        // Account for borders (2 lines)
        self.detail_visible_height.set(height.saturating_sub(2) as usize);
    }

    /// Update visible columns based on render width
    pub fn set_visible_width(&mut self, width: u16) {
        let (visible_cols, _) = self.columns_for_width(width);
        self.visible_cols = visible_cols;
    }

    pub fn add_row(&mut self, row: Vec<Value>) {
        // Update cached column widths incrementally based on display length
        for (i, cell) in row.iter().enumerate() {
            if i < self.col_widths.len() {
                let display_len = format_cell_value(cell, 40).len();
                self.col_widths[i] = self.col_widths[i].max(display_len);
            }
        }
        self.rows.push(row);
    }

    pub fn scroll_cols_right(&mut self) {
        // Allow scrolling as long as there are more columns
        if self.col_offset + 1 < self.columns.len() {
            self.col_offset += 1;
        }
    }

    /// Returns true if scrolled, false if already at leftmost
    pub fn scroll_cols_left(&mut self) -> bool {
        if self.col_offset > 0 {
            self.col_offset -= 1;
            true
        } else {
            false
        }
    }

    pub fn sort_by_column(&mut self, col: usize) {
        if Some(col) == self.sort_column {
            self.sort_order = match self.sort_order {
                SortOrder::Ascending => SortOrder::Descending,
                SortOrder::Descending => SortOrder::Ascending,
            };
        } else {
            self.sort_column = Some(col);
            self.sort_order = SortOrder::Ascending;
        }

        self.apply_sort();
    }

    /// Cycle sort on a column: None → Asc → Desc → None
    pub fn cycle_sort(&mut self, col: usize) {
        if Some(col) == self.sort_column {
            match self.sort_order {
                SortOrder::Ascending => {
                    self.sort_order = SortOrder::Descending;
                    self.apply_sort();
                }
                SortOrder::Descending => {
                    // Clear sort
                    self.sort_column = None;
                }
            }
        } else {
            self.sort_column = Some(col);
            self.sort_order = SortOrder::Ascending;
            self.apply_sort();
        }
    }

    fn apply_sort(&mut self) {
        if let Some(col) = self.sort_column {
            let order = self.sort_order;
            self.rows.sort_by(|a, b| {
                let a_val = a.get(col);
                let b_val = b.get(col);

                let ord = match (a_val, b_val) {
                    // Both are numbers - compare numerically
                    (Some(Value::Number(a_num)), Some(Value::Number(b_num))) => {
                        match (a_num.as_f64(), b_num.as_f64()) {
                            (Some(a_f), Some(b_f)) => {
                                a_f.partial_cmp(&b_f).unwrap_or(Ordering::Equal)
                            }
                            _ => Ordering::Equal,
                        }
                    }
                    // Fall back to string comparison for other types
                    _ => {
                        let a_str = a_val.map(|v| format_cell_value(v, 100));
                        let b_str = b_val.map(|v| format_cell_value(v, 100));
                        a_str.cmp(&b_str)
                    }
                };

                match order {
                    SortOrder::Ascending => ord,
                    SortOrder::Descending => ord.reverse(),
                }
            });
        }
    }

    pub fn next_row(&mut self) {
        if !self.rows.is_empty() && self.selected_row < self.rows.len() - 1 {
            self.selected_row += 1;
            // Scroll when selected row goes past visible area
            if self.selected_row >= self.scroll_offset + self.visible_height {
                self.scroll_offset += 1;
            }
        }
    }

    pub fn prev_row(&mut self) {
        if self.selected_row > 0 {
            self.selected_row -= 1;
            if self.selected_row < self.scroll_offset {
                self.scroll_offset = self.selected_row;
            }
        }
    }

    pub fn page_down(&mut self) {
        let jump = self.visible_height.max(1);
        let new_row = (self.selected_row + jump).min(self.rows.len().saturating_sub(1));
        self.selected_row = new_row;
        self.scroll_offset = self.selected_row.saturating_sub(jump / 2);
    }

    pub fn page_up(&mut self) {
        let jump = self.visible_height.max(1);
        let new_row = self.selected_row.saturating_sub(jump);
        self.selected_row = new_row;
        self.scroll_offset = self.selected_row.saturating_sub(jump / 2);
    }

    // === Header navigation methods ===

    /// Focus the header row, positioning on the current visible column
    pub fn focus_header(&mut self) {
        self.header_focused = true;
        self.focused_col = self.col_offset;
        self.maybe_start_stats_computation(self.focused_col, vec![]);
    }

    /// Exit header focus and return to data rows
    pub fn unfocus_header(&mut self) { self.header_focused = false; }

    /// Move focus left in header
    pub fn header_left(&mut self) {
        if self.focused_col > 0 {
            self.focused_col -= 1;
            self.selected_field = self.focused_col; // Sync with detail pane
            // Scroll columns if needed
            if self.focused_col < self.col_offset {
                self.col_offset = self.focused_col;
            }
            self.ensure_selected_field_visible(); // Keep detail pane scroll synced
            self.maybe_start_stats_computation(self.focused_col, vec![]);
        }
    }

    /// Move focus right in header
    pub fn header_right(&mut self) {
        if self.focused_col < self.columns.len().saturating_sub(1) {
            self.focused_col += 1;
            self.selected_field = self.focused_col; // Sync with detail pane
            // Scroll right if focused column would be off-screen
            // Use visible_cols as estimate (updated elsewhere or use conservative default)
            let visible = self.visible_cols.max(1);
            if self.focused_col >= self.col_offset + visible {
                self.col_offset = self.focused_col.saturating_sub(visible - 1);
            }
            self.ensure_selected_field_visible(); // Keep detail pane scroll synced
            self.maybe_start_stats_computation(self.focused_col, vec![]);
        }
    }

    // === Tree navigation methods ===

    /// Get the cell value at given row/field
    fn get_cell_value(&self, row: usize, field: usize) -> Option<&Value> {
        self.rows.get(row)?.get(field)
    }

    /// Compute which field headers are currently visible in the viewport.
    /// Returns indices of fields whose headers are within [scroll_top, scroll_bottom).
    /// Fallback: if no headers visible (deep in long field), returns the owning field.
    fn compute_visible_headers(&self) -> Vec<usize> {
        let positions = self.detail_field_positions.borrow();
        if positions.is_empty() {
            return vec![];
        }

        let scroll_top = self.value_scroll;
        let scroll_bottom = scroll_top + self.detail_visible_height.get();

        let mut visible = Vec::new();

        // Find headers within viewport
        for (field_idx, &line_pos) in positions.iter().enumerate() {
            if line_pos >= scroll_top && line_pos < scroll_bottom {
                visible.push(field_idx);
            }
        }

        // Fallback: if no headers visible, find owning field
        if visible.is_empty() {
            for (field_idx, &line_pos) in positions.iter().enumerate().rev() {
                if line_pos <= scroll_top {
                    visible.push(field_idx);
                    break;
                }
            }
            // Edge case: scrolled above first field
            if visible.is_empty() {
                visible.push(0);
            }
        }

        visible
    }

    /// Update selected_field based on visible headers and selected_visible_index.
    /// Also sync focused_col to keep both views consistent.
    fn sync_selected_field(&mut self) {
        let visible = self.compute_visible_headers();
        self.selected_field = visible.get(self.selected_visible_index).copied().unwrap_or(0);
        self.focused_col = self.selected_field; // Sync with table header
    }

    /// Ensure selected field is visible in detail pane by adjusting value_scroll
    fn ensure_selected_field_visible(&mut self) {
        let field_start_line =
            self.detail_field_positions.borrow().get(self.selected_field).copied();
        if let Some(field_start_line) = field_start_line {
            // Compute the expected end line of this field (approximately)
            let field_end_line = self
                .detail_field_positions
                .borrow()
                .get(self.selected_field + 1)
                .copied()
                .unwrap_or_else(|| self.detail_total_height.get());

            let visible_height = self.detail_visible_height.get() as usize;

            // Adjust scroll to ensure field is visible
            if field_start_line < self.value_scroll {
                // Field is above visible area - scroll up
                self.value_scroll = field_start_line;
            } else if field_end_line > self.value_scroll + visible_height {
                // Field is below visible area - scroll down
                self.value_scroll = field_end_line.saturating_sub(visible_height);
            }

            self.clamp_value_scroll();
        }
    }

    /// Clamp value_scroll to valid range (prevent scrolling past content)
    fn clamp_value_scroll(&mut self) {
        let max_scroll =
            self.detail_total_height.get().saturating_sub(self.detail_visible_height.get());
        self.value_scroll = self.value_scroll.min(max_scroll);
    }

    /// Page down in detail view
    pub fn page_down_detail(&mut self) {
        let jump = self.detail_visible_height.get().max(1);
        self.value_scroll += jump;
        self.clamp_value_scroll();
        // Clamp selected_visible_index to new visible headers
        let visible = self.compute_visible_headers();
        self.selected_visible_index =
            self.selected_visible_index.min(visible.len().saturating_sub(1));
        self.sync_selected_field();
    }

    /// Page up in detail view
    pub fn page_up_detail(&mut self) {
        let jump = self.detail_visible_height.get().max(1);
        self.value_scroll = self.value_scroll.saturating_sub(jump);
        // Clamp selected_visible_index to new visible headers
        let visible = self.compute_visible_headers();
        self.selected_visible_index =
            self.selected_visible_index.min(visible.len().saturating_sub(1));
        self.sync_selected_field();
    }

    /// Navigate down in current view mode
    pub fn nav_down(&mut self) {
        match &mut self.view_mode {
            ResultsViewMode::Table => {
                if self.detail_focused {
                    // Navigate between visible headers, scroll at edges
                    let visible = self.compute_visible_headers();
                    if visible.is_empty() {
                        return;
                    }
                    if self.selected_visible_index < visible.len().saturating_sub(1) {
                        // Move within visible headers (no scroll)
                        self.selected_visible_index += 1;
                    } else {
                        // At edge: scroll down 1 line, ride the edge
                        self.value_scroll += 1;
                        self.clamp_value_scroll();
                        let new_visible = self.compute_visible_headers();
                        self.selected_visible_index = new_visible.len().saturating_sub(1);
                    }
                    self.sync_selected_field();
                } else {
                    self.next_row();
                }
            }
            ResultsViewMode::FieldValue { row, field, path, selected_index, scroll_offset } => {
                // Navigate within collection, or scroll scalar value
                if let Some(cell) = self.rows.get(*row).and_then(|r| r.get(*field))
                    && let Some(current) = resolve_path(cell, path)
                {
                    let len = collection_len(current);
                    if len > 0 {
                        // Navigating within a collection
                        if *selected_index + 1 < len {
                            *selected_index += 1;
                            // Update scroll if needed
                            if *selected_index >= *scroll_offset + self.visible_height {
                                *scroll_offset += 1;
                            }
                        }
                    } else {
                        // Scalar value - scroll text
                        self.value_scroll += 1;
                        self.clamp_value_scroll();
                    }
                }
            }
        }
    }

    /// Navigate up in current view mode
    pub fn nav_up(&mut self) {
        match &mut self.view_mode {
            ResultsViewMode::Table => {
                if self.detail_focused {
                    // Navigate between visible headers, scroll at edges
                    let visible = self.compute_visible_headers();
                    if visible.is_empty() {
                        return;
                    }
                    if self.selected_visible_index > 0 {
                        // Move within visible headers (no scroll)
                        self.selected_visible_index -= 1;
                    } else {
                        // At edge: scroll up 1 line, stay at index 0
                        self.value_scroll = self.value_scroll.saturating_sub(1);
                    }
                    self.sync_selected_field();
                } else {
                    self.prev_row();
                }
            }
            ResultsViewMode::FieldValue { row, field, path, selected_index, scroll_offset } => {
                if let Some(cell) = self.rows.get(*row).and_then(|r| r.get(*field))
                    && let Some(current) = resolve_path(cell, path)
                {
                    let len = collection_len(current);
                    if len > 0 {
                        // Navigating within a collection
                        if *selected_index > 0 {
                            *selected_index -= 1;
                            if *selected_index < *scroll_offset {
                                *scroll_offset = *selected_index;
                            }
                        }
                    } else {
                        // Scalar value - scroll text
                        self.value_scroll = self.value_scroll.saturating_sub(1);
                    }
                }
            }
        }
    }

    /// Move to previous row (works in all view modes)
    pub fn prev_detail_row(&mut self) {
        match &mut self.view_mode {
            ResultsViewMode::Table => {
                if self.selected_row > 0 {
                    self.selected_row -= 1;
                    if self.selected_row < self.scroll_offset {
                        self.scroll_offset = self.selected_row;
                    }
                }
            }
            ResultsViewMode::FieldValue { row, scroll_offset, .. } => {
                if *row > 0 {
                    *row -= 1;
                    *scroll_offset = 0;
                    self.value_scroll = 0;
                    self.selected_row = *row;
                }
            }
        }
    }

    /// Move to next row (works in all view modes)
    pub fn next_detail_row(&mut self) {
        let max_row = self.rows.len().saturating_sub(1);
        match &mut self.view_mode {
            ResultsViewMode::Table => {
                if self.selected_row < max_row {
                    self.selected_row += 1;
                    let max_visible = self.scroll_offset + self.visible_height.saturating_sub(1);
                    if self.selected_row > max_visible {
                        self.scroll_offset =
                            self.selected_row.saturating_sub(self.visible_height.saturating_sub(1));
                    }
                }
            }
            ResultsViewMode::FieldValue { row, scroll_offset, .. } => {
                if *row < max_row {
                    *row += 1;
                    *scroll_offset = 0;
                    self.value_scroll = 0;
                    self.selected_row = *row;
                }
            }
        }
    }

    /// Jump backward by `count` rows (works in all view modes)
    pub fn prev_detail_row_jump(&mut self, count: usize) {
        match &mut self.view_mode {
            ResultsViewMode::Table => {
                self.selected_row = self.selected_row.saturating_sub(count);
                if self.selected_row < self.scroll_offset {
                    self.scroll_offset = self.selected_row;
                }
            }
            ResultsViewMode::FieldValue { row, scroll_offset, .. } => {
                let max_row = self.rows.len().saturating_sub(1);
                *row = row.saturating_sub(count).min(max_row);
                *scroll_offset = 0;
                self.value_scroll = 0;
                self.selected_row = *row;
            }
        }
    }

    /// Jump forward by `count` rows (works in all view modes)
    pub fn next_detail_row_jump(&mut self, count: usize) {
        let max_row = self.rows.len().saturating_sub(1);
        match &mut self.view_mode {
            ResultsViewMode::Table => {
                self.selected_row = (self.selected_row + count).min(max_row);
                let max_visible = self.scroll_offset + self.visible_height.saturating_sub(1);
                if self.selected_row > max_visible {
                    self.scroll_offset =
                        self.selected_row.saturating_sub(self.visible_height.saturating_sub(1));
                }
            }
            ResultsViewMode::FieldValue { row, scroll_offset, .. } => {
                *row = (*row + count).min(max_row);
                *scroll_offset = 0;
                self.value_scroll = 0;
                self.selected_row = *row;
            }
        }
    }

    /// Expand/drill into current selection. Returns false if can't expand further.
    pub fn expand(&mut self) -> bool {
        match &self.view_mode {
            ResultsViewMode::Table => {
                if self.rows.is_empty() {
                    return false;
                }
                if !self.detail_focused {
                    // Focus the detail panel
                    self.detail_focused = true;
                    self.selected_field = 0;
                    self.selected_visible_index = 0;
                    self.value_scroll = 0;
                    true
                } else {
                    // Already focused on detail, drill into selected field
                    let row = self.selected_row;
                    // Don't expand empty collections
                    if let Some(cell) = self.get_cell_value(row, self.selected_field) {
                        match cell {
                            Value::Array(arr) if arr.is_empty() => return false,
                            Value::Object(obj) if obj.is_empty() => return false,
                            _ => {} // Allow: scalars, non-empty collections
                        }
                    }
                    let field = self.selected_field;
                    // Save scroll state for restoring when exiting FieldValue
                    self.saved_value_scroll = Some(self.value_scroll);
                    self.saved_selected_field = Some(self.selected_field);
                    self.saved_selected_visible_index = Some(self.selected_visible_index);
                    self.view_mode = ResultsViewMode::FieldValue {
                        row,
                        field,
                        path: Vec::new(),
                        selected_index: 0,
                        scroll_offset: 0,
                    };
                    self.value_scroll = 0;
                    self.maybe_start_stats_computation(field, Vec::new());
                    true
                }
            }
            ResultsViewMode::FieldValue { row, field, path, selected_index, .. } => {
                // Try to drill into the selected item within a collection
                let row = *row;
                let field = *field;
                let mut new_path = path.clone();
                let selected = *selected_index;

                if let Some(cell) = self.get_cell_value(row, field)
                    && let Some(current) = resolve_path(cell, &new_path)
                {
                    match current {
                        Value::Array(arr) if is_map_like(arr) => {
                            // Map-like: drill into value (index 1 of the pair)
                            if let Some(pair) = arr.get(selected)
                                && let Value::Array(kv) = pair
                                && kv.len() == 2
                            {
                                new_path.push(PathSegment::Index(selected));
                                new_path.push(PathSegment::Index(1));
                                let stats_path = new_path.clone();
                                self.view_mode = ResultsViewMode::FieldValue {
                                    row,
                                    field,
                                    path: new_path,
                                    selected_index: 0,
                                    scroll_offset: 0,
                                };
                                self.value_scroll = 0;
                                self.maybe_start_stats_computation(field, stats_path);
                                return true;
                            }
                        }
                        Value::Array(arr) => {
                            // Regular array: drill into selected element (even scalars)
                            if selected < arr.len() {
                                new_path.push(PathSegment::Index(selected));
                                let stats_path = new_path.clone();
                                self.view_mode = ResultsViewMode::FieldValue {
                                    row,
                                    field,
                                    path: new_path,
                                    selected_index: 0,
                                    scroll_offset: 0,
                                };
                                self.value_scroll = 0;
                                self.maybe_start_stats_computation(field, stats_path);
                                return true;
                            }
                        }
                        Value::Object(obj) => {
                            // Object: drill into selected key's value (even scalars)
                            if let Some((key, _)) = obj.iter().nth(selected) {
                                new_path.push(PathSegment::Key(key.clone()));
                                let stats_path = new_path.clone();
                                self.view_mode = ResultsViewMode::FieldValue {
                                    row,
                                    field,
                                    path: new_path,
                                    selected_index: 0,
                                    scroll_offset: 0,
                                };
                                self.value_scroll = 0;
                                self.maybe_start_stats_computation(field, stats_path);
                                return true;
                            }
                        }
                        _ => {} // Already at scalar, can't expand further
                    }
                }
                false
            }
        }
    }

    /// Collapse/go back. Returns false if at top level (should exit edit mode).
    pub fn collapse(&mut self) -> bool {
        match &self.view_mode {
            ResultsViewMode::Table => {
                if self.detail_focused {
                    // Unfocus detail panel
                    self.detail_focused = false;
                    true
                } else {
                    // At top level, signal to exit edit mode
                    false
                }
            }
            ResultsViewMode::FieldValue { row, field, path, .. } => {
                let row = *row;
                let field = *field;
                if path.is_empty() {
                    // At field level, go back to Table with detail focused
                    self.view_mode = ResultsViewMode::Table;
                    self.selected_row = row;
                    self.detail_focused = true;
                    // Restore saved scroll state
                    if let Some(scroll) = self.saved_value_scroll.take() {
                        self.value_scroll = scroll;
                    }
                    if let Some(field) = self.saved_selected_field.take() {
                        self.selected_field = field;
                    }
                    if let Some(idx) = self.saved_selected_visible_index.take() {
                        self.selected_visible_index = idx;
                    }
                } else {
                    // Pop path segment to go up one level
                    let mut new_path = path.clone();
                    new_path.pop();
                    self.view_mode = ResultsViewMode::FieldValue {
                        row,
                        field,
                        path: new_path,
                        selected_index: 0,
                        scroll_offset: 0,
                    };
                }
                true
            }
        }
    }

    /// Get content for clipboard based on current view mode
    pub fn get_clipboard_content(&self) -> String {
        match &self.view_mode {
            ResultsViewMode::Table => {
                if self.detail_focused {
                    // Single row as JSON object
                    if let Some(row_data) = self.rows.get(self.selected_row) {
                        let obj: serde_json::Map<String, Value> = self
                            .columns
                            .iter()
                            .zip(row_data.iter())
                            .map(|(col, val)| (col.clone(), val.clone()))
                            .collect();
                        serde_json::to_string_pretty(&Value::Object(obj)).unwrap_or_default()
                    } else {
                        String::new()
                    }
                } else {
                    // All rows as JSON array of objects
                    let objects: Vec<Value> = self
                        .rows
                        .iter()
                        .map(|row| {
                            let obj: serde_json::Map<String, Value> = self
                                .columns
                                .iter()
                                .zip(row.iter())
                                .map(|(col, val)| (col.clone(), val.clone()))
                                .collect();
                            Value::Object(obj)
                        })
                        .collect();
                    serde_json::to_string_pretty(&objects).unwrap_or_default()
                }
            }
            ResultsViewMode::FieldValue { row, field, path, .. } => {
                // Nested value - raw string for strings, pretty JSON otherwise
                if let Some(row_data) = self.rows.get(*row)
                    && let Some(field_val) = row_data.get(*field)
                    && let Some(resolved) = resolve_path(field_val, path)
                {
                    return match resolved {
                        Value::String(s) => s.clone(),
                        other => serde_json::to_string_pretty(other).unwrap_or_default(),
                    };
                }
                String::new()
            }
        }
    }

    // === Path Stats Methods ===

    /// Start stats computation for a field/path if not cached
    pub fn maybe_start_stats_computation(&mut self, field: usize, path: ValuePath) {
        // Invalidate cache if rows changed
        if self.rows.len() != self.stats_cache_row_count {
            self.stats_cache.clear();
            self.stats_cache_row_count = self.rows.len();
        }

        // Check cache
        let key = (field, path.clone());
        if self.stats_cache.contains_key(&key) {
            return;
        }

        // Check if already computing this path
        if let Some((f, ref p, _)) = self.stats_pending {
            if f == field && p == &path {
                return;
            }
        }

        // Extract values at path (clone just what we need)
        let values: Vec<Option<Value>> = self
            .rows
            .iter()
            .map(|row| row.get(field).and_then(|v| resolve_path(v, &path).cloned()))
            .collect();

        // Spawn background task
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let stats = compute_path_stats(values);
            let _ = tx.send(stats);
        });

        self.stats_pending = Some((field, path, rx));
    }

    /// Poll for completed stats computation
    pub fn poll_stats_completion(&mut self) {
        let Some((field, ref path, ref mut rx)) = self.stats_pending else {
            return;
        };

        match rx.try_recv() {
            Ok(stats) => {
                let key = (field, path.clone());
                self.stats_cache.insert(key, stats);
                self.stats_pending = None;
            }
            Err(oneshot::error::TryRecvError::Empty) => {
                // Still computing
            }
            Err(oneshot::error::TryRecvError::Closed) => {
                // Task died, clear pending
                self.stats_pending = None;
            }
        }
    }

    /// Get stats state for a field/path
    pub fn get_path_stats(&self, field: usize, path: &ValuePath) -> PathStatsState<'_> {
        let key = (field, path.clone());

        if let Some(stats) = self.stats_cache.get(&key) {
            return PathStatsState::Ready(stats);
        }

        if let Some((f, p, _)) = &self.stats_pending {
            if *f == field && p == path {
                return PathStatsState::Computing;
            }
        }

        PathStatsState::NotStarted
    }

    /// Calculate how many columns fit and their widths, starting from col_offset.
    /// Returns (num_visible_cols, Vec<allocated_widths>)
    fn columns_for_width(&self, available_width: u16) -> (usize, Vec<u16>) {
        let usable = available_width.saturating_sub(2) as usize; // 2 for borders (left + right)
        let mut base_widths = Vec::new();
        let mut total_base = 0usize;

        // First pass: calculate base widths (content-aware, capped at 50)
        for i in self.col_offset..self.columns.len() {
            let header_width = self.columns[i].len();
            let content_width = self.col_widths.get(i).copied().unwrap_or(header_width);
            let col_width = content_width.max(header_width).min(50);

            // column_spacing(1) adds spacing BETWEEN columns, not after each
            let spacing = if base_widths.is_empty() { 0 } else { 1 };
            let needed = col_width + spacing;
            if total_base + needed > usable && !base_widths.is_empty() {
                break;
            }

            base_widths.push(col_width);
            total_base += needed;
        }

        // Second pass: distribute remaining space proportionally
        let remaining = usable.saturating_sub(total_base);
        if remaining > 0 && !base_widths.is_empty() {
            let extra_per_col = remaining / base_widths.len();
            for w in &mut base_widths {
                *w += extra_per_col;
            }
        }

        let widths: Vec<u16> = base_widths.into_iter().map(|w| w as u16).collect();
        (widths.len(), widths)
    }

    pub fn render_widget<'a>(
        &self,
        title: &'a str,
        available_width: u16,
        border_style: Style,
    ) -> Table<'a> {
        // Content-aware column fitting
        let (visible_cols, col_widths_allocated) = self.columns_for_width(available_width);
        let col_end = self.col_offset + visible_cols;

        let header_cells: Vec<Cell> = self
            .columns
            .iter()
            .skip(self.col_offset)
            .take(visible_cols)
            .enumerate()
            .map(|(i, h)| {
                let actual_idx = self.col_offset + i;
                let text = if Some(actual_idx) == self.sort_column {
                    let arrow = match self.sort_order {
                        SortOrder::Ascending => " ↑",
                        SortOrder::Descending => " ↓",
                    };
                    format!("{}{}", h, arrow)
                } else {
                    h.clone()
                };

                let style = if self.header_focused && actual_idx == self.focused_col {
                    // Highlighted: black text on yellow background
                    Style::default().fg(Color::Black).bg(Color::Yellow)
                } else {
                    Style::default().fg(Color::Yellow)
                };

                Cell::from(text).style(style)
            })
            .collect();

        let header = Row::new(header_cells).height(1);

        let col_offset = self.col_offset;
        let col_widths_for_rows = col_widths_allocated.clone();
        let visible_rows =
            self.rows.iter().skip(self.scroll_offset).take(self.visible_height).enumerate().map(
                move |(i, row)| {
                    let style = if self.scroll_offset + i == self.selected_row {
                        Style::default().bg(Color::DarkGray).fg(Color::White)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    // Format cells with allocated widths
                    let cells: Vec<String> = row
                        .iter()
                        .skip(col_offset)
                        .zip(&col_widths_for_rows)
                        .map(|(cell, &width)| format_cell_value(cell, width as usize))
                        .collect();
                    Row::new(cells).style(style).height(1)
                },
            );

        // Use allocated column widths
        let widths: Vec<Constraint> =
            col_widths_allocated.iter().map(|&w| Constraint::Length(w)).collect();

        let col_info = if visible_cols < self.columns.len() {
            format!("cols {}-{}/{}", self.col_offset + 1, col_end, self.columns.len())
        } else {
            format!("{} cols", self.columns.len())
        };

        Table::new(visible_rows, widths)
            .header(header)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("{} ({} rows, {})", title, self.rows.len(), col_info))
                    .border_style(border_style),
            )
            .column_spacing(1)
    }

    /// Render a nested value view (collection or scalar)
    fn render_nested_value(
        &self,
        title: &str,
        row: usize,
        field: usize,
        path: &ValuePath,
        selected_index: usize,
        scroll_offset: usize,
        border_style: Style,
        available_width: u16,
    ) -> ResultsWidget<'_> {
        let field_name = self.columns.get(field).map(|s| s.as_str()).unwrap_or("?");
        let breadcrumb = format_breadcrumb(field_name, path);
        let full_title = format!("{} - Row {}, {}", title, row + 1, breadcrumb);

        let cell = self.get_cell_value(row, field);
        let current = cell.and_then(|c| resolve_path(c, path));

        // Calculate dynamic max lengths based on available width
        // Reserve: borders(4) + indicator(2) + some padding
        let content_width = available_width.saturating_sub(10) as usize;
        // For key: value format, split ~40% key, ~60% value
        let key_max_len = (content_width * 2 / 5).max(15);
        let val_max_len = (content_width * 3 / 5).max(20);
        // For single value display (array items, object values)
        let item_max_len = content_width.max(30);

        match current {
            Some(Value::Array(arr)) if is_map_like(arr) => {
                // Render as map: "key: value" entries
                let items: Vec<ListItem> = arr
                    .iter()
                    .enumerate()
                    .skip(scroll_offset)
                    .take(self.visible_height)
                    .map(|(i, pair)| {
                        let (key_str, val_str, val_expandable) = if let Value::Array(kv) = pair {
                            let k = kv
                                .first()
                                .map(|v| format_cell_value(v, key_max_len))
                                .unwrap_or_default();
                            let v = kv
                                .get(1)
                                .map(|v| format_cell_value(v, val_max_len))
                                .unwrap_or_default();
                            let expandable = kv.get(1).is_some_and(is_expandable);
                            (k, v, expandable)
                        } else {
                            (String::new(), format_cell_value(pair, item_max_len), false)
                        };

                        let style = if i == selected_index {
                            Style::default().bg(Color::DarkGray).fg(Color::White)
                        } else {
                            Style::default().fg(Color::White)
                        };

                        let indicator = if val_expandable { "> " } else { "  " };
                        ListItem::new(Line::from(vec![
                            Span::styled(indicator, Style::default().fg(Color::DarkGray)),
                            Span::styled(
                                format!("{}: ", key_str),
                                Style::default().fg(Color::Yellow),
                            ),
                            Span::styled(val_str, style),
                        ]))
                    })
                    .collect();

                let count_info = format!(" ({} entries)", arr.len());
                ResultsWidget::List(
                    List::new(items).block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(format!("{}{}", full_title, count_info))
                            .border_style(border_style),
                    ),
                )
            }
            Some(Value::Array(arr)) => {
                // Render as indexed list: "[i]: value"
                let items: Vec<ListItem> = arr
                    .iter()
                    .enumerate()
                    .skip(scroll_offset)
                    .take(self.visible_height)
                    .map(|(i, v)| {
                        let display = format_cell_value(v, item_max_len);
                        let expandable = is_expandable(v);

                        let style = if i == selected_index {
                            Style::default().bg(Color::DarkGray).fg(Color::White)
                        } else {
                            Style::default().fg(Color::White)
                        };

                        let indicator = if expandable { "> " } else { "  " };
                        ListItem::new(Line::from(vec![
                            Span::styled(indicator, Style::default().fg(Color::DarkGray)),
                            Span::styled(format!("[{}]: ", i), Style::default().fg(Color::Yellow)),
                            Span::styled(display, style),
                        ]))
                    })
                    .collect();

                let count_info = format!(" ({} items)", arr.len());
                ResultsWidget::List(
                    List::new(items).block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(format!("{}{}", full_title, count_info))
                            .border_style(border_style),
                    ),
                )
            }
            Some(Value::Object(obj)) => {
                // Render as object: "key: value"
                let items: Vec<ListItem> = obj
                    .iter()
                    .enumerate()
                    .skip(scroll_offset)
                    .take(self.visible_height)
                    .map(|(i, (k, v))| {
                        let display = format_cell_value(v, item_max_len);
                        let expandable = is_expandable(v);

                        let style = if i == selected_index {
                            Style::default().bg(Color::DarkGray).fg(Color::White)
                        } else {
                            Style::default().fg(Color::White)
                        };

                        let indicator = if expandable { "> " } else { "  " };
                        ListItem::new(Line::from(vec![
                            Span::styled(indicator, Style::default().fg(Color::DarkGray)),
                            Span::styled(format!("{}: ", k), Style::default().fg(Color::Yellow)),
                            Span::styled(display, style),
                        ]))
                    })
                    .collect();

                let count_info = format!(" ({} keys)", obj.len());
                ResultsWidget::List(
                    List::new(items).block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(format!("{}{}", full_title, count_info))
                            .border_style(border_style),
                    ),
                )
            }
            Some(scalar) => {
                // Scalar value - render full content
                let content = match scalar {
                    Value::String(s) => {
                        if let Some(relative) = format_relative_time(s) {
                            format!("{s}\n\n({relative})")
                        } else {
                            s.clone()
                        }
                    }
                    Value::Null => "NULL".to_string(),
                    other => {
                        serde_json::to_string_pretty(other).unwrap_or_else(|_| format!("{other:?}"))
                    }
                };

                // Handle scrolling for long text
                let lines: Vec<&str> = content.lines().collect();
                let display_lines: String = lines
                    .iter()
                    .skip(self.value_scroll)
                    .take(50)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");

                let scroll_info = if lines.len() > 50 {
                    format!(
                        " (lines {}-{}/{})",
                        self.value_scroll + 1,
                        (self.value_scroll + 50).min(lines.len()),
                        lines.len()
                    )
                } else {
                    String::new()
                };

                ResultsWidget::Value(
                    Paragraph::new(display_lines)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .title(format!("{}{}", full_title, scroll_info))
                                .border_style(border_style),
                        )
                        .wrap(Wrap { trim: false }),
                )
            }
            None => {
                // Value not found
                ResultsWidget::Value(
                    Paragraph::new("(value not found)").block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(full_title)
                            .border_style(border_style),
                    ),
                )
            }
        }
    }

    /// Render based on current view mode. Returns a widget that can be rendered.
    pub fn render<'a>(
        &'a self,
        title: &'a str,
        available_width: u16,
        border_style: Style,
    ) -> ResultsWidget<'a> {
        match &self.view_mode {
            ResultsViewMode::Table => {
                ResultsWidget::Table(self.render_widget(title, available_width, border_style))
            }
            ResultsViewMode::FieldValue { row, field, path, selected_index, scroll_offset } => self
                .render_nested_value(
                    title,
                    *row,
                    *field,
                    path,
                    *selected_index,
                    *scroll_offset,
                    border_style,
                    available_width,
                ),
        }
    }

    /// Render the detail view for split view (Table mode detail panel or FieldValue).
    pub fn render_detail<'a>(
        &'a self,
        title: &'a str,
        available_width: u16,
        border_style: Style,
    ) -> ResultsWidget<'a> {
        match &self.view_mode {
            ResultsViewMode::FieldValue { row, field, path, selected_index, scroll_offset } => self
                .render_nested_value(
                    title,
                    *row,
                    *field,
                    path,
                    *selected_index,
                    *scroll_offset,
                    border_style,
                    available_width,
                ),
            ResultsViewMode::Table => {
                // Show selected row detail
                ResultsWidget::Value(self.render_selected_row_detail(
                    title,
                    available_width,
                    border_style,
                ))
            }
        }
    }

    /// Render full detail view for the currently selected row (used in Table mode split view)
    /// Shows every column's complete value with recursive rendering for nested data.
    pub fn render_selected_row_detail<'a>(
        &'a self,
        title: &'a str,
        available_width: u16,
        border_style: Style,
    ) -> Paragraph<'a> {
        let row_data = self.rows.get(self.selected_row);

        let mut lines: Vec<Line> = Vec::new();
        let mut field_positions: Vec<usize> = Vec::new();
        // Account for borders (2 chars) in content width for wrapping
        let content_width = available_width.saturating_sub(2) as usize;

        for (i, col_name) in self.columns.iter().enumerate() {
            // Record position - since we pre-wrap, lines.len() is accurate
            field_positions.push(lines.len());

            let value = row_data.and_then(|r| r.get(i));

            // Highlight selected field name when detail or header is focused and column matches
            let name_style =
                if (self.detail_focused || self.header_focused) && i == self.selected_field {
                    Style::default().fg(Color::Yellow).bg(Color::DarkGray)
                } else {
                    Style::default().fg(Color::Yellow)
                };

            // Field name line
            lines.push(Line::from(Span::styled(format!("{}:", col_name), name_style)));

            // Render value using recursive helper with type coloring and mini-tables
            if let Some(v) = value {
                render_value_exploded(&mut lines, "", v, 1, content_width);
            }

            // Empty line between fields
            lines.push(Line::from(""));
        }

        // Store field positions and total height for scroll-spy
        *self.detail_field_positions.borrow_mut() = field_positions;
        self.detail_total_height.set(lines.len());

        // Show current field name in title
        let field_name = self.columns.get(self.selected_field).map(|s| s.as_str()).unwrap_or("");
        let title_text = format!("{} - Row {} ({})", title, self.selected_row + 1, field_name);

        // Build paragraph - no .wrap() needed since content is pre-wrapped
        Paragraph::new(lines)
            .block(
                Block::default().borders(Borders::ALL).title(title_text).border_style(border_style),
            )
            .scroll((self.value_scroll as u16, 0))
    }
}

/// Wrapper enum for different result view widgets
pub enum ResultsWidget<'a> {
    Table(Table<'a>),
    List(List<'a>),
    Value(Paragraph<'a>),
}

impl Widget for ResultsWidget<'_> {
    fn render(self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        match self {
            ResultsWidget::Table(w) => w.render(area, buf),
            ResultsWidget::List(w) => w.render(area, buf),
            ResultsWidget::Value(w) => w.render(area, buf),
        }
    }
}

/// Compute stats for values at a path (runs in background task)
fn compute_path_stats(values: Vec<Option<Value>>) -> PathStats {
    let mut stats = PathStats::default();
    let mut unique_counts = HashMap::new();
    stats.total_rows = values.len();

    for value in values {
        match value {
            None => stats.null_count += 1,
            Some(v) => stats.add_value(&v, &mut unique_counts),
        }
    }

    stats.finalize(unique_counts);
    stats
}

/// Try to parse a string as a timestamp and return relative time (e.g., "3.3 hours ago")
fn format_relative_time(s: &str) -> Option<String> {
    use chrono::{NaiveDateTime, Utc};

    // Parse "2025-10-27 11:59:20 UTC" format
    let dt = NaiveDateTime::parse_from_str(s.trim_end_matches(" UTC"), "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|ndt| ndt.and_utc())?;
    let now = Utc::now();
    let duration = now.signed_duration_since(dt);
    let secs = duration.num_seconds().abs();
    let suffix = if duration.num_seconds() >= 0 { "ago" } else { "from now" };

    let relative = if secs < 60 {
        format!("{secs} seconds {suffix}")
    } else if secs < 3600 {
        format!("{:.1} minutes {suffix}", secs as f64 / 60.0)
    } else if secs < 86400 {
        format!("{:.1} hours {suffix}", secs as f64 / 3600.0)
    } else {
        format!("{:.1} days {suffix}", secs as f64 / 86400.0)
    };

    Some(relative)
}

// === Detail Pane Rendering Helpers ===

/// Wrap text to fit within width, breaking at word boundaries
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 || text.is_empty() {
        return vec![text.to_string()];
    }

    let mut result = Vec::new();
    let mut current_line = String::new();

    for word in text.split_whitespace() {
        if current_line.is_empty() {
            if word.chars().count() > width {
                // Word too long, break it
                let chars: Vec<char> = word.chars().collect();
                for chunk in chars.chunks(width) {
                    result.push(chunk.iter().collect());
                }
            } else {
                current_line = word.to_string();
            }
        } else if current_line.chars().count() + 1 + word.chars().count() <= width {
            current_line.push(' ');
            current_line.push_str(word);
        } else {
            result.push(current_line);
            if word.chars().count() > width {
                let chars: Vec<char> = word.chars().collect();
                for chunk in chars.chunks(width) {
                    result.push(chunk.iter().collect());
                }
                current_line = String::new();
            } else {
                current_line = word.to_string();
            }
        }
    }
    if !current_line.is_empty() {
        result.push(current_line);
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

/// Recursively render a value for exploded view with proper indentation
fn render_value_exploded<'a>(
    lines: &mut Vec<Line<'a>>,
    label: &str,
    value: &Value,
    indent: usize,
    available_width: usize,
) {
    let prefix = "  ".repeat(indent);
    let label_style = Style::default().fg(Color::Yellow);
    let value_style = Style::default().fg(Color::White);
    let dim_style = Style::default().fg(Color::DarkGray);

    match value {
        Value::Array(arr) if arr.is_empty() => {
            lines.push(Line::from(vec![
                Span::raw(prefix),
                Span::styled(format!("{}: ", label), label_style),
                Span::styled("[]", dim_style),
            ]));
        }
        Value::Array(arr) => {
            // Check if homogeneous object array (render as mini-table)
            if let Some(keys) = is_homogeneous_object_array(arr) {
                lines.push(Line::from(vec![
                    Span::raw(prefix.clone()),
                    Span::styled(format!("{}: ", label), label_style),
                    Span::styled(format!("[{} items]", arr.len()), dim_style),
                ]));
                render_object_array_table(lines, arr, &keys, indent + 1, available_width);
            } else {
                // Regular array - render items individually
                lines.push(Line::from(vec![
                    Span::raw(prefix.clone()),
                    Span::styled(format!("{}: ", label), label_style),
                    Span::styled(format!("[{} items]", arr.len()), dim_style),
                ]));
                for (i, item) in arr.iter().enumerate() {
                    render_value_exploded(
                        lines,
                        &format!("[{}]", i),
                        item,
                        indent + 1,
                        available_width,
                    );
                }
            }
        }
        Value::Object(obj) if obj.is_empty() => {
            lines.push(Line::from(vec![
                Span::raw(prefix),
                Span::styled(format!("{}: ", label), label_style),
                Span::styled("{}", dim_style),
            ]));
        }
        Value::Object(obj) => {
            lines.push(Line::from(vec![
                Span::raw(prefix.clone()),
                Span::styled(format!("{}:", label), label_style),
            ]));
            for (key, val) in obj {
                render_value_exploded(lines, key, val, indent + 1, available_width);
            }
        }
        Value::Null => {
            lines.push(Line::from(vec![
                Span::raw(prefix),
                Span::styled(format!("{}: ", label), label_style),
                Span::styled("null", dim_style),
            ]));
        }
        Value::String(s) => {
            // Wrap long strings to fit within available width
            let label_prefix = format!("{}: \"", label);
            let prefix_width = prefix.chars().count() + label_prefix.chars().count();
            let content_width = available_width.saturating_sub(prefix_width);

            if content_width > 10 && s.chars().count() > content_width {
                // String needs wrapping
                let wrapped = wrap_text(s, content_width);
                for (i, line_text) in wrapped.iter().enumerate() {
                    if i == 0 {
                        // First line: prefix + label + opening quote + content
                        lines.push(Line::from(vec![
                            Span::raw(prefix.clone()),
                            Span::styled(label_prefix.clone(), label_style),
                            Span::styled(line_text.clone(), value_style),
                        ]));
                    } else if i == wrapped.len() - 1 {
                        // Last line: continuation indent + content + closing quote
                        let cont_prefix = " ".repeat(prefix_width);
                        lines.push(Line::from(vec![
                            Span::raw(cont_prefix),
                            Span::styled(format!("{}\"", line_text), value_style),
                        ]));
                    } else {
                        // Middle lines: continuation indent + content
                        let cont_prefix = " ".repeat(prefix_width);
                        lines.push(Line::from(vec![
                            Span::raw(cont_prefix),
                            Span::styled(line_text.clone(), value_style),
                        ]));
                    }
                }
                // Handle single wrapped line (needs closing quote)
                if wrapped.len() == 1 {
                    if let Some(last_line) = lines.last_mut() {
                        if let Some(last_span) = last_line.spans.last_mut() {
                            last_span.content = format!("{}\"", last_span.content).into();
                        }
                    }
                }
            } else {
                // Short string, no wrapping needed
                lines.push(Line::from(vec![
                    Span::raw(prefix),
                    Span::styled(format!("{}: ", label), label_style),
                    Span::styled(format!("\"{}\"", s), value_style),
                ]));
            }
        }
        Value::Number(n) => {
            lines.push(Line::from(vec![
                Span::raw(prefix),
                Span::styled(format!("{}: ", label), label_style),
                Span::styled(n.to_string(), Style::default().fg(Color::Cyan)),
            ]));
        }
        Value::Bool(b) => {
            lines.push(Line::from(vec![
                Span::raw(prefix),
                Span::styled(format!("{}: ", label), label_style),
                Span::styled(b.to_string(), Style::default().fg(Color::Magenta)),
            ]));
        }
    }
}

/// Check if array is homogeneous objects (all items are objects with same keys)
fn is_homogeneous_object_array(arr: &[Value]) -> Option<Vec<String>> {
    if arr.len() < 2 {
        return None; // Not worth tabulating single item
    }

    let first = arr.first()?.as_object()?;
    let keys: Vec<String> = first.keys().cloned().collect();

    if keys.is_empty() {
        return None;
    }

    // Check all items have same keys
    for item in arr.iter().skip(1) {
        let obj = item.as_object()?;
        if obj.len() != keys.len() {
            return None;
        }
        for key in &keys {
            if !obj.contains_key(key) {
                return None;
            }
        }
    }

    Some(keys)
}

/// Render homogeneous object array as aligned table
fn render_object_array_table<'a>(
    lines: &mut Vec<Line<'a>>,
    arr: &[Value],
    keys: &[String],
    indent: usize,
    available_width: usize,
) {
    let prefix = "  ".repeat(indent);
    let header_style = Style::default().fg(Color::Yellow);
    let value_style = Style::default().fg(Color::White);
    let dim_style = Style::default().fg(Color::DarkGray);

    // Calculate column widths
    let mut col_widths: Vec<usize> = keys.iter().map(|k| k.len()).collect();
    for item in arr {
        if let Some(obj) = item.as_object() {
            for (i, key) in keys.iter().enumerate() {
                if let Some(val) = obj.get(key) {
                    let val_str = format_value_compact(val);
                    col_widths[i] = col_widths[i].max(val_str.len());
                }
            }
        }
    }

    // Cap column widths to fit available space
    let total_width: usize = col_widths.iter().sum::<usize>() + (keys.len() * 3); // " | " separators
    let content_width = available_width.saturating_sub(prefix.len() + 4);
    if total_width > content_width && content_width > 0 {
        let scale = content_width as f64 / total_width as f64;
        for w in &mut col_widths {
            *w = ((*w as f64 * scale) as usize).max(3);
        }
    }

    // Header row
    let header_parts: Vec<String> =
        keys.iter().zip(&col_widths).map(|(k, &w)| format!("{:width$}", k, width = w)).collect();
    lines.push(Line::from(vec![
        Span::raw(prefix.clone()),
        Span::styled(header_parts.join(" │ "), header_style),
    ]));

    // Separator
    let sep_parts: Vec<String> = col_widths.iter().map(|&w| "─".repeat(w)).collect();
    lines.push(Line::from(vec![
        Span::raw(prefix.clone()),
        Span::styled(sep_parts.join("─┼─"), dim_style),
    ]));

    // Data rows
    for item in arr {
        if let Some(obj) = item.as_object() {
            let row_parts: Vec<String> = keys
                .iter()
                .zip(&col_widths)
                .map(|(k, &w)| {
                    let val = obj.get(k).map(format_value_compact).unwrap_or_default();
                    if val.len() > w {
                        format!("{}…", &val[..w.saturating_sub(1)])
                    } else {
                        format!("{:width$}", val, width = w)
                    }
                })
                .collect();
            lines.push(Line::from(vec![
                Span::raw(prefix.clone()),
                Span::styled(row_parts.join(" │ "), value_style),
            ]));
        }
    }
}

/// Format a value compactly for table cells
fn format_value_compact(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(arr) => format!("[{}]", arr.len()),
        Value::Object(obj) => format!("{{{}}}", obj.len()),
    }
}
