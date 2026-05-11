use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub file: PathBuf,
    pub line: u32,
    pub end_line: u32,
    pub column: u32,
    pub container_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LocationHit {
    pub name: Option<String>,
    pub kind: Option<SymbolKind>,
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum SymbolKind {
    File,
    Module,
    Namespace,
    Package,
    Class,
    Method,
    Property,
    Field,
    Constructor,
    Enum,
    Interface,
    Function,
    Variable,
    Constant,
    String,
    Number,
    Boolean,
    Array,
    Object,
    Key,
    Null,
    EnumMember,
    Struct,
    Event,
    Operator,
    TypeParameter,
    Unknown,
}

impl SymbolKind {
    pub fn from_u64(value: u64) -> Self {
        match value {
            1 => Self::File,
            2 => Self::Module,
            3 => Self::Namespace,
            4 => Self::Package,
            5 => Self::Class,
            6 => Self::Method,
            7 => Self::Property,
            8 => Self::Field,
            9 => Self::Constructor,
            10 => Self::Enum,
            11 => Self::Interface,
            12 => Self::Function,
            13 => Self::Variable,
            14 => Self::Constant,
            15 => Self::String,
            16 => Self::Number,
            17 => Self::Boolean,
            18 => Self::Array,
            19 => Self::Object,
            20 => Self::Key,
            21 => Self::Null,
            22 => Self::EnumMember,
            23 => Self::Struct,
            24 => Self::Event,
            25 => Self::Operator,
            26 => Self::TypeParameter,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::File => "File",
            Self::Module => "Module",
            Self::Namespace => "Namespace",
            Self::Package => "Package",
            Self::Class => "Class",
            Self::Method => "Method",
            Self::Property => "Property",
            Self::Field => "Field",
            Self::Constructor => "Constructor",
            Self::Enum => "Enum",
            Self::Interface => "Interface",
            Self::Function => "Function",
            Self::Variable => "Variable",
            Self::Constant => "Constant",
            Self::String => "String",
            Self::Number => "Number",
            Self::Boolean => "Boolean",
            Self::Array => "Array",
            Self::Object => "Object",
            Self::Key => "Key",
            Self::Null => "Null",
            Self::EnumMember => "EnumMember",
            Self::Struct => "Struct",
            Self::Event => "Event",
            Self::Operator => "Operator",
            Self::TypeParameter => "TypeParameter",
            Self::Unknown => "Unknown",
        }
    }

    pub fn is_broad_container(self) -> bool {
        matches!(
            self,
            Self::File | Self::Module | Self::Namespace | Self::Package
        )
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CallDirection {
    Incoming,
    Outgoing,
}

#[derive(Debug)]
pub enum LspEvent {
    Symbols {
        request_id: i64,
        query: String,
        symbols: Vec<Symbol>,
    },
    References {
        key: String,
        hits: Vec<LocationHit>,
    },
    CallHierarchy {
        key: String,
        direction: CallDirection,
        hits: Vec<LocationHit>,
    },
    SideError {
        key: String,
        message: String,
    },
    Error(String),
    Progress {
        token: String,
        active: bool,
        message: String,
    },
    Ready,
}

#[derive(Clone)]
pub struct LspClient {
    root: PathBuf,
    writer: Arc<Mutex<ChildStdin>>,
    next_id: Arc<AtomicI64>,
    pending_requests: Arc<Mutex<HashMap<i64, PendingRequest>>>,
}

#[derive(Debug)]
enum PendingRequest {
    WorkspaceSymbol {
        query: String,
    },
    References {
        key: String,
    },
    PrepareCallHierarchy {
        key: String,
        direction: CallDirection,
    },
    IncomingCalls {
        key: String,
    },
    OutgoingCalls {
        key: String,
    },
}

impl LspClient {
    pub fn start(root: PathBuf, events: Sender<LspEvent>) -> Result<(Self, Child)> {
        let mut child = Command::new("rust-analyzer")
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to start rust-analyzer")?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to open rust-analyzer stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to open rust-analyzer stdout"))?;

        let client = Self {
            root,
            writer: Arc::new(Mutex::new(stdin)),
            next_id: Arc::new(AtomicI64::new(1_000_000)),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
        };

        let reader_client = client.clone();
        thread::spawn(move || {
            if let Err(err) = read_loop(stdout, reader_client, events.clone()) {
                let _ = events.send(LspEvent::Error(err.to_string()));
            }
        });

        client.initialize()?;
        Ok((client, child))
    }

    pub fn workspace_symbol(&self, query: String) -> Result<i64> {
        let request_id = self.next_request_id();
        let params = json!({ "query": query });
        self.insert_pending(request_id, PendingRequest::WorkspaceSymbol { query })?;
        self.send_request_with_id(request_id, "workspace/symbol", params)?;
        Ok(request_id)
    }

    pub fn references(&self, symbol: &Symbol, key: String) -> Result<i64> {
        let request_id = self.next_request_id();
        let params = self.text_document_position_params(
            symbol,
            json!({
                "includeDeclaration": false
            }),
        )?;
        self.insert_pending(request_id, PendingRequest::References { key })?;
        self.send_request_with_id(request_id, "textDocument/references", params)?;
        Ok(request_id)
    }

    pub fn incoming_calls(&self, symbol: &Symbol, key: String) -> Result<i64> {
        self.prepare_call_hierarchy(symbol, key, CallDirection::Incoming)
    }

    pub fn outgoing_calls(&self, symbol: &Symbol, key: String) -> Result<i64> {
        self.prepare_call_hierarchy(symbol, key, CallDirection::Outgoing)
    }

    pub fn shutdown(&self) {
        let id = self.next_request_id();
        let _ = self.send_request_with_id(id, "shutdown", json!(null));
        let _ = self.send_notification("exit", json!(null));
    }

    fn initialize(&self) -> Result<()> {
        let root_uri = path_to_uri(&self.root)?;
        let process_id = std::process::id();
        self.send_request_with_id(
            0,
            "initialize",
            json!({
                "processId": process_id,
                "rootUri": root_uri,
                "workspaceFolders": [{
                    "uri": root_uri,
                    "name": self.root.file_name().and_then(|s| s.to_str()).unwrap_or("workspace")
                }],
                "capabilities": {
                    "workspace": {
                        "configuration": true,
                        "workspaceFolders": true,
                        "symbol": {
                            "dynamicRegistration": false,
                            "symbolKind": {
                                "valueSet": (1..=26).collect::<Vec<_>>()
                            }
                        }
                    },
                    "window": {
                        "workDoneProgress": true
                    },
                    "textDocument": {
                        "definition": {
                            "linkSupport": true
                        },
                        "references": {
                            "dynamicRegistration": false
                        },
                        "callHierarchy": {
                            "dynamicRegistration": false
                        }
                    }
                },
                "initializationOptions": {
                    "cargo": {
                        "buildScripts": {
                            "enable": true
                        }
                    }
                }
            }),
        )?;
        Ok(())
    }

    fn next_request_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn prepare_call_hierarchy(
        &self,
        symbol: &Symbol,
        key: String,
        direction: CallDirection,
    ) -> Result<i64> {
        let request_id = self.next_request_id();
        let params = self.text_document_position_params(symbol, json!(null))?;
        self.insert_pending(
            request_id,
            PendingRequest::PrepareCallHierarchy { key, direction },
        )?;
        self.send_request_with_id(request_id, "textDocument/prepareCallHierarchy", params)?;
        Ok(request_id)
    }

    fn text_document_position_params(&self, symbol: &Symbol, context: Value) -> Result<Value> {
        let path = self.symbol_path(symbol);
        let uri = Url::from_file_path(&path)
            .map(|url| url.to_string())
            .map_err(|_| anyhow!("failed to convert path to file URI: {}", path.display()))?;
        let mut params = json!({
            "textDocument": { "uri": uri },
            "position": {
                "line": symbol.line.saturating_sub(1),
                "character": symbol.column.saturating_sub(1)
            }
        });
        if !context.is_null() {
            params["context"] = context;
        }
        Ok(params)
    }

    fn symbol_path(&self, symbol: &Symbol) -> PathBuf {
        if symbol.file.is_absolute() {
            symbol.file.clone()
        } else {
            self.root.join(&symbol.file)
        }
    }

    fn send_request_with_id(&self, id: i64, method: &str, params: Value) -> Result<()> {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
    }

    fn send_notification(&self, method: &str, params: Value) -> Result<()> {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
    }

    fn respond(&self, id: Value, result: Value) -> Result<()> {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
    }

    fn send(&self, value: Value) -> Result<()> {
        let body = serde_json::to_vec(&value)?;
        if std::env::var_os("NAVSPLAT_TRACE_LSP").is_some() {
            eprintln!(">>> {}", String::from_utf8_lossy(&body));
        }
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow!("rust-analyzer writer lock poisoned"))?;
        writer.write_all(header.as_bytes())?;
        writer.write_all(&body)?;
        writer.flush()?;
        Ok(())
    }

    fn insert_pending(&self, request_id: i64, request: PendingRequest) -> Result<()> {
        self.pending_requests
            .lock()
            .map_err(|_| anyhow!("pending request lock poisoned"))?
            .insert(request_id, request);
        Ok(())
    }

    fn take_pending_request(&self, request_id: i64) -> Option<PendingRequest> {
        self.pending_requests
            .lock()
            .ok()
            .and_then(|mut pending| pending.remove(&request_id))
    }
}

fn read_loop<R: Read>(reader: R, client: LspClient, events: Sender<LspEvent>) -> Result<()> {
    let mut reader = BufReader::new(reader);

    loop {
        let Some(message) = read_message(&mut reader)? else {
            break;
        };

        let value: Value = serde_json::from_slice(&message)?;
        if std::env::var_os("NAVSPLAT_TRACE_LSP").is_some() {
            eprintln!("<<< {}", String::from_utf8_lossy(&message));
        }
        if let Some(method) = value.get("method").and_then(Value::as_str) {
            if method == "$/progress" {
                if let Some(progress) = parse_progress(&value) {
                    let _ = events.send(progress);
                }
                continue;
            }
            if value.get("id").is_some() {
                handle_server_request(&client, &value, method)?;
            }
            continue;
        }

        let id = value.get("id").and_then(Value::as_i64);
        if id == Some(0) {
            client.send_notification("initialized", json!({}))?;
            let _ = events.send(LspEvent::Ready);
            continue;
        }

        if let Some(request_id) = id {
            if let Some(pending) = client.take_pending_request(request_id) {
                if let Some(error) = value.get("error") {
                    send_pending_error(&events, pending, error.to_string());
                    continue;
                }
                match pending {
                    PendingRequest::WorkspaceSymbol { query } => {
                        let symbols = parse_symbols(value.get("result"), &client.root)?;
                        let _ = events.send(LspEvent::Symbols {
                            request_id,
                            query,
                            symbols,
                        });
                    }
                    PendingRequest::References { key } => {
                        let hits = parse_locations(value.get("result"), &client.root)?;
                        let _ = events.send(LspEvent::References { key, hits });
                    }
                    PendingRequest::PrepareCallHierarchy { key, direction } => {
                        handle_call_hierarchy_prepare(&client, &events, key, direction, &value)?;
                    }
                    PendingRequest::IncomingCalls { key } => {
                        let hits = parse_incoming_calls(value.get("result"), &client.root)?;
                        let _ = events.send(LspEvent::CallHierarchy {
                            key,
                            direction: CallDirection::Incoming,
                            hits,
                        });
                    }
                    PendingRequest::OutgoingCalls { key } => {
                        let hits = parse_outgoing_calls(value.get("result"), &client.root)?;
                        let _ = events.send(LspEvent::CallHierarchy {
                            key,
                            direction: CallDirection::Outgoing,
                            hits,
                        });
                    }
                }
            }
            continue;
        }

        if let Some(err) = value.get("error") {
            let _ = events.send(LspEvent::Error(err.to_string()));
        }
    }

    Ok(())
}

fn send_pending_error(events: &Sender<LspEvent>, pending: PendingRequest, message: String) {
    match pending {
        PendingRequest::WorkspaceSymbol { .. } => {
            let _ = events.send(LspEvent::Error(message));
        }
        PendingRequest::References { key }
        | PendingRequest::PrepareCallHierarchy { key, .. }
        | PendingRequest::IncomingCalls { key }
        | PendingRequest::OutgoingCalls { key } => {
            let _ = events.send(LspEvent::SideError { key, message });
        }
    }
}

fn handle_call_hierarchy_prepare(
    client: &LspClient,
    events: &Sender<LspEvent>,
    key: String,
    direction: CallDirection,
    value: &Value,
) -> Result<()> {
    let Some(item) = value
        .get("result")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .cloned()
    else {
        let _ = events.send(LspEvent::CallHierarchy {
            key,
            direction,
            hits: Vec::new(),
        });
        return Ok(());
    };

    let next_id = client.next_request_id();
    let (method, pending) = match direction {
        CallDirection::Incoming => (
            "callHierarchy/incomingCalls",
            PendingRequest::IncomingCalls { key },
        ),
        CallDirection::Outgoing => (
            "callHierarchy/outgoingCalls",
            PendingRequest::OutgoingCalls { key },
        ),
    };
    client.insert_pending(next_id, pending)?;
    client.send_request_with_id(next_id, method, json!({ "item": item }))
}

fn parse_progress(value: &Value) -> Option<LspEvent> {
    let token = value.pointer("/params/token")?.to_string();
    let progress = value.pointer("/params/value")?;
    let kind = progress.get("kind").and_then(Value::as_str)?;
    let message = progress
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| progress.get("title").and_then(Value::as_str))
        .unwrap_or(kind)
        .to_string();

    Some(LspEvent::Progress {
        token,
        active: kind != "end",
        message,
    })
}

fn handle_server_request(client: &LspClient, value: &Value, method: &str) -> Result<()> {
    let id = value
        .get("id")
        .cloned()
        .ok_or_else(|| anyhow!("server request missing id"))?;

    let result = match method {
        "workspace/configuration" => {
            let count = value
                .pointer("/params/items")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            Value::Array((0..count).map(|_| json!({})).collect())
        }
        "window/workDoneProgress/create" => json!(null),
        "client/registerCapability" => json!(null),
        "client/unregisterCapability" => json!(null),
        "workspace/workspaceFolders" => {
            let uri = path_to_uri(&client.root)?;
            json!([{
                "uri": uri,
                "name": client.root.file_name().and_then(|s| s.to_str()).unwrap_or("workspace")
            }])
        }
        _ => json!(null),
    };

    client.respond(id, result)
}

fn read_message<R: BufRead>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut content_len = None;
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            return Ok(None);
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            content_len = Some(rest.trim().parse::<usize>()?);
        }
    }

    let Some(content_len) = content_len else {
        return Err(anyhow!("LSP message missing Content-Length"));
    };
    let mut body = vec![0; content_len];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

fn parse_symbols(value: Option<&Value>, root: &Path) -> Result<Vec<Symbol>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let symbols = serde_json::from_value::<Vec<WorkspaceSymbol>>(value.clone())?;

    let mut out = Vec::new();
    for symbol in symbols {
        if let Some(symbol) = symbol.into_symbol(root) {
            out.push(symbol?);
        }
    }
    Ok(out)
}

fn parse_locations(value: Option<&Value>, root: &Path) -> Result<Vec<LocationHit>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let locations = serde_json::from_value::<Vec<Location>>(value.clone())?;
    locations
        .into_iter()
        .map(|location| location.into_hit(root, None, None, None))
        .collect()
}

fn parse_incoming_calls(value: Option<&Value>, root: &Path) -> Result<Vec<LocationHit>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let calls = serde_json::from_value::<Vec<IncomingCall>>(value.clone())?;
    calls
        .into_iter()
        .map(|call| call.from.into_hit(root))
        .collect()
}

fn parse_outgoing_calls(value: Option<&Value>, root: &Path) -> Result<Vec<LocationHit>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let calls = serde_json::from_value::<Vec<OutgoingCall>>(value.clone())?;
    calls
        .into_iter()
        .map(|call| call.to.into_hit(root))
        .collect()
}

fn path_to_uri(path: &Path) -> Result<String> {
    Url::from_directory_path(path)
        .map(|url| url.to_string())
        .map_err(|_| anyhow!("failed to convert path to file URI: {}", path.display()))
}

fn uri_to_path(uri: &str) -> Result<PathBuf> {
    Url::parse(uri)
        .with_context(|| format!("invalid URI from language server: {uri}"))?
        .to_file_path()
        .map_err(|_| anyhow!("non-file URI from language server: {uri}"))
}

fn relative_file(mut file: PathBuf, root: &Path) -> PathBuf {
    if let Ok(relative) = file.strip_prefix(root) {
        file = relative.to_path_buf();
    }
    file
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WorkspaceSymbol {
    Symbol(WorkspaceSymbolInformation),
    Partial(PartialWorkspaceSymbol),
}

impl WorkspaceSymbol {
    fn into_symbol(self, root: &Path) -> Option<Result<Symbol>> {
        match self {
            Self::Symbol(symbol) => Some(symbol.into_symbol(root)),
            Self::Partial(symbol) => symbol.location.map(|location| {
                WorkspaceSymbolInformation {
                    name: symbol.name,
                    kind: symbol.kind,
                    location,
                    container_name: symbol.container_name,
                }
                .into_symbol(root)
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PartialWorkspaceSymbol {
    name: String,
    kind: u64,
    location: Option<Location>,
    container_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceSymbolInformation {
    name: String,
    kind: u64,
    location: Location,
    container_name: Option<String>,
}

impl WorkspaceSymbolInformation {
    fn into_symbol(self, root: &Path) -> Result<Symbol> {
        let file = relative_file(uri_to_path(&self.location.uri)?, root);
        Ok(Symbol {
            name: self.name,
            kind: SymbolKind::from_u64(self.kind),
            file,
            line: self.location.range.start.line + 1,
            end_line: self.location.range.end.line + 1,
            column: self.location.range.start.character + 1,
            container_name: self.container_name,
        })
    }
}

#[derive(Debug, Deserialize)]
struct Location {
    uri: String,
    range: Range,
}

impl Location {
    fn into_hit(
        self,
        root: &Path,
        name: Option<String>,
        kind: Option<SymbolKind>,
        detail: Option<String>,
    ) -> Result<LocationHit> {
        Ok(LocationHit {
            name,
            kind,
            file: relative_file(uri_to_path(&self.uri)?, root),
            line: self.range.start.line + 1,
            column: self.range.start.character + 1,
            detail,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CallHierarchyItem {
    name: String,
    kind: u64,
    uri: String,
    selection_range: Range,
    detail: Option<String>,
}

impl CallHierarchyItem {
    fn into_hit(self, root: &Path) -> Result<LocationHit> {
        Ok(LocationHit {
            name: Some(self.name),
            kind: Some(SymbolKind::from_u64(self.kind)),
            file: relative_file(uri_to_path(&self.uri)?, root),
            line: self.selection_range.start.line + 1,
            column: self.selection_range.start.character + 1,
            detail: self.detail,
        })
    }
}

#[derive(Debug, Deserialize)]
struct IncomingCall {
    from: CallHierarchyItem,
}

#[derive(Debug, Deserialize)]
struct OutgoingCall {
    to: CallHierarchyItem,
}

#[derive(Debug, Deserialize)]
struct Range {
    start: Position,
    end: Position,
}

#[derive(Debug, Deserialize)]
struct Position {
    line: u32,
    character: u32,
}

#[allow(dead_code)]
#[derive(Debug, Serialize)]
struct WorkspaceSymbolParams<'a> {
    query: &'a str,
}

pub fn wait_for_ready(events: &Receiver<LspEvent>, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(anyhow!("rust-analyzer did not initialize before timeout"));
        }

        match events.recv_timeout(remaining) {
            Ok(LspEvent::Ready) => return Ok(()),
            Ok(LspEvent::Error(err)) => return Err(anyhow!(err)),
            Ok(_) => continue,
            Err(err) => return Err(anyhow!("rust-analyzer did not initialize: {err}")),
        }
    }
}
