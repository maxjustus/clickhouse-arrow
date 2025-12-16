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
    /// A sub-query within a multi-query cell has started
    QueryStarted {
        query_id: usize,
        sub_idx:  usize,
    },
    /// A sub-query has completed successfully
    QueryComplete {
        query_id: usize,
        sub_idx:  usize,
    },
    /// A sub-query has failed
    QueryError {
        query_id: usize,
        sub_idx:  usize,
        error:    String,
    },
    /// A result row was received for a sub-query
    RowReceived {
        query_id: usize,
        sub_idx:  usize,
        row:      serde_json::Value,
    },
    /// A profile event for a sub-query
    ProfileEvent {
        query_id: usize,
        sub_idx:  usize,
        event:    serde_json::Value,
    },
    /// Final profile info for a sub-query
    ProfileInfoEvent {
        query_id:     usize,
        sub_idx:      usize,
        profile_info: serde_json::Value,
    },
    /// Log message (shared across all sub-queries in a cell)
    LogEvent {
        query_id: usize,
        log:      serde_json::Value,
    },
    /// Progress update for a sub-query
    ProgressEvent {
        query_id: usize,
        sub_idx:  usize,
        progress: serde_json::Value,
    },
    /// The entire query cell has been cached
    QueryCached {
        query_id: usize,
        entry:    QueryStoreEntry,
    },
    // Connection events
    ConnectionLost {
        error: String,
    },
    Reconnecting {
        attempt: u32,
    },
    Reconnected,
    // Archive loading events
    ArchiveRowLoaded {
        cache_id: String,
        sub_idx:  usize,
        row:      serde_json::Value,
    },
    ArchiveLoadComplete,
    ArchiveLoadError {
        cache_id: String,
        error:    String,
    },
}

#[derive(Debug, Clone)]
pub enum QueryCommand {
    /// Execute one or more SQL statements sequentially
    Execute { query_id: usize, statements: Vec<String> },
    /// Cancel a running query cell
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
                && let Some(table) = block.results_mut()
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
                        if matches!(self.session.focus, Focus::QueryEditor) {
                            // Already editing - normal paste at cursor
                            self.session.new_query.insert_str(&text);
                        } else {
                            // From other views - format and replace
                            let formatted = sqlformat::format(
                                &text,
                                &sqlformat::QueryParams::None,
                                &sqlformat::FormatOptions::default(),
                            );
                            let formatted = format_clickhouse(&formatted);
                            self.set_new_query_text(&formatted);
                            self.session.focus = Focus::QueryEditor;
                        }
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
        // 'n' opens query editor (empty)
        // Shift+N opens editor pre-populated with current query's SQL
        // Skip if history search is active (let search handle the key)
        if self.session.focus != Focus::QueryEditor
            && !self.session.history_search_active
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('n') | KeyCode::Char('N'))
        {
            self.show_help = false;

            // Clear editor first
            self.session.new_query = tui_textarea::TextArea::default();
            self.session
                .new_query
                .set_placeholder_text("Enter SQL query... (Cmd+Enter to execute)");

            // Shift variant: pre-populate with current query's SQL
            if key.modifiers.contains(KeyModifiers::SHIFT)
                && let Some(sql) = self.session.displayed_block().map(|b| b.sql().to_string())
            {
                self.set_new_query_text(&sql);
            }

            // Save where we came from and open editor
            self.session.previous_focus = Some(self.session.focus.clone());
            self.session.focus = Focus::QueryEditor;
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
            KeyCode::Char('?') if !self.session.history_search_active => {
                self.show_help = true;
                return Ok(());
            }
            // Ctrl+P: Previous query card (global, but not in query editor)
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.session.focus != Focus::QueryEditor {
                    self.save_current_view_state();
                    if self.session.card_prev() {
                        self.load_selected_entry().await;
                    }
                    return Ok(());
                }
                // Fall through to editor handler
            }
            // Ctrl+N: Next query card (global, but not in query editor)
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.session.focus != Focus::QueryEditor {
                    self.save_current_view_state();
                    if self.session.card_next() {
                        self.load_selected_entry().await;
                    }
                    return Ok(());
                }
                // Fall through to editor handler
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

        // Query editor has its own key handling
        if self.session.focus == Focus::QueryEditor {
            return self.handle_query_editor_key(key).await;
        }

        match self.session.mode {
            Mode::Navigation => self.handle_navigation_key(key).await,
            Mode::Edit => self.handle_edit_key(key).await,
        }
    }

    /// Handle keys in the query editor modal
    async fn handle_query_editor_key(&mut self, key: KeyEvent) -> Result<()> {
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
            // Escape closes the editor and returns to previous focus
            (KeyCode::Esc, _, _) => {
                self.session.focus =
                    self.session.previous_focus.take().unwrap_or(Focus::HistoryView);
                self.history.reset_nav();
            }
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
                let sql_preview: String = sql.chars().take(500).collect();

                // Split SQL into statements for multi-query support
                let statements = crate::tui::sql_split::split_statements(&sql);

                let entry = QueryStoreEntry {
                    id: id.clone(),
                    hash,
                    sql_preview,
                    timestamp,
                    duration_ms: None,
                    row_count: 0,
                    error: None,
                    rows_read: None,
                    bytes_read: None,
                    peak_memory: None,
                    sub_query_count: statements.len(),
                };

                // Insert entry at end of history (newest last)
                let hist_idx = self.session.add_history_entry(entry);

                // Create QueryBlock for execution (multi-query if needed)
                let mut block = if statements.len() > 1 {
                    QueryBlock::new_multi(statements.clone())
                } else {
                    QueryBlock::new(sql.clone())
                };
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
                    .set_placeholder_text("Enter SQL query... (Cmd+Enter to execute)");

                // Send execute command
                let query_id = self.next_query_id;
                self.next_query_id += 1;
                self.query_map.insert(query_id, hist_idx);

                let cmd = QueryCommand::Execute { query_id, statements };
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

    async fn handle_navigation_key(&mut self, key: KeyEvent) -> Result<()> {
        match &self.session.focus {
            Focus::QueryEditor => {
                // QueryEditor has its own key handler - this is a safety net
                return Ok(());
            }
            Focus::HistoryView => {
                // Search mode input handling
                if self.session.history_search_active {
                    match key.code {
                        KeyCode::Esc => {
                            self.session.history_search_active = false;
                            self.session.history_search_pattern.clear();
                        }
                        KeyCode::Enter => {
                            // Exit search but keep filter
                            self.session.history_search_active = false;
                        }
                        KeyCode::Backspace => {
                            if self.session.history_search_pattern.is_empty() {
                                self.session.history_search_active = false;
                            } else {
                                self.session.history_search_pattern.pop();
                                self.session.history_scroll_offset = 0;
                                // Always select newest match when filter changes
                                let filtered = self.session.filtered_history_indices();
                                self.session.selected_card = filtered.last().copied();
                            }
                        }
                        KeyCode::Down => {
                            self.save_current_view_state();
                            if self.session.card_next() {
                                self.load_selected_entry().await;
                            }
                        }
                        KeyCode::Up => {
                            self.save_current_view_state();
                            if self.session.card_prev() {
                                self.load_selected_entry().await;
                            }
                        }
                        KeyCode::Char(c) => {
                            self.session.history_search_pattern.push(c);
                            self.session.history_scroll_offset = 0;
                            // Always select newest match when typing
                            let filtered = self.session.filtered_history_indices();
                            self.session.selected_card = filtered.last().copied();
                        }
                        _ => {}
                    }
                    return Ok(());
                }

                match key.code {
                    KeyCode::Char('/') => {
                        // Resume editing existing filter, don't clear
                        self.session.history_search_active = true;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.save_current_view_state();
                        if self.session.card_next() {
                            self.load_selected_entry().await;
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.save_current_view_state();
                        if self.session.card_prev() {
                            self.load_selected_entry().await;
                        }
                    }
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                        // Enter full results view for selected card
                        if self.session.selected_card.is_some()
                            && self.session.displayed_block().is_some()
                        {
                            self.session.focus = Focus::SubPane(SubPane::Results);
                        }
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
                    // Multi-query navigation: [ and ] for prev/next, 1-9 for direct jump
                    KeyCode::Char('[') => {
                        if let Some(block) = self.session.displayed_block_mut() {
                            block.prev_query();
                        }
                    }
                    KeyCode::Char(']') => {
                        if let Some(block) = self.session.displayed_block_mut() {
                            block.next_query();
                        }
                    }
                    KeyCode::Char(c @ '1'..='9') => {
                        if let Some(block) = self.session.displayed_block_mut() {
                            let n = c.to_digit(10).unwrap_or(1) as usize;
                            block.select_query(n.saturating_sub(1));
                        }
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
                                    .results()
                                    .map(|t| !matches!(t.view_mode, ResultsViewMode::Table))
                                    .unwrap_or(false),
                                SubPane::Stats => {
                                    !matches!(block.stats().view_mode, MetricsViewMode::Table)
                                }
                                SubPane::Logs => {
                                    !matches!(block.logs_data.view_mode, LogsViewMode::Sources)
                                }
                                _ => false,
                            };
                            if in_detail {
                                match pane {
                                    SubPane::Results => {
                                        if let Some(t) = block.results_mut() {
                                            t.prev_detail_row();
                                        }
                                    }
                                    SubPane::Stats => block.stats_mut().prev_detail_row(),
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
                                    .results()
                                    .map(|t| !matches!(t.view_mode, ResultsViewMode::Table))
                                    .unwrap_or(false),
                                SubPane::Stats => {
                                    !matches!(block.stats().view_mode, MetricsViewMode::Table)
                                }
                                SubPane::Logs => {
                                    !matches!(block.logs_data.view_mode, LogsViewMode::Sources)
                                }
                                _ => false,
                            };
                            if in_detail {
                                match pane {
                                    SubPane::Results => {
                                        if let Some(t) = block.results_mut() {
                                            t.next_detail_row();
                                        }
                                    }
                                    SubPane::Stats => block.stats_mut().next_detail_row(),
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
            Focus::QueryEditor => {
                // QueryEditor has its own key handler - this is a safety net
                // Edit mode is always active in QueryEditor
            }
            Focus::HistoryView => {
                // Edit mode not meaningful in history view (no input here)
                self.session.mode = Mode::Navigation;
            }
            Focus::SubPane(pane) => {
                let pane = *pane;
                self.handle_subpane_key(key, pane).await?;
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
                && let Some(table) = block.results()
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

        // Handle fullscreen exit before block borrow to avoid borrow conflict
        if matches!(pane, SubPane::Results)
            && matches!(key.code, KeyCode::Left | KeyCode::Char('h'))
            && !key.modifiers.contains(KeyModifiers::ALT)
            && self.session.fullscreen
        {
            self.session.fullscreen = false;
            return Ok(());
        }

        let block = match self.session.displayed_block_mut() {
            Some(b) => b,
            None => return Ok(()),
        };

        match pane {
            SubPane::Sql => match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    block.set_sql_scroll(block.sql_scroll() + 1);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    block.set_sql_scroll(block.sql_scroll().saturating_sub(1));
                }
                KeyCode::PageDown => {
                    block.set_sql_scroll(block.sql_scroll() + 10);
                }
                KeyCode::PageUp => {
                    block.set_sql_scroll(block.sql_scroll().saturating_sub(10));
                }
                KeyCode::Left | KeyCode::Char('h') => {
                    self.session.mode = Mode::Navigation;
                }
                _ => {}
            },
            SubPane::Results => {
                // Handle error scrolling if error is displayed instead of results
                if block.error().is_some() {
                    match key.code {
                        KeyCode::Down | KeyCode::Char('j') => block.error_scroll += 1,
                        KeyCode::Up | KeyCode::Char('k') => {
                            block.error_scroll = block.error_scroll.saturating_sub(1);
                        }
                        KeyCode::PageDown => block.error_scroll += 10,
                        KeyCode::PageUp => {
                            block.error_scroll = block.error_scroll.saturating_sub(10);
                        }
                        KeyCode::Home => block.error_scroll = 0,
                        KeyCode::Left | KeyCode::Char('h') => {
                            self.session.mode = Mode::Navigation;
                        }
                        _ => {}
                    }
                    return Ok(());
                }
                if let Some(table) = block.results_mut() {
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
                                // Check if at row 0 in Table mode (not in detail pane) - focus
                                // header
                                if matches!(table.view_mode, ResultsViewMode::Table)
                                    && table.selected_row == 0
                                    && !table.detail_focused
                                {
                                    table.focus_header();
                                } else {
                                    table.nav_up();
                                }
                            }
                            (KeyCode::Right | KeyCode::Char('l'), false) => {
                                if !table.expand() && !self.session.fullscreen {
                                    // Can't expand further in table, go fullscreen
                                    self.session.fullscreen = true;
                                }
                            }
                            (KeyCode::Left | KeyCode::Char('h'), false) => {
                                // Fullscreen exit handled above before block borrow
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
                            (KeyCode::PageDown, _) => {
                                if table.detail_focused {
                                    table.page_down_detail();
                                } else {
                                    table.page_down();
                                }
                            }
                            (KeyCode::PageUp, _) => {
                                if table.detail_focused {
                                    table.page_up_detail();
                                } else {
                                    table.page_up();
                                }
                            }
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
                    KeyCode::Down | KeyCode::Char('j') => block.stats_mut().nav_down(),
                    KeyCode::Up | KeyCode::Char('k') => block.stats_mut().nav_up(),
                    KeyCode::PageDown => block.stats_mut().page_down(),
                    KeyCode::PageUp => block.stats_mut().page_up(),
                    KeyCode::Right | KeyCode::Char('l') => {
                        block.stats_mut().expand();
                    }
                    KeyCode::Left | KeyCode::Char('h') => {
                        if !block.stats_mut().collapse() {
                            // At table level, exit edit mode
                            self.session.mode = Mode::Navigation;
                        }
                    }
                    // Row navigation ([ / ] or Shift+K / Shift+J)
                    KeyCode::Char('[') | KeyCode::Char('K') => block.stats_mut().prev_detail_row(),
                    KeyCode::Char(']') | KeyCode::Char('J') => block.stats_mut().next_detail_row(),
                    KeyCode::Char('{') => block.stats_mut().prev_detail_row_jump(ROW_JUMP_COUNT),
                    KeyCode::Char('}') => block.stats_mut().next_detail_row_jump(ROW_JUMP_COUNT),
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
            AppEvent::QueryStarted { query_id, sub_idx } => {
                if let Some(hist_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_running_mut(hist_idx) {
                        block.running = true;
                        // Update sub-query status
                        if let Some(sq) = block.queries.get_mut(sub_idx) {
                            sq.status = crate::tui::session::QueryStatus::Running;
                        }
                    }
                }
            }
            AppEvent::QueryComplete { query_id, sub_idx } => {
                if let Some(hist_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_running_mut(hist_idx) {
                        // Update sub-query status based on whether cancellation was requested
                        if let Some(sq) = block.queries.get_mut(sub_idx) {
                            if block.cancel_requested {
                                sq.status = crate::tui::session::QueryStatus::Cancelled;
                            } else {
                                sq.status = crate::tui::session::QueryStatus::Completed;
                            }
                        }
                        // Check if all sub-queries are done
                        let all_done = block.queries.iter().all(|sq| {
                            matches!(
                                sq.status,
                                crate::tui::session::QueryStatus::Completed
                                    | crate::tui::session::QueryStatus::Failed
                                    | crate::tui::session::QueryStatus::Cancelled
                            )
                        });
                        if all_done {
                            block.running = false;
                            block.cancel_requested = false;
                        }
                    }
                }
            }
            AppEvent::QueryError { query_id, sub_idx, error } => {
                if let Some(hist_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_running_mut(hist_idx) {
                        block.set_error(sub_idx, Some(error.clone()));
                        block.running = false;
                        block.cancel_requested = false;
                    }

                    // Update history entry with error
                    if let Some(entry) = self.session.history.get_mut(hist_idx) {
                        entry.error = Some(error);
                    }
                }
            }
            AppEvent::RowReceived { query_id, sub_idx, row } => {
                if let Some(hist_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_running_mut(hist_idx) {
                        block.add_result_row(row, sub_idx);
                    }
                }
            }
            AppEvent::ProfileEvent { query_id, sub_idx, event } => {
                if let Some(hist_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_running_mut(hist_idx) {
                        block.add_profile_event(event, sub_idx);
                    }
                }
            }
            AppEvent::ProfileInfoEvent { query_id, sub_idx, profile_info } => {
                if let Some(hist_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_running_mut(hist_idx) {
                        block.set_final_stats(profile_info, sub_idx);
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
            AppEvent::ProgressEvent { query_id, sub_idx, progress } => {
                if let Some(hist_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_running_mut(hist_idx) {
                        block.add_progress(progress, sub_idx);
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
            AppEvent::ArchiveRowLoaded { cache_id, sub_idx, row } => {
                // Add row to current block if it matches the cache_id
                if let Some(block) = self.session.current_block.as_mut() {
                    if block.cache_id.as_ref() == Some(&cache_id) {
                        block.add_result_row(row, sub_idx);
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
    pub async fn load_selected_entry(&mut self) {
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

        // Detect multi-query format by checking sub_query_count
        let sub_query_count = if entry.sub_query_count > 1 {
            entry.sub_query_count
        } else {
            // Legacy entries: detect from archive structure
            match QueryArchiveReader::sub_query_count(&archive_path) {
                Ok(count) => count,
                Err(e) => {
                    self.session.app_error = Some(format!("Failed to open archive: {}", e));
                    self.session.loading_entry_id = None;
                    return;
                }
            }
        };

        if sub_query_count > 1 {
            self.load_archive_multi(entry, &archive_path, sub_query_count).await;
        } else {
            self.load_archive_single(entry, &archive_path).await;
        }
    }

    /// Load a single-query archive (backward compatible format)
    async fn load_archive_single(
        &mut self,
        entry: &QueryStoreEntry,
        archive_path: &std::path::Path,
    ) {
        let archive = match QueryArchiveReader::open(&archive_path.to_path_buf()) {
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
        query_block.set_error(0, entry.error.clone());

        // Parse profile events (usually small, do synchronously)
        if !archive.profile.is_empty() {
            let content = String::from_utf8_lossy(&archive.profile);
            for line in content.lines() {
                if let Ok(json) = serde_json::from_str(line) {
                    query_block.add_profile_event(json, 0);
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
                    query_block.set_final_stats(json, 0);
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
                                        sub_idx:  0,
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

    /// Load a multi-query archive (numbered subdirectories)
    async fn load_archive_multi(
        &mut self,
        entry: &QueryStoreEntry,
        archive_path: &std::path::Path,
        sub_query_count: usize,
    ) {
        // Collect SQL statements from each sub-archive
        let mut statements = Vec::with_capacity(sub_query_count);
        let mut sub_archives = Vec::with_capacity(sub_query_count);

        for sub_idx in 0..sub_query_count {
            match QueryArchiveReader::open_multi(&archive_path.to_path_buf(), sub_idx) {
                Ok(archive) => {
                    statements.push(archive.sql.clone());
                    sub_archives.push(archive);
                }
                Err(e) => {
                    self.session.app_error =
                        Some(format!("Failed to open sub-archive {}: {}", sub_idx, e));
                    self.session.loading_entry_id = None;
                    return;
                }
            }
        }

        // Create multi-query block
        let mut query_block = QueryBlock::new_multi(statements);
        query_block.running = false;
        query_block.cache_id = Some(entry.id.clone());
        query_block.set_error(0, entry.error.clone());

        // Parse profile events and stats for each sub-query
        for (sub_idx, archive) in sub_archives.iter().enumerate() {
            // Parse profile events
            if !archive.profile.is_empty() {
                let content = String::from_utf8_lossy(&archive.profile);
                for line in content.lines() {
                    if let Ok(json) = serde_json::from_str(line) {
                        query_block.add_profile_event(json, sub_idx);
                    }
                }
            }

            // Parse profile_info (final stats)
            if !archive.profile_info.is_empty() {
                let content = String::from_utf8_lossy(&archive.profile_info);
                if let Some(line) = content.lines().next() {
                    if let Ok(json) = serde_json::from_str(line) {
                        query_block.set_final_stats(json, sub_idx);
                    }
                }
            }
        }

        // Parse shared logs from first archive (logs are at root level, shared)
        if let Some(first_archive) = sub_archives.first() {
            if !first_archive.logs.is_empty() {
                let content = String::from_utf8_lossy(&first_archive.logs);
                for line in content.lines() {
                    if let Ok(json) = serde_json::from_str(line) {
                        query_block.add_log(json);
                    }
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

        // Spawn background tasks to stream result rows for each sub-query
        for (sub_idx, archive) in sub_archives.into_iter().enumerate() {
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
                                            sub_idx,
                                            row: row_to_json(row),
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
