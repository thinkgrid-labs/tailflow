use crate::app::App;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame,
};
use tailflow_core::{json::flatten_json, LogLevel};

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(area);

    // Guard against degenerate terminal sizes
    if chunks.len() < 3 {
        return;
    }
    let list_height = chunks[1].height as usize;

    // ── Build filter predicate (uses pre-compiled regex from App) ──────────
    let filter_lower = app.filter.to_lowercase();

    let matches = |payload: &str, source: &str| -> bool {
        if app.filter.is_empty() {
            return true;
        }
        if let Some(re) = &app.filter_re {
            re.is_match(payload) || re.is_match(source)
        } else {
            payload.to_lowercase().contains(&filter_lower)
                || source.to_lowercase().contains(&filter_lower)
        }
    };

    // ── Count filtered records and clamp scroll ────────────────────────────
    let filtered_count = app
        .records
        .iter()
        .filter(|r| matches(&r.payload, &r.source))
        .count();

    let max_scroll = filtered_count.saturating_sub(list_height);
    if app.scroll >= max_scroll {
        app.scroll = max_scroll;
    }
    let scroll = app.scroll;

    // ── Collect visible records as owned data (drops borrow on app.records) ─
    let pretty_json = app.pretty_json;
    let visible_data: Vec<(String, String, LogLevel, String)> = app
        .records
        .iter()
        .filter(|r| matches(&r.payload, &r.source))
        .skip(scroll)
        .take(list_height)
        .map(|r| {
            let payload = if pretty_json {
                flatten_json(&r.payload).unwrap_or_else(|| r.payload.clone())
            } else {
                r.payload.clone()
            };
            (
                r.timestamp.format("%H:%M:%S%.3f").to_string(),
                r.source.clone(),
                r.level,
                payload,
            )
        })
        .collect();

    // ── Assign source colors (needs &mut app.source_colors) ───────────────
    let colors: Vec<Color> = visible_data
        .iter()
        .map(|(_, src, _, _)| app.source_colors.color_for(src))
        .collect();

    // ── Header ─────────────────────────────────────────────────────────────
    let json_label = if app.pretty_json {
        "p:json-on"
    } else {
        "p:json-off"
    };
    let header_text = format!(
        " TailFlow  |  {} records  |  / filter  |  {}  |  q quit",
        app.records.len(),
        json_label,
    );
    let header = Paragraph::new(header_text)
        .style(Style::default().fg(Color::White).bg(Color::DarkGray))
        .block(Block::default().borders(Borders::NONE));
    f.render_widget(header, chunks[0]);

    // ── Log list ───────────────────────────────────────────────────────────
    let list_items: Vec<ListItem> = visible_data
        .iter()
        .zip(colors.iter())
        .map(|((ts, src, level, payload), &src_color)| {
            let level_color = match level {
                LogLevel::Error => Color::Red,
                LogLevel::Warn => Color::Yellow,
                LogLevel::Info => Color::Green,
                LogLevel::Debug => Color::Blue,
                LogLevel::Trace => Color::DarkGray,
                LogLevel::Unknown => Color::White,
            };

            let level_str = format!("{:?}", level).to_uppercase();
            let line = Line::from(vec![
                Span::styled(format!("{} ", ts), Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{:<20} ", src),
                    Style::default().fg(src_color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:5} ", level_str),
                    Style::default().fg(level_color),
                ),
                Span::raw(payload.as_str()),
            ]);
            ListItem::new(line)
        })
        .collect();

    let log_block = Block::default()
        .title(" Logs ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let list = List::new(list_items).block(log_block);
    f.render_widget(list, chunks[1]);

    // ── Filter bar ─────────────────────────────────────────────────────────
    let filter_label = if app.filter_mode {
        "Filter> "
    } else {
        "Filter: "
    };
    let filter_style = if app.filter_mode {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let filter_bar = Paragraph::new(format!("{}{}", filter_label, app.filter))
        .style(filter_style)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(filter_style),
        );
    f.render_widget(filter_bar, chunks[2]);
}
