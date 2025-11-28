use std::collections::VecDeque;

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Chart, Clear, Dataset, GraphType, LineGauge, List, ListItem, Paragraph,
    Row, Table, Wrap,
};
use ratatui::{Frame, symbols};

use crate::tui::app::App;
use crate::tui::session::{Focus, LogsViewMode, MetricsViewMode, Mode, QueryBlock, SubPane};
use crate::tui::widgets::table::{PathStatsState, PathValueType, ResultsViewMode};

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
}

fn render_session(f: &mut Frame, area: Rect, app: &mut App) {
    // Split: sidebar | main content
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(28), Constraint::Min(40)])
        .split(area);

    render_sidebar(f, chunks[0], app);
    render_main_content(f, chunks[1], app);
}

fn render_sidebar(f: &mut Frame, area: Rect, app: &App) {
    let sidebar_focused = matches!(app.session.focus, Focus::Sidebar);
    let border_style = if sidebar_focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::default().borders(Borders::ALL).title("History").border_style(border_style);

    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.session.history.is_empty() {
        let empty = Paragraph::new("No queries yet").style(Style::default().fg(Color::DarkGray));
        f.render_widget(empty, inner);
        return;
    }

    let mut items: Vec<ListItem> = Vec::new();

    // Unified history list
    for (idx, entry) in app.session.history.iter().enumerate() {
        let is_selected = app.session.selected_history == Some(idx);
        let is_running = app.session.running_queries.contains_key(&idx);
        let running_block = app.session.running_queries.get(&idx);

        // Determine indicator
        let indicator = if running_block.map(|b| b.cancel_requested).unwrap_or(false) {
            "x"
        } else if is_running {
            "*"
        } else if entry.error.is_some() {
            "!"
        } else if is_selected {
            ">"
        } else {
            " "
        };

        // Truncate SQL preview
        let sql_preview: String =
            entry.sql_preview.chars().take(18).collect::<String>().replace('\n', " ");

        // Format row count - use running block's count if available
        let row_count = running_block.map(|b| b.result_count()).unwrap_or(entry.row_count as usize);

        // Format timestamp
        let age = format_relative_time(entry.timestamp);

        let text = format!("{} {} ({} rows) {}", indicator, sql_preview, row_count, age);

        let style = if is_selected {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else if entry.error.is_some() {
            Style::default().fg(Color::Red)
        } else if is_running {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::Gray)
        };

        items.push(ListItem::new(text).style(style));
    }

    let list = List::new(items);
    f.render_widget(list, inner);
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
        format!("{}s", diff)
    } else if diff < 3600 {
        format!("{}m", diff / 60)
    } else if diff < 86400 {
        format!("{}h", diff / 3600)
    } else {
        format!("{}d", diff / 86400)
    }
}

fn render_main_content(f: &mut Frame, area: Rect, app: &mut App) {
    // Full-screen editor when Focus::NewQuery
    if matches!(app.session.focus, Focus::NewQuery) {
        render_new_query_fullscreen(f, area, app);
        return;
    }

    // Check if we're loading
    if app.session.loading_entry_id.is_some() {
        let loading = Paragraph::new("Loading...")
            .style(Style::default().fg(Color::Yellow))
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL).title("Query Results"));
        f.render_widget(loading, area);
        return;
    }

    // Check for app-level error (archive loading, clipboard, etc.)
    if let Some(ref error) = app.session.app_error {
        let error_block = Block::default()
            .borders(Borders::ALL)
            .title("Error")
            .border_style(Style::default().fg(Color::Red));
        let para = Paragraph::new(error.as_str())
            .block(error_block)
            .style(Style::default().fg(Color::Red))
            .wrap(Wrap { trim: false });
        f.render_widget(para, area);
        return;
    }

    // Display current block (from running_queries or current_block)
    let hist_idx = app.session.selected_history.unwrap_or(0);
    let focus = app.session.focus.clone();
    let mode = app.session.mode;

    if let Some(block) = app.session.displayed_block_mut() {
        render_selected_query(f, area, block, hist_idx, &focus, mode);
    } else {
        let empty = Paragraph::new("No queries yet. Press 'n' to write a new query.")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL).title("Query Results"));
        f.render_widget(empty, area);
    }
}

/// Determine which pane is expanded based on focus and mode
/// In Navigation mode: all panes expanded
/// In Edit mode on a specific pane: only that pane expanded
fn focused_pane(focus: &Focus, mode: Mode) -> Option<SubPane> {
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
) {
    // Determine which pane (if any) is exclusively expanded
    let focused = focused_pane(focus, mode);

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
            } else if block.error.is_some() {
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
        if is_pane_expanded(SubPane::Sql, focused) {
            // Lines needed = SQL line count + 2 (borders)
            let sql_lines = block.sql.lines().count() as u16 + 2;
            // Max height = equal share (total height / 4 panes)
            let max_height = inner.height / 4;
            Constraint::Length(sql_lines.min(max_height).max(3))
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

    if expanded {
        let sql_block = Block::default()
            .borders(Borders::ALL)
            .title(format!("{} SQL", expand_char))
            .border_style(style);
        let para = Paragraph::new(block.sql.as_str())
            .block(sql_block)
            .wrap(Wrap { trim: false })
            .scroll((block.sql_scroll, 0));
        f.render_widget(para, area);
    } else {
        // Collapsed: show truncated SQL
        let sql_preview: String = block.sql.chars().take(60).collect();
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
    let expand_char = if expanded { "▼" } else { "▶" };
    let row_count = block.result_count();

    // Update visible dimensions for scroll calculations
    if let Some(ref mut table) = block.results {
        table.set_visible_height(area.height);
        table.set_visible_width(area.width);
    }

    if let Some(ref error) = block.error {
        let error_block = Block::default()
            .borders(Borders::ALL)
            .title(format!("{} Error", expand_char))
            .border_style(Style::default().fg(Color::Red));
        let para = Paragraph::new(error.as_str())
            .block(error_block)
            .style(Style::default().fg(Color::Red))
            .wrap(Wrap { trim: false });
        f.render_widget(para, area);
        return;
    }

    if expanded {
        if let Some(ref table) = block.results {
            // Check if we're in FieldValue mode and should show stats panel
            let show_stats = matches!(table.view_mode, ResultsViewMode::FieldValue { .. });

            if show_stats {
                // Split area: main view (top), stats panel (bottom 5 lines)
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(0), Constraint::Length(5)])
                    .split(area);

                let title = format!("{} Results", expand_char);
                let widget = table.render(&title, chunks[0].width, style);
                f.render_widget(widget, chunks[0]);

                // Render stats panel
                if let ResultsViewMode::FieldValue { field, ref path, .. } = table.view_mode {
                    render_path_stats_panel(f, chunks[1], table.get_path_stats(field, path), style);
                }
            } else {
                let title = format!("{} Results", expand_char);
                let widget = table.render(&title, area.width, style);
                f.render_widget(widget, area);
            }
        } else if block.running {
            let block_widget = Block::default()
                .borders(Borders::ALL)
                .title(format!("{} Results (loading...)", expand_char))
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
    let elapsed_ns = block.stats.elapsed_ns;

    // Calculate rates
    let read_rows_rate = calc_rate(block.stats.rows_read, elapsed_ns);
    let read_bytes_rate = calc_rate(block.stats.bytes_read, elapsed_ns);
    let write_rows_rate = calc_rate(block.stats.rows_written, elapsed_ns);
    let write_bytes_rate = calc_rate(block.stats.bytes_written, elapsed_ns);

    // Progress ratio (if we know total)
    let progress = block.stats.total_rows.and_then(|total| {
        if total > 0 {
            Some((block.stats.rows_read as f64 / total as f64).min(1.0))
        } else {
            None
        }
    });

    let has_writes = block.stats.rows_written > 0;

    if expanded {
        // Check if we're in expanded metric view
        match block.stats.view_mode {
            MetricsViewMode::Expanded { index } => {
                render_stats_metric_expanded(f, area, block, index, style);
            }
            MetricsViewMode::Table => {
                // Split area: header lines + progress line + metrics table
                let text_lines: u16 = if has_writes { 3 } else { 2 };

                let constraints: Vec<Constraint> =
                    vec![Constraint::Length(text_lines), Constraint::Length(1), Constraint::Min(0)];

                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints(constraints)
                    .split(area);

                // Line 1: Read stats with rates
                let read_line = format!(
                    "{} Stats: Read {} rows ({}) @ {}/s, {}/s",
                    expand_char,
                    format_number(block.stats.rows_read),
                    format_bytes(block.stats.bytes_read),
                    format_rate(read_rows_rate),
                    format_bytes(read_bytes_rate as u64),
                );

                let mut lines = vec![Line::from(read_line)];

                // Line 2: Write stats (if any)
                if has_writes {
                    let write_line = format!(
                        "         Write {} rows ({}) @ {}/s, {}/s",
                        format_number(block.stats.rows_written),
                        format_bytes(block.stats.bytes_written),
                        format_rate(write_rows_rate),
                        format_bytes(write_bytes_rate as u64),
                    );
                    lines.push(Line::from(write_line));
                }

                // CPU and RAM sparklines (fixed 16 char width)
                let cpu_sparkline = sparkline_str(&block.stats.cpu_history, 16);
                let ram_sparkline = sparkline_str(&block.stats.ram_history, 16);
                let peak_ram_sparkline = sparkline_str(&block.stats.peak_ram_history, 16);
                let cpu_str = format!("CPU {} {}%", cpu_sparkline, block.stats.cpu_current);
                let ram_str = format!(
                    "RAM {} {} (peak {} {})",
                    ram_sparkline,
                    format_bytes(block.stats.ram_current),
                    peak_ram_sparkline,
                    format_bytes(block.stats.peak_ram_current)
                );
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
                    let dots = ".".repeat((block.stats.rows_read as usize / 1000) % 4);
                    let progress_text =
                        format!("Reading{} {} rows", dots, format_number(block.stats.rows_read));
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
        let cpu_pct = block.stats.cpu_current;
        let ram_str = format_bytes(block.stats.ram_current);
        let peak_ram_str = format_bytes(block.stats.peak_ram_current);

        let text = if has_writes {
            format!(
                "{} Stats: R {} @ {}/s | W {} @ {}/s | CPU {}% | RAM {} (peak {})",
                expand_char,
                format_number(block.stats.rows_read),
                format_rate(read_rows_rate),
                format_number(block.stats.rows_written),
                format_rate(write_rows_rate),
                cpu_pct,
                ram_str,
                peak_ram_str
            )
        } else {
            format!(
                "{} Stats: R {} @ {}/s | CPU {}% | RAM {} (peak {})",
                expand_char,
                format_number(block.stats.rows_read),
                format_rate(read_rows_rate),
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
    let metric_count = block.stats.metric_count();

    if metric_count == 0 {
        let empty = Paragraph::new("No profile events").style(Style::default().fg(Color::DarkGray));
        f.render_widget(empty, area);
        return;
    }

    // Calculate visible height (area height - 3 for borders and header row)
    let visible_height = area.height.saturating_sub(3) as usize;
    block.stats.visible_height = visible_height.max(1);

    let scroll_offset = block.stats.scroll_offset;
    let selected_row = block.stats.selected_row;

    // Build only visible rows (skip to scroll_offset, take visible_height)
    let rows: Vec<Row> = block
        .stats
        .metric_names
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_height)
        .filter_map(|(i, name)| {
            let metric = block.stats.metrics.get(name)?;
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
    let metric_name = match block.stats.metric_names.get(index) {
        Some(name) => name,
        None => {
            let empty = Paragraph::new("Metric not found").style(Style::default().fg(Color::Red));
            f.render_widget(empty, area);
            return;
        }
    };

    let metric = match block.stats.metrics.get(metric_name) {
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

    let expand_char = if expanded { "▼" } else { "▶" };
    let thread_count = block.logs_data.thread_count();
    let total_logs = block.logs_data.total_log_count();

    // Update visible height for scroll calculations
    block.logs_data.visible_height = area.height.saturating_sub(3) as usize;

    if !expanded || thread_count == 0 {
        let text =
            format!("{} Logs ({} threads, {} entries)", expand_char, thread_count, total_logs);
        let para = Paragraph::new(text).style(style);
        f.render_widget(para, area);
        return;
    }

    match block.logs_data.view_mode {
        LogsViewMode::Grouped => {
            render_logs_grouped(f, area, block, expand_char, style);
        }
        LogsViewMode::Expanded { thread_id } => {
            render_logs_thread_expanded(f, area, block, thread_id, style);
        }
    }
}

fn render_logs_grouped(
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
        .sorted_threads
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_height)
        .filter_map(|(i, thread_id)| {
            let group = logs_data.groups.get(thread_id)?;

            let row_style = if i == selected_row {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            } else {
                Style::default()
            };

            let text_truncated: String =
                group.latest_text.chars().take(60).collect::<String>().replace('\n', " ");
            let text_display = if group.latest_text.len() > 60 {
                format!("{}...", text_truncated)
            } else {
                text_truncated
            };

            Some(
                Row::new(vec![
                    format!("{}", group.thread_id),
                    format!("({})", group.entries.len()),
                    group.latest_source.clone(),
                    text_display,
                ])
                .style(row_style),
            )
        })
        .collect();

    let widths = [
        Constraint::Length(12),
        Constraint::Length(6),
        Constraint::Length(20),
        Constraint::Min(30),
    ];

    let title = format!(
        "{} Logs ({} threads, {} total)",
        expand_char,
        logs_data.thread_count(),
        logs_data.total_log_count()
    );

    let table = Table::new(rows, widths)
        .header(
            Row::new(vec!["Thread", "Count", "Source", "Latest Text"])
                .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::ALL).title(title).border_style(style));

    f.render_widget(table, area);
}

fn render_logs_thread_expanded(
    f: &mut Frame,
    area: Rect,
    block: &QueryBlock,
    thread_id: u64,
    style: Style,
) {
    let logs_data = &block.logs_data;

    let group = match logs_data.groups.get(&thread_id) {
        Some(g) => g,
        None => {
            let para = Paragraph::new("Thread not found").style(Style::default().fg(Color::Red));
            f.render_widget(para, area);
            return;
        }
    };

    let visible_height = logs_data.visible_height;
    let scroll_offset = logs_data.expanded_scroll;
    let selected_row = logs_data.expanded_selected;

    let rows: Vec<Row> = group
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

            let text_truncated: String =
                entry.text.chars().take(80).collect::<String>().replace('\n', " ");
            let text_display = if entry.text.len() > 80 {
                format!("{}...", text_truncated)
            } else {
                text_truncated
            };

            Row::new(vec![entry.time.clone(), entry.source.clone(), text_display]).style(row_style)
        })
        .collect();

    let widths = [Constraint::Length(26), Constraint::Length(20), Constraint::Min(40)];

    let title =
        format!("Thread {} ({} entries) - h/Left to go back", thread_id, group.entries.len());

    let table = Table::new(rows, widths)
        .header(
            Row::new(vec!["Time", "Source", "Text"])
                .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::ALL).title(title).border_style(style));

    f.render_widget(table, area);
}

fn render_new_query_fullscreen(f: &mut Frame, area: Rect, app: &App) {
    let style = Style::default().fg(Color::Cyan);

    let title = if app.session.mode == Mode::Edit {
        "New Query [EDIT] (Ctrl+Enter: run, Escape: cancel, Ctrl+P/N: history)"
    } else {
        "New Query (press Enter to edit, Escape to cancel)"
    };

    let block = Block::default().borders(Borders::ALL).title(title).border_style(style);

    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(&app.session.new_query, inner);
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
    let mode_str = match app.session.mode {
        Mode::Navigation => "NAV",
        Mode::Edit => "EDIT",
    };

    let focus_str = match &app.session.focus {
        Focus::NewQuery => "New Query".to_string(),
        Focus::Sidebar => "Sidebar".to_string(),
        Focus::SubPane(pane) => {
            let pane_name = match pane {
                SubPane::Sql => "SQL",
                SubPane::Results => "Results",
                SubPane::Stats => "Stats",
                SubPane::Logs => "Logs",
            };
            let query_str = app
                .session
                .selected_history
                .map(|id| format!("Q{}", id + 1))
                .unwrap_or_else(|| "?".to_string());
            format!("{} > {}", query_str, pane_name)
        }
    };

    let query_count = app.session.history.len();
    let query_info =
        if query_count > 0 { format!(" ({} queries)", query_count) } else { String::new() };

    // Context-sensitive hints
    let hints = if matches!(app.session.focus, Focus::NewQuery) {
        "Ctrl+Enter: run | Esc: cancel | Ctrl+P/N: history | ?: help".to_string()
    } else {
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
        Line::from("Navigation Mode:"),
        Line::from("  j/Down            Move down"),
        Line::from("  k/Up              Move up"),
        Line::from("  l/Right/Enter     Enter pane (auto-expands)"),
        Line::from("  h/Left/Esc        Exit pane (auto-collapses)"),
        Line::from("  n                 Jump to New Query"),
        Line::from("  ?                 Toggle help"),
        Line::from("  Ctrl+Q            Quit"),
        Line::from(""),
        Line::from("Edit Mode (New Query):"),
        Line::from("  Escape            Exit to navigation mode"),
        Line::from("  Ctrl+Enter        Execute query"),
        Line::from("  Alt+Enter         Execute query (alternative)"),
        Line::from("  Ctrl+P            Previous history entry"),
        Line::from("  Ctrl+N            Next history entry"),
        Line::from(""),
        Line::from("Edit Mode (Results/Stats/Logs):"),
        Line::from("  j/k or Up/Down    Navigate rows"),
        Line::from("  l/Right           Drill into row"),
        Line::from("  h/Left            Go back (or exit pane)"),
        Line::from("  Alt+h/l           Scroll columns"),
        Line::from("  PgUp/PgDown       Page navigation"),
        Line::from("  s                 Sort by column"),
        Line::from("  y                 Copy to clipboard (Results only)"),
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
