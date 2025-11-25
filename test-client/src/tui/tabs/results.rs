use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::tui::widgets::table::SortableTable;

pub struct ResultsTab {
    pub table:    Option<SortableTable>,
    pub progress: Option<serde_json::Value>,
}

impl ResultsTab {
    pub fn new() -> Self { Self { table: None, progress: None } }

    pub fn clear(&mut self) {
        self.table = None;
        self.progress = None;
    }

    pub fn add_row(&mut self, row: serde_json::Value) {
        if let serde_json::Value::Object(map) = row {
            if self.table.is_none() {
                let columns: Vec<String> = map.keys().cloned().collect();
                self.table = Some(SortableTable::new(columns));
            }

            if let Some(table) = &mut self.table {
                let row_data: Vec<String> = table
                    .columns
                    .iter()
                    .map(|col| map.get(col).map(|v| format_json_value(v)).unwrap_or_default())
                    .collect();
                table.add_row(row_data);
            }
        }
    }

    pub fn update_progress(&mut self, progress: serde_json::Value) {
        self.progress = Some(progress);
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        if let Some(table) = &mut self.table {
            match key.code {
                KeyCode::Down => table.next_row(),
                KeyCode::Up => table.prev_row(),
                KeyCode::PageDown => table.page_down(),
                KeyCode::PageUp => table.page_up(),
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    // TODO: Prompt for column number
                    if !table.columns.is_empty() {
                        table.sort_by_column(0);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

pub fn render(f: &mut Frame, area: Rect, tab: &ResultsTab) {
    if let Some(table) = &tab.table {
        let widget = table.render_widget("Query Results");
        f.render_widget(widget, area);
    } else {
        let empty = ratatui::widgets::Paragraph::new(
            "No results yet. Execute a query to see results here.",
        )
        .block(
            ratatui::widgets::Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .title("Query Results"),
        );
        f.render_widget(empty, area);
    }
}

fn format_json_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "NULL".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => format!("[{}]", arr.len()),
        serde_json::Value::Object(obj) => format!("{{{}}}", obj.len()),
    }
}
