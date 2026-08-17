//! 手写的最小 LSP 客户端（多 server）：只做 diagnostics。
//!
//! 依赖 `lsp-types` 仅作协议类型基础（`InitializeParams`/`Diagnostic` 等）；
//! JSON-RPC 帧（Content-Length）、握手、server 进程管理、`publishDiagnostics`
//! 缓存全部手写，不引入 nexo-lsp / codive-lsp 这类重依赖（design §8.4）。
//!
//! 架构（高内聚低耦合）：
//! - 纯函数（无 I/O，可单测）：`frame_message` / `decode_frames` 帧编解码、
//!   `build_*` 消息构造、`parse_publish_diagnostics` / `flatten_diagnostic` 解析。
//! - `LspClient`（有状态）：启动一个 server（argv 由注册表 `lang::LspSpec` 给定），
//!   起 reader / driver 两个后台线程负责读 stdout / 写 stdin，诊断缓存进
//!   `Arc<Mutex<HashMap<…>>>` 供工具层读。启动或握手失败记进 `state.error`，
//!   工具层经 `health()` 感知并降级到 `verify`。
//! - `LspManager`：按文件语言惰性路由到对应 server（进程共享）。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use lsp_types::{
    notification::{
        DidChangeTextDocument, DidOpenTextDocument, DidSaveTextDocument, Initialized,
        Notification, PublishDiagnostics,
    },
    request::{Initialize, Request},
    ClientCapabilities, ClientInfo, Diagnostic, DiagnosticSeverity, DidChangeTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, InitializeParams, InitializedParams,
    NumberOrString, PublishDiagnosticsParams, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, Url, VersionedTextDocumentIdentifier,
    WorkspaceFolder,
};
use serde_json::{Value, json};

/// 握手（initialize 响应）超时。
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(20);

// ---------------- 扁平化诊断 ----------------

/// 扁平化后的诊断严重级别（剥离 lsp_types 的 `DiagnosticSeverity`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Information,
    Hint,
}

impl Severity {
    /// 摘要里的标签列（诊断没有 code 时兜底）。
    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Information => "info",
            Severity::Hint => "hint",
        }
    }
}

/// 扁平化诊断条目：1-based 行列、消息、可选错误码。工具层不感知协议类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticEntry {
    pub severity: Severity,
    pub line: u32,
    pub column: u32,
    pub message: String,
    pub code: Option<String>,
}

// ---------------- 纯函数（无 I/O，可单测） ----------------

/// 把一条 JSON-RPC 消息编码成 Content-Length 帧（body 按**字节**计数）。
pub fn frame_message(body: &Value) -> String {
    let body = serde_json::to_string(body).expect("lsp message serializes");
    format!("Content-Length: {}\r\n\r\n{}", body.len(), body)
}

/// 从字节缓冲里解析出完整帧。
///
/// 返回 `(已解析出的 JSON 消息, 已消费的字节数)`。遇到不完整的帧头/帧体时停止并
/// 保留未消费字节——`pos` 指向首个未解析字节，调用方据此 `drain(..pos)` 截断缓冲。
pub fn decode_frames(buf: &[u8]) -> (Vec<Value>, usize) {
    let mut frames = Vec::new();
    let mut pos = 0usize;

    loop {
        let Some(off) = buf[pos..].windows(4).position(|w| w == &b"\r\n\r\n"[..]) else {
            break;
        };
        let header_end = pos + off;
        let header = &buf[pos..header_end];

        let mut content_len = None;
        if let Ok(h) = std::str::from_utf8(header) {
            for line in h.split("\r\n") {
                if let Some(v) = line.strip_prefix("Content-Length:") {
                    content_len = v.trim().parse::<usize>().ok();
                }
            }
        }
        let Some(len) = content_len else {
            // 帧头缺 Content-Length：跳过一字节继续，避免死循环。
            pos += 1;
            continue;
        };

        let body_start = header_end + 4;
        let body_end = body_start + len;
        if buf.len() < body_end {
            break; // 帧体不完整，等更多字节。
        }
        if let Ok(v) = serde_json::from_slice::<Value>(&buf[body_start..body_end]) {
            frames.push(v);
        }
        pos = body_end;
    }

    (frames, pos)
}

/// 请求 / 通知 / 响应的 JSON-RPC 信封。
fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}
fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

/// 绝对路径 → `file://` URI（rust-analyzer 用 URI 标识文档）。
pub fn uri_from_path(path: &Path) -> Url {
    Url::from_file_path(path).unwrap_or_else(|()| {
        // 兜底：正常情况 `from_file_path` 对绝对路径总会成功。
        Url::parse(&format!("file://{}", path.to_string_lossy())).expect("path converts to url")
    })
}

/// `file://` URI → 绝对路径；非 file 协议或非法路径返回 None。
pub fn path_from_uri(uri: &Url) -> Option<PathBuf> {
    if uri.scheme() != "file" {
        return None;
    }
    uri.to_file_path().ok()
}

/// 构造 `initialize` 请求（只声明 `workspaceFolders`，回避已废弃的 `rootUri`）。
pub fn build_initialize_request(id: u64, workspace_root: &Path) -> Value {
    let uri = uri_from_path(workspace_root);
    let name = workspace_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace")
        .to_string();
    let params = InitializeParams {
        process_id: Some(std::process::id()),
        workspace_folders: Some(vec![WorkspaceFolder { uri, name }]),
        capabilities: ClientCapabilities::default(),
        client_info: Some(ClientInfo {
            name: "phimint".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
        }),
        ..Default::default()
    };
    request(
        id,
        Initialize::METHOD,
        serde_json::to_value(params).expect("initialize params serialize"),
    )
}

/// 构造 `initialized` 通知（握手第二步）。
pub fn build_initialized_notification() -> Value {
    notification(
        Initialized::METHOD,
        serde_json::to_value(InitializedParams {}).expect("initialized params serialize"),
    )
}

/// 构造 `textDocument/didOpen` 通知（version=1）。`language_id` 来自注册表
/// （`lang::lsp_language_id`），例如 "rust"/"typescript"/"cpp"。
pub fn build_did_open(uri: &Url, text: &str, language_id: &str) -> Value {
    let params = DidOpenTextDocumentParams {
        text_document: TextDocumentItem::new(uri.clone(), language_id.into(), 1, text.to_string()),
    };
    notification(
        DidOpenTextDocument::METHOD,
        serde_json::to_value(params).expect("didOpen serialize"),
    )
}

/// 构造 `textDocument/didChange` 通知（全量同步：省略 range = 整篇替换）。
pub fn build_did_change(uri: &Url, version: i32, text: &str) -> Value {
    let params = DidChangeTextDocumentParams {
        text_document: VersionedTextDocumentIdentifier::new(uri.clone(), version),
        content_changes: vec![TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: text.to_string(),
        }],
    };
    notification(
        DidChangeTextDocument::METHOD,
        serde_json::to_value(params).expect("didChange serialize"),
    )
}

/// 构造 `textDocument/didSave` 通知（带 text，触发 rust-analyzer 的 checkOnSave →
/// cargo check → publishDiagnostics）。
pub fn build_did_save(uri: &Url, text: &str) -> Value {
    let params = DidSaveTextDocumentParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        text: Some(text.to_string()),
    };
    notification(
        DidSaveTextDocument::METHOD,
        serde_json::to_value(params).expect("didSave serialize"),
    )
}

/// 把 lsp-types 的 `Diagnostic` 扁平化成 `DiagnosticEntry`（0-based → 1-based）。
pub fn flatten_diagnostic(d: &Diagnostic) -> DiagnosticEntry {
    let severity = if d.severity == Some(DiagnosticSeverity::ERROR) {
        Severity::Error
    } else if d.severity == Some(DiagnosticSeverity::WARNING) {
        Severity::Warning
    } else if d.severity == Some(DiagnosticSeverity::INFORMATION) {
        Severity::Information
    } else if d.severity == Some(DiagnosticSeverity::HINT) {
        Severity::Hint
    } else {
        // 规范里 severity 可省略，保守当作 error 突出显示。
        Severity::Error
    };
    let code = match &d.code {
        Some(NumberOrString::Number(n)) => Some(n.to_string()),
        Some(NumberOrString::String(s)) => Some(s.clone()),
        None => None,
    };
    DiagnosticEntry {
        severity,
        line: d.range.start.line + 1,
        column: d.range.start.character + 1,
        // rust-analyzer 有时给多行 message（如 `mismatched types\nexpected …`），
        // 压缩成单行，保证摘要里每条诊断一行（对齐 verify 的 `--> file:line:col`）。
        message: d.message.split_whitespace().collect::<Vec<_>>().join(" "),
        code,
    }
}

/// 从 `textDocument/publishDiagnostics` 的 params 提取 `(绝对路径, 诊断列表)`。
///
/// 非 file 协议或反序列化失败返回 None（该消息被忽略）。
pub fn parse_publish_diagnostics(params: &Value) -> Option<(PathBuf, Vec<DiagnosticEntry>)> {
    let p: PublishDiagnosticsParams = serde_json::from_value(params.clone()).ok()?;
    let path = path_from_uri(&p.uri)?;
    let entries = p.diagnostics.iter().map(flatten_diagnostic).collect();
    Some((path, entries))
}

// ---------------- 有状态客户端 ----------------

/// 发往 driver 线程的指令。
enum LspCommand {
    /// 打开/更新一个文件（didOpen 或 didChange，随后 didSave 触发 save-time 检查）。
    Sync { path: PathBuf, content: String, language_id: String },
}

/// 可观测状态：工具层读，driver 线程写。
#[derive(Default)]
struct LspState {
    ready: bool,
    error: Option<String>,
}

/// LSP server 客户端句柄。克隆它共享同一进程 + 缓存。
pub struct LspClient {
    server_name: String,
    tx: Sender<LspCommand>,
    diagnostics: Arc<Mutex<HashMap<PathBuf, Vec<DiagnosticEntry>>>>,
    state: Arc<Mutex<LspState>>,
    child: Arc<Mutex<Option<Child>>>,
    reader: Mutex<Option<JoinHandle<()>>>,
    driver: Mutex<Option<JoinHandle<()>>>,
}

impl LspClient {
    /// 启动一个 LSP server 并完成后台握手。
    ///
    /// `command` 是 argv（第一个元素是二进制名，其余是参数）。永不 panic：spawn
    /// 失败把错误记进 `state.error`，`health()` 会报告；`sync` / `snapshot` 变成
    /// no-op / 空，工具层据此降级到 `verify`。
    pub fn start(workspace_root: &Path, command: &[&str]) -> Arc<LspClient> {
        let server_name = command.first().copied().unwrap_or("lsp-server");
        let args = command.get(1..).unwrap_or(&[]);

        let diagnostics = Arc::new(Mutex::new(HashMap::new()));
        let state = Arc::new(Mutex::new(LspState::default()));
        let child = Arc::new(Mutex::new(None::<Child>));
        let (tx, rx) = mpsc::channel::<LspCommand>();

        let client = Arc::new(LspClient {
            server_name: server_name.to_string(),
            tx,
            diagnostics: diagnostics.clone(),
            state: state.clone(),
            child: child.clone(),
            reader: Mutex::new(None),
            driver: Mutex::new(None),
        });

        let mut proc = match std::process::Command::new(server_name)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(p) => p,
            Err(e) => {
                state.lock().unwrap().error = Some(format!("failed to spawn {server_name}: {e}"));
                return client;
            }
        };

        let stdin = proc.stdin.take();
        let stdout = proc.stdout.take();
        *child.lock().unwrap() = Some(proc);

        let (Some(mut stdin), Some(stdout)) = (stdin, stdout) else {
            state.lock().unwrap().error =
                Some(format!("{server_name} did not expose stdin/stdout"));
            return client;
        };

        // reader 线程：阻塞读 stdout，解帧，把 JSON 消息发给 driver。
        let (msg_tx, msg_rx) = mpsc::channel::<Value>();
        let reader = std::thread::spawn(move || {
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            let mut stdout = stdout;
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) => break, // EOF：进程退出或管道关闭。
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    Err(_) => break,
                }
                let (frames, consumed) = decode_frames(&buf);
                buf.drain(..consumed);
                for f in frames {
                    if msg_tx.send(f).is_err() {
                        return; // driver 已退出。
                    }
                }
            }
            // EOF：显式 drop 发送端，让 driver 的 recv 收到 Disconnected。
            drop(msg_tx);
        });

        // driver 线程：握手 + 处理 server 消息 + 消费 sync 指令。
        let workspace = workspace_root.to_path_buf();
        let diag = diagnostics.clone();
        let st = state.clone();
        let name = server_name.to_string();
        let driver = std::thread::spawn(move || {
            // 1) initialize。
            if stdin_write(&mut stdin, &build_initialize_request(1, &workspace)).is_err() {
                st.lock().unwrap().error = Some("failed to write initialize".into());
                return;
            }
            // 2) 等 initialize 响应（跳过头部的 window/logMessage 通知）。
            'init: loop {
                match msg_rx.recv_timeout(INITIALIZE_TIMEOUT) {
                    Ok(msg) => {
                        if msg.get("id").and_then(Value::as_u64) == Some(1) {
                            if msg.get("error").is_some() {
                                st.lock().unwrap().error =
                                    Some("initialize returned an error".into());
                                return;
                            }
                            break 'init;
                        }
                        // 响应之前的其它消息（logMessage 等）忽略。
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        st.lock().unwrap().error =
                            Some("timed out waiting for initialize response".into());
                        return;
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        st.lock().unwrap().error =
                            Some(format!("{name} exited during handshake"));
                        return;
                    }
                }
            }
            // 3) initialized。
            if stdin_write(&mut stdin, &build_initialized_notification()).is_err() {
                st.lock().unwrap().error = Some("failed to write initialized".into());
                return;
            }
            st.lock().unwrap().ready = true;

            // 已打开文件的版本号（driver 独占写）。
            let mut open_versions: HashMap<PathBuf, i32> = HashMap::new();

            loop {
                // 先消费 sync 指令（非阻塞，保证冷启动前累积的指令也被处理）。
                loop {
                    match rx.try_recv() {
                        Ok(LspCommand::Sync {
                            path,
                            content,
                            language_id,
                        }) => {
                            let uri = uri_from_path(&path);
                            match open_versions.get(&path) {
                                Some(_) => {
                                    let v = open_versions.entry(path.clone()).or_insert(0);
                                    *v += 1;
                                    let _ = stdin_write(
                                        &mut stdin,
                                        &build_did_change(&uri, *v, &content),
                                    );
                                }
                                None => {
                                    open_versions.insert(path.clone(), 1);
                                    let _ = stdin_write(
                                        &mut stdin,
                                        &build_did_open(&uri, &content, &language_id),
                                    );
                                }
                            }
                            // didSave 触发 save-time 检查（如 rust-analyzer 的
                            // checkOnSave → cargo check → publish）。
                            let _ = stdin_write(&mut stdin, &build_did_save(&uri, &content));
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return, // client 已 drop。
                    }
                }

                // 处理一条 server 消息（阻塞最多 100ms，好让 sync 指令插进来）。
                match msg_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(msg) => handle_server_message(&msg, &diag, &mut stdin),
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return, // rust-analyzer 退出。
                }
            }
        });

        *client.reader.lock().unwrap() = Some(reader);
        *client.driver.lock().unwrap() = Some(driver);
        client
    }

    /// 报告当前是否可用。启动/握手失败返回错误，工具层据此降级。
    pub fn health(&self) -> Result<(), String> {
        let st = self.state.lock().unwrap();
        if let Some(e) = &st.error {
            return Err(e.clone());
        }
        if !st.ready {
            return Err(format!("{} is still starting", self.server_name));
        }
        Ok(())
    }

    /// 启动是否已经失败（区别于「还在握手」），供 `wait_ready` 判断。
    pub fn failed_error(&self) -> Option<String> {
        self.state.lock().unwrap().error.clone()
    }

    /// 打开/更新一个文件的诊断（fire-and-forget，driver 线程消费）。
    pub fn sync(&self, path: &Path, content: &str, language_id: &str) {
        let _ = self.tx.send(LspCommand::Sync {
            path: path.to_path_buf(),
            content: content.to_string(),
            language_id: language_id.to_string(),
        });
    }

    /// 快照当前诊断缓存（克隆，按路径排序）。
    pub fn snapshot(&self) -> Vec<(PathBuf, Vec<DiagnosticEntry>)> {
        let mut out: Vec<_> = self
            .diagnostics
            .lock()
            .unwrap()
            .iter()
            .map(|(p, d)| (p.clone(), d.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // 先杀进程：reader 读到 EOF 退出 → 通道关闭 → driver 的 recv 收到
        // Disconnected 退出。随后 join 两个线程收尾。
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(r) = self.reader.lock().unwrap().take() {
            let _ = r.join();
        }
        if let Some(d) = self.driver.lock().unwrap().take() {
            let _ = d.join();
        }
    }
}

/// 处理一条来自 server 的消息：publishDiagnostics 进缓存，server→client 请求
/// 一律回 result（`workspace/configuration` 回空数组），其余通知忽略。
fn handle_server_message(
    msg: &Value,
    diagnostics: &Arc<Mutex<HashMap<PathBuf, Vec<DiagnosticEntry>>>>,
    stdin: &mut ChildStdin,
) {
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        return; // 响应消息（我们只在握手发过请求，此处无需处理）。
    };

    if method == PublishDiagnostics::METHOD {
        if let Some(params) = msg.get("params") {
            if let Some((path, entries)) = parse_publish_diagnostics(params) {
                diagnostics.lock().unwrap().insert(path, entries);
            }
        }
    } else if let Some(id) = msg.get("id") {
        // server→client 请求（workspace/configuration、client/registerCapability 等）。
        let result = if method == "workspace/configuration" {
            json!([])
        } else {
            Value::Null
        };
        let resp = json!({ "jsonrpc": "2.0", "id": id.clone(), "result": result });
        let _ = stdin_write(stdin, &resp);
    }
    // 其余纯通知（window/logMessage、$/progress 等）忽略。
}

/// 写一条消息到 server 的 stdin（driver 线程独占写）。
fn stdin_write(stdin: &mut ChildStdin, msg: &Value) -> std::io::Result<()> {
    let framed = frame_message(msg);
    stdin.write_all(framed.as_bytes())?;
    stdin.flush()
}

// ---------------- 多 server 路由 ----------------

/// 惰性启动的 LSP server 集合，按文件语言路由。
///
/// rust-analyzer（Rust）、typescript-language-server（TS/JS）、clangd（C/C++）
/// 各自在首次用到时启动，并供所有映射到它的文件共享。没有注册 server 的语言
/// （`lsp: None`，当前 Java）返回 `None`，`diagnostics` 工具据此降级到 `verify`。
pub struct LspManager {
    workspace_root: PathBuf,
    /// key = server 二进制名（首元素）：一个 server 进程被多门语言共享（TS 和
    /// JS 共用 typescript-language-server）。
    clients: Mutex<HashMap<String, Arc<LspClient>>>,
}

impl LspManager {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// `path` 所属语言的 LspClient（惰性启动，进程共享）；该语言没有注册 server
    /// 时返回 None。
    pub fn client_for(&self, path: &Path) -> Option<Arc<LspClient>> {
        let spec = crate::lang::lsp_spec_for_path(&path.to_string_lossy())?;
        let key = spec.command.first()?.to_string();
        let mut clients = self.clients.lock().unwrap();
        Some(
            clients
                .entry(key)
                .or_insert_with(|| LspClient::start(&self.workspace_root, spec.command))
                .clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};

    #[test]
    fn frame_message_uses_byte_length() {
        // body 里的中文按字节计数（不是字符数）。
        let body = json!({ "msg": "中文" });
        let framed = frame_message(&body);
        let body_len = serde_json::to_string(&body).unwrap().len();
        assert!(framed.starts_with(&format!("Content-Length: {body_len}\r\n\r\n")), "{framed:?}");
    }

    #[test]
    fn decode_frames_parses_single_and_multiple() {
        let a = json!({ "method": "one" });
        let b = json!({ "method": "two" });
        let mut buf = frame_message(&a).into_bytes();
        buf.extend_from_slice(frame_message(&b).as_bytes());

        let (frames, consumed) = decode_frames(&buf);
        assert_eq!(frames.len(), 2, "{frames:?}");
        assert_eq!(frames[0]["method"], "one");
        assert_eq!(frames[1]["method"], "two");
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn decode_frames_handles_partial_body() {
        let full = frame_message(&json!({ "method": "hello" }));
        let bytes = full.as_bytes();
        // 切掉 body 的一半。
        let (frames, consumed) = decode_frames(&bytes[..bytes.len() - 3]);
        assert!(frames.is_empty());
        assert_eq!(consumed, 0, "incomplete body must not be consumed");
    }

    #[test]
    fn decode_frames_handles_partial_header() {
        let (frames, consumed) = decode_frames(b"Content-Length: 2\r\n");
        assert!(frames.is_empty());
        assert_eq!(consumed, 0);
    }

    #[test]
    fn decode_frames_skips_garbage_between_frames() {
        let good = frame_message(&json!({ "method": "ok" }));
        let mut buf = b"garbage\r\n".to_vec();
        buf.extend_from_slice(good.as_bytes());
        let (frames, _) = decode_frames(&buf);
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert_eq!(frames[0]["method"], "ok");
    }

    #[test]
    fn roundtrip_frame_and_decode() {
        let body = json!({ "jsonrpc": "2.0", "id": 7, "result": [1, 2, 3] });
        let framed = frame_message(&body);
        let (frames, consumed) = decode_frames(framed.as_bytes());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], body);
        assert_eq!(consumed, framed.len());
    }

    #[test]
    fn build_did_open_has_correct_shape() {
        let uri = Url::parse("file:///tmp/a.rs").unwrap();
        let msg = build_did_open(&uri, "fn main() {}", "rust");
        assert_eq!(msg["method"], "textDocument/didOpen");
        assert_eq!(msg["params"]["textDocument"]["uri"], "file:///tmp/a.rs");
        assert_eq!(msg["params"]["textDocument"]["languageId"], "rust");
        assert_eq!(msg["params"]["textDocument"]["version"], 1);
    }

    #[test]
    fn build_did_open_respects_language_id() {
        let uri = Url::parse("file:///tmp/a.tsx").unwrap();
        let msg = build_did_open(&uri, "const x = 1;", "typescriptreact");
        assert_eq!(
            msg["params"]["textDocument"]["languageId"],
            "typescriptreact"
        );
    }

    #[test]
    fn build_did_change_is_full_sync() {
        let uri = Url::parse("file:///tmp/a.rs").unwrap();
        let msg = build_did_change(&uri, 3, "let x = 1;");
        assert_eq!(msg["params"]["textDocument"]["version"], 3);
        let cc = &msg["params"]["contentChanges"][0];
        assert!(cc["range"].is_null(), "full sync omits range");
        assert_eq!(cc["text"], "let x = 1;");
    }

    #[test]
    fn build_initialize_request_shape() {
        let msg = build_initialize_request(1, Path::new("/tmp/ws"));
        assert_eq!(msg["method"], "initialize");
        assert_eq!(msg["id"], 1);
        let folders = msg["params"]["workspaceFolders"].as_array().unwrap();
        assert_eq!(folders.len(), 1);
        assert!(msg["params"]["capabilities"].is_object());
    }

    #[test]
    fn flatten_diagnostic_maps_severity_and_offsets() {
        let d = Diagnostic::new(
            Range::new(Position::new(2, 3), Position::new(2, 9)),
            Some(DiagnosticSeverity::ERROR),
            Some(NumberOrString::String("E0308".into())),
            None,
            "mismatched types".into(),
            None,
            None,
        );
        let e = flatten_diagnostic(&d);
        assert_eq!(e.severity, Severity::Error);
        assert_eq!(e.line, 3); // 0-based 2 → 1-based 3
        assert_eq!(e.column, 4); // 0-based 3 → 1-based 4
        assert_eq!(e.code.as_deref(), Some("E0308"));
        assert_eq!(e.message, "mismatched types");
    }

    #[test]
    fn flatten_diagnostic_numeric_code() {
        let d = Diagnostic::new(
            Range::new(Position::new(0, 0), Position::new(0, 1)),
            Some(DiagnosticSeverity::WARNING),
            Some(NumberOrString::Number(42)),
            None,
            "warn".into(),
            None,
            None,
        );
        assert_eq!(flatten_diagnostic(&d).code.as_deref(), Some("42"));
    }

    #[test]
    fn flatten_diagnostic_collapses_multiline_message() {
        let d = Diagnostic::new(
            Range::new(Position::new(0, 0), Position::new(0, 1)),
            Some(DiagnosticSeverity::ERROR),
            None,
            None,
            "mismatched types\nexpected `u32`, found `&str`".into(),
            None,
            None,
        );
        assert_eq!(
            flatten_diagnostic(&d).message,
            "mismatched types expected `u32`, found `&str`"
        );
    }

    #[test]
    fn parse_publish_diagnostics_extracts_path_and_entries() {
        let uri = Url::from_file_path("/tmp/ws/src/main.rs").unwrap();
        let params = PublishDiagnosticsParams {
            uri,
            diagnostics: vec![Diagnostic::new(
                Range::new(Position::new(4, 5), Position::new(4, 6)),
                Some(DiagnosticSeverity::ERROR),
                Some(NumberOrString::String("E0425".into())),
                None,
                "unresolved name".into(),
                None,
                None,
            )],
            version: None,
        };
        let params = serde_json::to_value(params).unwrap();
        let (path, entries) = parse_publish_diagnostics(&params).unwrap();
        assert_eq!(path, Path::new("/tmp/ws/src/main.rs"));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].line, 5);
        assert_eq!(entries[0].column, 6);
        assert_eq!(entries[0].code.as_deref(), Some("E0425"));
    }

    #[test]
    fn path_from_uri_rejects_non_file() {
        let uri = Url::parse("untitled:Untitled-1").unwrap();
        assert!(path_from_uri(&uri).is_none());
    }
}
