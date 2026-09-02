# 四工程对比分析：可借鉴的设计模式

> 对比工程：phimint (当前) / pi / codex / deepseek-harness
> 生成时间：2026-08-27

---

## 一、各工程架构概览

| 维度 | phimint (当前) | pi | codex | deepseek-harness |
|------|---------------|-----|-------|-------------------|
| 语言 | Rust | TypeScript | Rust | TypeScript + Rust (native) |
| UI | ratatui TUI | Ink (CLI) + Web | 终端 CLI | Web + CLI (Cordis) |
| 架构风格 | 单体 + middleware | 三层分离 (Loop/Agent/Harness) | 多 crate 微服务 | Microkernel + DI |
| Session | JSONL 文件 | SQLite + branching | JSONL + history crate | JSONL + SQLite |
| 沙箱 | 无 | 无 | Landlock + Windows Sandbox | Landlock (native) |
| MCP | 无 | 无 | 完整 MCP client | 无 (plugin-based) |
| 可观测性 | tracing 日志 | 基础 telemetry | Rollout Trace (完整) | Telemetry patches |
| 构建 | Cargo | pnpm monorepo | Bazel + Cargo | pnpm monorepo |

---

## 二、最值得借鉴的 Top 10 设计

### 🔥 1. Agent Loop 三层分离 (来自 pi)

**现状问题**：phimint 的 agent loop、状态管理、session 持久化都耦合在 `ui/app.rs` 的大文件里。

**pi 的做法**：
- **Loop 层** (`agent-loop.ts`)：纯函数，无状态，只负责 streaming + tool 执行
- **Agent 层** (`agent.ts`)：拥有状态、事件分发、队列管理
- **Harness 层** (`agent-harness.ts`)：session 持久化、多 lane、compaction

**建议**：
```
src/
  agent_loop.rs    ← 纯 async fn run_loop(ctx, config) -> Result<()>
  agent.rs         ← AgentState + 事件 channel + 队列
  harness.rs       ← session 持久化 + 多 session 管理
```

---

### 🔥 2. 双层消息队列：Steering + Follow-up (来自 pi)

**现状问题**：phimint 只有单一的 prompt 输入，用户无法在 agent 运行中纠正方向。

**pi 的做法**：
- **Steering Queue**：agent 每轮结束后、下轮 LLM 调用前注入，用于「方向纠正」
- **Follow-up Queue**：agent 完全空闲后才注入，用于「继续工作」
- 支持 `drain-all` / `one-at-a-time` 两种模式

**建议**：在 TUI 中添加 `Ctrl+S` 注入 steering 消息（如"别管那个，先做 X"），在 agent 运行时可打断但不中断。

---

### 🔥 3. Agent Graph Store (来自 codex)

**现状问题**：phimint 的 sub-agent 只有父子关系，没有持久化的拓扑图。

**codex 的做法**：
- `AgentGraphStore` trait 定义了 thread-spawn 的 parent/child 边
- 支持 BFS 遍历后代、按状态过滤
- `ThreadSpawnEdgeStatus` 枚举追踪每个子 agent 的生命周期

**建议**：
```rust
trait AgentGraphStore: Send + Sync {
    fn upsert_edge(&self, parent: SessionId, child: SessionId, status: SpawnStatus) -> impl Future<Output = Result<()>>;
    fn list_children(&self, parent: SessionId, filter: Option<SpawnStatus>) -> impl Future<Output = Result<Vec<SessionId>>>;
    fn list_descendants(&self, root: SessionId) -> impl Future<Output = Result<Vec<SessionId>>>;
}
```

---

### 🔥 4. Rollout Trace 可观测性 (来自 codex)

**现状问题**：phimint 只有 tracing 日志，无法回放分析 agent 的决策过程。

**codex 的做法**：
- 每次 session 生成一个 trace bundle（`trace.jsonl` + 原始 payload）
- Reducer 模式：raw events → 语义化 `RolloutTrace`
- 记录 inference attempt、tool dispatch、compaction、MCP call 等全部生命周期事件
- 支持离线回放和分析

**建议**：为 phimint 添加轻量级 trace writer：
```rust
struct TraceWriter { path: PathBuf, seq: AtomicU64 }
impl TraceWriter {
    fn write_event(&self, event: TraceEvent) -> Result<()>;
}
enum TraceEvent {
    TurnStart { turn_id: u64 },
    InferenceRequest { model: String, tokens: usize },
    ToolCall { name: String, args: Value },
    ToolResult { name: String, duration_ms: u64, success: bool },
    TurnEnd { turn_id: u64, reason: StopReason },
}
```

---

### 🔥 5. Landlock 沙箱 (来自 codex + deepseek-harness)

**现状问题**：phimint 的 `execute_command` 没有任何文件系统隔离。

**两个工程的做法**：
- **codex**：独立的 `codex-linux-sandbox` binary，使用 Landlock 限制文件系统访问
- **deepseek-harness**：`landlock-run` native addon，`--probe` 探测可用性，三态返回（Full/Partial/Unusable）

**建议**：
```rust
enum SandboxEnforcement { Full, Partial, Unusable }

fn probe_sandbox() -> SandboxEnforcement {
    // Linux: 检查 landlock ABI 版本
    // macOS: 检查 sandbox-exec 可用性
    // Windows: 检查 Windows Sandbox
}

struct SandboxGrants {
    read_only: Vec<PathBuf>,    // workspace root
    read_write: Vec<PathBuf>,   // workspace root (需要写入时)
    // 其他路径默认拒绝
}
```

---

### 🔥 6. 三阶段 Tool 执行管线 (来自 pi)

**现状问题**：phimint 的 tool 执行是直接调用，没有 before/after hook。

**pi 的做法**：
1. **Prepare**：验证 tool 存在 → 参数预处理 → schema 验证 → beforeToolCall hook（可阻断）
2. **Execute**：实际执行，捕获错误
3. **Finalize**：afterToolCall hook（可修改结果）

**关键安全特性**：当 LLM 输出被截断（`stop_reason === length`）时，**自动失败所有 tool call**，防止执行残缺的 JSON 参数。

**建议**：
```rust
async fn execute_tool_pipeline(
    tool: &dyn Tool,
    args: Value,
    hooks: &ToolHooks,
    stop_reason: StopReason,
) -> ToolResult {
    if stop_reason == StopReason::Length {
        return ToolResult::error("truncated output, tool call skipped");
    }
    let prepared = hooks.before(tool, args).await?;
    let result = tool.execute(prepared).await;
    hooks.after(&result).await
}
```

---

### 🔥 7. 层叠配置系统 (来自 deepseek-harness)

**现状问题**：phimint 的配置只有 CLI 参数 + 环境变量，没有分层。

**deepseek-harness 的做法**：
```
[空根配置]
  ↓ bundle 层 (内建默认值)
  ↓ 平台适配层
  ↓ profile 层 (~/.config/phimint/config.yml)
  ↓ 项目层 (.phimint/config.yml)
  ↓ CLI --flag 覆盖层
```
每层都是一个 patch，按序叠加。支持 `--dump-config` 不启动就查看最终配置。

**建议**：用 `serde` + 自定义 merge 实现：
```rust
struct ConfigStack {
    defaults: Config,        // 硬编码默认值
    user: Option<Config>,    // ~/.config/phimint/config.yml
    project: Option<Config>, // .phimint/config.yml
    cli_overrides: Config,   // CLI 参数
}
impl ConfigStack {
    fn compose(self) -> Config { /* 按序 merge */ }
    fn dump(&self) -> String { /* 输出最终配置，不启动 */ }
}
```

---

### 🔥 8. MCP Client 架构 (来自 codex)

**现状问题**：phimint 没有 MCP 支持，无法连接外部 tool server。

**codex 的做法**：
- 完整的 `rmcp-client` crate (1600+ 行)
- 支持 stdio / streamable HTTP / SSE / in-process 多种 transport
- OAuth 2.0 认证流程（PKCE, refresh token, store pinning）
- Elicitation 支持（server 向 client 请求用户输入）
- 重试、重连、redirect 跟随

**建议**：phimint 可以先实现一个轻量级 MCP client：
```rust
// 先支持 stdio transport，后续加 HTTP
trait McpTransport {
    async fn send(&self, msg: JsonRpcMessage) -> Result<()>;
    async fn recv(&self) -> Result<JsonRpcMessage>;
}

struct McpClient {
    transport: Box<dyn McpTransport>,
    tools: Vec<McpTool>,
}
```

---

### 🔥 9. Plugin/Skill 热重载 (来自 deepseek-harness)

**现状问题**：phimint 的 skills 只在启动时加载，修改后需重启。

**deepseek-harness 的做法**：
- Cordis DI 容器支持 fiber 状态机（Active → Disposing → Disposed）
- HMR 热重载：配置文件变更时自动 recompose
- `structuredClone` 确保每次 reload 不会污染旧状态

**建议**：为 phimint 添加 skill 文件 watcher：
```rust
// 使用 notify crate 监听 .phimint/skills/ 变更
// 变更时重新加载 skill，通过 watch channel 通知 agent
let (skill_tx, skill_rx) = watch::channel(skills);
spawn_skill_watcher(skill_dir, skill_tx);
```

---

### 🔥 10. 压力测试 + E2E 测试框架 (来自 deepseek-harness)

**现状问题**：phimint 只有单元测试（app_tests, task_panel_tests），没有集成测试。

**deepseek-harness 的做法**：
- **Stress Test**：`reasoning-chunks.stress.ts` 模拟高频 reasoning chunk 风暴，测量延迟分布
- **E2E Test**：完整的 profile lifecycle fixture（启动 → ready → settled → disposed）
- **Snapshot Test**：ACP 协议消息的快照回归测试
- **Property-based Testing**：用于协议解析等纯函数

**建议**：
```rust
// 用 proptest 做 property-based testing
proptest! {
    #[test]
    fn tool_args_parse_roundtrip(args in arb_tool_args()) {
        let parsed = parse_tool_args(&serde_json::to_string(&args)?)?;
        prop_assert_eq!(args, parsed);
    }
}
```

---

## 三、优先级建议

| 优先级 | 改进项 | 来源 | 难度 | 影响 |
|--------|--------|------|------|------|
| P0 | 三阶段 tool 执行 + 截断安全 | pi | 低 | 🔥 安全性 |
| P0 | 双层消息队列 (steering/follow-up) | pi | 中 | 🔥 UX |
| P1 | Agent Loop 三层分离 | pi | 高 | 架构清晰度 |
| P1 | 层叠配置系统 | deepseek-harness | 中 | 可维护性 |
| P1 | Rollout Trace 可观测性 | codex | 中 | 可调试性 |
| P2 | Landlock 沙箱 | codex + deepseek | 中 | 安全性 |
| P2 | Agent Graph Store | codex | 低 | sub-agent 管理 |
| P2 | MCP Client | codex | 高 | 生态接入 |
| P3 | Skill 热重载 | deepseek-harness | 低 | DX |
| P3 | 压力测试框架 | deepseek-harness | 中 | 质量保障 |

---

## 四、各工程的反模式（应避免）

| 工程 | 反模式 | 说明 |
|------|--------|------|
| codex | Bazel + Cargo 双构建 | 维护成本高，除非团队规模大 |
| pi | TypeScript 单线程 | tool 执行受 Node.js 事件循环限制 |
| deepseek-harness | 过度抽象的 DI | Cordis fiber 状态机对小项目太重 |
| phimint (当前) | 大文件 | `app.rs` 400+ 行、`render.rs` 500+ 行应拆分 |
