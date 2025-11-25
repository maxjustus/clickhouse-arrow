use std::collections::HashMap;

use chrono::DateTime;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Color;

use crate::tui::widgets::charts::TimeSeriesData;
use crate::tui::widgets::table::SortableTable;

// Colors for different metrics
const METRIC_COLORS: [Color; 8] = [
    Color::Cyan,
    Color::Green,
    Color::Yellow,
    Color::Magenta,
    Color::Red,
    Color::Blue,
    Color::LightCyan,
    Color::LightGreen,
];

pub struct ProfileTab {
    pub events:      Vec<serde_json::Value>,
    pub time_series: HashMap<String, TimeSeriesData>,
    pub table:       SortableTable,
    color_index:     usize,
    base_timestamp:  Option<i64>, // First timestamp for relative x-axis
}

impl ProfileTab {
    pub fn new() -> Self {
        Self {
            events:         Vec::new(),
            time_series:    HashMap::new(),
            table:          SortableTable::new(vec![
                "Time".to_string(),
                "Thread".to_string(),
                "Metric".to_string(),
                "Value".to_string(),
            ]),
            color_index:    0,
            base_timestamp: None,
        }
    }

    pub fn clear(&mut self) {
        self.events.clear();
        self.time_series.clear();
        self.table.clear();
        self.color_index = 0;
        self.base_timestamp = None;
    }

    pub fn add_event(&mut self, event: serde_json::Value) {
        if let serde_json::Value::Object(ref map) = event {
            if let (Some(name), Some(value), Some(time_str)) = (
                map.get("name").and_then(|n| n.as_str()),
                map.get("value").and_then(|v| v.as_i64()),
                map.get("current_time").and_then(|t| t.as_str()),
            ) {
                let thread_id = map.get("thread_id").and_then(|t| t.as_u64()).unwrap_or(0);

                self.table.add_row(vec![
                    time_str.to_string(),
                    thread_id.to_string(),
                    name.to_string(),
                    value.to_string(),
                ]);

                // Parse timestamp and add to time series (chart ALL metrics)
                if let Ok(dt) = DateTime::parse_from_rfc3339(time_str) {
                    let timestamp_ms = dt.timestamp_millis();

                    // Set base timestamp on first event
                    if self.base_timestamp.is_none() {
                        self.base_timestamp = Some(timestamp_ms);
                    }

                    // Use relative time from base for x-axis
                    let relative_time =
                        (timestamp_ms - self.base_timestamp.unwrap_or(timestamp_ms)) as f64;

                    let series = self.time_series.entry(name.to_string()).or_insert_with(|| {
                        let color = METRIC_COLORS[self.color_index % METRIC_COLORS.len()];
                        self.color_index += 1;
                        TimeSeriesData::new(name.to_string(), color)
                    });

                    series.add_point(relative_time, value as f64);
                }
            }
        }

        self.events.push(event);
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        match key.code {
            KeyCode::Down => self.table.next_row(),
            KeyCode::Up => self.table.prev_row(),
            KeyCode::PageDown => self.table.page_down(),
            KeyCode::PageUp => self.table.page_up(),
            KeyCode::Char('s') | KeyCode::Char('S') => {
                if !self.table.columns.is_empty() {
                    self.table.sort_by_column(2);
                }
            }
            _ => {}
        }
        Ok(())
    }
}

pub fn render(f: &mut Frame, area: Rect, tab: &ProfileTab) {
    if tab.events.is_empty() {
        let empty = ratatui::widgets::Paragraph::new(
            "No profile events yet. Execute a query with profile events enabled.",
        )
        .block(
            ratatui::widgets::Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .title("Profile Events"),
        );
        f.render_widget(empty, area);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    // Get the metric name from the currently selected row (column index 2 is "Metric")
    let selected_metric = tab
        .table
        .rows
        .get(tab.table.selected_row)
        .and_then(|row| row.get(2))
        .cloned();

    if let Some(ref metric_name) = selected_metric {
        if let Some(series) = tab.time_series.get(metric_name) {
            let series_vec = vec![series.clone()];
            let title = format!("Profile: {}", metric_name);
            let chart =
                crate::tui::widgets::charts::render_time_series_chart(&series_vec, &title);
            f.render_widget(chart, chunks[0]);
        } else {
            render_no_chart(f, chunks[0], metric_name);
        }
    } else {
        render_no_chart(f, chunks[0], "No metric selected");
    }

    let table_widget = tab.table.render_widget("Profile Events (Up/Down to select)");
    f.render_widget(table_widget, chunks[1]);
}

fn render_no_chart(f: &mut Frame, area: Rect, message: &str) {
    let no_chart = ratatui::widgets::Paragraph::new(message).block(
        ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .title("Profile Metrics"),
    );
    f.render_widget(no_chart, area);
}
