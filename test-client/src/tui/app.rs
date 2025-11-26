use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::Backend;
use tokio::sync::mpsc;

use crate::tui::history::History;
use crate::tui::session::{Focus, Mode, Session, SubPane};
use crate::tui::ui::render;

#[derive(Debug, Clone)]
pub enum AppEvent {
    QueryStarted { query_id: usize },
    QueryComplete { query_id: usize },
    QueryError { query_id: usize, error: String },
    RowReceived { query_id: usize, row: serde_json::Value },
    ProfileEvent { query_id: usize, event: serde_json::Value },
    LogEvent { query_id: usize, log: serde_json::Value },
    ProgressEvent { query_id: usize, progress: serde_json::Value },
}

#[derive(Debug, Clone)]
pub enum QueryCommand {
    Execute { query_id: usize, sql: String },
    Cancel { query_id: usize },
}

pub struct App {
    pub session:     Session,
    pub should_quit: bool,
    pub show_help:   bool,
    pub history:     History,
    cmd_tx:          mpsc::Sender<QueryCommand>,
    event_rx:        mpsc::Receiver<AppEvent>,
}

impl App {
    pub fn new(
        cmd_tx: mpsc::Sender<QueryCommand>,
        event_rx: mpsc::Receiver<AppEvent>,
    ) -> Result<Self> {
        let history = History::load().unwrap_or_default();
        Ok(Self {
            session: Session::new(),
            should_quit: false,
            show_help: false,
            history,
            cmd_tx,
            event_rx,
        })
    }

    pub async fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()> {
        loop {
            self.session.clear_expired_toast();
            terminal.draw(|f| render(f, self))?;

            if self.should_quit {
                break;
            }

            // Poll keyboard with short timeout for responsive UI
            if event::poll(Duration::from_millis(16))? {
                match event::read()? {
                    Event::Key(key) => {
                        self.handle_key(key).await?;
                    }
                    Event::Paste(text) => {
                        // Always paste into new query editor
                        self.session.new_query.insert_str(&text);
                        self.session.focus = Focus::NewQuery;
                        self.session.mode = Mode::Edit;
                    }
                    _ => {}
                }
            }

            // Drain all pending app events (non-blocking)
            while let Ok(evt) = self.event_rx.try_recv() {
                self.handle_event(evt);
            }
        }

        Ok(())
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        // Help screen intercepts all keys
        if self.show_help {
            if let KeyCode::Esc | KeyCode::Char('?') = key.code {
                self.show_help = false;
            }
            return Ok(());
        }

        // Global keys work in all modes
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
                return Ok(());
            }
            KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
                return Ok(());
            }
            KeyCode::Char('?') => {
                self.show_help = true;
                return Ok(());
            }
            _ => {}
        }

        match self.session.mode {
            Mode::Navigation => self.handle_navigation_key(key).await,
            Mode::Edit => self.handle_edit_key(key).await,
        }
    }

    async fn handle_navigation_key(&mut self, key: KeyEvent) -> Result<()> {
        match &self.session.focus {
            Focus::Sidebar => {
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.session.sidebar_next();
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.session.sidebar_prev();
                    }
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                        // Enter selected query's sub-panes
                        if self.session.selected_query.is_some() {
                            self.session.focus = Focus::SubPane(SubPane::Results);
                        }
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') => {
                        self.session.focus = Focus::NewQuery;
                        self.session.mode = Mode::Edit;
                    }
                    KeyCode::Char('c') | KeyCode::Char('C') => {
                        self.cancel_selected_query().await;
                    }
                    _ => {}
                }
            }
            Focus::SubPane(_pane) => {
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.session.subpane_next();
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.session.subpane_prev();
                    }
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter | KeyCode::Char('i') => {
                        // Enter edit mode for any pane
                        self.session.mode = Mode::Edit;
                    }
                    KeyCode::Left | KeyCode::Char('h') => {
                        // Go back to sidebar
                        self.session.focus = Focus::Sidebar;
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') => {
                        self.session.focus = Focus::NewQuery;
                        self.session.mode = Mode::Edit;
                    }
                    KeyCode::Char('c') | KeyCode::Char('C') => {
                        self.cancel_selected_query().await;
                    }
                    _ => {}
                }
            }
            Focus::NewQuery => {
                match key.code {
                    KeyCode::Left | KeyCode::Char('h') => {
                        self.session.focus = Focus::Sidebar;
                    }
                    KeyCode::Enter | KeyCode::Char('i') => {
                        self.session.mode = Mode::Edit;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        // From new query, go to sidebar
                        self.session.focus = Focus::Sidebar;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        // From new query, go to sidebar
                        self.session.focus = Focus::Sidebar;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    async fn handle_edit_key(&mut self, key: KeyEvent) -> Result<()> {
        // Escape exits edit mode
        if key.code == KeyCode::Esc {
            self.session.mode = Mode::Navigation;
            // If escaping from new query, return to sidebar or selected query
            if matches!(self.session.focus, Focus::NewQuery) {
                if self.session.selected_query.is_some() {
                    self.session.focus = Focus::SubPane(SubPane::Results);
                } else {
                    self.session.focus = Focus::Sidebar;
                }
            }
            return Ok(());
        }

        match &self.session.focus {
            Focus::NewQuery => {
                self.handle_new_query_key(key).await?;
            }
            Focus::Sidebar => {
                // Sidebar doesn't have edit mode
                self.session.mode = Mode::Navigation;
            }
            Focus::SubPane(pane) => {
                let pane = *pane;
                self.handle_subpane_key(key, pane)?;
            }
        }
        Ok(())
    }

    async fn handle_new_query_key(&mut self, key: KeyEvent) -> Result<()> {
        let is_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let is_alt = key.modifiers.contains(KeyModifiers::ALT);

        match (key.code, is_ctrl, is_alt) {
            // Execute query
            (KeyCode::Enter, true, _) | (KeyCode::Enter, _, true) => {
                if let Some((query_id, sql)) = self.session.execute_new_query() {
                    // Save to history
                    let _ = self.history.add(sql.clone(), None);
                    self.history.reset_nav();

                    let cmd = QueryCommand::Execute { query_id, sql };
                    let _ = self.cmd_tx.send(cmd).await;
                }
            }
            // Navigate to previous history entry
            (KeyCode::Char('p'), true, _) => {
                let current = self.session.new_query.lines().join("\n");
                if let Some(query) = self.history.nav_prev(&current) {
                    let query = query.to_string();
                    self.set_new_query_text(&query);
                }
            }
            // Navigate to next history entry
            (KeyCode::Char('n'), true, _) => {
                if let Some(query) = self.history.nav_next() {
                    let query = query.to_string();
                    self.set_new_query_text(&query);
                }
            }
            // Any other key - pass to textarea and reset nav
            _ => {
                self.session.new_query.input(key);
                self.history.reset_nav();
            }
        }
        Ok(())
    }

    fn set_new_query_text(&mut self, text: &str) {
        // Clear and set new text
        self.session.new_query.select_all();
        self.session.new_query.cut();
        self.session.new_query.insert_str(text);
    }

    async fn cancel_selected_query(&mut self) {
        if let Some(query_id) = self.session.selected_query
            && let Some(block) = self.session.blocks.get_mut(query_id)
                && block.running && !block.cancel_requested {
                    block.cancel_requested = true;
                    let _ = self.cmd_tx.send(QueryCommand::Cancel { query_id }).await;
                }
    }

    fn handle_subpane_key(&mut self, key: KeyEvent, pane: SubPane) -> Result<()> {
        // Handle copy to clipboard (needs special handling due to borrow checker)
        if pane == SubPane::Results && key.code == KeyCode::Char('c') {
            if let Some(block) = self.session.selected_block()
                && let Some(table) = &block.results {
                    let content = table.get_clipboard_content();
                    if !content.is_empty()
                        && let Ok(mut clipboard) = arboard::Clipboard::new()
                            && clipboard.set_text(content).is_ok() {
                                self.session.show_toast("Copied to clipboard");
                            }
                }
            return Ok(());
        }

        let block = match self.session.selected_block_mut() {
            Some(b) => b,
            None => return Ok(()),
        };

        match pane {
            SubPane::Sql => match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    block.sql_scroll += 1;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    block.sql_scroll = block.sql_scroll.saturating_sub(1);
                }
                KeyCode::PageDown => {
                    block.sql_scroll += 10;
                }
                KeyCode::PageUp => {
                    block.sql_scroll = block.sql_scroll.saturating_sub(10);
                }
                KeyCode::Left | KeyCode::Char('h') => {
                    self.session.mode = Mode::Navigation;
                }
                _ => {}
            },
            SubPane::Results => {
                if let Some(ref mut table) = block.results {
                    match (key.code, key.modifiers.contains(KeyModifiers::ALT)) {
                        // Tree navigation
                        (KeyCode::Down | KeyCode::Char('j'), false) => table.nav_down(),
                        (KeyCode::Up | KeyCode::Char('k'), false) => table.nav_up(),
                        (KeyCode::Right | KeyCode::Char('l'), false) => {
                            table.expand();
                        }
                        (KeyCode::Left | KeyCode::Char('h'), false) => {
                            if !table.collapse() {
                                // At top level, exit edit mode
                                self.session.mode = Mode::Navigation;
                            }
                        }
                        // Alt+arrows for column scrolling (Table mode only)
                        (KeyCode::Right | KeyCode::Char('l'), true) => table.scroll_cols_right(),
                        (KeyCode::Left | KeyCode::Char('h'), true) => {
                            table.scroll_cols_left();
                        }
                        // Other navigation
                        (KeyCode::PageDown, _) => table.page_down(),
                        (KeyCode::PageUp, _) => table.page_up(),
                        (KeyCode::Char('s') | KeyCode::Char('S'), false) => {
                            if !table.columns.is_empty() {
                                table.sort_by_column(0);
                            }
                        }
                        _ => {}
                    }
                } else {
                    // No table, just exit
                    self.session.mode = Mode::Navigation;
                }
            }
            SubPane::Stats => {
                // Use new metrics navigation
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => block.stats.nav_down(),
                    KeyCode::Up | KeyCode::Char('k') => block.stats.nav_up(),
                    KeyCode::PageDown => block.stats.page_down(),
                    KeyCode::PageUp => block.stats.page_up(),
                    KeyCode::Right | KeyCode::Char('l') => {
                        block.stats.expand();
                    }
                    KeyCode::Left | KeyCode::Char('h') => {
                        if !block.stats.collapse() {
                            // At table level, exit edit mode
                            self.session.mode = Mode::Navigation;
                        }
                    }
                    _ => {}
                }
            }
            SubPane::Logs => match key.code {
                KeyCode::Down | KeyCode::Char('j') => block.logs_data.nav_down(),
                KeyCode::Up | KeyCode::Char('k') => block.logs_data.nav_up(),
                KeyCode::PageDown => block.logs_data.page_down(),
                KeyCode::PageUp => block.logs_data.page_up(),
                KeyCode::Right | KeyCode::Char('l') => {
                    block.logs_data.expand();
                }
                KeyCode::Left | KeyCode::Char('h') => {
                    if !block.logs_data.collapse() {
                        self.session.mode = Mode::Navigation;
                    }
                }
                _ => {}
            },
        }
        Ok(())
    }

    pub fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::QueryStarted { query_id } => {
                // Block already created by execute_new_query
                if let Some(block) = self.session.get_block_mut(query_id) {
                    block.running = true;
                }
            }
            AppEvent::QueryComplete { query_id } => {
                if let Some(block) = self.session.get_block_mut(query_id) {
                    block.running = false;
                    block.cancel_requested = false;
                }
            }
            AppEvent::QueryError { query_id, error } => {
                if let Some(block) = self.session.get_block_mut(query_id) {
                    block.error = Some(error);
                    block.running = false;
                    block.cancel_requested = false;
                }
            }
            AppEvent::RowReceived { query_id, row } => {
                if let Some(block) = self.session.get_block_mut(query_id) {
                    block.add_result_row(row);
                }
            }
            AppEvent::ProfileEvent { query_id, event } => {
                if let Some(block) = self.session.get_block_mut(query_id) {
                    block.add_profile_event(event);
                }
            }
            AppEvent::LogEvent { query_id, log } => {
                if let Some(block) = self.session.get_block_mut(query_id) {
                    block.add_log(log);
                }
            }
            AppEvent::ProgressEvent { query_id, progress } => {
                if let Some(block) = self.session.get_block_mut(query_id) {
                    block.add_progress(progress);
                }
            }
        }
    }
}
