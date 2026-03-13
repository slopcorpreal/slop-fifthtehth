use clap::Parser;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use memchr::{memchr_iter, memmem};
use memmap2::MmapOptions;
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
    Terminal,
};
use rayon::prelude::*;
use std::{
    fs::File,
    io,
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Parser, Debug)]
#[command(name = "Tachyon", about = "Faster-than-light JSON log explorer")]
struct Args {
    /// Path to the JSONL log file
    file: PathBuf,
}

#[derive(PartialEq)]
enum AppMode {
    Normal,
    Filtering,
    Inspecting,
}

struct App {
    mmap: memmap2::Mmap,
    line_starts: Vec<usize>,
    filtered_indices: Vec<usize>,
    parsed_line_cache: Vec<Option<(String, String, String)>>,
    state: TableState,
    mode: AppMode,
    search_query: String,
    selected_json: Option<String>,
    popup_scroll: u16,
    load_time: Duration,
    filter_time: Duration,
}

impl App {
    fn new(file_path: PathBuf) -> io::Result<Self> {
        let start = Instant::now();
        let file = File::open(&file_path)?;
        let mmap = unsafe { MmapOptions::new().map(&file)? };
        let line_starts = build_line_starts(&mmap);
        let load_time = start.elapsed();
        let line_count = line_starts.len().saturating_sub(1);
        let filtered_indices = (0..line_count).collect();
        let parsed_line_cache = (0..line_count).map(|_| None).collect();

        Ok(Self {
            mmap,
            line_starts,
            filtered_indices,
            parsed_line_cache,
            state: TableState::default(),
            mode: AppMode::Normal,
            search_query: String::new(),
            selected_json: None,
            popup_scroll: 0,
            load_time,
            filter_time: Duration::ZERO,
        })
    }

    fn apply_filter(&mut self) {
        let start = Instant::now();
        if self.search_query.is_empty() {
            self.filtered_indices = (0..self.line_starts.len().saturating_sub(1)).collect();
        } else {
            let query = self.search_query.as_bytes();
            self.filtered_indices = (0..self.line_starts.len().saturating_sub(1))
                .into_par_iter()
                .filter(|&i| {
                    let s = self.line_starts[i];
                    let e = self.line_starts[i + 1];
                    let line = &self.mmap[s..e];
                    memmem::find(line, query).is_some()
                })
                .collect();
        }

        self.state.select(Some(0));
        self.filter_time = start.elapsed();
    }

    fn parse_line_cached(&mut self, line_index: usize) -> &(String, String, String) {
        if self.parsed_line_cache[line_index].is_none() {
            let line_bytes = &self.mmap[self.line_starts[line_index]..self.line_starts[line_index + 1]];
            self.parsed_line_cache[line_index] = Some(parse_line(line_bytes));
        }
        self.parsed_line_cache[line_index]
            .as_ref()
            .expect("parsed line cache populated")
    }
}

fn build_line_starts(bytes: &[u8]) -> Vec<usize> {
    let mut line_starts = Vec::with_capacity(bytes.len() / 80 + 2);
    line_starts.push(0);
    line_starts.extend(memchr_iter(b'\n', bytes).map(|i| i + 1));
    if line_starts.last().copied().unwrap_or(0) != bytes.len() {
        line_starts.push(bytes.len());
    }
    line_starts
}

fn parse_line(bytes: &[u8]) -> (String, String, String) {
    if let Ok(val) = serde_json::from_slice::<serde_json::Value>(bytes) {
        let ts = val
            .get("time")
            .or_else(|| val.get("timestamp"))
            .or_else(|| val.get("@timestamp"));
        let ts_str = ts.and_then(|v| v.as_str()).unwrap_or("-").to_string();

        let lvl = val.get("level").or_else(|| val.get("severity"));
        let lvl_str = lvl
            .and_then(|v| v.as_str())
            .unwrap_or("INFO")
            .to_uppercase();

        let msg = val
            .get("msg")
            .or_else(|| val.get("message"))
            .or_else(|| val.get("log"));
        let msg_str = msg.and_then(|v| v.as_str()).unwrap_or("").to_string();

        if ts_str == "-" && lvl_str == "INFO" && msg_str.is_empty() {
            (ts_str, lvl_str, String::from_utf8_lossy(bytes).trim().to_string())
        } else {
            (ts_str, lvl_str, msg_str)
        }
    } else {
        (
            "-".to_string(),
            "RAW".to_string(),
            String::from_utf8_lossy(bytes).trim().to_string(),
        )
    }
}

const MAX_INSPECT_BYTES: usize = 128 * 1024;

fn inspection_text(line_bytes: &[u8]) -> String {
    if line_bytes.len() > MAX_INSPECT_BYTES {
        let preview = String::from_utf8_lossy(&line_bytes[..MAX_INSPECT_BYTES]);
        return format!(
            "Line too large to inspect safely ({} bytes). Showing first {} bytes only.\n\n{}",
            line_bytes.len(),
            MAX_INSPECT_BYTES,
            preview
        );
    }

    let line = String::from_utf8_lossy(line_bytes);
    if let Ok(val) = serde_json::from_slice::<serde_json::Value>(line_bytes) {
        serde_json::to_string_pretty(&val).unwrap_or_else(|_| line.to_string())
    } else {
        line.to_string()
    }
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

fn draw(f: &mut ratatui::Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(f.size());

    let header_text = match app.mode {
        AppMode::Filtering => format!(" Filter: {}_ ", app.search_query),
        _ => format!(
            " Tachyon | Total Lines: {} | Filtered: {} | Mmap Time: {:?} | Filter Time: {:?} ",
            app.line_starts.len().saturating_sub(1),
            app.filtered_indices.len(),
            app.load_time,
            app.filter_time
        ),
    };

    let header = Paragraph::new(header_text)
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(header, chunks[0]);

    let height = chunks[1].height.saturating_sub(2) as usize;
    let total = app.filtered_indices.len();

    let mut offset = app.state.selected().unwrap_or(0).saturating_sub(height / 2);
    if offset + height > total {
        offset = total.saturating_sub(height);
    }

    let mut rows = vec![];
    let relative_selected = app.state.selected().unwrap_or(0).saturating_sub(offset);

    for i in 0..height {
        let idx = offset + i;
        if idx >= total {
            break;
        }

        let real_idx = app.filtered_indices[idx];
        let (ts, lvl, msg) = app.parse_line_cached(real_idx).clone();

        let lvl_color = match lvl.as_str() {
            "ERROR" | "ERR" | "FATAL" => Color::Red,
            "WARN" | "WARNING" => Color::Yellow,
            "INFO" => Color::Green,
            "DEBUG" => Color::Blue,
            "TRACE" => Color::Magenta,
            _ => Color::White,
        };

        rows.push(Row::new(vec![
            Cell::from(ts),
            Cell::from(lvl).style(Style::default().fg(lvl_color).add_modifier(Modifier::BOLD)),
            Cell::from(msg),
        ]));
    }

    let mut table_state = TableState::default();
    if !rows.is_empty() {
        table_state.select(Some(relative_selected.min(rows.len().saturating_sub(1))));
    }

    let table = Table::new(
        rows,
        [
            Constraint::Length(24),
            Constraint::Length(8),
            Constraint::Percentage(100),
        ],
    )
    .header(
        Row::new(vec!["Timestamp", "Level", "Message"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::ALL).title(" Logs "))
    .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    f.render_stateful_widget(table, chunks[1], &mut table_state);

    let footer_text = match app.mode {
        AppMode::Normal => {
            " [q] Quit | [f] Filter | [Enter] Inspect JSON | [g/G] Top/Bottom | [↑/↓] Navigate "
        }
        AppMode::Filtering => " [Enter] Apply Filter | [Esc] Cancel ",
        AppMode::Inspecting => " [Esc] Close Inspector | [↑/↓] Scroll JSON ",
    };
    let footer = Paragraph::new(footer_text).style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, chunks[2]);

    if app.mode == AppMode::Inspecting {
        if let Some(ref json) = app.selected_json {
            let area = centered_rect(80, 80, f.size());
            let lines: Vec<Line> = json.lines().map(Line::from).collect();
            let paragraph = Paragraph::new(lines)
                .block(
                    Block::default()
                        .title(" Inspect (Esc to close) ")
                        .borders(Borders::ALL),
                )
                .wrap(Wrap { trim: false })
                .scroll((app.popup_scroll, 0));
            f.render_widget(Clear, area);
            f.render_widget(paragraph, area);
        }
    }
}

fn run_app(terminal: &mut Terminal<impl Backend>, mut app: App) -> io::Result<()> {
    app.state.select(Some(0));
    loop {
        terminal.draw(|f| draw(f, &mut app))?;

        if let Event::Key(key) = event::read()? {
            match app.mode {
                AppMode::Normal => match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Down | KeyCode::Char('j') => {
                        let i = app.state.selected().unwrap_or(0);
                        if i < app.filtered_indices.len().saturating_sub(1) {
                            app.state.select(Some(i + 1));
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        let i = app.state.selected().unwrap_or(0);
                        if i > 0 {
                            app.state.select(Some(i - 1));
                        }
                    }
                    KeyCode::PageDown => {
                        let i = app.state.selected().unwrap_or(0);
                        app.state.select(Some(
                            (i + 20).min(app.filtered_indices.len().saturating_sub(1)),
                        ));
                    }
                    KeyCode::PageUp => {
                        let i = app.state.selected().unwrap_or(0);
                        app.state.select(Some(i.saturating_sub(20)));
                    }
                    KeyCode::Char('g') => app.state.select(Some(0)),
                    KeyCode::Char('G') => app
                        .state
                        .select(Some(app.filtered_indices.len().saturating_sub(1))),
                    KeyCode::Char('f') | KeyCode::Char('/') => {
                        app.mode = AppMode::Filtering;
                        app.search_query.clear();
                    }
                    KeyCode::Enter => {
                        if let Some(i) = app.state.selected() {
                            if i < app.filtered_indices.len() {
                                let real_idx = app.filtered_indices[i];
                                let line_bytes = &app.mmap
                                    [app.line_starts[real_idx]..app.line_starts[real_idx + 1]];
                                app.selected_json = Some(inspection_text(line_bytes));
                                app.popup_scroll = 0;
                                app.mode = AppMode::Inspecting;
                            }
                        }
                    }
                    KeyCode::Esc => {
                        app.search_query.clear();
                        app.apply_filter();
                    }
                    _ => {}
                },
                AppMode::Filtering => match key.code {
                    KeyCode::Enter => {
                        app.apply_filter();
                        app.mode = AppMode::Normal;
                    }
                    KeyCode::Esc => app.mode = AppMode::Normal,
                    KeyCode::Backspace => {
                        app.search_query.pop();
                    }
                    KeyCode::Char(c) => {
                        app.search_query.push(c);
                    }
                    _ => {}
                },
                AppMode::Inspecting => match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        app.mode = AppMode::Normal;
                        app.selected_json = None;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        app.popup_scroll = app.popup_scroll.saturating_add(1)
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        app.popup_scroll = app.popup_scroll.saturating_sub(1)
                    }
                    _ => {}
                },
            }
        }
    }
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    let file = File::open(&args.file)?;
    if file.metadata()?.len() == 0 {
        println!("File is empty.");
        return Ok(());
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let app = App::new(args.file)?;
    let result = run_app(&mut terminal, app);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = result {
        println!("{err:?}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{build_line_starts, inspection_text, parse_line, MAX_INSPECT_BYTES};

    #[test]
    fn build_line_starts_handles_trailing_newline() {
        let starts = build_line_starts(b"one\ntwo\n");
        assert_eq!(starts, vec![0, 4, 8]);
    }

    #[test]
    fn parse_line_extracts_common_json_fields() {
        let line = br#"{"time":"2026-03-13T12:00:00Z","level":"warn","message":"hello"}"#;
        let (ts, level, msg) = parse_line(line);
        assert_eq!(ts, "2026-03-13T12:00:00Z");
        assert_eq!(level, "WARN");
        assert_eq!(msg, "hello");
    }

    #[test]
    fn parse_line_falls_back_for_raw_text() {
        let (ts, level, msg) = parse_line(b"not json");
        assert_eq!(ts, "-");
        assert_eq!(level, "RAW");
        assert_eq!(msg, "not json");
    }

    #[test]
    fn inspection_text_pretty_prints_json() {
        let rendered = inspection_text(br#"{"a":1}"#);
        assert!(rendered.contains('\n'));
        assert!(rendered.contains("\"a\": 1"));
    }

    #[test]
    fn inspection_text_truncates_large_lines() {
        let long_line = vec![b'x'; MAX_INSPECT_BYTES + 1];
        let rendered = inspection_text(&long_line);
        assert!(rendered.contains("Line too large to inspect safely"));
        assert!(rendered.contains("Showing first 131072 bytes only"));
    }
}
