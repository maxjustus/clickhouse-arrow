use std::collections::VecDeque;

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Chart, Clear, Dataset, GraphType, LineGauge, Paragraph, Row, Table, Wrap,
};
use ratatui::{Frame, symbols};

use crate::tui::app::App;
use crate::tui::session::{
    Focus, LogsViewMode, MetricsViewMode, Mode, QueryBlock, QueryStatus, SubPane,
};
use crate::tui::widgets::table::{
    NumericStats, PathStats, PathStatsState, PathValueType, ResultsViewMode, SortableTable,
    UniqueSample,
};

pub fn render(f: &mut Frame, app: &mut App) {
    if app.show_help {
        render_help(f);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(f.area());

    render_session(f, chunks[0], app);
    render_status_bar(f, chunks[1], app);

    // Render toast if present (on top of everything)
    if let Some((msg, _)) = &app.session.toast {
        const MAX_TOAST_WIDTH: u16 = 60;
        const MAX_TOAST_HEIGHT: u16 = 5; // 3 text lines + 2 borders

        let max_width = MAX_TOAST_WIDTH.min(f.area().width.saturating_sub(4));
        let text_width = max_width.saturating_sub(2); // Account for borders

        // Estimate lines needed for wrapped text
        let estimated_lines = ((msg.len() as u16 + text_width - 1) / text_width.max(1)).max(1);
        let toast_height = (estimated_lines + 2).min(MAX_TOAST_HEIGHT);

        let toast_area = Rect {
            x:      f.area().width.saturating_sub(max_width + 2),
            y:      1,
            width:  max_width,
            height: toast_height,
        };
        let toast = Paragraph::new(msg.as_str())
            .block(Block::default().borders(Borders::ALL))
            .wrap(Wrap { trim: false })
            .alignment(Alignment::Center);
        f.render_widget(Clear, toast_area);
        f.render_widget(toast, toast_area);
    }

    // Render column stats modal overlay (on top of content, below toasts)
    render_column_stats_modal_overlay(f, chunks[0], app);

    // Render app-level error modal (on top of everything, below toasts)
    if let Some(ref error) = app.session.app_error {
        const MAX_ERROR_WIDTH: u16 = 100;
        const MAX_ERROR_HEIGHT: u16 = 20;

        let error_width = MAX_ERROR_WIDTH.min(f.area().width.saturating_sub(4));
        let error_height = MAX_ERROR_HEIGHT.min(f.area().height.saturating_sub(4));

        // Center the error modal
        let error_area = centered_rect(
            (error_width * 100 / f.area().width).min(95), // percentage
            (error_height * 100 / f.area().height).min(90),
            f.area(),
        );

        let error_block = Block::default()
            .borders(Borders::ALL)
            .title("Error (Press any key to dismiss)")
            .border_style(Style::default().fg(Color::Red));

        let para = Paragraph::new(error.as_str())
            .block(error_block)
            .style(Style::default().fg(Color::Red))
            .wrap(Wrap { trim: false })
            .scroll((0, 0));

        f.render_widget(Clear, error_area);
        f.render_widget(para, error_area);
    }
}

fn render_session(f: &mut Frame, area: Rect, app: &mut App) {
    match &app.session.focus {
        Focus::HistoryView => {
            render_history_view(f, area, app);
        }
        Focus::QueryEditor => {
            render_query_editor_fullscreen(f, area, app);
        }
        Focus::SubPane(_) => {
            render_results_fullscreen(f, area, app);
        }
    }
}

/// Render the full-width history view with query cards (no input - input is in modal)
fn render_history_view(f: &mut Frame, area: Rect, app: &mut App) {
    render_history_cards(f, area, app);
}

/// Render the query editor as a full-page view
fn render_query_editor_fullscreen(f: &mut Frame, area: Rect, app: &mut App) {
    // Title changes based on search mode
    let title = if app.history.is_searching() {
        let pattern = app.history.search_pattern();
        let match_pos = app.history.search_match_position();
        let match_count = app.history.search_match_count();
        format!("(reverse-i-search)`{}': {}/{}", pattern, match_pos, match_count)
    } else {
        "New Query (Cmd+Enter to run, Esc to close)".to_string()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Yellow));

    app.session.new_query.set_block(block);
    app.session.new_query.set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));

    f.render_widget(&app.session.new_query, area);
}

/// Calculate card height based on SQL content and available width
fn calc_card_height(sql: &str, width: u16) -> u16 {
    // Inner width = total - 2 (borders)
    let inner_width = width.saturating_sub(2).max(1) as usize;
    // Count wrapped lines (character-based estimation)
    let sql_text = sql.replace('\n', " ");
    let char_count = sql_text.chars().count();
    let wrapped_lines = (char_count / inner_width + 1).max(1);
    // Height = 2 (borders) + sql_lines + 1 (footer)
    // Minimum 4 lines, maximum 20 lines
    (wrapped_lines as u16 + 3).clamp(4, 20)
}

/// Render the scrollable list of query cards with dynamic heights
fn render_history_cards(f: &mut Frame, area: Rect, app: &mut App) {
    let border_style = Style::default().fg(Color::DarkGray);

    // Build title with optional search indicator (yellow when active)
    let title: Line = if app.session.history_search_active {
        Line::from(vec![
            Span::raw("History "),
            Span::styled(
                format!("/ {}_", app.session.history_search_pattern),
                Style::default().fg(Color::Yellow),
            ),
        ])
    } else if !app.session.history_search_pattern.is_empty() {
        Line::from(vec![
            Span::raw("History "),
            Span::styled(
                format!("[{}]", app.session.history_search_pattern),
                Style::default().fg(Color::Yellow),
            ),
        ])
    } else {
        Line::from("History")
    };

    let block = Block::default().borders(Borders::ALL).title(title).border_style(border_style);
    let cards_inner = block.inner(area);
    f.render_widget(block, area);

    // Get filtered indices
    let filtered_indices = app.session.filtered_history_indices();

    if filtered_indices.is_empty() {
        let msg = if app.session.history.is_empty() {
            "No queries yet. Press 'n' to write a new query."
        } else {
            "No matching queries"
        };
        let empty = Paragraph::new(msg)
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center);
        f.render_widget(empty, cards_inner);
        return;
    }

    let total_cards = filtered_indices.len();

    // Calculate heights for filtered cards
    let card_heights: Vec<u16> = filtered_indices
        .iter()
        .map(|&i| calc_card_height(&app.session.history[i].sql_preview, cards_inner.width))
        .collect();

    // Ensure scroll_offset is valid (within filtered list)
    let scroll_offset = &mut app.session.history_scroll_offset;
    *scroll_offset = (*scroll_offset).min(total_cards.saturating_sub(1));

    // Find selected card's position in filtered list
    let selected_filtered_pos =
        app.session.selected_card.and_then(|sel| filtered_indices.iter().position(|&i| i == sel));

    // Adjust scroll_offset to ensure selected card is visible
    if let Some(selected_pos) = selected_filtered_pos {
        // If selected is above viewport, scroll up
        if selected_pos < *scroll_offset {
            *scroll_offset = selected_pos;
        }

        // If selected is below viewport, scroll down until it fits
        loop {
            let mut y = 0u16;
            let mut last_visible_pos = *scroll_offset;
            for (pos, &h) in card_heights.iter().enumerate().skip(*scroll_offset) {
                if y + h > cards_inner.height {
                    break;
                }
                last_visible_pos = pos;
                y += h;
            }

            if selected_pos <= last_visible_pos {
                break; // Selected is visible
            }

            // Scroll down by 1
            if *scroll_offset < total_cards.saturating_sub(1) {
                *scroll_offset += 1;
            } else {
                break; // Can't scroll further
            }
        }
    }

    // Find which cards are visible in current viewport
    let mut visible_cards: Vec<(usize, usize, u16, u16)> = vec![]; // (filtered_pos, history_idx, y_offset, height)
    let mut y = 0u16;
    for (pos, &h) in card_heights.iter().enumerate().skip(*scroll_offset) {
        if y + h > cards_inner.height {
            break;
        }
        visible_cards.push((pos, filtered_indices[pos], y, h));
        y += h;
    }

    // Render visible cards
    for (_, history_idx, y_offset, height) in &visible_cards {
        let entry = &app.session.history[*history_idx];
        let card_area = Rect {
            x:      cards_inner.x,
            y:      cards_inner.y + y_offset,
            width:  cards_inner.width,
            height: *height,
        };

        let is_selected = app.session.selected_card == Some(*history_idx);
        let is_running = app.session.running_queries.contains_key(history_idx);
        let running_block = app.session.running_queries.get(history_idx);

        render_query_card(f, card_area, entry, is_selected, is_running, running_block);
    }

    // Show scroll indicator if there are cards not visible
    let all_visible = visible_cards.len() == total_cards;
    if !all_visible {
        let position = selected_filtered_pos.map(|p| p + 1).unwrap_or(0);
        let indicator = format!(" {}/{} ", position, total_cards);
        let indicator_area = Rect {
            x:      cards_inner.x + cards_inner.width.saturating_sub(indicator.len() as u16 + 1),
            y:      cards_inner.y + cards_inner.height.saturating_sub(1),
            width:  indicator.len() as u16,
            height: 1,
        };
        let indicator_para = Paragraph::new(indicator).style(Style::default().fg(Color::DarkGray));
        f.render_widget(indicator_para, indicator_area);
    }
}

/// Render a single query card
fn render_query_card(
    f: &mut Frame,
    area: Rect,
    entry: &crate::tui::query_store::QueryStoreEntry,
    is_selected: bool,
    is_running: bool,
    running_block: Option<&QueryBlock>,
) {
    let border_style = if is_selected {
        Style::default().fg(Color::Yellow)
    } else if entry.error.is_some() {
        Style::default().fg(Color::Red)
    } else if is_running {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    // Card title: timestamp and duration
    let age = format_relative_time(entry.timestamp);
    let duration = entry
        .duration_ms
        .map(|ms| format!("{}ms", ms))
        .unwrap_or_else(|| if is_running { "running...".to_string() } else { "-".to_string() });

    let indicator = if running_block.map(|b| b.cancel_requested).unwrap_or(false) {
        "x "
    } else if is_running {
        "* "
    } else if entry.error.is_some() {
        "! "
    } else {
        ""
    };

    let title = format!("{}{} | {}", indicator, age, duration);

    let block = Block::default().borders(Borders::ALL).title(title).border_style(border_style);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Card content - use full available height
    let sql_style = if is_selected {
        Style::default().fg(Color::White)
    } else {
        Style::default().fg(Color::Gray)
    };

    // Reserve last line for row count/error footer
    let sql_height = inner.height.saturating_sub(1);
    let sql_area = Rect { height: sql_height, ..inner };
    let footer_area = Rect { y: inner.y + sql_height, height: 1.min(inner.height), ..inner };

    // SQL preview with wrapping to fill available space
    let sql_text = entry.sql_preview.replace('\n', " ");
    let sql_para = Paragraph::new(sql_text).style(sql_style).wrap(Wrap { trim: true });
    f.render_widget(sql_para, sql_area);

    // Footer: error message or stats
    let footer = if let Some(ref error) = entry.error {
        let error_preview: String = error.chars().take(inner.width as usize).collect();
        Span::styled(error_preview, Style::default().fg(Color::Red))
    } else if let Some(block) = running_block {
        // Running query: show live stats
        let stats = block.stats();
        let rows = stats.final_rows_read.unwrap_or(stats.max_rows_read);
        let bytes = stats.final_bytes_read.unwrap_or(stats.max_bytes_read);
        let mem = stats.peak_ram_current;
        Span::styled(
            format!(
                "{} read | {} | {} mem",
                format_number(rows),
                format_bytes(bytes),
                format_bytes(mem)
            ),
            Style::default().fg(Color::DarkGray),
        )
    } else if entry.rows_read.is_some() || entry.bytes_read.is_some() || entry.peak_memory.is_some()
    {
        // Completed query with stats
        let rows = entry.rows_read.unwrap_or(0);
        let bytes = entry.bytes_read.unwrap_or(0);
        let mem = entry.peak_memory.unwrap_or(0);
        Span::styled(
            format!(
                "{} read | {} | {} mem",
                format_number(rows),
                format_bytes(bytes),
                format_bytes(mem)
            ),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        // Fallback for old entries without stats
        Span::styled(format!("{} rows", entry.row_count), Style::default().fg(Color::DarkGray))
    };
    let footer_para = Paragraph::new(Line::from(footer));
    f.render_widget(footer_para, footer_area);
}

/// Render the left sidebar showing query list for multi-query cells
fn render_query_sidebar(f: &mut Frame, area: Rect, block: &QueryBlock) {
    let outer = Block::default()
        .borders(Borders::ALL)
        .title("Queries")
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let active = block.active_query;

    // Build rows for each query
    let rows: Vec<Row> = block
        .queries
        .iter()
        .enumerate()
        .map(|(idx, sub)| {
            // Status indicator
            let status_indicator = match sub.status {
                QueryStatus::Pending => Span::styled(" ", Style::default().fg(Color::DarkGray)),
                QueryStatus::Running => Span::styled(">", Style::default().fg(Color::Blue)),
                QueryStatus::Completed => Span::styled("*", Style::default().fg(Color::Green)),
                QueryStatus::Failed => Span::styled("!", Style::default().fg(Color::Red)),
                QueryStatus::Cancelled => Span::styled("X", Style::default().fg(Color::Yellow)),
            };

            // Truncated SQL preview (first line, max 15 chars)
            let sql_preview: String =
                sub.sql.lines().next().unwrap_or("").chars().take(13).collect();
            let sql_preview =
                if sub.sql.len() > 13 { format!("{}..", sql_preview) } else { sql_preview };

            // Row style based on selection
            let row_style = if idx == active {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            } else {
                Style::default()
            };

            Row::new(vec![
                format!("{}", idx + 1),
                status_indicator.content.to_string(),
                sql_preview,
            ])
            .style(row_style)
        })
        .collect();

    // Widths: index (2), status (1), sql preview (rest)
    let widths = [Constraint::Length(2), Constraint::Length(1), Constraint::Min(0)];

    let table = Table::new(rows, widths);
    f.render_widget(table, inner);
}

/// Render full-screen results view (when viewing a specific query)
fn render_results_fullscreen(f: &mut Frame, area: Rect, app: &mut App) {
    // Check if we're loading
    if app.session.loading_entry_id.is_some() {
        let loading = Paragraph::new("Loading...")
            .style(Style::default().fg(Color::Yellow))
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL).title("Query Results"));
        f.render_widget(loading, area);
        return;
    }

    // Display current block
    let hist_idx = app.session.selected_card.unwrap_or(0);
    let focus = app.session.focus.clone();
    let mode = app.session.mode;
    let fullscreen = app.session.fullscreen;

    if let Some(block) = app.session.displayed_block_mut() {
        // If multi-query cell, split area into sidebar + main content
        if block.is_multi() {
            const SIDEBAR_WIDTH: u16 = 22;
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(0)])
                .split(area);

            render_query_sidebar(f, chunks[0], block);
            render_selected_query(f, chunks[1], block, hist_idx, &focus, mode, fullscreen);
        } else {
            render_selected_query(f, area, block, hist_idx, &focus, mode, fullscreen);
        }
    } else {
        let empty = Paragraph::new("No data. Press Esc to return to history.")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL).title("Query Results"));
        f.render_widget(empty, area);
    }
}

/// Format unix timestamp as relative time (e.g., "2m ago", "1h ago", "3d ago")
fn format_relative_time(timestamp: u64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);

    if timestamp > now {
        return "now".to_string();
    }

    let diff = now - timestamp;

    if diff < 60 {
        format!("{}s ago", diff)
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86400 {
        format!("{}h ago", diff / 3600)
    } else {
        format!("{}d ago", diff / 86400)
    }
}

/// Determine which pane is expanded based on focus and mode
/// In Navigation mode: all panes expanded (unless fullscreen is true)
/// In Edit mode on a specific pane: only that pane expanded
/// With fullscreen flag: current SubPane is expanded regardless of mode
fn focused_pane(focus: &Focus, mode: Mode, fullscreen: bool) -> Option<SubPane> {
    // Fullscreen can be triggered independently via 'f' key
    if fullscreen {
        if let Focus::SubPane(pane) = focus {
            return Some(*pane);
        }
    }
    // Original Edit mode logic still works
    match (focus, mode) {
        (Focus::SubPane(pane), Mode::Edit) => Some(*pane),
        _ => None, // Navigation mode = all expanded
    }
}

fn is_pane_expanded(pane: SubPane, focused: Option<SubPane>) -> bool {
    focused.is_none() || focused == Some(pane)
}

fn render_selected_query(
    f: &mut Frame,
    area: Rect,
    block: &mut QueryBlock,
    block_idx: usize,
    focus: &Focus,
    mode: Mode,
    fullscreen: bool,
) {
    // Determine which pane (if any) is exclusively expanded
    let focused = focused_pane(focus, mode, fullscreen);

    // Title with breadcrumbs in edit mode
    let title = if let Some(pane) = focused {
        let pane_name = match pane {
            SubPane::Sql => "SQL",
            SubPane::Results => "Results",
            SubPane::Stats => "Stats",
            SubPane::Logs => "Logs",
        };
        format!(
            "Query {} > {} {}",
            block_idx + 1,
            pane_name,
            if block.cancel_requested {
                "[cancelling...]"
            } else if block.running {
                "[running...]"
            } else {
                ""
            }
        )
    } else {
        format!(
            "Query {} {}",
            block_idx + 1,
            if block.cancel_requested {
                "[cancelling...]"
            } else if block.running {
                "[running...]"
            } else if block.error().is_some() {
                "[error]"
            } else {
                ""
            }
        )
    };

    let outer = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::White));

    let inner = outer.inner(area);
    f.render_widget(outer, area);

    // Sub-pane layout - in edit mode only focused pane expands, otherwise all expand
    let sub_constraints = vec![
        // SQL
        if is_pane_expanded(SubPane::Sql, focused) {
            if focused == Some(SubPane::Sql) {
                Constraint::Min(0) // Fill all space when exclusively focused
            } else {
                // Dynamic height in navigation mode
                let sql_lines = block.sql().lines().count() as u16 + 2;
                let max_height = inner.height / 4;
                Constraint::Length(sql_lines.min(max_height).max(3))
            }
        } else if fullscreen {
            Constraint::Length(0) // Hidden in fullscreen
        } else {
            Constraint::Length(1)
        },
        // Results
        if is_pane_expanded(SubPane::Results, focused) {
            if focused == Some(SubPane::Results) {
                Constraint::Min(0) // Fill all space when exclusively focused
            } else {
                Constraint::Ratio(2, 4) // 50% in navigation mode
            }
        } else if fullscreen {
            Constraint::Length(0) // Hidden in fullscreen
        } else {
            Constraint::Length(1)
        },
        // Stats
        if is_pane_expanded(SubPane::Stats, focused) {
            if focused == Some(SubPane::Stats) {
                Constraint::Min(0)
            } else {
                Constraint::Ratio(1, 4) // 25% in navigation mode
            }
        } else if fullscreen {
            Constraint::Length(0) // Hidden in fullscreen
        } else {
            Constraint::Length(1)
        },
        // Logs
        if is_pane_expanded(SubPane::Logs, focused) {
            if focused == Some(SubPane::Logs) {
                Constraint::Min(0)
            } else {
                Constraint::Ratio(1, 4) // 25% in navigation mode
            }
        } else if fullscreen {
            Constraint::Length(0) // Hidden in fullscreen
        } else {
            Constraint::Length(1)
        },
    ];

    let sub_chunks =
        Layout::default().direction(Direction::Vertical).constraints(sub_constraints).split(inner);

    let sql_focused = matches!(focus, Focus::SubPane(SubPane::Sql));
    let sql_expanded = is_pane_expanded(SubPane::Sql, focused);
    render_sql_pane(f, sub_chunks[0], block, sql_focused, sql_expanded, mode);

    let results_focused = matches!(focus, Focus::SubPane(SubPane::Results));
    let results_expanded = is_pane_expanded(SubPane::Results, focused);
    render_results_pane(f, sub_chunks[1], block, results_focused, results_expanded, mode);

    let stats_focused = matches!(focus, Focus::SubPane(SubPane::Stats));
    let stats_expanded = is_pane_expanded(SubPane::Stats, focused);
    render_stats_pane(f, sub_chunks[2], block, stats_focused, stats_expanded, mode);

    let logs_focused = matches!(focus, Focus::SubPane(SubPane::Logs));
    let logs_expanded = is_pane_expanded(SubPane::Logs, focused);
    render_logs_pane(f, sub_chunks[3], block, logs_focused, logs_expanded);
}

fn render_sql_pane(
    f: &mut Frame,
    area: Rect,
    block: &QueryBlock,
    focused: bool,
    expanded: bool,
    mode: Mode,
) {
    let style = pane_style(focused, mode);
    let expand_char = if expanded { "▼" } else { "▶" };

    // Format SQL for display
    let formatted_sql = sqlformat::format(
        block.sql(),
        &sqlformat::QueryParams::None,
        &sqlformat::FormatOptions::default(),
    );

    if expanded {
        let sql_block = Block::default()
            .borders(Borders::ALL)
            .title(format!("{} SQL", expand_char))
            .border_style(style);
        let para = Paragraph::new(formatted_sql.as_str())
            .block(sql_block)
            .wrap(Wrap { trim: false })
            .scroll((block.sql_scroll(), 0));
        f.render_widget(para, area);
    } else {
        // Collapsed: show truncated SQL (use original, not formatted)
        let sql_preview: String = block.sql().chars().take(60).collect();
        let text = format!("{} SQL: {}", expand_char, sql_preview.replace('\n', " "));
        let para = Paragraph::new(text).style(style);
        f.render_widget(para, area);
    }
}

fn render_results_pane(
    f: &mut Frame,
    area: Rect,
    block: &mut QueryBlock,
    focused: bool,
    expanded: bool,
    mode: Mode,
) {
    let style = pane_style(focused, mode);
    let dimmed_style = Style::default().fg(Color::DarkGray);
    let expand_char = if expanded { "▼" } else { "▶" };
    let row_count = block.result_count();

    // Update visible dimensions for scroll calculations
    // Table uses 67% of width when expanded (67/33 split with detail panel)
    let table_width = area.width * 67 / 100;
    if let Some(table) = block.results_mut() {
        table.set_visible_height(area.height);
        table.set_visible_width(table_width);
    }

    if let Some(error) = block.error() {
        // Combine error color (red) with focus state
        let border_style = if focused && mode == Mode::Edit {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else if focused {
            Style::default().fg(Color::LightRed)
        } else {
            Style::default().fg(Color::Red)
        };
        let error_block = Block::default()
            .borders(Borders::ALL)
            .title(format!("{} Error", expand_char))
            .border_style(border_style);
        let para = Paragraph::new(error)
            .block(error_block)
            .style(Style::default().fg(Color::Red))
            .wrap(Wrap { trim: false })
            .scroll((block.error_scroll, 0));
        f.render_widget(para, area);
        return;
    }

    if expanded {
        if let Some(table) = block.results() {
            let title = format!("{} Results", expand_char);

            match &table.view_mode {
                ResultsViewMode::Table => {
                    // Split view: table left (67%), selected row detail right (33%)
                    let chunks = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Percentage(67), Constraint::Percentage(33)])
                        .split(area);

                    // Style depends on which panel is focused
                    let (table_style, detail_style) = if table.detail_focused {
                        (dimmed_style, style)
                    } else {
                        (style, dimmed_style)
                    };

                    let table_widget = table.render(&title, chunks[0].width, table_style);
                    f.render_widget(table_widget, chunks[0]);

                    // Tell the table how tall the detail panel is for scroll calculations
                    table.set_detail_visible_height(chunks[1].height);
                    let detail_widget =
                        table.render_selected_row_detail(&title, chunks[1].width, detail_style);
                    f.render_widget(detail_widget, chunks[1]);
                }
                ResultsViewMode::FieldValue { field, path, .. } => {
                    // Split view: table left (67%), detail with stats right (33%)
                    let field = *field;
                    let path = path.clone();

                    let h_chunks = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Percentage(67), Constraint::Percentage(33)])
                        .split(area);

                    // Left: table view (dimmed)
                    let table_widget = table.render_widget(&title, h_chunks[0].width, dimmed_style);
                    f.render_widget(table_widget, h_chunks[0]);

                    // Right: detail view with stats panel at bottom
                    let v_chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Min(0), Constraint::Length(5)])
                        .split(h_chunks[1]);

                    let detail_widget = table.render_detail(&title, v_chunks[0].width, style);
                    f.render_widget(detail_widget, v_chunks[0]);

                    // Stats panel
                    render_path_stats_panel(
                        f,
                        v_chunks[1],
                        table.get_path_stats(field, &path),
                        style,
                    );
                }
            }
        } else if block.running {
            let block_widget = Block::default()
                .borders(Borders::ALL)
                .title(format!("{} Results (loading...)", expand_char))
                .border_style(style);
            f.render_widget(block_widget, area);
        } else if block
            .queries
            .iter()
            .any(|sq| sq.status == crate::tui::session::QueryStatus::Cancelled)
        {
            let block_widget = Block::default()
                .borders(Borders::ALL)
                .title(format!("{} Results (cancelled)", expand_char))
                .border_style(style);
            f.render_widget(block_widget, area);
        } else {
            let block_widget = Block::default()
                .borders(Borders::ALL)
                .title(format!("{} Results (no data)", expand_char))
                .border_style(style);
            f.render_widget(block_widget, area);
        }
    } else {
        let text = format!("{} Results ({} rows)", expand_char, row_count);
        let para = Paragraph::new(text).style(style);
        f.render_widget(para, area);
    }
}

/// Render the path stats panel below the value view
fn render_path_stats_panel(
    f: &mut Frame,
    area: Rect,
    stats_state: PathStatsState<'_>,
    style: Style,
) {
    let block = Block::default().borders(Borders::ALL).title("Stats").border_style(style);
    let inner = block.inner(area);
    f.render_widget(block, area);

    match stats_state {
        PathStatsState::Computing => {
            let text =
                Paragraph::new("Computing stats...").style(Style::default().fg(Color::Yellow));
            f.render_widget(text, inner);
        }
        PathStatsState::NotStarted => {
            let text =
                Paragraph::new("Stats not available").style(Style::default().fg(Color::DarkGray));
            f.render_widget(text, inner);
        }
        PathStatsState::Ready(stats) => {
            let mut lines = Vec::new();

            // Type and null info
            let type_str = match stats.value_type {
                PathValueType::Numeric => "Numeric",
                PathValueType::String => "String",
                PathValueType::Boolean => "Boolean",
                PathValueType::Array => "Array",
                PathValueType::Object => "Object",
                PathValueType::Mixed => "Mixed",
                PathValueType::AllNull => "All NULL",
                PathValueType::Unknown => "Unknown",
            };
            let null_info = if stats.null_count > 0 {
                format!(" ({} nulls)", stats.null_count)
            } else {
                String::new()
            };

            // First line: basic info + numeric stats if applicable
            if let Some(ref num) = stats.numeric {
                let sparkline = sparkline_f64(&num.values);
                let p50 = num.percentile(50.0).map(|v| format!("{:.2}", v)).unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{} ({} vals{}): ", type_str, stats.total_rows, null_info),
                        Style::default().fg(Color::Gray),
                    ),
                    Span::styled(format!("min={:.2} ", num.min), Style::default().fg(Color::White)),
                    Span::styled(format!("max={:.2} ", num.max), Style::default().fg(Color::White)),
                    Span::styled(
                        format!("avg={:.2} ", num.avg()),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(format!("p50={} ", p50), Style::default().fg(Color::White)),
                    Span::styled(sparkline, Style::default().fg(Color::Green)),
                ]));
            } else {
                lines.push(Line::from(Span::styled(
                    format!("{} ({} vals{})", type_str, stats.total_rows, null_info),
                    Style::default().fg(Color::Gray),
                )));
            }

            // Second line: unique value sample
            if let Some(ref sample) = stats.unique_sample {
                let truncated = if sample.truncated { "+" } else { "" };
                let mut spans = vec![Span::styled(
                    format!("Top of {} unique{}: ", sample.total_unique, truncated),
                    Style::default().fg(Color::Gray),
                )];
                for (i, (val, count)) in sample.values.iter().take(5).enumerate() {
                    if i > 0 {
                        spans.push(Span::raw(", "));
                    }
                    let display_val: String = val.chars().take(15).collect();
                    let ellipsis = if val.len() > 15 { ".." } else { "" };
                    spans.push(Span::styled(
                        format!("\"{}{}\"({})", display_val, ellipsis, count),
                        Style::default().fg(Color::White),
                    ));
                }
                lines.push(Line::from(spans));
            }

            let para = Paragraph::new(lines);
            f.render_widget(para, inner);
        }
    }
}

/// Generate sparkline from f64 values
fn sparkline_f64(values: &[f64]) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

    if values.is_empty() {
        return String::new();
    }

    let min = values.iter().cloned().fold(f64::MAX, f64::min);
    let max = values.iter().cloned().fold(f64::MIN, f64::max);
    let range = (max - min).max(0.001);

    // Take last 30 values for display
    values
        .iter()
        .rev()
        .take(30)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|&v| {
            let normalized = (v - min) / range;
            let idx = (normalized * 7.0).round() as usize;
            BARS[idx.min(7)]
        })
        .collect()
}

fn render_stats_pane(
    f: &mut Frame,
    area: Rect,
    block: &mut QueryBlock,
    focused: bool,
    expanded: bool,
    mode: Mode,
) {
    let style = pane_style(focused, mode);
    let expand_char = if expanded { "▼" } else { "▶" };
    let elapsed_ns = block.stats().elapsed_ns;

    // Calculate rates
    let read_rows_rate = calc_rate(block.stats().rows_read, elapsed_ns);
    let read_bytes_rate = calc_rate(block.stats().bytes_read, elapsed_ns);
    let write_rows_rate = calc_rate(block.stats().rows_written, elapsed_ns);
    let write_bytes_rate = calc_rate(block.stats().bytes_written, elapsed_ns);

    // Progress ratio (if we know total)
    let progress = block.stats().total_rows.and_then(|total| {
        if total > 0 {
            Some((block.stats().rows_read as f64 / total as f64).min(1.0))
        } else {
            None
        }
    });

    let has_writes = block.stats().rows_written > 0;

    if expanded {
        // Check if we're in expanded metric view
        match block.stats().view_mode {
            MetricsViewMode::Expanded { index } => {
                // 50/50 split: table on left (dimmed), expanded detail on right (focused)
                let chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .split(area);

                let dimmed_style = Style::default().fg(Color::DarkGray);
                render_stats_metrics_table(f, chunks[0], block, dimmed_style);
                render_stats_metric_expanded(f, chunks[1], block, index, style);
            }
            MetricsViewMode::Table => {
                // Split area: header lines + progress line + metrics table
                // 3 lines for current/max/final, +1 if writes, +1 for CPU/RAM
                let text_lines: u16 = if has_writes { 5 } else { 4 };

                let constraints: Vec<Constraint> =
                    vec![Constraint::Length(text_lines), Constraint::Length(1), Constraint::Min(0)];

                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints(constraints)
                    .split(area);

                // Stats line - show progress while running, server totals when complete
                let stats_line = if block.running {
                    format!(
                        "{} Running: {} rows read, {} read @ {}/s, {}/s",
                        expand_char,
                        format_number(block.stats().rows_read),
                        format_bytes(block.stats().bytes_read),
                        format_rate(read_rows_rate),
                        format_bytes(read_bytes_rate as u64),
                    )
                } else {
                    match (block.stats().final_rows_read, block.stats().final_bytes_read) {
                        (Some(rows), Some(bytes)) => {
                            let blocks_str = block
                                .stats()
                                .final_blocks
                                .map(|b| format!(" in {} blocks", format_number(b)))
                                .unwrap_or_default();
                            let peak_mem = format_bytes(block.stats().peak_ram_current);
                            format!(
                                "{} Finished: {} rows read, {} read{}, peak {}",
                                expand_char,
                                format_number(rows),
                                format_bytes(bytes),
                                blocks_str,
                                peak_mem
                            )
                        }
                        _ => format!("{} Finished: (server stats not available)", expand_char),
                    }
                };

                let mut lines = vec![Line::from(stats_line)];

                // Write stats (if any)
                if has_writes {
                    let write_line = format!(
                        "           Write: {} rows written, {} written @ {}/s, {}/s",
                        format_number(block.stats().rows_written),
                        format_bytes(block.stats().bytes_written),
                        format_rate(write_rows_rate),
                        format_bytes(write_bytes_rate as u64),
                    );
                    lines.push(Line::from(write_line));
                }

                // CPU and RAM sparklines (fixed 16 char width)
                let cpu_sparkline = sparkline_str(&block.stats().cpu_history, 16);
                let ram_sparkline = sparkline_str(&block.stats().ram_history, 16);
                let cpu_str = format!("CPU {} {}%", cpu_sparkline, block.stats().cpu_current);
                // Only show peak in RAM line when running - when finished, peak is in the Finished
                // line
                let is_finished = block.stats().final_rows_read.is_some();
                let ram_str = if is_finished {
                    format!("RAM {} {}", ram_sparkline, format_bytes(block.stats().ram_current))
                } else {
                    format!(
                        "RAM {} {}, peak {}",
                        ram_sparkline,
                        format_bytes(block.stats().ram_current),
                        format_bytes(block.stats().peak_ram_current)
                    )
                };
                let metrics_line = format!("  {} | {}", cpu_str, ram_str);
                lines.push(Line::from(metrics_line));

                let header = Paragraph::new(lines).style(style);
                f.render_widget(header, chunks[0]);

                // Progress line: gauge if we know total, otherwise text indicator
                if let Some(ratio) = progress {
                    let pct = (ratio * 100.0) as u16;
                    let gauge = LineGauge::default()
                        .filled_style(Style::default().fg(Color::Green))
                        .line_set(symbols::line::NORMAL)
                        .ratio(ratio)
                        .label(format!("{}%", pct));
                    f.render_widget(gauge, chunks[1]);
                } else {
                    // No total known - show reading indicator
                    let dots = ".".repeat((block.stats().rows_read as usize / 1000) % 4);
                    let progress_text =
                        format!("Reading{} {} rows", dots, format_number(block.stats().rows_read));
                    let para =
                        Paragraph::new(progress_text).style(Style::default().fg(Color::Yellow));
                    f.render_widget(para, chunks[1]);
                }

                // Render grouped metrics table
                render_stats_metrics_table(f, chunks[2], block, style);
            }
        }
    } else {
        // Collapsed: single line summary with rates + optional progress gauge
        let cpu_pct = block.stats().cpu_current;
        let ram_str = format_bytes(block.stats().ram_current);
        let peak_ram_str = format_bytes(block.stats().peak_ram_current);

        // Prefer final values if available, otherwise use current
        let (rows, bytes, label) =
            match (block.stats().final_rows_read, block.stats().final_bytes_read) {
                (Some(r), Some(b)) => (r, b, "Finished"),
                _ => (block.stats().rows_read, block.stats().bytes_read, "Running"),
            };

        let text = if has_writes {
            format!(
                "{} {}: {} rows read, {} read | W {} @ {}/s | CPU {}% | RAM {}, peak {}",
                expand_char,
                label,
                format_number(rows),
                format_bytes(bytes),
                format_number(block.stats().rows_written),
                format_rate(write_rows_rate),
                cpu_pct,
                ram_str,
                peak_ram_str
            )
        } else {
            format!(
                "{} {}: {} rows read, {} read | CPU {}% | RAM {}, peak {}",
                expand_char,
                label,
                format_number(rows),
                format_bytes(bytes),
                cpu_pct,
                ram_str,
                peak_ram_str
            )
        };

        if let Some(ratio) = progress {
            // Split: text | gauge
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(40), Constraint::Length(30)])
                .split(area);

            let para = Paragraph::new(text).style(style);
            f.render_widget(para, chunks[0]);

            let pct = (ratio * 100.0) as u16;
            let gauge = LineGauge::default()
                .filled_style(Style::default().fg(Color::Green))
                .line_set(symbols::line::NORMAL)
                .ratio(ratio)
                .label(format!("{}%", pct));
            f.render_widget(gauge, chunks[1]);
        } else {
            let para = Paragraph::new(text).style(style);
            f.render_widget(para, area);
        }
    }
}

/// Render the grouped metrics table view
fn render_stats_metrics_table(f: &mut Frame, area: Rect, block: &mut QueryBlock, style: Style) {
    let metric_count = block.stats().metric_count();

    if metric_count == 0 {
        let empty = Paragraph::new("No profile events").style(Style::default().fg(Color::DarkGray));
        f.render_widget(empty, area);
        return;
    }

    // Calculate visible height (area height - 3 for borders and header row)
    let visible_height = area.height.saturating_sub(3) as usize;
    block.stats_mut().visible_height = visible_height.max(1);

    let scroll_offset = block.stats().scroll_offset;
    let selected_row = block.stats().selected_row;

    // Build only visible rows (skip to scroll_offset, take visible_height)
    let stats = block.stats();
    let rows: Vec<Row> = stats
        .metric_names
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_height)
        .filter_map(|(i, name)| {
            let metric = stats.metrics.get(name)?;
            let sparkline = sparkline_str_i64(&metric.history, 16);
            let current = format_metric_value(metric.current);
            let min_str = format_metric_value(metric.min);
            let max_str = format_metric_value(metric.max);
            let avg_str = format_metric_value(metric.avg() as i64);

            let row_style = if i == selected_row {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            } else {
                Style::default()
            };

            Some(
                Row::new(vec![name.clone(), sparkline, current, min_str, max_str, avg_str])
                    .style(row_style),
            )
        })
        .collect();

    let widths = [
        Constraint::Min(20),
        Constraint::Length(16),
        Constraint::Min(10),
        Constraint::Min(10),
        Constraint::Min(10),
        Constraint::Min(10),
    ];

    let table = Table::new(rows, widths)
        .header(
            Row::new(vec!["Metric", "Sparkline", "Current", "Min", "Max", "Avg"])
                .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::ALL).title("Metrics").border_style(style));

    f.render_widget(table, area);
}

/// Render expanded view for a single metric (60% chart, 40% stats)
fn render_stats_metric_expanded(
    f: &mut Frame,
    area: Rect,
    block: &QueryBlock,
    index: usize,
    style: Style,
) {
    let metric_name = match block.stats().metric_names.get(index) {
        Some(name) => name,
        None => {
            let empty = Paragraph::new("Metric not found").style(Style::default().fg(Color::Red));
            f.render_widget(empty, area);
            return;
        }
    };

    let metric = match block.stats().metrics.get(metric_name) {
        Some(m) => m,
        None => {
            let empty =
                Paragraph::new("Metric data not found").style(Style::default().fg(Color::Red));
            f.render_widget(empty, area);
            return;
        }
    };

    // 60/40 split: chart on top, stats below
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    // Render braille chart
    if metric.chart_points.is_empty() {
        let empty_chart =
            Block::default().borders(Borders::ALL).title(metric_name.as_str()).border_style(style);
        f.render_widget(empty_chart, chunks[0]);
    } else {
        let dataset = Dataset::default()
            .name(metric_name.as_str())
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Cyan))
            .data(&metric.chart_points);

        // Calculate bounds
        let (min_x, max_x, min_y, max_y) = {
            let mut min_x = f64::MAX;
            let mut max_x = f64::MIN;
            let mut min_y = f64::MAX;
            let mut max_y = f64::MIN;
            for (x, y) in &metric.chart_points {
                min_x = min_x.min(*x);
                max_x = max_x.max(*x);
                min_y = min_y.min(*y);
                max_y = max_y.max(*y);
            }
            // Add some padding
            let y_range = max_y - min_y;
            let y_padding = if y_range > 0.0 { y_range * 0.1 } else { 1.0 };
            (min_x, max_x, (min_y - y_padding).max(0.0), max_y + y_padding)
        };

        let x_axis =
            Axis::default().style(Style::default().fg(Color::Gray)).bounds([min_x, max_x]).labels(
                vec![Span::raw(format!("{:.0}ms", min_x)), Span::raw(format!("{:.0}ms", max_x))],
            );

        let y_axis = Axis::default()
            .style(Style::default().fg(Color::Gray))
            .bounds([min_y, max_y])
            .labels(vec![
                Span::raw(format_metric_value(min_y as i64)),
                Span::raw(format_metric_value(max_y as i64)),
            ]);

        let chart = Chart::new(vec![dataset])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(metric_name.as_str())
                    .border_style(style),
            )
            .x_axis(x_axis)
            .y_axis(y_axis);

        f.render_widget(chart, chunks[0]);
    }

    // Render stats below
    let stats_lines = vec![
        Line::from(vec![
            Span::styled("Min: ", Style::default().fg(Color::Gray)),
            Span::styled(format_metric_value(metric.min), Style::default().fg(Color::White)),
            Span::raw("  "),
            Span::styled("Max: ", Style::default().fg(Color::Gray)),
            Span::styled(format_metric_value(metric.max), Style::default().fg(Color::White)),
            Span::raw("  "),
            Span::styled("Avg: ", Style::default().fg(Color::Gray)),
            Span::styled(format!("{:.2}", metric.avg()), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("Current: ", Style::default().fg(Color::Gray)),
            Span::styled(
                format_metric_value(metric.current),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("Count: ", Style::default().fg(Color::Gray)),
            Span::styled(format!("{}", metric.count), Style::default().fg(Color::White)),
        ]),
        Line::from(""),
        Line::from(Span::styled("Press h/Left to go back", Style::default().fg(Color::DarkGray))),
    ];

    let stats_para = Paragraph::new(stats_lines)
        .block(Block::default().borders(Borders::ALL).title("Statistics").border_style(style));
    f.render_widget(stats_para, chunks[1]);
}

/// Format metric values with appropriate suffixes
fn format_metric_value(value: i64) -> String {
    let abs_value = value.unsigned_abs();
    let sign = if value < 0 { "-" } else { "" };

    if abs_value >= 1_000_000_000 {
        format!("{}{:.2}G", sign, abs_value as f64 / 1_000_000_000.0)
    } else if abs_value >= 1_000_000 {
        format!("{}{:.2}M", sign, abs_value as f64 / 1_000_000.0)
    } else if abs_value >= 1_000 {
        format!("{}{:.2}K", sign, abs_value as f64 / 1_000.0)
    } else {
        format!("{}{}", sign, abs_value)
    }
}

/// Format a number with K/M/B suffixes
fn format_number(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Format bytes with appropriate suffix
fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1}GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1}MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1_024 {
        format!("{:.1}KB", bytes as f64 / 1_024.0)
    } else {
        format!("{}B", bytes)
    }
}

/// Format rate per second with K/M suffix
fn format_rate(per_sec: f64) -> String {
    if per_sec >= 1_000_000.0 {
        format!("{:.1}M", per_sec / 1_000_000.0)
    } else if per_sec >= 1_000.0 {
        format!("{:.1}K", per_sec / 1_000.0)
    } else {
        format!("{:.0}", per_sec)
    }
}

/// Calculate rate (count per second) from elapsed nanoseconds
fn calc_rate(count: u64, elapsed_ns: u64) -> f64 {
    if elapsed_ns == 0 {
        return 0.0;
    }
    (count as f64) * 1_000_000_000.0 / (elapsed_ns as f64)
}

/// Generate sparkline string from history with fixed width
fn sparkline_str(history: &std::collections::VecDeque<u64>, width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

    if history.is_empty() {
        return BARS[0].to_string().repeat(width);
    }

    let max = history.iter().copied().max().unwrap_or(1).max(1);

    // Take most recent `width` items, pad left if fewer
    let start = history.len().saturating_sub(width);
    let items: Vec<_> = history.iter().skip(start).copied().collect();
    let pad_count = width.saturating_sub(items.len());

    let mut result = BARS[0].to_string().repeat(pad_count);
    for v in items {
        let idx = ((v * 7) / max).min(7) as usize;
        result.push(BARS[idx]);
    }
    result
}

/// Generate sparkline string from i64 history (shifts values so min becomes 0)
fn sparkline_str_i64(history: &VecDeque<i64>, width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

    if history.is_empty() {
        return BARS[0].to_string().repeat(width);
    }

    let min = history.iter().copied().min().unwrap_or(0);
    let max = history.iter().copied().max().unwrap_or(0);
    let range = (max - min).max(1) as u64;

    // Take most recent `width` items, pad left if fewer
    let start = history.len().saturating_sub(width);
    let items: Vec<_> = history.iter().skip(start).copied().collect();
    let pad_count = width.saturating_sub(items.len());

    let mut result = BARS[0].to_string().repeat(pad_count);
    for v in items {
        let shifted = (v - min) as u64;
        let idx = ((shifted * 7) / range).min(7) as usize;
        result.push(BARS[idx]);
    }
    result
}

fn render_logs_pane(
    f: &mut Frame,
    area: Rect,
    block: &mut QueryBlock,
    focused: bool,
    expanded: bool,
) {
    let style = if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let dimmed_style = Style::default().fg(Color::DarkGray);

    let expand_char = if expanded { "▼" } else { "▶" };
    let source_count = block.logs_data.source_count();
    let thread_count = block.logs_data.thread_count();
    let total_logs = block.logs_data.total_log_count();

    // Update visible height for scroll calculations
    block.logs_data.visible_height = area.height.saturating_sub(3) as usize;

    if !expanded || source_count == 0 {
        let text = format!(
            "{} Logs ({} sources, {} threads, {} entries)",
            expand_char, source_count, thread_count, total_logs
        );
        let para = Paragraph::new(text).style(style);
        f.render_widget(para, area);
        return;
    }

    match &block.logs_data.view_mode {
        LogsViewMode::Sources => {
            render_logs_sources(f, area, block, expand_char, style);
        }
        LogsViewMode::Threads { source } => {
            let source = source.clone();
            // Split view: sources left, threads right
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                .split(area);

            render_logs_sources(f, chunks[0], block, expand_char, dimmed_style);
            render_logs_threads(f, chunks[1], block, &source, style);
        }
        LogsViewMode::Entries { source, thread_id } => {
            let source = source.clone();
            let thread_id = *thread_id;
            // Split view: sources left, entries right
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                .split(area);

            render_logs_sources(f, chunks[0], block, expand_char, dimmed_style);
            render_logs_entries(f, chunks[1], block, &source, thread_id, style);
        }
        LogsViewMode::EntryDetail { source, thread_id, entry_index, scroll_offset } => {
            let source = source.clone();
            let thread_id = *thread_id;
            let entry_index = *entry_index;
            let scroll_offset = *scroll_offset;
            // Split view: sources left, entry detail right
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                .split(area);

            render_logs_sources(f, chunks[0], block, expand_char, dimmed_style);
            render_log_entry_detail(
                f,
                chunks[1],
                block,
                &source,
                thread_id,
                entry_index,
                scroll_offset,
                style,
            );
        }
    }
}

fn render_logs_sources(
    f: &mut Frame,
    area: Rect,
    block: &QueryBlock,
    expand_char: &str,
    style: Style,
) {
    let logs_data = &block.logs_data;
    let visible_height = logs_data.visible_height;
    let scroll_offset = logs_data.scroll_offset;
    let selected_row = logs_data.selected_row;

    let rows: Vec<Row> = logs_data
        .sorted_sources
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_height)
        .filter_map(|(i, source_name)| {
            let source = logs_data.sources.get(source_name)?;

            let row_style = if i == selected_row {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            } else {
                Style::default()
            };

            Some(
                Row::new(vec![
                    source.source.clone(),
                    format!("{}", source.thread_count()),
                    format!("{}", source.entry_count()),
                    source.latest_text.replace('\n', " "),
                ])
                .style(row_style),
            )
        })
        .collect();

    let widths =
        [Constraint::Fill(1), Constraint::Length(8), Constraint::Length(8), Constraint::Fill(2)];

    let title = format!(
        "{} Logs ({} sources, {} total)",
        expand_char,
        logs_data.source_count(),
        logs_data.total_log_count()
    );

    let table = Table::new(rows, widths)
        .header(
            Row::new(vec!["Source", "Threads", "Entries", "Latest"])
                .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::ALL).title(title).border_style(style));

    f.render_widget(table, area);
}

fn render_logs_threads(
    f: &mut Frame,
    area: Rect,
    block: &QueryBlock,
    source_name: &str,
    style: Style,
) {
    let logs_data = &block.logs_data;

    let source = match logs_data.sources.get(source_name) {
        Some(s) => s,
        None => {
            let para = Paragraph::new("Source not found").style(Style::default().fg(Color::Red));
            f.render_widget(para, area);
            return;
        }
    };

    let visible_height = logs_data.visible_height;
    let scroll_offset = logs_data.secondary_scroll;
    let selected_row = logs_data.secondary_selected;

    let rows: Vec<Row> = source
        .sorted_threads
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_height)
        .filter_map(|(i, thread_id)| {
            let thread = source.threads.get(thread_id)?;

            let row_style = if i == selected_row {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            } else {
                Style::default()
            };

            Some(
                Row::new(vec![
                    format!("{}", thread.thread_id),
                    format!("{}", thread.entries.len()),
                    thread.latest_text.replace('\n', " "),
                ])
                .style(row_style),
            )
        })
        .collect();

    let widths = [Constraint::Length(12), Constraint::Length(8), Constraint::Fill(1)];

    let title = format!("{} ({} threads) - h/Left to go back", source_name, source.thread_count());

    let table = Table::new(rows, widths)
        .header(
            Row::new(vec!["Thread", "Entries", "Latest"])
                .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::ALL).title(title).border_style(style));

    f.render_widget(table, area);
}

fn render_logs_entries(
    f: &mut Frame,
    area: Rect,
    block: &QueryBlock,
    source_name: &str,
    thread_id: u64,
    style: Style,
) {
    let logs_data = &block.logs_data;

    let thread = match logs_data.sources.get(source_name).and_then(|s| s.threads.get(&thread_id)) {
        Some(t) => t,
        None => {
            let para = Paragraph::new("Thread not found").style(Style::default().fg(Color::Red));
            f.render_widget(para, area);
            return;
        }
    };

    let visible_height = logs_data.visible_height;
    let scroll_offset = logs_data.tertiary_scroll;
    let selected_row = logs_data.tertiary_selected;

    let rows: Vec<Row> = thread
        .entries
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_height)
        .map(|(i, entry)| {
            let row_style = if i == selected_row {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            } else {
                Style::default()
            };

            Row::new(vec![entry.time.clone(), entry.text.replace('\n', " ")]).style(row_style)
        })
        .collect();

    let widths = [Constraint::Length(26), Constraint::Fill(1)];

    let title =
        format!("Thread {} ({} entries) - h/Left to go back", thread_id, thread.entries.len());

    let table = Table::new(rows, widths)
        .header(
            Row::new(vec!["Time", "Text"])
                .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::ALL).title(title).border_style(style));

    f.render_widget(table, area);
}

fn render_log_entry_detail(
    f: &mut Frame,
    area: Rect,
    block: &QueryBlock,
    source_name: &str,
    thread_id: u64,
    entry_index: usize,
    scroll_offset: u16,
    style: Style,
) {
    let entry = block
        .logs_data
        .sources
        .get(source_name)
        .and_then(|s| s.threads.get(&thread_id))
        .and_then(|t| t.entries.get(entry_index));

    if let Some(entry) = entry {
        let content = format!(
            "Time: {}\nThread: {}\nSource: {}\n\n{}",
            entry.time, entry.thread_id, entry.source, entry.text
        );
        let entry_count = block
            .logs_data
            .sources
            .get(source_name)
            .and_then(|s| s.threads.get(&thread_id))
            .map(|t| t.entries.len())
            .unwrap_or(0);
        let title = format!("Log Entry {}/{} - h/Left to go back", entry_index + 1, entry_count);
        let para = Paragraph::new(content)
            .block(Block::default().borders(Borders::ALL).title(title).border_style(style))
            .wrap(Wrap { trim: false })
            .scroll((scroll_offset, 0));
        f.render_widget(para, area);
    } else {
        let para = Paragraph::new("Entry not found").style(Style::default().fg(Color::Red));
        f.render_widget(para, area);
    }
}

fn pane_style(focused: bool, mode: Mode) -> Style {
    if focused {
        if mode == Mode::Edit {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Yellow)
        }
    } else {
        Style::default().fg(Color::White)
    }
}

fn render_status_bar(f: &mut Frame, area: Rect, app: &App) {
    let mode_str = match app.session.focus {
        Focus::QueryEditor => "EDIT",
        _ => match app.session.mode {
            Mode::Navigation => "NAV",
            Mode::Edit => "EDIT",
        },
    };

    let focus_str = match &app.session.focus {
        Focus::QueryEditor => "New Query".to_string(),
        Focus::HistoryView => "History".to_string(),
        Focus::SubPane(pane) => {
            let pane_name = match pane {
                SubPane::Sql => "SQL",
                SubPane::Results => "Results",
                SubPane::Stats => "Stats",
                SubPane::Logs => "Logs",
            };
            let query_str = app
                .session
                .selected_card
                .map(|id| format!("Q{}", id + 1))
                .unwrap_or_else(|| "?".to_string());
            format!("{} > {}", query_str, pane_name)
        }
    };

    let query_count = app.session.history.len();
    let query_info =
        if query_count > 0 { format!(" ({} queries)", query_count) } else { String::new() };

    // Context-sensitive hints
    let hints = match app.session.focus {
        Focus::QueryEditor => {
            "Cmd+Enter: run | Esc: close | Ctrl+P/N: history | Ctrl+R: search | ?: help".to_string()
        }
        _ => {
            let mut parts = vec!["j/k: navigate", "l: enter", "h: back", "n: new query"];

            // Cancel hint when query is running
            if app.session.selected_is_running()
                && app
                    .session
                    .displayed_block()
                    .filter(|block| block.running && !block.cancel_requested)
                    .is_some()
            {
                parts.push("C: cancel");
            }

            // Copy hint when in Results pane + Edit mode
            if matches!(
                (&app.session.focus, &app.session.mode),
                (Focus::SubPane(SubPane::Results), Mode::Edit)
            ) {
                parts.push("y: copy");
            }

            parts.push("?: help");
            parts.join(" | ")
        }
    };

    let status = format!(" [{}] {}{} | {}", mode_str, focus_str, query_info, hints);

    let status_line = Paragraph::new(status).style(Style::default().fg(Color::Gray));
    f.render_widget(status_line, area);
}

fn render_help(f: &mut Frame) {
    let help_text = vec![
        Line::from(vec![Span::styled(
            "Keybindings",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        Line::from("History View:"),
        Line::from("  j/k or Down/Up    Navigate cards"),
        Line::from("  l/Right/Enter     View query results"),
        Line::from("  n/i               Open new query editor"),
        Line::from("  N/I (Shift)       Open editor with current query's SQL"),
        Line::from("  Ctrl+P/N          Previous/next query card"),
        Line::from("  Alt+1..9          Jump to query 1-9"),
        Line::from("  ?                 Toggle help"),
        Line::from("  Ctrl+Q            Quit"),
        Line::from(""),
        Line::from("Query Editor (Modal):"),
        Line::from("  Cmd+Enter         Execute query"),
        Line::from("  Escape            Close editor"),
        Line::from("  Ctrl+P/N          Recall command history"),
        Line::from("  Ctrl+R            Search command history"),
        Line::from("  Ctrl+L            Format SQL"),
        Line::from(""),
        Line::from("Results View:"),
        Line::from("  j/k or Down/Up    Navigate panes/rows"),
        Line::from("  l/Right/Enter     Expand pane / drill into row"),
        Line::from("  h/Left/Esc        Collapse / go back"),
        Line::from("  Alt+h/l           Scroll columns"),
        Line::from("  PgUp/PgDown       Page navigation"),
        Line::from("  s                 Sort by column"),
        Line::from("  y                 Copy to clipboard"),
        Line::from("  C                 Cancel running query"),
        Line::from(""),
        Line::from("Press ? or Esc to close help"),
    ];

    let help_block = Paragraph::new(help_text)
        .block(Block::default().borders(Borders::ALL).title("Help"))
        .style(Style::default().fg(Color::White));

    let area = centered_rect(60, 80, f.area());
    f.render_widget(ratatui::widgets::Clear, area);
    f.render_widget(help_block, area);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn bottom_right_rect(percent_width: u16, percent_height: u16, full_area: Rect) -> Rect {
    let width = (full_area.width * percent_width / 100).max(30);
    let height = (full_area.height * percent_height / 100).max(12);

    Rect {
        x: full_area.width.saturating_sub(width + 1),
        y: full_area.height.saturating_sub(height + 1),
        width,
        height,
    }
}

fn render_column_stats_modal(
    f: &mut Frame,
    area: Rect,
    _table: &SortableTable,
    column_name: &str,
    stats: &PathStats,
) {
    let modal_area = bottom_right_rect(25, 25, area);
    f.render_widget(Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(format!(" {} ", column_name));

    let inner = block.inner(modal_area);
    f.render_widget(block, modal_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(0)])
        .split(inner);

    let null_pct = if stats.total_rows > 0 {
        (stats.null_count as f64 / stats.total_rows as f64) * 100.0
    } else {
        0.0
    };

    let header_text = format!(
        "Rows: {}  Nulls: {} ({:.1}%)\nType: {:?}",
        stats.total_rows, stats.null_count, null_pct, stats.value_type,
    );

    let header_para = Paragraph::new(header_text).style(Style::default().fg(Color::Gray));
    f.render_widget(header_para, chunks[0]);

    if let Some(ref num) = stats.numeric {
        render_numeric_column_viz(f, chunks[1], num);
    } else if let Some(ref sample) = stats.unique_sample {
        render_categorical_column_viz(f, chunks[1], sample);
    } else {
        let placeholder =
            Paragraph::new("No data to visualize").style(Style::default().fg(Color::DarkGray));
        f.render_widget(placeholder, chunks[1]);
    }
}

fn render_numeric_column_viz(f: &mut Frame, area: Rect, num_stats: &NumericStats) {
    let sparkline = sparkline_f64(&num_stats.values);

    let lines = vec![
        Line::from(vec![
            Span::styled("Min: ", Style::default().fg(Color::Gray)),
            Span::styled(format!("{:.2}", num_stats.min), Style::default().fg(Color::White)),
            Span::raw("  "),
            Span::styled("Max: ", Style::default().fg(Color::Gray)),
            Span::styled(format!("{:.2}", num_stats.max), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("Avg: ", Style::default().fg(Color::Gray)),
            Span::styled(format!("{:.2}", num_stats.avg()), Style::default().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled("Distribution: ", Style::default().fg(Color::Gray)),
            Span::styled(sparkline, Style::default().fg(Color::Green)),
        ]),
    ];

    let para = Paragraph::new(lines);
    f.render_widget(para, area);
}

fn render_categorical_column_viz(f: &mut Frame, area: Rect, unique_sample: &UniqueSample) {
    let mut lines = vec![Line::from(Span::styled(
        format!("Unique values: {}", unique_sample.total_unique),
        Style::default().fg(Color::Gray),
    ))];

    let max_count = unique_sample.values.iter().map(|(_, c)| c).max().unwrap_or(&1);

    for (val, count) in unique_sample.values.iter().take(5) {
        let bar_width =
            if *max_count > 0 { ((*count as f64 / *max_count as f64) * 20.0) as usize } else { 0 };
        let bar = "█".repeat(bar_width);

        let display_val: String = val.chars().take(40).collect();
        let ellipsis = if val.len() > 40 { ".." } else { "" };

        lines.push(Line::from(vec![
            Span::styled(
                format!("{:40}{}: ", display_val, ellipsis),
                Style::default().fg(Color::White),
            ),
            Span::styled(bar, Style::default().fg(Color::Cyan)),
            Span::raw(format!(" {}", count)),
        ]));
    }

    let para = Paragraph::new(lines);
    f.render_widget(para, area);
}

fn render_column_stats_modal_overlay(f: &mut Frame, area: Rect, app: &App) {
    let session = &app.session;

    let block = if let Some(idx) = session.selected_card {
        // Check running query first, then current block
        session.running_queries.get(&idx).or(session.current_block.as_ref())
    } else {
        session.current_block.as_ref()
    };

    if let Some(block) = block {
        if let Some(table) = block.results() {
            if table.header_focused {
                let column_name: &str =
                    table.columns.get(table.focused_col).map(String::as_str).unwrap_or("Unknown");

                match table.get_path_stats(table.focused_col, &vec![]) {
                    PathStatsState::Ready(stats) => {
                        // Skip if only one unique value (no variation to show)
                        if let Some(ref sample) = stats.unique_sample {
                            if sample.total_unique <= 1 {
                                return;
                            }
                        }

                        // Skip if no visualizable data (Array, Object, AllNull types)
                        if stats.numeric.is_none() && stats.unique_sample.is_none() {
                            return;
                        }

                        render_column_stats_modal(f, area, table, column_name, stats);
                    }
                    PathStatsState::Computing => {
                        let modal_area = bottom_right_rect(25, 25, area);
                        f.render_widget(Clear, modal_area);

                        let loading = Paragraph::new("Computing stats...")
                            .style(Style::default().fg(Color::Yellow))
                            .block(
                                Block::default()
                                    .borders(Borders::ALL)
                                    .border_style(Style::default().fg(Color::Yellow))
                                    .title(" Column Stats "),
                            );
                        f.render_widget(loading, modal_area);
                    }
                    PathStatsState::NotStarted => {}
                }
            }
        }
    }
}
