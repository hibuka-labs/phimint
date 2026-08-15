# Phase 6 实施计划（验证闭环产品化）

产品招牌「永远不把编不过的代码交给你」的落地，两半：

- **6a 强制 verify 闸门**（保证，已完成）——动过代码不验就拦下「报 done」。
- **6b LSP 诊断**（加速，下一步）——不重编译的快速内环，边写边报错。

## 6a 强制闸门 ✅（已完成，2026-08-16）

- `src/gate.rs`：`VerifyEnforcementMiddleware`（consumer-side `Middleware`，零改框架，design §8.3）。
  - dirty 跟踪：`write_file`/`edit_file` 落在 `.rs`/`Cargo.toml`/`Cargo.lock` 置位；`verify`/`merge` 清位；文档写不置位。
  - `on_post_llm`：纯文本 + dirty → `skip_push=true` + `follow_up_message`（逼循环再来一轮）；`max_nudges`(3) 后降级为追加「⚠️ Unverified」。
  - `on_user_message`：每轮重置 dirty/nudges。
  - `deny` 模式（`writes_possible=false`）整体关闭，不误伤只读 agent。
- 接线：`agent.rs::build` 加 `.middleware(...)`；`main.rs` 传 `cli.approval != "deny"`。
- 验证：单测 114 全绿（+17 gate）；真机 smoke `write_file`→「done」被拦（nudge 1）→`verify` ✓→done；`middleware_count` 1→2。

**已知边界**（记下来，后续按需补）：

1. `execute_command cargo check` 不清 dirty（只认 `verify`/`merge`）——agent 用 shell 验证会多 nudge 一次，无害（verify 幂等便宜）。后续可解析 shell 命令补上。
2. 路径过滤 Rust-first（`.rs`+Cargo 文件）；多语言时需扩展 `is_code_path`。

## 6b LSP 诊断 ✅（已完成，2026-08-16）

- `src/lsp.rs`：手写最小 LSP 客户端（reader/driver 双线程 + `Arc<Mutex<HashMap<path, Vec<DiagnosticEntry>>>>` 缓存），`initialize`/`initialized` 握手 + `didOpen`/`didChange`/`didSave` + 收 `publishDiagnostics`；`Drop` 杀进程收尾；启动/握手失败记 `state.error` 并降级。
- `src/tools/diagnostics.rs`：pull 工具，同步工作区 `.rs` → 等握手 + 沉降 2s → 读缓存返回 `file:line:col  code  message` 摘要（只保留 error/warning、多行 message 压单行、截断 4000 字）。
- 接线 `agent.rs`（`register_tool` + 系统提示）+ `main.rs`（`mod lsp`）。`lsp-types = "0.95"` 仅作协议类型基础。
- 单测 133 全绿（+18）；真机 smoke：改坏 `src/main.rs` → `diagnostics` 秒级报 `2 error(s):\n  src/main.rs:2:22  E0308  mismatched types …`，`tool_count` 10→11。

目标：把「编译报错」提前到敲完代码的秒级，不用每次 `cargo check` 重编译。

**范围（首版克制）**：

- 只做 **diagnostics**（`publishDiagnostics`），不做 completion / goto / hover。
- 只 **Rust**（rust-analyzer）；`lsp-types` 当协议类型基础。
- 形态：`diagnostics` 工具（pull 式，返回当前报错列表，对齐 `verify` 的 `file:line:col  code  message` 摘要格式）。

**技术选型**（design §8.4）：`lsp-types`（base 类型）+ rust-analyzer 进程；客户端握手/JSON-RPC 序列化手写 ~150 行，或用 `nexo-lsp`（2026-05）/`codive-lsp`（2026-01）省事。启动一个进程级单例 rust-analyzer，`didOpen`/`didChange` 工作区文件，收 `publishDiagnostics` 存内存，`diagnostics` 工具读缓存。

**任务清单**：

1. 定 LSP 客户端库（手写 vs nexo-lsp / codive-lsp），锁依赖。
2. `src/lsp.rs`：rust-analyzer 启动 + JSON-RPC 握手（`initialize`/`initialized`）+ `textDocument/didOpen`/`didChange` + 收 `publishDiagnostics` 进 `Arc<Mutex<HashMap<path, Vec<Diagnostic>>>>`；进程崩溃重启、超时兜底。
3. `src/tools/diagnostics.rs`：pull 工具，读缓存返回摘要（复用 `verify` 的 `parse_diagnostics`/`summarize_errors`）。
4. 接线 `agent.rs`（`register_tool`）+ 系统提示加 `diagnostics` 说明。
5. 单测（消息序列化/反序列化、diagnostics 摘要格式）+ 真机 smoke（改坏一个文件 → `diagnostics` 秒级报错）。
6. （可选）闸门联动：`diagnostics` 也列为 `VERIFY_TOOLS`，或诊断结果有 error 也算「未验」。

**依赖/风险**：

- rust-analyzer 冷启动 ~1-2s，首次诊断有延迟；进程管理（崩溃重启、超时）要兜底。
- 大 workspace 诊断量大，`diagnostics` 输出要截断（复用 `MAX_SUMMARY_CHARS`）。
