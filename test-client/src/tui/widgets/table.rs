use ratatui::layout::Constraint;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Row, Table, Widget, Wrap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    Ascending,
    Descending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultsViewMode {
    Table,
    FieldList { row: usize },
    FieldValue { row: usize, field: usize },
}

impl Default for ResultsViewMode {
    fn default() -> Self { Self::Table }
}

#[derive(Debug)]
pub struct SortableTable {
    pub columns:        Vec<String>,
    pub rows:           Vec<Vec<String>>,
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

    pub fn add_row(&mut self, row: Vec<String>) {
        // Update cached column widths incrementally
        for (i, cell) in row.iter().enumerate() {
            if i < self.col_widths.len() {
                self.col_widths[i] = self.col_widths[i].max(cell.len());
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

        self.rows.sort_by(|a, b| {
            let ord = a.get(col).cmp(&b.get(col));
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

    /// Navigate down in current view mode
    pub fn nav_down(&mut self) {
        match self.view_mode {
            ResultsViewMode::Table => self.next_row(),
            ResultsViewMode::FieldList { .. } => {
                if self.selected_field < self.columns.len().saturating_sub(1) {
                    self.selected_field += 1;
                }
            }
            ResultsViewMode::FieldValue { .. } => {
                self.value_scroll += 1;
            }
        }
    }

    /// Navigate up in current view mode
    pub fn nav_up(&mut self) {
        match self.view_mode {
            ResultsViewMode::Table => self.prev_row(),
            ResultsViewMode::FieldList { .. } => {
                if self.selected_field > 0 {
                    self.selected_field -= 1;
                }
            }
            ResultsViewMode::FieldValue { .. } => {
                self.value_scroll = self.value_scroll.saturating_sub(1);
            }
        }
    }

    /// Expand/drill into current selection. Returns false if can't expand further.
    pub fn expand(&mut self) -> bool {
        match self.view_mode {
            ResultsViewMode::Table => {
                if !self.rows.is_empty() {
                    self.view_mode = ResultsViewMode::FieldList { row: self.selected_row };
                    self.selected_field = 0;
                    true
                } else {
                    false
                }
            }
            ResultsViewMode::FieldList { row } => {
                self.view_mode = ResultsViewMode::FieldValue { row, field: self.selected_field };
                self.value_scroll = 0;
                true
            }
            ResultsViewMode::FieldValue { .. } => {
                // Already at deepest level
                false
            }
        }
    }

    /// Collapse/go back. Returns false if at top level (should exit edit mode).
    pub fn collapse(&mut self) -> bool {
        match self.view_mode {
            ResultsViewMode::Table => {
                // At top level, signal to exit edit mode
                false
            }
            ResultsViewMode::FieldList { row } => {
                self.view_mode = ResultsViewMode::Table;
                self.selected_row = row;
                true
            }
            ResultsViewMode::FieldValue { row, field } => {
                self.view_mode = ResultsViewMode::FieldList { row };
                self.selected_field = field;
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
                    // Truncate cells and limit columns
                    let truncated: Vec<String> = row
                        .iter()
                        .skip(col_offset)
                        .take(visible_cols)
                        .map(|cell| {
                            if cell.len() > 40 {
                                format!("{}...", &cell[..37])
                            } else {
                                cell.clone()
                            }
                        })
                        .collect();
                    Row::new(truncated).style(style).height(1)
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

    fn render_field_list(&self, title: &str, row: usize, border_style: Style) -> List<'_> {
        let row_data = self.rows.get(row);

        let items: Vec<ListItem> = self
            .columns
            .iter()
            .enumerate()
            .map(|(i, col_name)| {
                let value = row_data.and_then(|r| r.get(i)).map(|s| s.as_str()).unwrap_or("");

                let truncated = if value.len() > 60 {
                    format!("{}...", &value[..57])
                } else {
                    value.to_string()
                };

                let style = if i == self.selected_field {
                    Style::default().bg(Color::DarkGray).fg(Color::White)
                } else {
                    Style::default().fg(Color::White)
                };

                let expand_indicator =
                    if value.len() > 60 || value.contains('\n') { "▶ " } else { "  " };

                ListItem::new(Line::from(vec![
                    Span::styled(expand_indicator, Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("{}: ", col_name), Style::default().fg(Color::Yellow)),
                    Span::styled(truncated.replace('\n', "↵"), style),
                ]))
            })
            .collect();

        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("{} - Row {} of {}", title, row + 1, self.rows.len()))
                .border_style(border_style),
        )
    }

    fn render_field_value(
        &self,
        title: &str,
        row: usize,
        field: usize,
        border_style: Style,
    ) -> Paragraph<'_> {
        let field_name = self.columns.get(field).map(|s| s.as_str()).unwrap_or("?");
        let value = self.rows.get(row).and_then(|r| r.get(field)).map(|s| s.as_str()).unwrap_or("");

        // Split into lines and handle scrolling
        let lines: Vec<&str> = value.lines().collect();
        let display_lines: String = lines
            .iter()
            .skip(self.value_scroll)
            .take(50) // Max lines to show
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

        Paragraph::new(display_lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("{} - Row {}, {}{}", title, row + 1, field_name, scroll_info))
                    .border_style(border_style),
            )
            .wrap(Wrap { trim: false })
    }

    /// Render based on current view mode. Returns a widget that can be rendered.
    pub fn render<'a>(
        &'a self,
        title: &'a str,
        available_width: u16,
        border_style: Style,
    ) -> ResultsWidget<'a> {
        match self.view_mode {
            ResultsViewMode::Table => {
                ResultsWidget::Table(self.render_widget(title, available_width, border_style))
            }
            ResultsViewMode::FieldList { row } => {
                ResultsWidget::List(self.render_field_list(title, row, border_style))
            }
            ResultsViewMode::FieldValue { row, field } => {
                ResultsWidget::Value(self.render_field_value(title, row, field, border_style))
            }
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
