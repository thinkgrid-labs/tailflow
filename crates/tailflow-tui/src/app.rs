use crate::ui;
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{io, time::Duration};
use tailflow_core::{LogReceiver, LogRecord};
use tokio::sync::broadcast;

const MAX_RECORDS: usize = 2000;

pub struct App {
    pub records: Vec<LogRecord>,
    pub filter: String,
    pub filter_mode: bool,
    rx: LogReceiver,
    pub scroll: usize,
    pub source_colors: SourceColorMap,
}

pub struct SourceColorMap {
    inner: std::collections::HashMap<String, ratatui::style::Color>,
    palette: Vec<ratatui::style::Color>,
    next: usize,
}

impl SourceColorMap {
    fn new() -> Self {
        use ratatui::style::Color;
        Self {
            inner: Default::default(),
            palette: vec![
                Color::Cyan,
                Color::Green,
                Color::Yellow,
                Color::Magenta,
                Color::Blue,
                Color::LightCyan,
                Color::LightGreen,
                Color::LightYellow,
                Color::LightMagenta,
                Color::LightBlue,
            ],
            next: 0,
        }
    }

    pub fn color_for(&mut self, source: &str) -> ratatui::style::Color {
        if let Some(&c) = self.inner.get(source) {
            return c;
        }
        let c = self.palette[self.next % self.palette.len()];
        self.next += 1;
        self.inner.insert(source.to_string(), c);
        c
    }
}

impl App {
    pub fn new(rx: LogReceiver) -> Self {
        Self {
            records: Vec::with_capacity(MAX_RECORDS),
            filter: String::new(),
            filter_mode: false,
            rx,
            scroll: 0,
            source_colors: SourceColorMap::new(),
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let result = self.event_loop(&mut terminal).await;

        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;

        result
    }

    async fn event_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<()> {
        loop {
            // Drain all pending log records (non-blocking)
            loop {
                match self.rx.try_recv() {
                    Ok(record) => {
                        // Pre-assign color so the map is populated before render
                        self.source_colors.color_for(&record.source);
                        self.records.push(record);
                        if self.records.len() > MAX_RECORDS {
                            self.records.remove(0);
                        }
                    }
                    Err(broadcast::error::TryRecvError::Empty) => break,
                    Err(broadcast::error::TryRecvError::Lagged(n)) => {
                        tracing::warn!(dropped = n, "TUI receiver lagged");
                    }
                    Err(broadcast::error::TryRecvError::Closed) => return Ok(()),
                }
            }

            terminal.draw(|f| ui::draw(f, self))?;

            // Poll for keyboard input with a short timeout so log records
            // are still processed even when no key is pressed.
            if event::poll(Duration::from_millis(50))? {
                if let Event::Key(key) = event::read()? {
                    match (self.filter_mode, key.code) {
                        // Quit
                        (false, KeyCode::Char('q')) | (false, KeyCode::Char('c'))
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            return Ok(())
                        }

                        // Enter filter mode
                        (false, KeyCode::Char('/')) => {
                            self.filter_mode = true;
                        }

                        // Scroll
                        (false, KeyCode::Down) | (false, KeyCode::Char('j')) => {
                            self.scroll = self.scroll.saturating_add(1);
                        }
                        (false, KeyCode::Up) | (false, KeyCode::Char('k')) => {
                            self.scroll = self.scroll.saturating_sub(1);
                        }
                        (false, KeyCode::Char('G')) => {
                            self.scroll = usize::MAX; // snap to bottom on next render
                        }

                        // Filter mode input
                        (true, KeyCode::Esc) => {
                            self.filter_mode = false;
                        }
                        (true, KeyCode::Enter) => {
                            self.filter_mode = false;
                            self.scroll = usize::MAX;
                        }
                        (true, KeyCode::Backspace) => {
                            self.filter.pop();
                        }
                        (true, KeyCode::Char(c)) => {
                            self.filter.push(c);
                        }

                        _ => {}
                    }
                }
            }
        }
    }
}
