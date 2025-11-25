use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};

pub struct LogsTab {
    pub logs:         Vec<LogEntry>,
    pub scroll_state: ListState,
    pub auto_scroll:  bool,
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub time:     String,
    pub priority: i8,
    pub source:   String,
    pub text:     String,
}

impl LogsTab {
    pub fn new() -> Self {
        Self { logs: Vec::new(), scroll_state: ListState::default(), auto_scroll: true }
    }

    pub fn clear(&mut self) {
        self.logs.clear();
        self.scroll_state = ListState::default();
    }

    pub fn add_log(&mut self, log: serde_json::Value) {
        if let serde_json::Value::Object(ref map) = log {
            let entry = LogEntry {
                time:     map.get("time").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                priority: map.get("priority").and_then(|v| v.as_i64()).unwrap_or(0) as i8,
                source:   map.get("source").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                text:     map.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            };

            self.logs.push(entry);

            if self.auto_scroll {
                let last_idx = self.logs.len().saturating_sub(1);
                self.scroll_state.select(Some(last_idx));
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        match key.code {
            KeyCode::Down => self.scroll_down(),
            KeyCode::Up => self.scroll_up(),
            KeyCode::PageDown => {
                for _ in 0..10 {
                    self.scroll_down();
                }
            }
            KeyCode::PageUp => {
                for _ in 0..10 {
                    self.scroll_up();
                }
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                self.auto_scroll = !self.auto_scroll;
            }
            _ => {}
        }
        Ok(())
    }

    fn scroll_down(&mut self) {
        if self.logs.is_empty() {
            return;
        }
        let i = match self.scroll_state.selected() {
            Some(i) => {
                if i < self.logs.len() - 1 {
                    i + 1
                } else {
                    i
                }
            }
            None => 0,
        };
        self.scroll_state.select(Some(i));
    }

    fn scroll_up(&mut self) {
        if self.logs.is_empty() {
            return;
        }
        let i = match self.scroll_state.selected() {
            Some(i) => {
                if i > 0 {
                    i - 1
                } else {
                    0
                }
            }
            None => 0,
        };
        self.scroll_state.select(Some(i));
    }
}

pub fn render(f: &mut Frame, area: Rect, tab: &mut LogsTab) {
    if tab.logs.is_empty() {
        let empty = ratatui::widgets::Paragraph::new(
            "No logs yet. Execute a query with send_logs_level setting to see logs here.",
        )
        .block(Block::default().borders(Borders::ALL).title("Trace Logs"));
        f.render_widget(empty, area);
        return;
    }

    let items: Vec<ListItem> = tab
        .logs
        .iter()
        .map(|log| {
            let color = match log.priority {
                1 => Color::Red,
                2 => Color::LightRed,
                3 => Color::Yellow,
                4 => Color::Blue,
                5 => Color::Cyan,
                _ => Color::White,
            };

            let line = Line::from(vec![
                Span::styled(&log.time, Style::default().fg(Color::Gray)),
                Span::raw(" "),
                Span::styled(&log.source, Style::default().fg(Color::Green)),
                Span::raw(" "),
                Span::styled(&log.text, Style::default().fg(color)),
            ]);

            ListItem::new(line)
        })
        .collect();

    let auto_scroll_indicator = if tab.auto_scroll { " [AUTO]" } else { "" };
    let title = format!("Trace Logs ({} entries){}", tab.logs.len(), auto_scroll_indicator);

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD));

    f.render_stateful_widget(list, area, &mut tab.scroll_state);
}
