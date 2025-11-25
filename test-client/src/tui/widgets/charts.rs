use ratatui::style::{Color, Style};
use ratatui::symbols;
use ratatui::text::Span;
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, GraphType};

#[derive(Debug, Clone)]
pub struct TimeSeriesData {
    pub name:   String,
    pub points: Vec<(f64, f64)>,
    pub color:  Color,
}

impl TimeSeriesData {
    pub fn new(name: String, color: Color) -> Self { Self { name, points: Vec::new(), color } }

    pub fn add_point(&mut self, x: f64, y: f64) { self.points.push((x, y)); }

    pub fn clear(&mut self) { self.points.clear(); }

    pub fn bounds(&self) -> (f64, f64, f64, f64) {
        if self.points.is_empty() {
            return (0.0, 1.0, 0.0, 1.0);
        }

        let mut min_x = f64::MAX;
        let mut max_x = f64::MIN;
        let mut min_y = f64::MAX;
        let mut max_y = f64::MIN;

        for (x, y) in &self.points {
            min_x = min_x.min(*x);
            max_x = max_x.max(*x);
            min_y = min_y.min(*y);
            max_y = max_y.max(*y);
        }

        (min_x, max_x, min_y, max_y)
    }
}

pub fn render_time_series_chart<'a>(series: &'a [TimeSeriesData], title: &'a str) -> Chart<'a> {
    let datasets: Vec<Dataset> = series
        .iter()
        .map(|s| {
            Dataset::default()
                .name(s.name.as_str())
                .marker(symbols::Marker::Dot)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(s.color))
                .data(&s.points)
        })
        .collect();

    let (min_x, max_x, min_y, max_y) = if series.is_empty() {
        (0.0, 1.0, 0.0, 1.0)
    } else {
        let bounds: Vec<_> = series.iter().map(|s| s.bounds()).collect();
        let min_x = bounds.iter().map(|(x, _, _, _)| *x).fold(f64::MAX, f64::min);
        let max_x = bounds.iter().map(|(_, x, _, _)| *x).fold(f64::MIN, f64::max);
        let min_y = bounds.iter().map(|(_, _, y, _)| *y).fold(f64::MAX, f64::min);
        let max_y = bounds.iter().map(|(_, _, _, y)| *y).fold(f64::MIN, f64::max);
        (min_x, max_x, min_y, max_y)
    };

    let x_axis = Axis::default()
        .style(Style::default().fg(Color::Gray))
        .bounds([min_x, max_x])
        .labels(vec![
            Span::raw(format!("{:.1}", min_x)),
            Span::raw(format!("{:.1}", (min_x + max_x) / 2.0)),
            Span::raw(format!("{:.1}", max_x)),
        ]);

    let y_axis = Axis::default()
        .style(Style::default().fg(Color::Gray))
        .bounds([min_y, max_y * 1.1])
        .labels(vec![
            Span::raw(format!("{:.1}", min_y)),
            Span::raw(format!("{:.1}", (min_y + max_y) / 2.0)),
            Span::raw(format!("{:.1}", max_y)),
        ]);

    Chart::new(datasets)
        .block(Block::default().borders(Borders::ALL).title(title))
        .x_axis(x_axis)
        .y_axis(y_axis)
}
