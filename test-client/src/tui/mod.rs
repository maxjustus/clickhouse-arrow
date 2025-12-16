mod app;
mod backend;
mod history;
pub mod query_store;
mod session;
mod sql_split;
mod ui;
mod widgets;

use std::io;

use anyhow::Result;
pub use app::App;
use clickhouse_arrow::{Client, NativeFormat};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use crate::client::ConnectionParams;

pub async fn run_tui(client: Client<NativeFormat>, params: ConnectionParams) -> Result<()> {
    // Create channels
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let (event_tx, event_rx) = mpsc::channel(1024);

    // Clone event_tx for App (backend also needs one)
    let app_event_tx = event_tx.clone();

    // Spawn backend task
    let _backend = backend::spawn_backend(client, params, cmd_rx, event_tx);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(cmd_tx, app_event_tx, event_rx)?;

    // Load persisted queries from index
    if let Ok(store) = query_store::QueryStore::load().await {
        app.session.load_history(store.entries());
        app.load_selected_entry().await;
    }

    let res = app.run(&mut terminal).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    terminal.show_cursor()?;

    res
}
