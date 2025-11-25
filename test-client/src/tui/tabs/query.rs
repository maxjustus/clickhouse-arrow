use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use tui_textarea::TextArea;

use crate::tui::app::QueryCommand;

pub struct QueryTab {
    pub editor:   TextArea<'static>,
    pub query_id: Option<String>,
    pub error:    Option<String>,
}

impl QueryTab {
    pub fn new() -> Self {
        let mut editor = TextArea::default();
        editor.set_block(Block::default().borders(Borders::ALL).title("SQL Query"));
        editor.set_placeholder_text("Enter SQL query here... (Ctrl+Enter or Alt+Enter to execute)");

        Self { editor, query_id: None, error: None }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> anyhow::Result<Option<QueryCommand>> {
        let execute_modifiers = key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::ALT);

        match key.code {
            KeyCode::Enter if execute_modifiers => {
                let sql = self.query_text();
                if !sql.trim().is_empty() {
                    return Ok(Some(QueryCommand::Execute { sql }));
                }
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // TODO: Show history popup
            }
            _ => {
                self.editor.input(key);
            }
        }
        Ok(None)
    }

    pub fn set_query_id(&mut self, query_id: Option<String>) {
        if query_id.is_some() {
            self.error = None;
        }
        self.query_id = query_id;
    }

    pub fn set_error(&mut self, error: Option<String>) { self.error = error; }

    pub fn query_text(&self) -> String { self.editor.lines().join("\n") }
}

pub fn render(f: &mut Frame, area: Rect, tab: &QueryTab) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(10), Constraint::Length(3)])
        .split(area);

    f.render_widget(&tab.editor, chunks[0]);

    let status = if let Some(ref err) = tab.error {
        Paragraph::new(Line::from(vec![
            Span::styled("Error: ", Style::default().fg(Color::Red)),
            Span::raw(err),
        ]))
        .style(Style::default().fg(Color::Red))
    } else if tab.query_id.is_some() {
        Paragraph::new("Executing query...")
            .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
    } else {
        Paragraph::new("Ready to execute (Ctrl+Enter)").style(Style::default().fg(Color::Green))
    };

    f.render_widget(
        status.block(Block::default().borders(Borders::ALL).title("Status")),
        chunks[1],
    );
}
