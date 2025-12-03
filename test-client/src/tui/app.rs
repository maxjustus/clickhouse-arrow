use std::collections::HashMap;
use std::io::Cursor;
use std::time::Duration;

use anyhow::Result;
use clickhouse_arrow::file_stream::FileStreamReader;
use clickhouse_arrow::{CompressionMethod, NativeFormat};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::Backend;
use tokio::sync::mpsc;

use crate::tui::backend::row_to_json;
use crate::tui::history::History;
use crate::tui::query_store::{QueryArchiveReader, QueryStore, QueryStoreEntry};
use crate::tui::session::{
    Focus, LogsViewMode, MetricsViewMode, Mode, QueryBlock, ROW_JUMP_COUNT, Session, SubPane,
    ViewState,
};
use crate::tui::ui::render;
use crate::tui::widgets::table::ResultsViewMode;

#[derive(Debug, Clone)]
pub enum AppEvent {
    QueryStarted { query_id: usize },
    QueryComplete { query_id: usize },
    QueryError { query_id: usize, error: String },
    RowReceived { query_id: usize, row: serde_json::Value },
    ProfileEvent { query_id: usize, event: serde_json::Value },
    ProfileInfoEvent { query_id: usize, profile_info: serde_json::Value },
    LogEvent { query_id: usize, log: serde_json::Value },
    ProgressEvent { query_id: usize, progress: serde_json::Value },
    QueryCached { query_id: usize, entry: QueryStoreEntry },
    // Connection events
    ConnectionLost { error: String },
    Reconnecting { attempt: u32 },
    Reconnected,
    // Archive loading events
    ArchiveRowLoaded { cache_id: String, row: serde_json::Value },
    ArchiveLoadComplete,
    ArchiveLoadError { cache_id: String, error: String },
}

#[derive(Debug, Clone)]
pub enum QueryCommand {
    Execute { query_id: usize, sql: String },
    Cancel { query_id: usize },
}

pub struct App {
    pub session:      Session,
    pub should_quit:  bool,
    pub show_help:    bool,
    pub history:      History,
    cmd_tx:           mpsc::Sender<QueryCommand>,
    event_tx:         mpsc::Sender<AppEvent>,
    event_rx:         mpsc::Receiver<AppEvent>,
    /// Maps query_id -> block_index for event routing
    query_map:        HashMap<usize, usize>,
    next_query_id:    usize,
    /// Cached UI state per query (keyed by cache_id)
    view_state_cache: HashMap<String, ViewState>,
}

impl App {
    pub fn new(
        cmd_tx: mpsc::Sender<QueryCommand>,
        event_tx: mpsc::Sender<AppEvent>,
        event_rx: mpsc::Receiver<AppEvent>,
    ) -> Result<Self> {
        let history = History::load().unwrap_or_default();
        Ok(Self {
            session: Session::new(),
            should_quit: false,
            show_help: false,
            history,
            cmd_tx,
            event_tx,
            event_rx,
            query_map: HashMap::new(),
            next_query_id: 0,
            view_state_cache: HashMap::new(),
        })
    }

    pub async fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()> {
        loop {
            self.session.clear_expired_toast();

            // Poll for completed zoomed value stats computations
            if let Some(block) = self.session.displayed_block_mut()
                && let Some(table) = &mut block.results
            {
                table.poll_stats_completion();
            }

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
                        self.session.focus = Focus::HistoryView;
                        self.session.selected_card = None; // Focus input
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
        // 'n' or 'i' jumps to new query input from anywhere - except when already editing input
        // Shift+N or Shift+I pre-populates with current query's SQL
        let in_input_edit = matches!(self.session.focus, Focus::HistoryView)
            && self.session.selected_card.is_none()
            && self.session.mode == Mode::Edit;
        if !in_input_edit
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(
                key.code,
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('i') | KeyCode::Char('I')
            )
        {
            self.show_help = false;

            // Shift variant: pre-populate with current query's SQL
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                if let Some(sql) = self.session.displayed_block().map(|b| b.sql.clone()) {
                    self.set_new_query_text(&sql);
                }
            }

            // Go to history view with input focused
            self.session.focus = Focus::HistoryView;
            self.session.selected_card = None;
            self.session.mode = Mode::Edit;
            return Ok(());
        }

        // Help screen intercepts all other keys
        if self.show_help {
            if let KeyCode::Esc | KeyCode::Char('?') = key.code {
                self.show_help = false;
            }
            return Ok(());
        }

        // Clear app error on any key press
        self.session.app_error = None;

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
            // Ctrl+P: Previous query card (global, except in input edit)
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let in_input_edit = self.session.mode == Mode::Edit
                    && matches!(self.session.focus, Focus::HistoryView)
                    && self.session.selected_card.is_none();
                if !in_input_edit {
                    self.save_current_view_state();
                    if self.session.card_prev() {
                        self.load_selected_entry().await;
                    }
                    return Ok(());
                }
                // Fall through to mode handler for input edit
            }
            // Ctrl+N: Next query card (global, except in input edit)
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let in_input_edit = self.session.mode == Mode::Edit
                    && matches!(self.session.focus, Focus::HistoryView)
                    && self.session.selected_card.is_none();
                if !in_input_edit {
                    self.save_current_view_state();
                    if self.session.card_next() {
                        self.load_selected_entry().await;
                    }
                    return Ok(());
                }
                // Fall through to mode handler for input edit
            }
            // Alt+1 through Alt+9: Jump directly to history entry
            KeyCode::Char(c @ '1'..='9') if key.modifiers.contains(KeyModifiers::ALT) => {
                let index = (c as usize) - ('1' as usize);
                self.save_current_view_state();
                if self.session.select_card_by_index(index) {
                    self.load_selected_entry().await;
                }
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
            Focus::HistoryView => {
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => {
                        // Navigate to next card (or from input to first card)
                        self.save_current_view_state();
                        if self.session.card_next() {
                            self.load_selected_entry().await;
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        // Navigate to previous card
                        self.save_current_view_state();
                        if self.session.card_prev() {
                            self.load_selected_entry().await;
                        }
                    }
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                        // If on a card, enter full results view
                        if self.session.selected_card.is_some()
                            && self.session.displayed_block().is_some()
                        {
                            self.session.focus = Focus::SubPane(SubPane::Results);
                        } else {
                            // On input, enter edit mode
                            self.session.mode = Mode::Edit;
                        }
                    }
                    KeyCode::Char('i') => {
                        // Enter edit mode (for input or for viewing card details)
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
                    KeyCode::Left | KeyCode::Char('h') | KeyCode::Esc => {
                        // Go back to history view
                        self.session.focus = Focus::HistoryView;
                    }
                    KeyCode::Char('c') | KeyCode::Char('C') => {
                        self.cancel_selected_query().await;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    async fn handle_edit_key(&mut self, key: KeyEvent) -> Result<()> {
        // Escape handling
        if key.code == KeyCode::Esc {
            // In input area (HistoryView with no card selected)
            if matches!(self.session.focus, Focus::HistoryView)
                && self.session.selected_card.is_none()
            {
                let text = self.session.new_query.lines().join("\n");
                if !text.trim().is_empty() {
                    // Clear the editor instead of exiting
                    self.session.new_query = tui_textarea::TextArea::default();
                    self.session
                        .new_query
                        .set_placeholder_text("Enter SQL query... (Ctrl+Enter to execute)");
                    return Ok(());
                }
                // Empty editor - just exit edit mode, stay in history view
                self.session.mode = Mode::Navigation;
                return Ok(());
            }
            // Other focuses: just exit edit mode
            self.session.mode = Mode::Navigation;
            return Ok(());
        }

        // Ctrl+P/N in SubPane: context-dependent behavior
        // - Base table view: switch queries in history
        // - Detail/split view: navigate rows
        if let Focus::SubPane(pane) = self.session.focus {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('p') => {
                        if let Some(block) = self.session.displayed_block_mut() {
                            let in_detail = match pane {
                                SubPane::Results => block
                                    .results
                                    .as_ref()
                                    .map(|t| !matches!(t.view_mode, ResultsViewMode::Table))
                                    .unwrap_or(false),
                                SubPane::Stats => {
                                    !matches!(block.stats.view_mode, MetricsViewMode::Table)
                                }
                                SubPane::Logs => {
                                    !matches!(block.logs_data.view_mode, LogsViewMode::Grouped)
                                }
                                _ => false,
                            };
                            if in_detail {
                                match pane {
                                    SubPane::Results => {
                                        if let Some(t) = &mut block.results {
                                            t.prev_detail_row();
                                        }
                                    }
                                    SubPane::Stats => block.stats.prev_detail_row(),
                                    SubPane::Logs => block.logs_data.prev_detail_entry(),
                                    _ => {}
                                }
                            } else {
                                self.session.card_prev();
                            }
                        } else {
                            self.session.card_prev();
                        }
                        return Ok(());
                    }
                    KeyCode::Char('n') => {
                        if let Some(block) = self.session.displayed_block_mut() {
                            let in_detail = match pane {
                                SubPane::Results => block
                                    .results
                                    .as_ref()
                                    .map(|t| !matches!(t.view_mode, ResultsViewMode::Table))
                                    .unwrap_or(false),
                                SubPane::Stats => {
                                    !matches!(block.stats.view_mode, MetricsViewMode::Table)
                                }
                                SubPane::Logs => {
                                    !matches!(block.logs_data.view_mode, LogsViewMode::Grouped)
                                }
                                _ => false,
                            };
                            if in_detail {
                                match pane {
                                    SubPane::Results => {
                                        if let Some(t) = &mut block.results {
                                            t.next_detail_row();
                                        }
                                    }
                                    SubPane::Stats => block.stats.next_detail_row(),
                                    SubPane::Logs => block.logs_data.next_detail_entry(),
                                    _ => {}
                                }
                            } else {
                                self.session.card_next();
                            }
                        } else {
                            self.session.card_next();
                        }
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }

        match &self.session.focus {
            Focus::HistoryView => {
                // In history view: if on input, handle keys; if on a card, enter navigation
                if self.session.selected_card.is_none() {
                    self.handle_new_query_key(key).await?;
                } else {
                    // On a card - edit mode not meaningful, go to navigation
                    self.session.mode = Mode::Navigation;
                }
            }
            Focus::SubPane(pane) => {
                let pane = *pane;
                self.handle_subpane_key(key, pane).await?;
            }
        }
        Ok(())
    }

    async fn handle_new_query_key(&mut self, key: KeyEvent) -> Result<()> {
        let is_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let is_alt = key.modifiers.contains(KeyModifiers::ALT);

        // Search mode (Ctrl+R) handling
        if self.history.is_searching() {
            match key.code {
                KeyCode::Esc => {
                    let draft = self.history.cancel_search().to_string();
                    self.set_new_query_text(&draft);
                    return Ok(());
                }
                KeyCode::Enter if is_ctrl || is_alt => {
                    // Ctrl+Enter: accept and execute immediately (fall through)
                    self.history.end_search();
                }
                KeyCode::Enter => {
                    // Plain Enter: accept current match, exit search, don't execute
                    self.history.end_search();
                    return Ok(());
                }
                KeyCode::Char('p') if is_ctrl => {
                    if let Some(query) = self.history.search_prev() {
                        let query = query.to_string();
                        self.set_new_query_text(&query);
                    }
                    return Ok(());
                }
                KeyCode::Char('n') if is_ctrl => {
                    if let Some(query) = self.history.search_next() {
                        let query = query.to_string();
                        self.set_new_query_text(&query);
                    }
                    return Ok(());
                }
                KeyCode::Char('r') if is_ctrl => {
                    // Ctrl+R again navigates to next match (like bash)
                    if let Some(query) = self.history.search_prev() {
                        let query = query.to_string();
                        self.set_new_query_text(&query);
                    }
                    return Ok(());
                }
                KeyCode::Backspace => {
                    let mut pattern = self.history.search_pattern().to_string();
                    pattern.pop();
                    self.history.update_search(&pattern);
                    if let Some(query) = self.history.current_search_result() {
                        let query = query.to_string();
                        self.set_new_query_text(&query);
                    }
                    return Ok(());
                }
                KeyCode::Char(c) if !is_ctrl && !is_alt => {
                    let mut pattern = self.history.search_pattern().to_string();
                    pattern.push(c);
                    self.history.update_search(&pattern);
                    if let Some(query) = self.history.current_search_result() {
                        let query = query.to_string();
                        self.set_new_query_text(&query);
                    }
                    return Ok(());
                }
                _ => {
                    return Ok(());
                }
            }
            // Only Ctrl+Enter reaches here - falls through to execute below
        }

        match (key.code, is_ctrl, is_alt) {
            // Start history search
            (KeyCode::Char('r'), true, _) => {
                let current = self.session.new_query.lines().join("\n");
                self.history.start_search(&current);
                if let Some(query) = self.history.current_search_result() {
                    let query = query.to_string();
                    self.set_new_query_text(&query);
                }
            }
            // Execute query
            (KeyCode::Enter, true, _) | (KeyCode::Enter, _, true) => {
                let sql = self.session.new_query.lines().join("\n");
                if sql.trim().is_empty() {
                    return Ok(());
                }

                // Save to command history
                let _ = self.history.add(sql.clone(), None);
                self.history.reset_nav();

                // Create history entry (metadata)
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let hash = QueryStore::hash_sql(&sql);
                let id = format!("{}-{}", timestamp, hash);
                let sql_preview: String = sql.chars().take(80).collect();

                let entry = QueryStoreEntry {
                    id: id.clone(),
                    hash,
                    sql_preview,
                    timestamp,
                    duration_ms: None,
                    row_count: 0,
                    error: None,
                };

                // Insert entry at end of history (newest last)
                let hist_idx = self.session.add_history_entry(entry);

                // Create QueryBlock for execution
                let mut block = QueryBlock::new(sql.clone());
                block.running = true;
                self.session.running_queries.insert(hist_idx, block);

                // Select this new entry and show it
                self.session.selected_card = Some(hist_idx);
                self.session.focus = Focus::SubPane(SubPane::Results);
                self.session.mode = Mode::Navigation;

                // Clear editor
                self.session.new_query = tui_textarea::TextArea::default();
                self.session
                    .new_query
                    .set_placeholder_text("Enter SQL query... (Ctrl+Enter to execute)");

                // Send execute command
                let query_id = self.next_query_id;
                self.next_query_id += 1;
                self.query_map.insert(query_id, hist_idx);

                let cmd = QueryCommand::Execute { query_id, sql };
                let _ = self.cmd_tx.send(cmd).await;
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
            // Format SQL with Ctrl+L
            (KeyCode::Char('l'), true, _) => {
                let sql = self.session.new_query.lines().join("\n");
                let formatted = sqlformat::format(
                    &sql,
                    &sqlformat::QueryParams::None,
                    &sqlformat::FormatOptions::default(),
                );
                // ClickHouse-specific: format SETTINGS clause
                let formatted = format_clickhouse(&formatted);
                self.set_new_query_text(&formatted);
                self.session.show_toast("SQL formatted");
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
        // reset cursor to start
        self.session.new_query.move_cursor(tui_textarea::CursorMove::Jump(0, 0));
    }

    async fn cancel_selected_query(&mut self) {
        // Cancel currently selected entry if it's running
        if let Some(hist_idx) = self.session.selected_card
            && let Some(block) = self.session.running_queries.get_mut(&hist_idx)
            && block.running
            && !block.cancel_requested
        {
            block.cancel_requested = true;
            // Find the query_id for this history index
            if let Some((&query_id, _)) = self.query_map.iter().find(|&(_, &idx)| idx == hist_idx) {
                let _ = self.cmd_tx.send(QueryCommand::Cancel { query_id }).await;
            }
        }
    }

    async fn handle_subpane_key(&mut self, key: KeyEvent, pane: SubPane) -> Result<()> {
        // Handle copy to clipboard (needs special handling due to borrow checker)
        if pane == SubPane::Results && key.code == KeyCode::Char('y') {
            let content = if let Some(block) = self.session.displayed_block()
                && let Some(table) = &block.results
            {
                table.get_clipboard_content()
            } else {
                String::new()
            };

            if !content.is_empty() {
                // Run clipboard operation on blocking thread pool to satisfy macOS NSPasteboard
                // requirements
                let result = tokio::task::spawn_blocking(move || {
                    arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(content))
                })
                .await;

                match result {
                    Ok(Ok(())) => {
                        self.session.show_toast("Copied to clipboard");
                    }
                    Ok(Err(e)) => {
                        self.session.app_error = Some(format!("Copy failed: {}", e));
                    }
                    Err(e) => {
                        self.session.app_error = Some(format!("Copy error: {}", e));
                    }
                }
            }
            return Ok(());
        }

        let block = match self.session.displayed_block_mut() {
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
                    if table.header_focused {
                        // Header navigation mode
                        match key.code {
                            KeyCode::Down | KeyCode::Char('j') => table.unfocus_header(),
                            KeyCode::Left | KeyCode::Char('h') => table.header_left(),
                            KeyCode::Right | KeyCode::Char('l') => table.header_right(),
                            KeyCode::Enter => {
                                table.cycle_sort(table.focused_col);
                            }
                            KeyCode::Esc => table.unfocus_header(),
                            _ => {}
                        }
                    } else {
                        // Normal table navigation
                        match (key.code, key.modifiers.contains(KeyModifiers::ALT)) {
                            // Tree navigation
                            (KeyCode::Down | KeyCode::Char('j'), false) => table.nav_down(),
                            (KeyCode::Up | KeyCode::Char('k'), false) => {
                                // Check if at row 0 in Table mode - focus header
                                if matches!(table.view_mode, ResultsViewMode::Table)
                                    && table.selected_row == 0
                                {
                                    table.focus_header();
                                } else {
                                    table.nav_up();
                                }
                            }
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
                            (KeyCode::Right | KeyCode::Char('l'), true) => {
                                table.scroll_cols_right()
                            }
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
                            // Row navigation in detail view ([ / ] or Shift+K / Shift+J)
                            (KeyCode::Char('[') | KeyCode::Char('K'), _) => table.prev_detail_row(),
                            (KeyCode::Char(']') | KeyCode::Char('J'), _) => table.next_detail_row(),
                            (KeyCode::Char('{'), _) => table.prev_detail_row_jump(ROW_JUMP_COUNT),
                            (KeyCode::Char('}'), _) => table.next_detail_row_jump(ROW_JUMP_COUNT),
                            // Tab: toggle between Table and Exploded view
                            (KeyCode::Tab, _) => {
                                table.view_mode = match &table.view_mode {
                                    ResultsViewMode::Table => {
                                        ResultsViewMode::Exploded { scroll_offset: 0 }
                                    }
                                    ResultsViewMode::Exploded { .. } => ResultsViewMode::Table,
                                    _ => table.view_mode.clone(), // Stay in FieldValue
                                };
                            }
                            _ => {}
                        }
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
                    // Row navigation ([ / ] or Shift+K / Shift+J)
                    KeyCode::Char('[') | KeyCode::Char('K') => block.stats.prev_detail_row(),
                    KeyCode::Char(']') | KeyCode::Char('J') => block.stats.next_detail_row(),
                    KeyCode::Char('{') => block.stats.prev_detail_row_jump(ROW_JUMP_COUNT),
                    KeyCode::Char('}') => block.stats.next_detail_row_jump(ROW_JUMP_COUNT),
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
                // Entry navigation ([ / ] or Shift+K / Shift+J)
                KeyCode::Char('[') | KeyCode::Char('K') => block.logs_data.prev_detail_entry(),
                KeyCode::Char(']') | KeyCode::Char('J') => block.logs_data.next_detail_entry(),
                KeyCode::Char('{') => block.logs_data.prev_detail_entry_jump(ROW_JUMP_COUNT),
                KeyCode::Char('}') => block.logs_data.next_detail_entry_jump(ROW_JUMP_COUNT),
                _ => {}
            },
        }
        Ok(())
    }

    /// Look up history_index from query_id
    fn lookup_query(&self, query_id: usize) -> Option<usize> {
        self.query_map.get(&query_id).copied()
    }

    pub fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::QueryStarted { query_id } => {
                if let Some(hist_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_running_mut(hist_idx) {
                        block.running = true;
                    }
                }
            }
            AppEvent::QueryComplete { query_id } => {
                if let Some(hist_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_running_mut(hist_idx) {
                        block.running = false;
                        block.cancel_requested = false;
                    }
                }
            }
            AppEvent::QueryError { query_id, error } => {
                if let Some(hist_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_running_mut(hist_idx) {
                        block.error = Some(error.clone());
                        block.running = false;
                        block.cancel_requested = false;
                    }
                    // Update history entry with error
                    if let Some(entry) = self.session.history.get_mut(hist_idx) {
                        entry.error = Some(error);
                    }
                }
            }
            AppEvent::RowReceived { query_id, row } => {
                if let Some(hist_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_running_mut(hist_idx) {
                        block.add_result_row(row);
                    }
                }
            }
            AppEvent::ProfileEvent { query_id, event } => {
                if let Some(hist_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_running_mut(hist_idx) {
                        block.add_profile_event(event);
                    }
                }
            }
            AppEvent::ProfileInfoEvent { query_id, profile_info } => {
                if let Some(hist_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_running_mut(hist_idx) {
                        block.stats.set_final_stats(profile_info);
                    }
                }
            }
            AppEvent::LogEvent { query_id, log } => {
                if let Some(hist_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_running_mut(hist_idx) {
                        block.add_log(log);
                    }
                }
            }
            AppEvent::ProgressEvent { query_id, progress } => {
                if let Some(hist_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_running_mut(hist_idx) {
                        block.add_progress(progress);
                    }
                }
            }
            AppEvent::QueryCached { query_id, entry } => {
                // Update the history entry with cache info
                if let Some(hist_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_running_mut(hist_idx) {
                        block.cache_id = Some(entry.id.clone());
                    }
                    // Update row count in history entry
                    if let Some(hist_entry) = self.session.history.get_mut(hist_idx) {
                        hist_entry.row_count = entry.row_count;
                        hist_entry.duration_ms = entry.duration_ms;
                    }
                }
            }
            AppEvent::ConnectionLost { error } => {
                let truncated: String = error.chars().take(50).collect();
                self.session.show_toast(format!("Connection lost: {}", truncated));
            }
            AppEvent::Reconnecting { attempt } => {
                if attempt == 1 {
                    self.session.show_toast("Reconnecting...");
                }
            }
            AppEvent::Reconnected => {
                self.session.show_toast("Reconnected");
            }
            AppEvent::ArchiveRowLoaded { cache_id, row } => {
                // Add row to current block if it matches the cache_id
                if let Some(block) = self.session.current_block.as_mut() {
                    if block.cache_id.as_ref() == Some(&cache_id) {
                        block.add_result_row(row);
                    }
                }
            }
            AppEvent::ArchiveLoadComplete => {
                // Loading complete - nothing special to do, rows are already added
            }
            AppEvent::ArchiveLoadError { cache_id, error } => {
                // Show error if still viewing this archive
                if let Some(block) = self.session.current_block.as_ref() {
                    if block.cache_id.as_ref() == Some(&cache_id) {
                        self.session.app_error =
                            Some(format!("Failed to parse results: {}", error));
                    }
                }
            }
        }
    }

    /// Save current query's view state to cache before switching
    fn save_current_view_state(&mut self) {
        if let Some(idx) = self.session.selected_card
            && let Some(entry) = self.session.history.get(idx)
            && let Some(block) = self.session.displayed_block()
        {
            let state = block.extract_view_state();
            self.view_state_cache.insert(entry.id.clone(), state);
        }
    }

    /// Load selected history entry's data (async)
    async fn load_selected_entry(&mut self) {
        let Some(hist_idx) = self.session.selected_card else {
            return;
        };

        // If it's a running query, data is already in running_queries
        if self.session.running_queries.contains_key(&hist_idx) {
            self.session.loading_entry_id = None;
            return;
        }

        // Get entry to load
        let Some(entry) = self.session.history.get(hist_idx).cloned() else {
            return;
        };

        // Mark as loading
        self.session.loading_entry_id = Some(entry.id.clone());
        self.session.current_block = None;

        // Load from archive
        self.load_archive(&entry).await;
    }

    /// Load archive data into current_block with background streaming for results
    async fn load_archive(&mut self, entry: &QueryStoreEntry) {
        let base_path = QueryStore::find_chc_dir();
        let archive_path = base_path.join(format!("{}.chc", entry.id));

        let archive = match QueryArchiveReader::open(&archive_path) {
            Ok(a) => a,
            Err(e) => {
                self.session.app_error = Some(format!("Failed to open archive: {}", e));
                self.session.loading_entry_id = None;
                return;
            }
        };

        // Create query block immediately with metadata
        let mut query_block = QueryBlock::new(archive.sql.clone());
        query_block.running = false;
        query_block.cache_id = Some(entry.id.clone());
        query_block.error = entry.error.clone();

        // Parse profile events (usually small, do synchronously)
        if !archive.profile.is_empty() {
            let content = String::from_utf8_lossy(&archive.profile);
            for line in content.lines() {
                if let Ok(json) = serde_json::from_str(line) {
                    query_block.add_profile_event(json);
                }
            }
        }

        // Parse logs (usually small, do synchronously)
        if !archive.logs.is_empty() {
            let content = String::from_utf8_lossy(&archive.logs);
            for line in content.lines() {
                if let Ok(json) = serde_json::from_str(line) {
                    query_block.add_log(json);
                }
            }
        }

        // Parse profile_info (final stats from server)
        if !archive.profile_info.is_empty() {
            let content = String::from_utf8_lossy(&archive.profile_info);
            if let Some(line) = content.lines().next() {
                if let Ok(json) = serde_json::from_str(line) {
                    query_block.stats.set_final_stats(json);
                }
            }
        }

        // Set block immediately so UI shows query structure
        self.session.current_block = Some(query_block);
        self.session.loading_entry_id = None;

        // Restore cached view state if available
        if let Some(state) = self.view_state_cache.get(&entry.id) {
            if let Some(block) = self.session.current_block.as_mut() {
                block.apply_view_state(state);
            }
        }

        // Spawn background task to stream result rows
        if !archive.results.is_empty() {
            let cache_id = entry.id.clone();
            let results_data = archive.results;
            let event_tx = self.event_tx.clone();

            tokio::spawn(async move {
                let cursor = Cursor::new(results_data);
                let mut file_reader = FileStreamReader::<NativeFormat, _>::new(
                    cursor,
                    CompressionMethod::LZ4,
                    Default::default(),
                );

                loop {
                    match file_reader.next().await {
                        Ok(Some(mut block)) => {
                            for row in block.take_iter_rows() {
                                let _ = event_tx
                                    .send(AppEvent::ArchiveRowLoaded {
                                        cache_id: cache_id.clone(),
                                        row:      row_to_json(row),
                                    })
                                    .await;
                            }
                        }
                        Ok(None) => {
                            let _ = event_tx.send(AppEvent::ArchiveLoadComplete).await;
                            break;
                        }
                        Err(e) => {
                            let _ = event_tx
                                .send(AppEvent::ArchiveLoadError {
                                    cache_id: cache_id.clone(),
                                    error:    e.to_string(),
                                })
                                .await;
                            break;
                        }
                    }
                }
            });
        }
    }
}

/// ClickHouse keywords that should start on their own line
const CLICKHOUSE_NEWLINE_KEYWORDS: &[&str] =
    &["PREWHERE", "GLOBAL", "FINAL", "SAMPLE", "ARRAY JOIN"];

/// Apply ClickHouse-specific formatting after sqlformat
fn format_clickhouse(sql: &str) -> String {
    let mut result = sql.to_string();

    // Put certain keywords on their own line
    for kw in CLICKHOUSE_NEWLINE_KEYWORDS {
        let pattern = format!(" {} ", kw);
        let replacement = format!("\n{} ", kw);
        result = result.replace(&pattern, &replacement);
    }

    // SETTINGS gets special treatment: each setting on its own indented line
    if let Some(pos) = result.find(" SETTINGS ") {
        let (before, rest) = result.split_at(pos);
        let settings_part = &rest[10..]; // skip " SETTINGS "

        let settings: Vec<&str> = settings_part.split(',').map(|s| s.trim()).collect();
        let formatted_settings = settings.join(",\n  ");

        result = format!("{}\nSETTINGS\n  {}", before, formatted_settings);
    }

    result
}
