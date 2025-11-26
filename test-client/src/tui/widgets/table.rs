use ratatui::layout::Constraint;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Row, Table, Widget, Wrap};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    Ascending,
    Descending,
}

/// A segment in a navigation path into nested JSON values
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSegment {
    Index(usize), // Array index: [0], [1], ...
    Key(String),  // Object key or map key
}

/// Full navigation path into a nested value
pub type ValuePath = Vec<PathSegment>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultsViewMode {
    Table,
    FieldList {
        row:           usize,
        scroll_offset: usize,
    },
    FieldValue {
        row:            usize,
        field:          usize,
        path:           ValuePath,
        selected_index: usize,
        scroll_offset:  usize,
    },
}

impl Default for ResultsViewMode {
    fn default() -> Self { Self::Table }
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

    for item in arr {
        let item_str = format_cell_value(item, 15);
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

    for item in arr {
        if let Value::Array(pair) = item {
            if pair.len() == 2 {
                let key_str = format_cell_value(&pair[0], 10);
                let val_str = format_cell_value(&pair[1], 10);
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
    }

    result.push('}');
    result
}

/// Format object preview: {a: 1, b: 2...}
fn format_object_preview(obj: &serde_json::Map<String, Value>, max_len: usize) -> String {
    let mut result = String::from("{");
    let mut first = true;

    for (key, val) in obj {
        let val_str = format_cell_value(val, 10);
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
    pub page_size:      usize,
    col_widths:         Vec<usize>,
    pub col_offset:     usize,
    pub view_mode:      ResultsViewMode,
    pub selected_field: usize,
    pub value_scroll:   usize,
    pub visible_height: usize, // Updated during render
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
            page_size: 100,
            col_widths,
            col_offset: 0,
            view_mode: ResultsViewMode::Table,
            selected_field: 0,
            value_scroll: 0,
            visible_height: 20, // Default, updated during render
        }
    }

    /// Update visible height based on render area. Call before navigation operations.
    pub fn set_visible_height(&mut self, height: u16) {
        // Account for borders (2) and header row (1)
        self.visible_height = height.saturating_sub(3) as usize;
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

    pub fn clear(&mut self) {
        self.rows.clear();
        self.selected_row = 0;
        self.scroll_offset = 0;
        self.col_offset = 0;
        self.view_mode = ResultsViewMode::Table;
        self.selected_field = 0;
        self.value_scroll = 0;
        // Reset widths to header lengths
        self.col_widths = self.columns.iter().map(|h| h.len() + 2).collect();
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

        // Sort using display string representation for consistency
        self.rows.sort_by(|a, b| {
            let a_str = a.get(col).map(|v| format_cell_value(v, 100));
            let b_str = b.get(col).map(|v| format_cell_value(v, 100));
            let ord = a_str.cmp(&b_str);
            match self.sort_order {
                SortOrder::Ascending => ord,
                SortOrder::Descending => ord.reverse(),
            }
        });
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

    // === Tree navigation methods ===

    /// Get the cell value at given row/field
    fn get_cell_value(&self, row: usize, field: usize) -> Option<&Value> {
        self.rows.get(row)?.get(field)
    }

    /// Navigate down in current view mode
    pub fn nav_down(&mut self) {
        match &mut self.view_mode {
            ResultsViewMode::Table => self.next_row(),
            ResultsViewMode::FieldList { scroll_offset, .. } => {
                if self.selected_field < self.columns.len().saturating_sub(1) {
                    self.selected_field += 1;
                    // Scroll if selection goes past visible area
                    if self.selected_field >= *scroll_offset + self.visible_height {
                        *scroll_offset += 1;
                    }
                }
            }
            ResultsViewMode::FieldValue { row, field, path, selected_index, scroll_offset } => {
                // Navigate within collection, or scroll scalar value
                if let Some(cell) = self.rows.get(*row).and_then(|r| r.get(*field)) {
                    if let Some(current) = resolve_path(cell, path) {
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
                        }
                    }
                }
            }
        }
    }

    /// Navigate up in current view mode
    pub fn nav_up(&mut self) {
        match &mut self.view_mode {
            ResultsViewMode::Table => self.prev_row(),
            ResultsViewMode::FieldList { scroll_offset, .. } => {
                if self.selected_field > 0 {
                    self.selected_field -= 1;
                    if self.selected_field < *scroll_offset {
                        *scroll_offset = self.selected_field;
                    }
                }
            }
            ResultsViewMode::FieldValue { row, field, path, selected_index, scroll_offset } => {
                if let Some(cell) = self.rows.get(*row).and_then(|r| r.get(*field)) {
                    if let Some(current) = resolve_path(cell, path) {
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
    }

    /// Expand/drill into current selection. Returns false if can't expand further.
    pub fn expand(&mut self) -> bool {
        match &self.view_mode {
            ResultsViewMode::Table => {
                if !self.rows.is_empty() {
                    self.view_mode = ResultsViewMode::FieldList {
                        row:           self.selected_row,
                        scroll_offset: 0,
                    };
                    self.selected_field = 0;
                    true
                } else {
                    false
                }
            }
            ResultsViewMode::FieldList { row, .. } => {
                let row = *row;
                self.view_mode = ResultsViewMode::FieldValue {
                    row,
                    field: self.selected_field,
                    path: Vec::new(),
                    selected_index: 0,
                    scroll_offset: 0,
                };
                self.value_scroll = 0;
                true
            }
            ResultsViewMode::FieldValue { row, field, path, selected_index, .. } => {
                // Try to drill into the selected item within a collection
                let row = *row;
                let field = *field;
                let mut new_path = path.clone();
                let selected = *selected_index;

                if let Some(cell) = self.get_cell_value(row, field) {
                    if let Some(current) = resolve_path(cell, &new_path) {
                        match current {
                            Value::Array(arr) if is_map_like(arr) => {
                                // Map-like: drill into value (index 1 of the pair)
                                if let Some(pair) = arr.get(selected) {
                                    if let Value::Array(kv) = pair {
                                        if kv.len() == 2 {
                                            new_path.push(PathSegment::Index(selected));
                                            new_path.push(PathSegment::Index(1));
                                            self.view_mode = ResultsViewMode::FieldValue {
                                                row,
                                                field,
                                                path: new_path,
                                                selected_index: 0,
                                                scroll_offset: 0,
                                            };
                                            self.value_scroll = 0;
                                            return true;
                                        }
                                    }
                                }
                            }
                            Value::Array(arr) => {
                                // Regular array: drill into selected element (even scalars)
                                if selected < arr.len() {
                                    new_path.push(PathSegment::Index(selected));
                                    self.view_mode = ResultsViewMode::FieldValue {
                                        row,
                                        field,
                                        path: new_path,
                                        selected_index: 0,
                                        scroll_offset: 0,
                                    };
                                    self.value_scroll = 0;
                                    return true;
                                }
                            }
                            Value::Object(obj) => {
                                // Object: drill into selected key's value (even scalars)
                                if let Some((key, _)) = obj.iter().nth(selected) {
                                    new_path.push(PathSegment::Key(key.clone()));
                                    self.view_mode = ResultsViewMode::FieldValue {
                                        row,
                                        field,
                                        path: new_path,
                                        selected_index: 0,
                                        scroll_offset: 0,
                                    };
                                    self.value_scroll = 0;
                                    return true;
                                }
                            }
                            _ => {} // Already at scalar, can't expand further
                        }
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
                // At top level, signal to exit edit mode
                false
            }
            ResultsViewMode::FieldList { row, .. } => {
                let row = *row;
                self.view_mode = ResultsViewMode::Table;
                self.selected_row = row;
                true
            }
            ResultsViewMode::FieldValue { row, field, path, .. } => {
                let row = *row;
                let field = *field;
                if path.is_empty() {
                    // At field level, go back to field list
                    self.view_mode = ResultsViewMode::FieldList { row, scroll_offset: 0 };
                    self.selected_field = field;
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

    /// Calculate max columns that fit in given width
    pub fn max_cols_for_width(&self, width: u16) -> usize {
        // Each column: min 8 chars + 1 spacing, borders take ~2
        let usable = width.saturating_sub(4) as usize;
        (usable / 9).max(1).min(30) // At least 1, at most 30
    }

    pub fn render_widget<'a>(
        &self,
        title: &'a str,
        available_width: u16,
        border_style: Style,
    ) -> Table<'a> {
        // Dynamic column count based on width
        let max_cols = self.max_cols_for_width(available_width);
        let visible_cols = (self.columns.len() - self.col_offset).min(max_cols);
        let col_end = self.col_offset + visible_cols;

        let header_cells =
            self.columns.iter().skip(self.col_offset).take(visible_cols).enumerate().map(
                |(i, h)| {
                    let actual_idx = self.col_offset + i;
                    if Some(actual_idx) == self.sort_column {
                        let arrow = match self.sort_order {
                            SortOrder::Ascending => " ↑",
                            SortOrder::Descending => " ↓",
                        };
                        return format!("{}{}", h, arrow);
                    }
                    h.clone()
                },
            );

        let header = Row::new(header_cells).style(Style::default().fg(Color::Yellow)).height(1);

        let col_offset = self.col_offset;
        let visible_rows =
            self.rows.iter().skip(self.scroll_offset).take(self.visible_height).enumerate().map(
                move |(i, row)| {
                    let style = if self.scroll_offset + i == self.selected_row {
                        Style::default().bg(Color::DarkGray).fg(Color::White)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    // Format cells and limit columns
                    let cells: Vec<String> = row
                        .iter()
                        .skip(col_offset)
                        .take(visible_cols)
                        .map(|cell| format_cell_value(cell, 40))
                        .collect();
                    Row::new(cells).style(style).height(1)
                },
            );

        // Use cached column widths (min = header length, max = 40)
        let widths: Vec<Constraint> = self
            .col_widths
            .iter()
            .zip(self.columns.iter())
            .skip(self.col_offset)
            .take(visible_cols)
            .map(|(&w, col)| {
                let min_width = col.len().max(4); // At least 4 chars
                Constraint::Min(w.clamp(min_width, 40) as u16)
            })
            .collect();

        let col_info = if self.columns.len() > max_cols {
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

    fn render_field_list(
        &self,
        title: &str,
        row: usize,
        scroll_offset: usize,
        border_style: Style,
    ) -> List<'_> {
        let row_data = self.rows.get(row);

        let items: Vec<ListItem> = self
            .columns
            .iter()
            .enumerate()
            .skip(scroll_offset)
            .take(self.visible_height)
            .map(|(i, col_name)| {
                let value = row_data.and_then(|r| r.get(i));
                let display = value.map(|v| format_cell_value(v, 60)).unwrap_or_default();

                let style = if i == self.selected_field {
                    Style::default().bg(Color::DarkGray).fg(Color::White)
                } else {
                    Style::default().fg(Color::White)
                };

                // Show expand indicator for expandable values
                let expandable = value.is_some_and(is_expandable);
                let expand_indicator = if expandable { "> " } else { "  " };

                ListItem::new(Line::from(vec![
                    Span::styled(expand_indicator, Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("{}: ", col_name), Style::default().fg(Color::Yellow)),
                    Span::styled(display, style),
                ]))
            })
            .collect();

        let scroll_info = if self.columns.len() > self.visible_height {
            format!(
                " (fields {}-{}/{})",
                scroll_offset + 1,
                (scroll_offset + self.visible_height).min(self.columns.len()),
                self.columns.len()
            )
        } else {
            format!(" ({} fields)", self.columns.len())
        };

        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("{} - Row {}{}", title, row + 1, scroll_info))
                .border_style(border_style),
        )
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
    ) -> ResultsWidget<'_> {
        let field_name = self.columns.get(field).map(|s| s.as_str()).unwrap_or("?");
        let breadcrumb = format_breadcrumb(field_name, path);
        let full_title = format!("{} - Row {}, {}", title, row + 1, breadcrumb);

        let cell = self.get_cell_value(row, field);
        let current = cell.and_then(|c| resolve_path(c, path));

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
                            let k =
                                kv.first().map(|v| format_cell_value(v, 30)).unwrap_or_default();
                            let v = kv.get(1).map(|v| format_cell_value(v, 40)).unwrap_or_default();
                            let expandable = kv.get(1).is_some_and(is_expandable);
                            (k, v, expandable)
                        } else {
                            (String::new(), format_cell_value(pair, 60), false)
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
                        let display = format_cell_value(v, 60);
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
                        let display = format_cell_value(v, 50);
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
                    Value::String(s) => s.clone(),
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
            ResultsViewMode::FieldList { row, scroll_offset } => ResultsWidget::List(
                self.render_field_list(title, *row, *scroll_offset, border_style),
            ),
            ResultsViewMode::FieldValue { row, field, path, selected_index, scroll_offset } => self
                .render_nested_value(
                    title,
                    *row,
                    *field,
                    path,
                    *selected_index,
                    *scroll_offset,
                    border_style,
                ),
        }
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
