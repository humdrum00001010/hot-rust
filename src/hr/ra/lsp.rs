use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use crate::util::{merged_rustflags, merged_rustflags_string};
use crate::{
    DEFAULT_WATCH_PROOF_SYMBOL, INITIALIZE_ID, SHUTDOWN_ID, VIEW_FILE_TEXT_ID, WATCH_PROOF_ENV,
    WATCH_PROOF_SYMBOL_ENV, WORKSPACE_SYMBOL_ID,
};

pub(crate) struct RustAnalyzerSession {
    writer: Arc<Mutex<ChildStdin>>,
    child: Child,
    reader: Option<thread::JoinHandle<()>>,
    state: SharedRaState,
}

impl RustAnalyzerSession {
    pub(crate) fn start(workspace_root: &Path) -> Result<Self, Box<dyn Error>> {
        let mut child = Command::new(rust_analyzer_command())
            .current_dir(workspace_root)
            .env("RA_LOG", "error")
            .env("RUSTC_BOOTSTRAP", "1")
            .env("RUSTFLAGS", merged_rustflags())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|err| format!("failed to spawn rust-analyzer: {err}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or("failed to open rust-analyzer stdin")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("failed to open rust-analyzer stdout")?;

        let writer = Arc::new(Mutex::new(stdin));
        let state = SharedRaState::default();
        let root = WorkspaceRoot::new(workspace_root)?;
        let config = rust_analyzer_config();
        let reader_writer = Arc::clone(&writer);
        let reader_root = root.clone();
        let reader_config = config.clone();
        let reader_state = state.clone();
        let reader = thread::spawn(move || {
            lsp_reader_loop(
                stdout,
                reader_writer,
                reader_state,
                reader_root,
                reader_config,
            );
        });

        send_lsp(
            &writer,
            json!({
                "jsonrpc": "2.0",
                "id": INITIALIZE_ID,
                "method": "initialize",
                "params": initialize_params(&root, &config),
            }),
        )?;
        wait_for_initialize(&state)?;
        send_lsp(
            &writer,
            json!({
                "jsonrpc": "2.0",
                "method": "initialized",
                "params": {},
            }),
        )?;
        println!("hr: rust-analyzer initialized; requested server-side file watching");

        Ok(Self {
            writer,
            child,
            reader: Some(reader),
            state,
        })
    }

    pub(crate) fn activity_seq(&self) -> u64 {
        ra_activity_seq(&self.state)
    }

    pub(crate) fn wait_for_activity_after(
        &self,
        baseline: u64,
        duration: Duration,
    ) -> Result<Option<String>, Box<dyn Error>> {
        wait_for_activity_after(&self.state, baseline, duration)
    }

    pub(crate) fn wait_for_quiescent(&self, duration: Duration) -> Result<bool, Box<dyn Error>> {
        wait_for_quiescent(&self.state, duration)
    }

    pub(crate) fn wait_for_workspace_symbol(
        &self,
        query: &str,
        duration: Duration,
    ) -> Result<bool, Box<dyn Error>> {
        let deadline = SystemTime::now() + duration;
        loop {
            if self.workspace_symbol_contains(query)? {
                return Ok(true);
            }

            let now = SystemTime::now();
            if now >= deadline {
                return Ok(false);
            }
            let sleep_for = deadline
                .duration_since(now)
                .unwrap_or_default()
                .min(Duration::from_millis(750));
            thread::sleep(sleep_for);
        }
    }

    pub(crate) fn workspace_symbol_contains(&self, query: &str) -> Result<bool, Box<dyn Error>> {
        Ok(self.workspace_symbol_uri(query)?.is_some())
    }

    pub(crate) fn workspace_symbol_uri(
        &self,
        query: &str,
    ) -> Result<Option<String>, Box<dyn Error>> {
        reset_workspace_symbol_response(&self.state);
        send_lsp(
            &self.writer,
            json!({
                "jsonrpc": "2.0",
                "id": WORKSPACE_SYMBOL_ID,
                "method": "workspace/symbol",
                "params": {
                    "query": query,
                },
            }),
        )?;
        let symbols = wait_for_workspace_symbol_response(&self.state, Duration::from_secs(5))?;
        Ok(symbols
            .into_iter()
            .find(|symbol| symbol.name == query)
            .map(|symbol| symbol.uri))
    }

    pub(crate) fn view_file_text(&self, uri: &str) -> Result<String, Box<dyn Error>> {
        reset_file_text_response(&self.state);
        send_lsp(
            &self.writer,
            json!({
                "jsonrpc": "2.0",
                "id": VIEW_FILE_TEXT_ID,
                "method": "rust-analyzer/viewFileText",
                "params": {
                    "uri": uri,
                },
            }),
        )?;
        wait_for_file_text_response(&self.state, Duration::from_secs(5))
    }

    pub(crate) fn patch_diagnostics(
        &self,
        uri: &str,
        source: &str,
        range_start: usize,
        range_end: usize,
        duration: Duration,
    ) -> Result<PatchDiagnosticStatus, Box<dyn Error>> {
        let baseline = self.sync_document_for_diagnostics(uri, source)?;
        wait_for_patch_diagnostics(
            &self.state,
            uri,
            source,
            range_start,
            range_end,
            baseline,
            duration,
        )
    }

    fn sync_document_for_diagnostics(
        &self,
        uri: &str,
        source: &str,
    ) -> Result<u64, Box<dyn Error>> {
        let sync = next_document_sync(&self.state, uri)?;
        let message = if sync.opened {
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": {
                        "uri": uri,
                        "version": sync.version,
                    },
                    "contentChanges": [{
                        "text": source,
                    }],
                },
            })
        } else {
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {
                    "textDocument": {
                        "uri": uri,
                        "languageId": "rust",
                        "version": sync.version,
                        "text": source,
                    },
                },
            })
        };
        send_lsp(&self.writer, message)?;
        Ok(sync.baseline)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PatchDiagnosticStatus {
    pub(crate) fresh: bool,
    pub(crate) error_count: usize,
    pub(crate) first_error: Option<String>,
}

impl PatchDiagnosticStatus {
    pub(crate) fn is_blocking(&self) -> bool {
        self.error_count > 0
    }
}

pub(crate) fn maybe_hold_for_project_watch_proof(
    ra: &RustAnalyzerSession,
) -> Result<(), Box<dyn Error>> {
    let Some(raw) = std::env::var_os(WATCH_PROOF_ENV) else {
        return Ok(());
    };
    let seconds = raw
        .to_string_lossy()
        .parse::<u64>()
        .map_err(|err| format!("{WATCH_PROOF_ENV} must be seconds: {err}"))?;
    if seconds == 0 {
        return Ok(());
    }

    let duration = Duration::from_secs(seconds);
    println!(
        "hr: holding rust-analyzer workspace watcher for {seconds}s; edit any project source now"
    );
    let _ = ra.wait_for_quiescent(Duration::from_secs(20));
    let baseline = ra.activity_seq();
    let symbol = std::env::var(WATCH_PROOF_SYMBOL_ENV)
        .unwrap_or_else(|_| DEFAULT_WATCH_PROOF_SYMBOL.to_string());
    println!("hr: workspace-symbol proof query={symbol}");
    if ra.wait_for_workspace_symbol(&symbol, duration)? {
        println!("hr: project symbol observed after launch: {symbol}");
        let _ = ra.wait_for_quiescent(Duration::from_secs(20));
        return Ok(());
    }
    println!("hr: workspace symbol not observed yet; falling back to activity wait");

    match ra.wait_for_activity_after(baseline, duration)? {
        Some(reason) => {
            println!("hr: project activity observed after launch: {reason}");
            let _ = ra.wait_for_quiescent(Duration::from_secs(20));
        }
        None => {
            println!("hr: no rust-analyzer project activity observed during hold window");
        }
    }
    Ok(())
}

impl Drop for RustAnalyzerSession {
    fn drop(&mut self) {
        if !ra_is_quiescent(&self.state) {
            println!("hr: waiting for rust-analyzer to settle before shutdown");
            let _ = wait_for_quiescent(&self.state, Duration::from_secs(20));
        }

        let _ = send_lsp(
            &self.writer,
            json!({
                "jsonrpc": "2.0",
                "id": SHUTDOWN_ID,
                "method": "shutdown",
                "params": null,
            }),
        );
        let got_shutdown_ack =
            wait_for_shutdown_ack(&self.state, Duration::from_secs(2)).unwrap_or(false);
        if got_shutdown_ack {
            let _ = send_lsp(
                &self.writer,
                json!({
                    "jsonrpc": "2.0",
                    "method": "exit",
                    "params": null,
                }),
            );
        }

        for _ in 0..10 {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    if let Some(handle) = self.reader.take() {
                        let _ = handle.join();
                    }
                    return;
                }
                Ok(None) => thread::sleep(Duration::from_millis(50)),
                Err(_) => break,
            }
        }

        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Clone)]
struct WorkspaceRoot {
    path: PathBuf,
    uri: String,
    name: String,
}

impl WorkspaceRoot {
    fn new(path: &Path) -> Result<Self, Box<dyn Error>> {
        let path = path.canonicalize()?;
        let uri = path_to_file_uri(&path);
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace")
            .to_string();
        Ok(Self { path, uri, name })
    }
}

#[derive(Clone, Default)]
struct SharedRaState(Arc<(Mutex<RaState>, Condvar)>);

#[derive(Debug, Default)]
struct RaState {
    initialized: bool,
    init_error: Option<String>,
    shutdown_ack: bool,
    reader_stopped: bool,
    last_health: Option<String>,
    last_quiescent: bool,
    activity_seq: u64,
    last_activity: Option<String>,
    symbol_response_ready: bool,
    symbol_response_error: Option<String>,
    symbol_response_symbols: Vec<WorkspaceSymbol>,
    file_text_ready: bool,
    file_text_error: Option<String>,
    file_text: Option<String>,
    diagnostics_by_uri: BTreeMap<String, DocumentDiagnostics>,
    open_documents: BTreeMap<String, i32>,
}

#[derive(Debug, Clone)]
struct WorkspaceSymbol {
    name: String,
    uri: String,
}

#[derive(Debug, Clone)]
struct DocumentDiagnostics {
    seq: u64,
    diagnostics: Vec<RaDiagnostic>,
}

#[derive(Debug, Clone)]
struct RaDiagnostic {
    severity: u64,
    start_line: u32,
    end_line: u32,
    message: String,
}

struct DocumentSync {
    opened: bool,
    version: i32,
    baseline: u64,
}

fn wait_for_initialize(state: &SharedRaState) -> Result<(), Box<dyn Error>> {
    let (lock, cvar) = &*state.0;
    let guard = lock
        .lock()
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    let (guard, timeout) = cvar
        .wait_timeout_while(guard, Duration::from_secs(20), |state| {
            !state.initialized && state.init_error.is_none() && !state.reader_stopped
        })
        .map_err(|_| "rust-analyzer state lock poisoned")?;

    if guard.initialized {
        return Ok(());
    }
    if let Some(error) = &guard.init_error {
        return Err(format!("rust-analyzer initialize failed: {error}").into());
    }
    if guard.reader_stopped {
        return Err("rust-analyzer LSP reader stopped during initialize".into());
    }
    if timeout.timed_out() {
        return Err("timed out waiting for rust-analyzer initialize response".into());
    }

    Err("rust-analyzer initialize wait ended unexpectedly".into())
}

fn wait_for_shutdown_ack(
    state: &SharedRaState,
    duration: Duration,
) -> Result<bool, Box<dyn Error>> {
    let (lock, cvar) = &*state.0;
    let guard = lock
        .lock()
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    let (guard, _timeout) = cvar
        .wait_timeout_while(guard, duration, |state| {
            !state.shutdown_ack && !state.reader_stopped
        })
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    Ok(guard.shutdown_ack)
}

fn wait_for_quiescent(state: &SharedRaState, duration: Duration) -> Result<bool, Box<dyn Error>> {
    let (lock, cvar) = &*state.0;
    let guard = lock
        .lock()
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    let (guard, _timeout) = cvar
        .wait_timeout_while(guard, duration, |state| {
            !state.last_quiescent && !state.reader_stopped
        })
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    Ok(guard.last_quiescent || guard.reader_stopped)
}

fn wait_for_activity_after(
    state: &SharedRaState,
    baseline: u64,
    duration: Duration,
) -> Result<Option<String>, Box<dyn Error>> {
    let (lock, cvar) = &*state.0;
    let guard = lock
        .lock()
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    let (guard, timeout) = cvar
        .wait_timeout_while(guard, duration, |state| {
            state.activity_seq <= baseline && !state.reader_stopped
        })
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    if guard.activity_seq > baseline {
        Ok(guard.last_activity.clone())
    } else if timeout.timed_out() || guard.reader_stopped {
        Ok(None)
    } else {
        Ok(None)
    }
}

fn reset_workspace_symbol_response(state: &SharedRaState) {
    update_ra_state(state, |state| {
        state.symbol_response_ready = false;
        state.symbol_response_error = None;
        state.symbol_response_symbols.clear();
    });
}

fn wait_for_workspace_symbol_response(
    state: &SharedRaState,
    duration: Duration,
) -> Result<Vec<WorkspaceSymbol>, Box<dyn Error>> {
    let (lock, cvar) = &*state.0;
    let guard = lock
        .lock()
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    let (guard, timeout) = cvar
        .wait_timeout_while(guard, duration, |state| {
            !state.symbol_response_ready && !state.reader_stopped
        })
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    if let Some(error) = &guard.symbol_response_error {
        return Err(format!("workspace/symbol failed: {error}").into());
    }
    if guard.symbol_response_ready {
        return Ok(guard.symbol_response_symbols.clone());
    }
    if timeout.timed_out() {
        return Err("timed out waiting for workspace/symbol response".into());
    }
    Err("rust-analyzer reader stopped before workspace/symbol response".into())
}

fn reset_file_text_response(state: &SharedRaState) {
    update_ra_state(state, |state| {
        state.file_text_ready = false;
        state.file_text_error = None;
        state.file_text = None;
    });
}

fn wait_for_file_text_response(
    state: &SharedRaState,
    duration: Duration,
) -> Result<String, Box<dyn Error>> {
    let (lock, cvar) = &*state.0;
    let guard = lock
        .lock()
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    let (guard, timeout) = cvar
        .wait_timeout_while(guard, duration, |state| {
            !state.file_text_ready && !state.reader_stopped
        })
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    if let Some(error) = &guard.file_text_error {
        return Err(format!("rust-analyzer/viewFileText failed: {error}").into());
    }
    if guard.file_text_ready {
        return Ok(guard.file_text.clone().unwrap_or_default());
    }
    if timeout.timed_out() {
        return Err("timed out waiting for rust-analyzer/viewFileText response".into());
    }
    Err("rust-analyzer reader stopped before rust-analyzer/viewFileText response".into())
}

fn wait_for_patch_diagnostics(
    state: &SharedRaState,
    uri: &str,
    source: &str,
    range_start: usize,
    range_end: usize,
    baseline: u64,
    duration: Duration,
) -> Result<PatchDiagnosticStatus, Box<dyn Error>> {
    let (lock, cvar) = &*state.0;
    let guard = lock
        .lock()
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    let (guard, _timeout) = cvar
        .wait_timeout_while(guard, duration, |state| {
            !state.reader_stopped
                && state
                    .diagnostics_by_uri
                    .get(uri)
                    .map(|diagnostics| diagnostics.seq <= baseline)
                    .unwrap_or(true)
        })
        .map_err(|_| "rust-analyzer state lock poisoned")?;

    let Some(document) = guard.diagnostics_by_uri.get(uri) else {
        return Ok(PatchDiagnosticStatus {
            fresh: false,
            error_count: 0,
            first_error: None,
        });
    };
    Ok(patch_diagnostic_status(
        document,
        source,
        range_start,
        range_end,
        baseline,
    ))
}

fn next_document_sync(state: &SharedRaState, uri: &str) -> Result<DocumentSync, Box<dyn Error>> {
    let (lock, _) = &*state.0;
    let mut guard = lock
        .lock()
        .map_err(|_| "rust-analyzer state lock poisoned")?;
    let baseline = guard.activity_seq;
    let entry = guard.open_documents.entry(uri.to_string()).or_insert(0);
    let opened = *entry > 0;
    *entry += 1;
    Ok(DocumentSync {
        opened,
        version: *entry,
        baseline,
    })
}

fn patch_diagnostic_status(
    document: &DocumentDiagnostics,
    source: &str,
    range_start: usize,
    range_end: usize,
    baseline: u64,
) -> PatchDiagnosticStatus {
    let start_line = source_line_for_offset(source, range_start);
    let end_line = source_line_for_offset(source, range_end);
    let mut error_count = 0usize;
    let mut first_error = None;
    for diagnostic in &document.diagnostics {
        if diagnostic.severity != 1 {
            continue;
        }
        if diagnostic.end_line < start_line || diagnostic.start_line > end_line {
            continue;
        }
        error_count += 1;
        if first_error.is_none() {
            first_error = Some(diagnostic.message.clone());
        }
    }
    PatchDiagnosticStatus {
        fresh: document.seq > baseline,
        error_count,
        first_error,
    }
}

fn source_line_for_offset(source: &str, offset: usize) -> u32 {
    source
        .as_bytes()
        .iter()
        .take(offset.min(source.len()))
        .filter(|byte| **byte == b'\n')
        .count() as u32
}

fn ra_activity_seq(state: &SharedRaState) -> u64 {
    let (lock, _) = &*state.0;
    lock.lock().map(|state| state.activity_seq).unwrap_or(0)
}

fn ra_is_quiescent(state: &SharedRaState) -> bool {
    let (lock, _) = &*state.0;
    lock.lock()
        .map(|state| state.last_quiescent || state.reader_stopped)
        .unwrap_or(true)
}

fn update_ra_state(state: &SharedRaState, update: impl FnOnce(&mut RaState)) {
    let (lock, cvar) = &*state.0;
    if let Ok(mut guard) = lock.lock() {
        update(&mut guard);
        cvar.notify_all();
    }
}

fn record_project_activity(state: &SharedRaState, reason: impl Into<String>) {
    let reason = reason.into();
    update_ra_state(state, |state| {
        state.activity_seq += 1;
        state.last_activity = Some(reason);
    });
}

fn log_ra_status(health: &str, quiescent: bool, message: Option<&str>) {
    if let Some(message) = message {
        println!("hr: ra status health={health} quiescent={quiescent} message={message}");
    } else {
        println!("hr: ra status health={health} quiescent={quiescent}");
    }
}

fn response_id(message: &Value) -> Option<i64> {
    let id = message.get("id")?;
    id.as_i64()
        .or_else(|| id.as_str().and_then(|value| value.parse().ok()))
}

fn response_error(message: &Value) -> Option<String> {
    message.get("error").map(ToString::to_string)
}

fn record_reader_stop(state: &SharedRaState) {
    update_ra_state(state, |state| {
        state.reader_stopped = true;
    });
}

fn record_initialize_response(state: &SharedRaState, message: &Value) {
    let error = response_error(message);
    update_ra_state(state, |state| {
        if let Some(error) = error {
            state.init_error = Some(error);
        } else {
            state.initialized = true;
        }
    });
}

fn record_workspace_symbol_response(state: &SharedRaState, message: &Value) {
    let error = response_error(message);
    let symbols = message
        .get("result")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let name = item.get("name").and_then(Value::as_str)?;
                    let location = item.get("location")?;
                    let uri = location
                        .get("uri")
                        .or_else(|| location.get("targetUri"))
                        .and_then(Value::as_str)?;
                    Some(WorkspaceSymbol {
                        name: name.to_string(),
                        uri: uri.to_string(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    update_ra_state(state, |state| {
        state.symbol_response_ready = true;
        state.symbol_response_error = error;
        state.symbol_response_symbols = symbols;
    });
}

fn record_file_text_response(state: &SharedRaState, message: &Value) {
    let error = response_error(message);
    let text = message
        .get("result")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    update_ra_state(state, |state| {
        state.file_text_ready = true;
        state.file_text_error = error;
        state.file_text = text;
    });
}

fn record_document_diagnostics(state: &SharedRaState, uri: String, diagnostics: Vec<RaDiagnostic>) {
    let count = diagnostics.len();
    update_ra_state(state, |state| {
        state.activity_seq += 1;
        let seq = state.activity_seq;
        state.last_activity = Some(format!("diagnostics count={count} uri={uri}"));
        state
            .diagnostics_by_uri
            .insert(uri, DocumentDiagnostics { seq, diagnostics });
    });
}

fn parse_diagnostics(params: &Value) -> Vec<RaDiagnostic> {
    params
        .get("diagnostics")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let range = item.get("range")?;
                    let start = range.get("start")?;
                    let end = range.get("end")?;
                    Some(RaDiagnostic {
                        severity: item.get("severity").and_then(Value::as_u64).unwrap_or(1),
                        start_line: start.get("line").and_then(Value::as_u64).unwrap_or(0) as u32,
                        end_line: end.get("line").and_then(Value::as_u64).unwrap_or(0) as u32,
                        message: item
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("<diagnostic without message>")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn rust_analyzer_command() -> OsString {
    std::env::var_os("HR_RUST_ANALYZER").unwrap_or_else(|| "rust-analyzer".into())
}

fn initialize_params(root: &WorkspaceRoot, config: &Value) -> Value {
    json!({
        "processId": std::process::id(),
        "rootPath": root.path,
        "rootUri": root.uri,
        "workspaceFolders": [{
            "uri": root.uri,
            "name": root.name,
        }],
        "capabilities": {
            "workspace": {
                "workspaceFolders": true,
                "configuration": true,
                "applyEdit": false,
                "symbol": {
                    "dynamicRegistration": false,
                },
                "didChangeWatchedFiles": {
                    "dynamicRegistration": false,
                    "relativePatternSupport": true,
                },
                "inlayHint": {
                    "refreshSupport": false,
                },
                "semanticTokens": {
                    "refreshSupport": false,
                },
            },
            "window": {
                "workDoneProgress": true,
                "showMessage": {
                    "messageActionItem": {
                        "additionalPropertiesSupport": false,
                    },
                },
            },
            "textDocument": {
                "synchronization": {
                    "dynamicRegistration": false,
                    "didSave": true,
                    "willSave": false,
                    "willSaveWaitUntil": false,
                },
                "publishDiagnostics": {
                    "relatedInformation": true,
                    "dataSupport": true,
                },
            },
            "experimental": {
                "serverStatusNotification": true,
            },
        },
        "initializationOptions": config,
    })
}

fn rust_analyzer_config() -> Value {
    json!({
        "files": {
            "watcher": "server",
        },
        "cargo": {
            "autoreload": true,
            "extraEnv": {
                "RUSTC_BOOTSTRAP": "1",
                "RUSTFLAGS": merged_rustflags_string(),
            },
        },
        "checkOnSave": true,
        "check": {
            "allTargets": false,
            "extraEnv": {
                "RUSTC_BOOTSTRAP": "1",
                "RUSTFLAGS": merged_rustflags_string(),
            },
        },
        "diagnostics": {
            "enable": true,
        },
        "server": {
            "extraEnv": {
                "RUSTC_BOOTSTRAP": "1",
                "RUSTFLAGS": merged_rustflags_string(),
            },
        },
    })
}

fn lsp_reader_loop(
    stdout: std::process::ChildStdout,
    writer: Arc<Mutex<ChildStdin>>,
    state: SharedRaState,
    root: WorkspaceRoot,
    config: Value,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_lsp(&mut reader) {
            Ok(Some(message)) => handle_lsp_message(message, &writer, &state, &root, &config),
            Ok(None) => {
                record_reader_stop(&state);
                break;
            }
            Err(err) => {
                println!("hr: ra reader stopped: {err}");
                record_reader_stop(&state);
                break;
            }
        }
    }
}

fn handle_lsp_message(
    message: Value,
    writer: &Arc<Mutex<ChildStdin>>,
    state: &SharedRaState,
    root: &WorkspaceRoot,
    config: &Value,
) {
    if message.get("method").is_none() {
        match response_id(&message) {
            Some(INITIALIZE_ID) => record_initialize_response(state, &message),
            Some(SHUTDOWN_ID) => update_ra_state(state, |state| {
                state.shutdown_ack = true;
            }),
            Some(WORKSPACE_SYMBOL_ID) => record_workspace_symbol_response(state, &message),
            Some(VIEW_FILE_TEXT_ID) => record_file_text_response(state, &message),
            _ => {}
        }
        return;
    }

    if let (Some(method), Some(id)) = (
        message.get("method").and_then(Value::as_str),
        message.get("id").cloned(),
    ) {
        let result = match method {
            "workspace/configuration" => workspace_configuration_result(&message, config),
            "workspace/workspaceFolders" => json!([{
                "uri": root.uri,
                "name": root.name,
            }]),
            "client/registerCapability"
            | "client/unregisterCapability"
            | "window/workDoneProgress/create"
            | "window/showMessageRequest" => Value::Null,
            "workspace/applyEdit" => json!({ "applied": false }),
            _ => Value::Null,
        };
        let _ = send_response(writer, id, result);
        return;
    }

    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return;
    };
    match method {
        "experimental/serverStatus" => {
            let params = message.get("params").unwrap_or(&Value::Null);
            let health = params
                .get("health")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let quiescent = params
                .get("quiescent")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let message = params
                .get("message")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            update_ra_state(state, |state| {
                state.last_health = Some(health.clone());
                state.last_quiescent = quiescent;
            });
            record_project_activity(
                state,
                format!("serverStatus health={health} quiescent={quiescent}"),
            );
            log_ra_status(&health, quiescent, message.as_deref());
        }
        "textDocument/publishDiagnostics" => {
            let params = message.get("params").unwrap_or(&Value::Null);
            let uri = params
                .get("uri")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>")
                .to_string();
            let diagnostics = parse_diagnostics(params);
            let count = diagnostics.len();
            record_document_diagnostics(state, uri.clone(), diagnostics);
            println!("hr: ra diagnostics {count} {uri}");
        }
        "window/logMessage" | "window/showMessage" => {
            if let Some(text) = message
                .get("params")
                .and_then(|params| params.get("message"))
                .and_then(Value::as_str)
            {
                println!("hr: ra {text}");
            }
        }
        _ => {}
    }
}

fn workspace_configuration_result(message: &Value, config: &Value) -> Value {
    let item_count = message
        .get("params")
        .and_then(|params| params.get("items"))
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(1);
    Value::Array((0..item_count).map(|_| config.clone()).collect())
}

fn send_response(
    writer: &Arc<Mutex<ChildStdin>>,
    id: Value,
    result: Value,
) -> Result<(), Box<dyn Error>> {
    send_lsp(
        writer,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }),
    )
}

fn send_lsp(writer: &Arc<Mutex<ChildStdin>>, value: Value) -> Result<(), Box<dyn Error>> {
    let body = serde_json::to_vec(&value)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut writer = writer
        .lock()
        .map_err(|_| "rust-analyzer stdin lock poisoned")?;
    writer.write_all(header.as_bytes())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

fn read_lsp(reader: &mut BufReader<std::process::ChildStdout>) -> io::Result<Option<Value>> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Ok(None);
        }

        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }

        if let Some(raw) = line.strip_prefix("Content-Length:") {
            let len = raw
                .trim()
                .parse::<usize>()
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            content_length = Some(len);
        }
    }

    let len = content_length.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "missing Content-Length in LSP frame",
        )
    })?;
    let mut body = vec![0; len];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map(Some).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid LSP JSON body: {err}"),
        )
    })
}

pub(crate) fn path_to_file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    let text = path.to_string_lossy();
    if !text.starts_with('/') {
        uri.push('/');
    }
    for byte in text.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                uri.push(*byte as char)
            }
            other => {
                let _ = fmt::write(&mut uri, format_args!("%{other:02X}"));
            }
        }
    }
    uri
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_diagnostics_only_blocks_errors_overlapping_function() {
        let source =
            "fn before() {}\nfn target() {\n    let value: Missing = 1;\n}\nfn after() {}\n";
        let range_start = source.find("fn target").unwrap();
        let range_end = source.find("\nfn after").unwrap();
        let document = DocumentDiagnostics {
            seq: 10,
            diagnostics: vec![
                RaDiagnostic {
                    severity: 1,
                    start_line: 2,
                    end_line: 2,
                    message: "cannot find type `Missing`".to_string(),
                },
                RaDiagnostic {
                    severity: 1,
                    start_line: 0,
                    end_line: 0,
                    message: "outside target".to_string(),
                },
                RaDiagnostic {
                    severity: 2,
                    start_line: 2,
                    end_line: 2,
                    message: "warning inside target".to_string(),
                },
            ],
        };

        let status = patch_diagnostic_status(&document, source, range_start, range_end, 5);

        assert!(status.fresh);
        assert_eq!(status.error_count, 1);
        assert_eq!(
            status.first_error.as_deref(),
            Some("cannot find type `Missing`")
        );
    }
}
