mod app;
mod backend;
mod history;
mod session;
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

pub async fn run_tui(client: Client<NativeFormat>) -> Result<()> {
    // Create channels
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let (event_tx, event_rx) = mpsc::channel(1024);

    // Spawn backend task
    let _backend = backend::spawn_backend(client, cmd_rx, event_tx);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(cmd_tx, event_rx)?;
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
