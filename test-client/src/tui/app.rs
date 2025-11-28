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
use crate::tui::session::{Focus, Mode, QueryBlock, Session, SidebarSection, SubPane};
use crate::tui::ui::render;
use crate::tui::widgets::table::ResultsViewMode;

#[derive(Debug, Clone)]
pub enum AppEvent {
    QueryStarted { query_id: usize },
    QueryComplete { query_id: usize },
    QueryError { query_id: usize, error: String },
    RowReceived { query_id: usize, row: serde_json::Value },
    ProfileEvent { query_id: usize, event: serde_json::Value },
    LogEvent { query_id: usize, log: serde_json::Value },
    ProgressEvent { query_id: usize, progress: serde_json::Value },
    QueryCached { query_id: usize, entry: QueryStoreEntry },
    // Connection events
    ConnectionLost { error: String },
    Reconnecting { attempt: u32 },
    Reconnected,
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
    /// Maps query_id -> block_index for event routing
    query_map:       HashMap<usize, usize>,
    next_query_id:   usize,
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
            query_map: HashMap::new(),
            next_query_id: 0,
        })
    }

    pub async fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()> {
        loop {
            self.session.clear_expired_toast();

            // Poll for completed zoomed value stats computations
            if let Some(block) = self.session.selected_block_mut()
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
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.session.focus = Focus::NewQuery;
                self.session.mode = Mode::Edit;
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
                    KeyCode::Tab => {
                        // Switch between Session and Persisted sections
                        self.session.sidebar_toggle_section();
                    }
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                        // Enter selected item
                        match self.session.sidebar_section {
                            SidebarSection::Session => {
                                if self.session.sidebar_enter() {
                                    // sidebar_enter returned true = focus results pane
                                    self.session.focus = Focus::SubPane(SubPane::Results);
                                }
                            }
                            SidebarSection::Persisted => {
                                // Load persisted query from archive
                                if let Some(entry) =
                                    self.session.selected_persisted_entry().cloned()
                                {
                                    self.load_persisted_query(&entry).await;
                                }
                            }
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
            // If escaping from new query, return to sidebar or selected block
            if matches!(self.session.focus, Focus::NewQuery) {
                if self.session.selected_block.is_some() {
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
                self.handle_subpane_key(key, pane).await?;
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
                // Get original SQL for history before execute_new_query clears the editor
                let original_sql = self.session.new_query.lines().join("\n");

                if let Some(statements) = self.session.execute_new_query() {
                    // Save to history
                    let _ = self.history.add(original_sql, None);
                    self.history.reset_nav();

                    // Execute each statement with a unique query_id
                    // For now, execute all in parallel (TODO: sequential with stop on error)
                    for (block_idx, sql) in statements {
                        let query_id = self.next_query_id;
                        self.next_query_id += 1;
                        self.query_map.insert(query_id, block_idx);

                        // Mark statement as running
                        if let Some(block) = self.session.get_block_mut(block_idx) {
                            block.running = true;
                        }

                        let cmd = QueryCommand::Execute { query_id, sql };
                        let _ = self.cmd_tx.send(cmd).await;
                    }
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
        // Cancel currently selected block if running
        if let Some(block_idx) = self.session.selected_block
            && let Some(block) = self.session.get_block_mut(block_idx)
            && block.running
            && !block.cancel_requested
        {
            block.cancel_requested = true;
            // Find the query_id for this block
            if let Some((&query_id, _)) = self.query_map.iter().find(|&(_, &idx)| idx == block_idx)
            {
                let _ = self.cmd_tx.send(QueryCommand::Cancel { query_id }).await;
            }
        }
    }

    async fn handle_subpane_key(&mut self, key: KeyEvent, pane: SubPane) -> Result<()> {
        // Handle copy to clipboard (needs special handling due to borrow checker)
        if pane == SubPane::Results && key.code == KeyCode::Char('y') {
            let content = if let Some(block) = self.session.selected_block()
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
                        self.session.show_toast(format!("Copy failed: {}", e));
                    }
                    Err(e) => {
                        self.session.show_toast(format!("Copy error: {}", e));
                    }
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

    /// Look up block_index from query_id
    fn lookup_query(&self, query_id: usize) -> Option<usize> {
        self.query_map.get(&query_id).copied()
    }

    pub fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::QueryStarted { query_id } => {
                // Block already created by execute_new_query
                if let Some(block_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_block_mut(block_idx) {
                        block.running = true;
                    }
                }
            }
            AppEvent::QueryComplete { query_id } => {
                if let Some(block_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_block_mut(block_idx) {
                        block.running = false;
                        block.cancel_requested = false;
                    }
                }
            }
            AppEvent::QueryError { query_id, error } => {
                if let Some(block_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_block_mut(block_idx) {
                        block.error = Some(error);
                        block.running = false;
                        block.cancel_requested = false;
                    }
                }
            }
            AppEvent::RowReceived { query_id, row } => {
                if let Some(block_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_block_mut(block_idx) {
                        block.add_result_row(row);
                    }
                }
            }
            AppEvent::ProfileEvent { query_id, event } => {
                if let Some(block_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_block_mut(block_idx) {
                        block.add_profile_event(event);
                    }
                }
            }
            AppEvent::LogEvent { query_id, log } => {
                if let Some(block_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_block_mut(block_idx) {
                        block.add_log(log);
                    }
                }
            }
            AppEvent::ProgressEvent { query_id, progress } => {
                if let Some(block_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_block_mut(block_idx) {
                        block.add_progress(progress);
                    }
                }
            }
            AppEvent::QueryCached { query_id, entry } => {
                // Store the cache entry reference in the block
                if let Some(block_idx) = self.lookup_query(query_id) {
                    if let Some(block) = self.session.get_block_mut(block_idx) {
                        block.cache_id = Some(entry.id.clone());
                    }
                }
                // Add to persisted queries list
                self.session.add_persisted_query(entry);
            }
            AppEvent::ConnectionLost { error } => {
                let truncated: String = error.chars().take(50).collect();
                self.session.show_toast(format!("Connection lost: {}", truncated));
            }
            AppEvent::Reconnecting { attempt } => {
                // Only show toast on first attempt to avoid spam
                if attempt == 1 {
                    self.session.show_toast("Reconnecting...");
                }
            }
            AppEvent::Reconnected => {
                self.session.show_toast("Reconnected");
            }
        }
    }

    async fn load_persisted_query(&mut self, entry: &QueryStoreEntry) {
        // Get archive path
        let base_path = QueryStore::find_chc_dir();
        let archive_path = base_path.join(format!("{}.chc", entry.id));

        // Open archive (sync - blocking)
        let archive = match QueryArchiveReader::open(&archive_path) {
            Ok(a) => a,
            Err(e) => {
                self.session.show_toast(format!("Failed to open: {}", e));
                return;
            }
        };

        // Create a new QueryBlock for the loaded query
        let block_idx = self.session.blocks.len();
        let mut query_block = QueryBlock::new(archive.sql.clone());
        query_block.running = false;
        query_block.cache_id = Some(entry.id.clone());

        // Parse native results back to JSON rows
        if !archive.results.is_empty() {
            let cursor = Cursor::new(archive.results);
            let mut file_reader = FileStreamReader::<NativeFormat, _>::new(
                cursor,
                CompressionMethod::LZ4,
                Default::default(),
            );

            match file_reader.read_all().await {
                Ok(blocks) => {
                    for mut block in blocks {
                        for row in block.take_iter_rows() {
                            query_block.add_result_row(row_to_json(row));
                        }
                    }
                }
                Err(e) => {
                    self.session.show_toast(format!("Failed to parse results: {}", e));
                }
            }
        }

        // Parse profile events from JSONL
        if !archive.profile.is_empty() {
            let content = String::from_utf8_lossy(&archive.profile);
            for line in content.lines() {
                if let Ok(json) = serde_json::from_str(line) {
                    query_block.add_profile_event(json);
                }
            }
        }

        // Parse logs from JSONL
        if !archive.logs.is_empty() {
            let content = String::from_utf8_lossy(&archive.logs);
            for line in content.lines() {
                if let Ok(json) = serde_json::from_str(line) {
                    query_block.add_log(json);
                }
            }
        }

        // Add to session and select
        self.session.blocks.push(query_block);
        self.session.selected_block = Some(block_idx);
        self.session.sidebar_section = SidebarSection::Session;
        self.session.focus = Focus::SubPane(SubPane::Results);
    }
}
