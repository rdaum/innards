#[path = "navsplat/clipboard.rs"]
mod clipboard;
#[path = "navsplat/ui.rs"]
mod ui;

use std::collections::{HashMap, HashSet};
use std::env;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use crossterm::event::{self, Event, KeyEvent};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use navsplat::config::{InnardsConfig, KeyPress, Keymap, KeymapMatch};
use navsplat::lsp::{self, CallDirection, LocationHit, LspClient, LspEvent, Symbol, SymbolKind};
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};
use tui_input::backend::crossterm::to_input_request;
use tui_input::{Input, InputRequest};

const DEBOUNCE: Duration = Duration::from_millis(140);

const NAVSPLAT_KEY_BINDINGS: &[(&str, &[&str])] = &[
    ("quit", &["esc", "ctrl-c"]),
    ("quit_if_empty", &["q"]),
    ("open", &["enter"]),
    ("pop", &["backspace"]),
    ("toggle_focus", &["tab"]),
    ("references", &["alt-r"]),
    ("callers", &["alt-c"]),
    ("callees", &["alt-e"]),
    ("source", &["alt-s"]),
    ("copy", &["alt-y"]),
    ("select_prev", &["ctrl-p", "up"]),
    ("select_next", &["ctrl-n", "down"]),
    ("preview_up", &["shift-up"]),
    ("preview_down", &["shift-down"]),
    ("page_up", &["pageup"]),
    ("page_down", &["pagedown"]),
    ("delete_next_char", &["ctrl-d"]),
];

const NAVSPLAT_ACTIONS: &[&str] = &[
    "quit",
    "quit_if_empty",
    "open",
    "pop",
    "toggle_focus",
    "references",
    "callers",
    "callees",
    "source",
    "copy",
    "select_prev",
    "select_next",
    "preview_up",
    "preview_down",
    "page_up",
    "page_down",
    "delete_next_char",
];

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
enum SidePaneMode {
    Source,
    References,
    Callers,
    Callees,
}

impl SidePaneMode {
    fn title(self) -> &'static str {
        match self {
            Self::Source => "Source",
            Self::References => "References",
            Self::Callers => "Callers",
            Self::Callees => "Callees",
        }
    }

    fn loading_label(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::References => "references",
            Self::Callers => "callers",
            Self::Callees => "callees",
        }
    }
}

enum SidePaneState {
    Loading,
    Ready(Vec<LocationHit>),
    Error(String),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum FocusArea {
    Symbols,
    SidePane,
}

#[derive(Debug, Clone)]
struct OpenTarget {
    file: PathBuf,
    line: u32,
}

#[derive(Debug, Clone)]
struct NavigationFrame {
    symbol: Symbol,
    mode: SidePaneMode,
    hits: Vec<LocationHit>,
    selected: usize,
}

#[derive(Debug, Parser)]
#[command(about = "Rust workspace symbol picker powered by rust-analyzer")]
struct Cli {
    #[arg(
        long,
        short,
        help = "Workspace root. Defaults to nearest Cargo.toml or .git ancestor"
    )]
    root: Option<PathBuf>,

    #[arg(
        long,
        help = "Editor command. Defaults to $VISUAL, then $EDITOR, then vi"
    )]
    editor: Option<String>,

    #[arg(
        long,
        default_value_t = 20,
        help = "Inline picker height in terminal rows"
    )]
    height: u16,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    #[command(about = "Open the interactive picker")]
    Pick {
        #[arg(help = "Initial query")]
        query: Option<String>,
    },
    #[command(about = "Print workspace symbol matches without opening the TUI")]
    Symbols {
        #[arg(help = "Query to send to rust-analyzer workspace/symbol")]
        query: String,
    },
}

struct App {
    root: PathBuf,
    client: LspClient,
    events: Receiver<LspEvent>,
    input: Input,
    symbols: Vec<Symbol>,
    promoted_symbol: Option<Symbol>,
    navigation_stack: Vec<NavigationFrame>,
    selected: usize,
    preview_scroll: isize,
    dirty_since: Option<Instant>,
    last_sent_query: String,
    last_request_id: i64,
    empty_retry_count: u8,
    status: String,
    loading: bool,
    progress_tokens: HashSet<String>,
    completions_ready: bool,
    side_mode: SidePaneMode,
    side_states: HashMap<String, SidePaneState>,
    side_selected: HashMap<String, usize>,
    focus: FocusArea,
    pending_keys: Vec<KeyPress>,
    tick: u64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let innards_config = InnardsConfig::load()?;
    let root = match cli.root {
        Some(root) => root.canonicalize()?,
        None => detect_workspace_root(env::current_dir()?)?,
    };

    let command = cli.command.unwrap_or(Commands::Pick { query: None });
    match command {
        Commands::Pick { query } => run_picker(
            root,
            cli.editor,
            cli.height,
            query.unwrap_or_default(),
            &innards_config,
        ),
        Commands::Symbols { query } => run_symbols(root, query),
    }
}

fn run_symbols(root: PathBuf, query: String) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let (client, mut child) = LspClient::start(root.clone(), tx)?;
    lsp::wait_for_ready(&rx, Duration::from_secs(20))?;
    let mut request_id = client.workspace_symbol(query.clone())?;
    let mut progress_tokens = HashSet::new();
    let deadline = Instant::now() + Duration::from_secs(20);

    while Instant::now() < deadline {
        let Ok(event) = rx.recv_timeout(Duration::from_millis(250)) else {
            continue;
        };
        match event {
            LspEvent::Symbols {
                request_id: id,
                symbols,
                ..
            } if id == request_id && (!symbols.is_empty() || progress_tokens.is_empty()) => {
                for symbol in symbols {
                    println!(
                        "{}:{}:{}\t{} [{}]",
                        symbol.file.display(),
                        symbol.line,
                        symbol.column,
                        symbol.name,
                        symbol.kind.label()
                    );
                }
                client.shutdown();
                let _ = child.wait();
                return Ok(());
            }
            LspEvent::Symbols { request_id: id, .. } if id == request_id => {}
            LspEvent::Progress { token, active, .. } => {
                let was_active = !progress_tokens.is_empty();
                if active {
                    progress_tokens.insert(token);
                } else {
                    progress_tokens.remove(&token);
                }
                if was_active && progress_tokens.is_empty() {
                    request_id = client.workspace_symbol(query.clone())?;
                }
            }
            LspEvent::Error(err) => return Err(anyhow!(err)),
            _ => {}
        }
    }

    client.shutdown();
    let _ = child.wait();
    Err(anyhow!("timed out waiting for workspace/symbol response"))
}

fn run_picker(
    root: PathBuf,
    editor: Option<String>,
    height: u16,
    initial_query: String,
    innards_config: &InnardsConfig,
) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let (client, mut child) = LspClient::start(root.clone(), tx)?;
    lsp::wait_for_ready(&rx, Duration::from_secs(20))?;
    let mut keymap = Keymap::from_defaults(NAVSPLAT_KEY_BINDINGS)?;
    keymap.apply_overrides(&innards_config.keybindings.navsplat)?;

    let mut terminal = TerminalGuard::enter(height)?;
    let mut app = App {
        root,
        client,
        events: rx,
        input: Input::from(initial_query),
        symbols: Vec::new(),
        promoted_symbol: None,
        navigation_stack: Vec::new(),
        selected: 0,
        preview_scroll: 0,
        dirty_since: Some(Instant::now() - DEBOUNCE),
        last_sent_query: String::new(),
        last_request_id: -1,
        empty_retry_count: 0,
        status: "type to search, enter to open, esc to quit".to_string(),
        loading: false,
        progress_tokens: HashSet::new(),
        completions_ready: false,
        side_mode: SidePaneMode::Source,
        side_states: HashMap::new(),
        side_selected: HashMap::new(),
        focus: FocusArea::Symbols,
        pending_keys: Vec::new(),
        tick: 0,
    };

    let selected = run_event_loop(&mut terminal.terminal, &mut app, &keymap)?;
    drop(terminal);

    app.client.shutdown();
    let _ = child.wait();

    if let Some(target) = selected {
        open_in_editor(&editor_command(editor), &app.root, &target)?;
    }

    Ok(())
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    keymap: &Keymap,
) -> Result<Option<OpenTarget>> {
    loop {
        drain_lsp_events(app);
        maybe_send_query(app);
        maybe_request_side_pane(app);
        app.tick = app.tick.wrapping_add(1);
        terminal.draw(|frame| ui::draw(frame, app))?;

        if event::poll(Duration::from_millis(40))? {
            match event::read()? {
                Event::Key(key) => {
                    if let Some(result) = handle_key(app, key, keymap) {
                        return Ok(result);
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }
}

fn handle_key(app: &mut App, key: KeyEvent, keymap: &Keymap) -> Option<Option<OpenTarget>> {
    match keymap.match_key_for_actions(NAVSPLAT_ACTIONS, &app.pending_keys, &key) {
        KeymapMatch::Prefix => {
            if let Some(key) = keymap.keypress_from_event(&key) {
                app.pending_keys.push(key);
                app.status = pending_status(&app.pending_keys);
            }
            return None;
        }
        KeymapMatch::Action(action) => {
            app.pending_keys.clear();
            return handle_action(app, key, action.as_str());
        }
        KeymapMatch::None if !app.pending_keys.is_empty() => {
            app.pending_keys.clear();
            app.status = "unknown key sequence".to_string();
            return None;
        }
        KeymapMatch::None => {}
    }

    handle_input_key(app, key);
    None
}

fn handle_action(app: &mut App, key: KeyEvent, action: &str) -> Option<Option<OpenTarget>> {
    match action {
        "quit" => return Some(None),
        "quit_if_empty" if app.input.value().is_empty() => return Some(None),
        "quit_if_empty" => handle_input_key(app, key),
        "open" => {
            if let Some(result) = handle_enter(app) {
                return Some(result);
            }
        }
        "pop" if app.promoted_symbol.is_some() => pop_navigation(app),
        "pop" => handle_input_key(app, key),
        "toggle_focus" => toggle_focus(app),
        "references" => set_side_mode(app, SidePaneMode::References),
        "callers" => set_side_mode(app, SidePaneMode::Callers),
        "callees" => set_side_mode(app, SidePaneMode::Callees),
        "source" => set_side_mode(app, SidePaneMode::Source),
        "copy" => copy_selection(app),
        "select_prev" => select_prev(app),
        "select_next" => select_next(app),
        "preview_up" => scroll_preview(app, -1),
        "preview_down" => scroll_preview(app, 1),
        "page_up" => select_delta(app, -10),
        "page_down" => select_delta(app, 10),
        "delete_next_char" if app.promoted_symbol.is_none() => {
            handle_input_request(app, InputRequest::DeleteNextChar);
        }
        _ => {}
    }
    None
}

fn pending_status(pending: &[KeyPress]) -> String {
    let keys = pending
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    format!("{keys} ...")
}

fn handle_enter(app: &mut App) -> Option<Option<OpenTarget>> {
    if app.focus == FocusArea::SidePane {
        let hit = selected_side_hit(app)?.clone();
        if let Some(frame) = current_navigation_frame(app) {
            app.navigation_stack.push(frame);
        }
        app.promoted_symbol = Some(symbol_from_hit(&hit));
        app.focus = FocusArea::Symbols;
        app.preview_scroll = 0;
        app.side_states.clear();
        app.side_selected.clear();
        app.status = format!("promoted {}:{}", hit.file.display(), hit.line);
        return None;
    }

    Some(selected_open_target(app))
}

fn pop_navigation(app: &mut App) {
    if let Some(frame) = app.navigation_stack.pop() {
        let key = side_key(frame.mode, &frame.symbol);
        app.promoted_symbol = Some(frame.symbol);
        app.side_mode = frame.mode;
        app.side_states.clear();
        app.side_selected.clear();
        app.side_states
            .insert(key.clone(), SidePaneState::Ready(frame.hits));
        app.side_selected.insert(key, frame.selected);
        app.focus = FocusArea::SidePane;
        app.status = "popped to previous selection".to_string();
    } else {
        app.promoted_symbol = None;
        app.focus = FocusArea::Symbols;
        app.side_states.clear();
        app.side_selected.clear();
        app.status = format!("{} match(es)", app.symbols.len());
    }
    app.preview_scroll = 0;
}

fn select_prev(app: &mut App) {
    if app.focus == FocusArea::SidePane {
        select_side_delta(app, -1);
        return;
    }

    if app.promoted_symbol.is_some() {
        return;
    }

    let previous = app.selected;
    app.selected = app.selected.saturating_sub(1);
    if app.selected != previous {
        app.preview_scroll = 0;
    }
}

fn select_next(app: &mut App) {
    if app.focus == FocusArea::SidePane {
        select_side_delta(app, 1);
        return;
    }

    if app.promoted_symbol.is_some() {
        return;
    }

    if !app.symbols.is_empty() {
        let previous = app.selected;
        app.selected = (app.selected + 1).min(app.symbols.len() - 1);
        if app.selected != previous {
            app.preview_scroll = 0;
        }
    }
}

fn select_delta(app: &mut App, delta: isize) {
    if app.focus == FocusArea::SidePane {
        select_side_delta(app, delta);
        return;
    }

    if app.promoted_symbol.is_some() {
        return;
    }

    if app.symbols.is_empty() {
        app.selected = 0;
        return;
    }
    if delta.is_negative() {
        app.selected = app.selected.saturating_sub(delta.unsigned_abs());
    } else {
        app.selected = (app.selected + delta as usize).min(app.symbols.len() - 1);
    }
    app.preview_scroll = 0;
}

fn set_side_mode(app: &mut App, mode: SidePaneMode) {
    if app.side_mode != mode {
        app.side_mode = mode;
        app.preview_scroll = 0;
        if mode == SidePaneMode::Source {
            app.focus = FocusArea::Symbols;
        }
    }
}

fn toggle_focus(app: &mut App) {
    if app.side_mode == SidePaneMode::Source {
        return;
    }

    app.focus = match app.focus {
        FocusArea::Symbols if selected_side_hit_count(app) > 0 => FocusArea::SidePane,
        FocusArea::Symbols => FocusArea::Symbols,
        FocusArea::SidePane => FocusArea::Symbols,
    };
    app.preview_scroll = 0;
}

fn select_side_delta(app: &mut App, delta: isize) {
    let Some(key) = current_side_key(app) else {
        app.focus = FocusArea::Symbols;
        return;
    };
    let count = selected_side_hit_count(app);
    if count == 0 {
        app.focus = FocusArea::Symbols;
        return;
    }

    let selected = app.side_selected.entry(key).or_insert(0);
    let previous = *selected;
    if delta.is_negative() {
        *selected = selected.saturating_sub(delta.unsigned_abs());
    } else {
        *selected = (*selected + delta as usize).min(count - 1);
    }
    if *selected != previous {
        app.preview_scroll = 0;
    }
}

fn scroll_preview(app: &mut App, delta: isize) {
    if !app.symbols.is_empty() {
        app.preview_scroll = app.preview_scroll.saturating_add(delta);
    }
}

fn handle_input_key(app: &mut App, key: KeyEvent) {
    if app.promoted_symbol.is_some() {
        return;
    }

    let event = Event::Key(key);
    if let Some(request) = to_input_request(&event) {
        handle_input_request(app, request);
    }
}

fn handle_input_request(app: &mut App, request: InputRequest) {
    if let Some(changed) = app.input.handle(request)
        && changed.value
    {
        mark_dirty(app);
    }
}

fn mark_dirty(app: &mut App) {
    app.dirty_since = Some(Instant::now());
    app.status = "searching...".to_string();
    app.loading = true;
    app.empty_retry_count = 0;
    app.promoted_symbol = None;
    app.navigation_stack.clear();
}

fn maybe_send_query(app: &mut App) {
    let Some(dirty_since) = app.dirty_since else {
        return;
    };
    let query = app.input.value();
    if dirty_since.elapsed() < DEBOUNCE || query == app.last_sent_query {
        return;
    }

    match app.client.workspace_symbol(query.to_string()) {
        Ok(request_id) => {
            app.last_request_id = request_id;
            app.last_sent_query = query.to_string();
            app.dirty_since = None;
            app.loading = true;
            app.completions_ready = false;
        }
        Err(err) => {
            app.status = err.to_string();
            app.dirty_since = None;
            app.loading = false;
        }
    }
}

fn schedule_empty_retry(app: &mut App) {
    app.empty_retry_count = app.empty_retry_count.saturating_add(1);
    app.status = "rust-analyzer warming up; retrying search...".to_string();
    app.loading = true;
    app.completions_ready = false;
    app.dirty_since = Some(Instant::now());
    app.last_sent_query.clear();
}

fn maybe_request_side_pane(app: &mut App) {
    if !app.completions_ready || app.side_mode == SidePaneMode::Source {
        return;
    }

    let Some(symbol) = active_symbol(app).cloned() else {
        return;
    };
    let key = side_key(app.side_mode, &symbol);
    if app.side_states.contains_key(&key) {
        return;
    }

    app.side_states.insert(key.clone(), SidePaneState::Loading);
    let result = match app.side_mode {
        SidePaneMode::Source => return,
        SidePaneMode::References => app.client.references(&symbol, key.clone()),
        SidePaneMode::Callers => app.client.incoming_calls(&symbol, key.clone()),
        SidePaneMode::Callees => app.client.outgoing_calls(&symbol, key.clone()),
    };

    match result {
        Ok(_) => {
            app.status = format!("loading {}...", app.side_mode.loading_label());
            app.loading = true;
        }
        Err(err) => {
            app.side_states
                .insert(key, SidePaneState::Error(err.to_string()));
            app.status = err.to_string();
            app.loading = false;
        }
    }
}

fn side_key(mode: SidePaneMode, symbol: &Symbol) -> String {
    format!(
        "{mode:?}\t{}\t{}\t{}\t{}",
        symbol.file.display(),
        symbol.line,
        symbol.column,
        symbol.name
    )
}

fn current_side_key(app: &App) -> Option<String> {
    let symbol = active_symbol(app)?;
    (app.side_mode != SidePaneMode::Source).then(|| side_key(app.side_mode, symbol))
}

fn active_symbol(app: &App) -> Option<&Symbol> {
    app.promoted_symbol
        .as_ref()
        .or_else(|| app.symbols.get(app.selected))
}

fn selected_side_hit_count(app: &App) -> usize {
    let Some(key) = current_side_key(app) else {
        return 0;
    };
    match app.side_states.get(&key) {
        Some(SidePaneState::Ready(hits)) => hits.len(),
        _ => 0,
    }
}

fn selected_side_hit(app: &App) -> Option<&LocationHit> {
    let key = current_side_key(app)?;
    let selected = app.side_selected.get(&key).copied().unwrap_or(0);
    match app.side_states.get(&key) {
        Some(SidePaneState::Ready(hits)) => hits.get(selected),
        _ => None,
    }
}

fn current_navigation_frame(app: &App) -> Option<NavigationFrame> {
    let symbol = active_symbol(app)?.clone();
    let key = current_side_key(app)?;
    let selected = app.side_selected.get(&key).copied().unwrap_or(0);
    let hits = match app.side_states.get(&key) {
        Some(SidePaneState::Ready(hits)) => hits.clone(),
        _ => return None,
    };
    Some(NavigationFrame {
        symbol,
        mode: app.side_mode,
        hits,
        selected,
    })
}

fn selected_open_target(app: &App) -> Option<OpenTarget> {
    if app.focus == FocusArea::SidePane
        && let Some(hit) = selected_side_hit(app)
    {
        return Some(OpenTarget {
            file: hit.file.clone(),
            line: hit.line,
        });
    }

    active_symbol(app).map(|symbol| OpenTarget {
        file: symbol.file.clone(),
        line: symbol.line,
    })
}

fn selected_clipboard_text(app: &App) -> Option<String> {
    if app.focus == FocusArea::SidePane
        && let Some(hit) = selected_side_hit(app)
    {
        let name = hit.name.as_deref().unwrap_or("");
        let kind = hit.kind.map(|kind| kind.label()).unwrap_or("Location");
        return Some(format!(
            "{}:{}:{}\t{} [{}]",
            hit.file.display(),
            hit.line,
            hit.column,
            name,
            kind
        ));
    }

    active_symbol(app).map(|symbol| {
        format!(
            "{}:{}:{}\t{} [{}]",
            symbol.file.display(),
            symbol.line,
            symbol.column,
            symbol.name,
            symbol.kind.label()
        )
    })
}

fn copy_selection(app: &mut App) {
    let Some(text) = selected_clipboard_text(app) else {
        app.status = "nothing selected to copy".to_string();
        return;
    };

    match clipboard::copy_to_clipboard(&text) {
        Ok(tool) => app.status = format!("copied selection with {tool}"),
        Err(err) => app.status = err.to_string(),
    }
}

fn symbol_from_hit(hit: &LocationHit) -> Symbol {
    Symbol {
        name: hit
            .name
            .clone()
            .unwrap_or_else(|| format!("{}:{}", hit.file.display(), hit.line)),
        kind: hit.kind.unwrap_or(SymbolKind::Unknown),
        file: hit.file.clone(),
        line: hit.line,
        end_line: hit.line,
        column: hit.column,
        container_name: hit.detail.clone(),
    }
}

fn drain_lsp_events(app: &mut App) {
    while let Ok(event) = app.events.try_recv() {
        match event {
            LspEvent::Symbols {
                request_id,
                query,
                symbols,
            } if request_id == app.last_request_id && query == app.last_sent_query => {
                if symbols.is_empty()
                    && !query.is_empty()
                    && (!app.progress_tokens.is_empty() || app.empty_retry_count < 5)
                {
                    schedule_empty_retry(app);
                    continue;
                }
                app.symbols = symbols;
                if app.promoted_symbol.is_none() {
                    app.navigation_stack.clear();
                    app.selected = app.selected.min(app.symbols.len().saturating_sub(1));
                    app.preview_scroll = 0;
                    app.side_states.clear();
                    app.side_selected.clear();
                    app.focus = FocusArea::Symbols;
                    if !app.progress_tokens.is_empty() && app.symbols.is_empty() {
                        app.status = "rust-analyzer is still indexing...".to_string();
                        app.loading = true;
                        app.completions_ready = false;
                    } else {
                        app.empty_retry_count = 0;
                        app.status = format!("{} match(es)", app.symbols.len());
                        app.loading = false;
                        app.completions_ready = true;
                    }
                }
            }
            LspEvent::Progress {
                token,
                active,
                message,
            } => {
                let was_active = !app.progress_tokens.is_empty();
                if active {
                    app.progress_tokens.insert(token);
                    app.status = format!("rust-analyzer: {message}");
                    app.loading = true;
                } else {
                    app.progress_tokens.remove(&token);
                    let is_active = !app.progress_tokens.is_empty();
                    app.status = if is_active {
                        "rust-analyzer is indexing...".to_string()
                    } else {
                        "rust-analyzer ready".to_string()
                    };
                    if was_active && !is_active && !app.input.value().is_empty() {
                        app.dirty_since = Some(Instant::now() - DEBOUNCE);
                        app.loading = true;
                    } else {
                        app.loading = is_active;
                    }
                }
            }
            LspEvent::Error(err) => {
                app.status = err;
                app.loading = false;
                app.completions_ready = false;
            }
            LspEvent::References { key, hits } => {
                app.side_selected.insert(key.clone(), 0);
                app.side_states
                    .insert(key.clone(), SidePaneState::Ready(hits));
                if current_side_key(app).as_deref() == Some(key.as_str()) {
                    app.status = "references loaded".to_string();
                    app.loading = !app.progress_tokens.is_empty();
                }
            }
            LspEvent::CallHierarchy {
                key,
                direction,
                hits,
                ..
            } => {
                let label = match direction {
                    CallDirection::Incoming => "callers",
                    CallDirection::Outgoing => "callees",
                };
                app.side_selected.insert(key.clone(), 0);
                app.side_states
                    .insert(key.clone(), SidePaneState::Ready(hits));
                if current_side_key(app).as_deref() == Some(key.as_str()) {
                    app.status = format!("{label} loaded");
                    app.loading = !app.progress_tokens.is_empty();
                }
            }
            LspEvent::SideError { key, message } => {
                app.side_states
                    .insert(key.clone(), SidePaneState::Error(message.clone()));
                if current_side_key(app).as_deref() == Some(key.as_str()) {
                    app.status = message;
                    app.loading = !app.progress_tokens.is_empty();
                }
            }
            _ => {}
        }
    }
}

fn input_display_value(app: &App) -> String {
    if let Some(symbol) = &app.promoted_symbol {
        format!(
            "current: {} [{}] {}:{}  (backspace pops)",
            symbol.name,
            symbol.kind.label(),
            symbol.file.display(),
            symbol.line
        )
    } else {
        app.input.value().to_string()
    }
}

fn open_in_editor(editor: &str, root: &Path, target: &OpenTarget) -> Result<()> {
    let path = if target.file.is_absolute() {
        target.file.clone()
    } else {
        root.join(&target.file)
    };

    let mut parts = editor.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| anyhow!("empty editor command"))?;
    let args: Vec<&str> = parts.collect();

    Command::new(program)
        .args(args)
        .arg(format!("+{}", target.line))
        .arg(path)
        .status()
        .with_context(|| format!("failed to run editor command: {editor}"))?;
    Ok(())
}

fn editor_command(editor: Option<String>) -> String {
    editor
        .or_else(|| env::var("VISUAL").ok())
        .or_else(|| env::var("EDITOR").ok())
        .unwrap_or_else(|| "vi".to_string())
}

fn detect_workspace_root(mut start: PathBuf) -> Result<PathBuf> {
    start = start.canonicalize()?;
    let mut current = Some(start.as_path());
    while let Some(path) = current {
        if path.join("Cargo.toml").exists() || path.join(".git").exists() {
            return Ok(path.to_path_buf());
        }
        current = path.parent();
    }
    Err(anyhow!(
        "could not find workspace root from current directory; pass --root"
    ))
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter(height: u16) -> Result<Self> {
        enable_raw_mode()?;
        let stdout = io::stdout();
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(height.max(10)),
            },
        )?;
        terminal.clear()?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.terminal.clear();
        let _ = disable_raw_mode();
        let _ = self.terminal.show_cursor();
    }
}
