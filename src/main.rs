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
use serde::Deserialize;
use std::{
    env,
    fs,
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

#[derive(Parser, Debug)]
#[command(name = "Tachyon", about = "Faster-than-light JSON log explorer")]
struct Args {
    /// Path to the JSONL log file
    file: Option<PathBuf>,
    /// Checks for updates and exits.
    #[arg(long)]
    check_update: bool,
    /// Applies the latest available update and exits.
    #[arg(long)]
    self_update: bool,
    /// Skips confirmation prompts when self-updating.
    #[arg(long)]
    yes: bool,
}

#[derive(PartialEq)]
enum AppMode {
    Normal,
    Filtering,
    Inspecting,
}

const REPOSITORY_OWNER: &str = "slopcorpreal";
const REPOSITORY_NAME: &str = "slop-fifthtehth";
const CRATE_NAME: &str = "tachyon";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    fn parse(value: &str) -> Option<Self> {
        let trimmed = value.trim_start_matches('v');
        let mut pieces = trimmed.split('.');
        let major = pieces.next()?.parse().ok()?;
        let minor = pieces.next()?.parse().ok()?;
        let patch = pieces.next()?.parse().ok()?;
        if pieces.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdatePriority {
    None,
    Patch,
    Minor,
    Major,
}

impl UpdatePriority {
    fn requires_prompt(self) -> bool {
        matches!(self, Self::Minor | Self::Major)
    }
}

fn classify_update(current: Version, latest: Version) -> UpdatePriority {
    if latest < current {
        return UpdatePriority::None;
    }
    if latest == current {
        return UpdatePriority::None;
    }
    if latest.major > current.major {
        return UpdatePriority::Major;
    }
    if latest.minor > current.minor {
        return UpdatePriority::Minor;
    }
    UpdatePriority::Patch
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallMethod {
    Cargo,
    StandaloneBinary,
}

fn detect_install_method(executable_path: &Path) -> InstallMethod {
    if executable_path.components().any(|component| component.as_os_str() == "target") {
        return InstallMethod::Cargo;
    }
    let cargo_home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .or_else(|| {
            env::var_os("USERPROFILE").map(|profile| PathBuf::from(profile).join(".cargo"))
        });
    if let Some(home) = cargo_home {
        let cargo_bin = home.join("bin");
        if executable_path.starts_with(cargo_bin) {
            return InstallMethod::Cargo;
        }
    }
    InstallMethod::StandaloneBinary
}

#[derive(Deserialize)]
struct CratesVersionResponse {
    #[serde(rename = "crate")]
    crate_info: CratesVersionInfo,
}

#[derive(Deserialize)]
struct CratesVersionInfo {
    max_version: String,
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubReleaseAsset>,
}

#[derive(Deserialize)]
struct GitHubReleaseAsset {
    name: String,
    browser_download_url: String,
}

struct AvailableUpdate {
    latest: Version,
    download_url: Option<String>,
}

fn fetch_latest_crates_update() -> Result<AvailableUpdate, String> {
    let response: CratesVersionResponse = ureq::get(&format!(
        "https://crates.io/api/v1/crates/{CRATE_NAME}"
    ))
    .set("User-Agent", "tachyon-self-updater")
    .call()
    .map_err(|err| format!("failed to query crates.io: {err}"))?
    .into_json()
    .map_err(|err| format!("invalid crates.io response: {err}"))?;

    let latest = Version::parse(&response.crate_info.max_version)
        .ok_or_else(|| format!("invalid crates.io version: {}", response.crate_info.max_version))?;
    Ok(AvailableUpdate {
        latest,
        download_url: None,
    })
}

fn target_asset_name() -> Option<String> {
    let target = match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        _ => return None,
    };
    let suffix = if env::consts::OS == "windows" {
        ".exe"
    } else {
        ""
    };
    Some(format!("{CRATE_NAME}-{target}{suffix}"))
}

fn fetch_latest_github_release_update() -> Result<AvailableUpdate, String> {
    let release: GitHubRelease = ureq::get(&format!(
        "https://api.github.com/repos/{REPOSITORY_OWNER}/{REPOSITORY_NAME}/releases/latest"
    ))
    .set("User-Agent", "tachyon-self-updater")
    .call()
    .map_err(|err| format!("failed to query latest GitHub release: {err}"))?
    .into_json()
    .map_err(|err| format!("invalid GitHub release response: {err}"))?;

    let latest = Version::parse(&release.tag_name)
        .ok_or_else(|| format!("invalid release tag version: {}", release.tag_name))?;

    let asset_name = target_asset_name().ok_or_else(|| {
        format!(
            "unsupported platform for standalone updater: {}-{}",
            env::consts::OS,
            env::consts::ARCH
        )
    })?;
    let asset = release
        .assets
        .into_iter()
        .find(|entry| entry.name == asset_name)
        .ok_or_else(|| format!("release asset '{asset_name}' not found"))?;

    Ok(AvailableUpdate {
        latest,
        download_url: Some(asset.browser_download_url),
    })
}

fn prompt_user_for_update(priority: UpdatePriority, latest: Version) -> io::Result<bool> {
    let update_type = match priority {
        UpdatePriority::Minor => "minor",
        UpdatePriority::Major => "major",
        _ => "patch",
    };
    print!(
        "A {update_type} update to version {latest} is available. Apply now? [y/N]: "
    );
    io::stdout().flush()?;
    let mut response = String::new();
    io::stdin().read_line(&mut response)?;
    Ok(matches!(response.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn perform_cargo_update() -> Result<(), String> {
    let status = Command::new("cargo")
        .args(["install", "--force", CRATE_NAME])
        .status()
        .map_err(|err| format!("failed to execute cargo install: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo install failed with status: {status}"))
    }
}

fn perform_standalone_update(download_url: &str, executable_path: &Path) -> Result<(), String> {
    let mut reader = ureq::get(download_url)
        .set("User-Agent", "tachyon-self-updater")
        .call()
        .map_err(|err| format!("failed to download release asset: {err}"))?
        .into_reader();
    let mut binary = Vec::new();
    reader
        .read_to_end(&mut binary)
        .map_err(|err| format!("failed to read downloaded asset: {err}"))?;

    let temp_path = executable_path.with_extension("download");
    fs::write(&temp_path, &binary).map_err(|err| format!("failed to stage update: {err}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o755))
            .map_err(|err| format!("failed to set executable permissions: {err}"))?;
    }

    #[cfg(windows)]
    {
        let staged_path = executable_path.with_extension("new.exe");
        fs::rename(&temp_path, &staged_path)
            .map_err(|err| format!("failed to stage updated binary: {err}"))?;
        println!(
            "Downloaded update to '{}'. Replace the running binary after exit.",
            staged_path.display()
        );
        return Ok(());
    }

    #[cfg(not(windows))]
    {
        let backup_path = executable_path.with_extension("backup");
        if let Err(err) = fs::remove_file(&backup_path) {
            if err.kind() != io::ErrorKind::NotFound {
                eprintln!(
                    "warning: failed to clear previous backup '{}': {err}",
                    backup_path.display()
                );
            }
        }
        fs::rename(executable_path, &backup_path)
            .map_err(|err| format!("failed to move current binary aside: {err}"))?;
        fs::rename(&temp_path, executable_path)
            .map_err(|err| format!("failed to activate updated binary: {err}"))?;
        if let Err(err) = fs::remove_file(&backup_path) {
            eprintln!(
                "warning: updated binary activated but failed to remove backup '{}': {err}",
                backup_path.display()
            );
        }
    }

    Ok(())
}

fn check_or_apply_update(apply: bool, assume_yes: bool) -> Result<(), String> {
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .ok_or_else(|| format!("invalid current package version: {}", env!("CARGO_PKG_VERSION")))?;
    let executable_path =
        env::current_exe().map_err(|err| format!("failed to determine binary path: {err}"))?;
    let install_method = detect_install_method(&executable_path);
    let primary_fetch = match install_method {
        InstallMethod::Cargo => fetch_latest_crates_update()?,
        InstallMethod::StandaloneBinary => {
            let release = fetch_latest_github_release_update();
            if !apply {
                release.or_else(|err| {
                    eprintln!("{err}");
                    eprintln!(
                        "Falling back to crates.io for version checking only (not standalone binary replacement)."
                    );
                    fetch_latest_crates_update()
                })?
            } else {
                release?
            }
        }
    };
    let available = primary_fetch;

    let priority = classify_update(current, available.latest);
    if priority == UpdatePriority::None {
        if available.latest < current {
            println!(
                "Current version ({current}) is newer than the latest published version ({}).",
                available.latest
            );
        } else {
            println!("Tachyon is up to date ({current}).");
        }
        return Ok(());
    }

    println!("Current version: {current}");
    println!("Latest version: {}", available.latest);
    println!("Install method: {:?}", install_method);

    if !apply {
        return Ok(());
    }

    if priority.requires_prompt()
        && !assume_yes
        && !prompt_user_for_update(priority, available.latest)
            .map_err(|err| format!("failed to read prompt response: {err}"))?
    {
        println!("Update cancelled.");
        return Ok(());
    }

    match install_method {
        InstallMethod::Cargo => perform_cargo_update()?,
        InstallMethod::StandaloneBinary => {
            let url = available
                .download_url
                .as_deref()
                .ok_or_else(|| "missing release asset URL".to_string())?;
            perform_standalone_update(url, &executable_path)?;
        }
    }

    println!("Update complete.");
    Ok(())
}

type ParsedLine = (String, String, String);

struct App {
    mmap: memmap2::Mmap,
    line_starts: Vec<usize>,
    filtered_indices: Vec<usize>,
    parsed_line_cache: Vec<Option<Box<ParsedLine>>>,
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

    fn get_or_cache_parsed_line(&mut self, line_index: usize) -> &ParsedLine {
        if self.parsed_line_cache[line_index].is_none() {
            let line_bytes = &self.mmap[self.line_starts[line_index]..self.line_starts[line_index + 1]];
            self.parsed_line_cache[line_index] = Some(Box::new(parse_line(line_bytes)));
        }
        self.parsed_line_cache[line_index]
            .as_deref()
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
        let msg_str = msg
            .map(|value| match value {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();

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

    if let Ok(val) = serde_json::from_slice::<serde_json::Value>(line_bytes) {
        serde_json::to_string_pretty(&val)
            .unwrap_or_else(|_| String::from_utf8_lossy(line_bytes).to_string())
    } else {
        String::from_utf8_lossy(line_bytes).to_string()
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

    let mut visible_real_indices = Vec::with_capacity(height);
    for i in 0..height {
        let idx = offset + i;
        if idx >= total {
            break;
        }
        let real_idx = app.filtered_indices[idx];
        app.get_or_cache_parsed_line(real_idx);
        visible_real_indices.push(real_idx);
    }

    for &real_idx in &visible_real_indices {
        let (ts, lvl, msg) = app.parsed_line_cache[real_idx]
            .as_deref()
            .expect("line cache populated");

        let lvl_color = match lvl.as_str() {
            "ERROR" | "ERR" | "FATAL" => Color::Red,
            "WARN" | "WARNING" => Color::Yellow,
            "INFO" => Color::Green,
            "DEBUG" => Color::Blue,
            "TRACE" => Color::Magenta,
            _ => Color::White,
        };

        rows.push(Row::new(vec![
            Cell::from(ts.as_str()),
            Cell::from(lvl.as_str())
                .style(Style::default().fg(lvl_color).add_modifier(Modifier::BOLD)),
            Cell::from(msg.as_str()),
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
    if args.check_update || args.self_update {
        if let Err(err) = check_or_apply_update(args.self_update, args.yes) {
            eprintln!("{err}");
            std::process::exit(1);
        }
        return Ok(());
    }

    let file_path = args.file.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing log file path (or use --check-update / --self-update)",
        )
    })?;
    let file = File::open(&file_path)?;
    if file.metadata()?.len() == 0 {
        println!("File is empty.");
        return Ok(());
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let app = App::new(file_path)?;
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
    use super::{
        build_line_starts, classify_update, inspection_text, parse_line, target_asset_name,
        UpdatePriority, Version, MAX_INSPECT_BYTES,
    };

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
    fn parse_line_preserves_non_string_message_values() {
        let line = br#"{"time":"2026-03-13T12:00:00Z","level":"info","message":{"nested":true}}"#;
        let (ts, level, msg) = parse_line(line);
        assert_eq!(ts, "2026-03-13T12:00:00Z");
        assert_eq!(level, "INFO");
        assert_eq!(msg, r#"{"nested":true}"#);
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
        assert!(rendered.contains(&format!(
            "Showing first {} bytes only",
            MAX_INSPECT_BYTES
        )));
    }

    #[test]
    fn version_parse_handles_v_prefix() {
        let version = Version::parse("v2.13.7").expect("version parsed");
        assert_eq!(version.major, 2);
        assert_eq!(version.minor, 13);
        assert_eq!(version.patch, 7);
    }

    #[test]
    fn classify_update_prefers_major_and_minor_prompts() {
        let current = Version::parse("1.12.4").expect("current parsed");
        let major = Version::parse("2.0.0").expect("major parsed");
        let minor = Version::parse("1.13.0").expect("minor parsed");
        let patch = Version::parse("1.12.5").expect("patch parsed");

        assert_eq!(classify_update(current, major), UpdatePriority::Major);
        assert_eq!(classify_update(current, minor), UpdatePriority::Minor);
        assert_eq!(classify_update(current, patch), UpdatePriority::Patch);
    }

    #[test]
    fn target_asset_name_matches_supported_matrix_or_none() {
        let asset = target_asset_name();
        if let Some(name) = asset {
            assert!(name.starts_with("tachyon-"));
        }
    }
}
