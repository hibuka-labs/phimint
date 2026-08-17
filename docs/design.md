# phiforge 设计文档

> 状态：Phase 1–5 完成，Phase 6 设计中（产品定位已反转，§1）
> 关联框架：`phi-agent`（本仓库的运行时框架）

## 1. 定位

**phiforge 是一个基于 `phi-agent` 的 AI 编码 agent —— 产品优先，验证框架是副产品。**

- 首要目标：**做一款好用的编码产品**。招牌是「**永远不把编不过的代码交给你**」——改完先验、验过才交。
- 次要目标：**验证 `phi-agent`**。做好产品与压测框架是一件事的两面：只有真实用户会因框架的弱点而痛，框架的 bug 才会暴露；产品够好，本身就是框架的证明。

> 2026-08-16 修订：原「用写代码压测框架」翻转为「产品优先」。写代码仍是压测框架最狠的场景，但方向从「为压测而写代码」变成「为做好产品而写代码，顺带压测框架」。

---

## 2. 为什么「写代码」是最好的验证载体

写代码是唯一一个把整条链路全跑一遍的任务：

```
理解需求 → 分解/计划 → 读代码 → 多文件改 → 跑测试/编译 → 报错 → 迭代修 → 完成
```

它同时压测：React 循环、计划执行、工具编排、批量编辑、错误恢复、**多 agent 大任务分解**。

尤其 **多 agent**：一个大的编码任务来了，怎么拆、拆给谁、子 agent 怎么通信、结果怎么合——这套东西不在真实大任务里跑，永远测不出真的坑。这正是本项目的核心验证点。

---

## 3. 参照标准（好 AI coder 的通用架构）

研究 Claude Code / Aider / Cursor / Cline 这些成熟工具，抽出 8 条共性模式，phiforge 按这套标准设计：

1. **Agent 循环** —— LLM 想 → 调工具 → 看结果 → 再想，循环到完成。所有好 coder 都是这个 ReAct 循环。
2. **最小工具面** —— 读 / 写 / 搜 / 列 / 跑，不多不少。工具的「形状」（参数、返回格式）决定 agent 能不能用好。
3. **上下文架构（分水岭）** —— 不把整个仓库塞进 context；给一张「仓库地图」让 LLM 知道结构；agent 主动搜、主动读它需要的（拉取式，非推送式）。**这是好 coder 和差 coder 最大的差别。**
4. **精准编辑** —— SEARCH/REPLACE 块、精确匹配、原子写，编辑必须无歧义、可验证。
5. **验证闭环** —— 改完跑测试/编译，失败喂回，迭代到绿。
6. **安全** —— 危险操作审批、diff 呈现。（无工作区沙箱——对齐 Claude Code，安全靠审批层。）
7. **流式 TUI** —— 逐字流式、工具调用可视化、diff 高亮、内联审批。
8. **上下文预算** —— 长对话的截断 / 摘要 / 压缩。

**phiforge 已覆盖 1/2/3/4/5/6/8，当前最大缺口是 #7（流式 TUI）**，对应 §9 的 TUI 设计（Phase 5）。

---

## 4. 核心能力需求

| 能力 | 来源 | 状态 |
|---|---|---|
| Agent 循环 / ReAct / 计划 | `agent-base`（react_loop、plan_runner） | ✅ 复用 |
| 文件读/写/精准编辑 | `phi-kernel-tools/file`（edit_file 原子写 + 4 级匹配） | ✅ 复用 |
| Shell（跑测试/编译） | `phi-kernel-tools/local_shell`（feature `shell`） | ✅ 复用 |
| 多 agent 原语 | `phi-kernel-tools/multi_agent`（spawn/send/wait/close/list） | ✅ 复用 |
| 审批 / 权限 | `agent-base` ApprovalHandler + `phi-agent` Auto/DenyAll | ✅ 复用 |
| Session / 事件流 | `phi-agent` session + render + event_log | ✅ 复用 |
| **仓库地图（RepoMap）** | 无 → 新增：tree-sitter 符号/结构索引（多语言，加语言只加 grammar + query） | 🔧 新增 |
| **代码搜索**（按内容定位） | 无 → 新增 `RipgrepTool` | 🔧 新增 |
| **LSP 诊断**（改完即时反馈类型错误） | 无 → 新增 LSP client | 🔧 新增（后置，见 §8） |

---

## 5. 架构

### 5.1 依赖链

```
agent-base (运行时内核)
    ↑
agent-works (MCP / Skills)
    ↑
phi-agent (框架 + CLI)          ← 复用，不修改
    ↑
phiforge (本工具)               ← consumer，注入应用工具
```

### 5.2 仓库结构（规划）

```
phiforge/
├── src/
│   ├── main.rs            # CLI 入口：启动 TUI REPL
│   ├── repomap.rs         # 仓库地图：tree-sitter 符号/结构索引
│   ├── lang/              # LanguageAdapter（多语言边界）
│   │   ├── mod.rs         # trait + 项目类型检测注册表
│   │   ├── rust.rs        # RustAdapter
│   │   └── python.rs      # PythonAdapter（后续）
│   ├── decompose.rs       # 大任务分解（多 agent 的「拆」）
│   ├── merge.rs           # 结果合并 + 冲突处理
│   ├── verify.rs          # 验证闭环（编译/测试反馈）
│   ├── ui/                # ratatui TUI
│   │   ├── app.rs         # 状态 + 事件循环
│   │   ├── render.rs      # 画帧
│   │   └── input.rs       # 键盘输入
│   └── tools/
│       ├── ripgrep.rs     # 代码搜索
│       └── lsp.rs         # LSP 诊断 client（后置）
├── docs/
└── Cargo.toml
```

### 5.3 LanguageAdapter（多语言边界）

多语言的核心抽象——语言相关的只有「检测 / 构建命令 / 测试命令 / LSP server」四件事，收进一个 trait，核心 agent 一行不改：

```rust
trait LanguageAdapter {
    fn detect(root: &Path) -> Option<ProjectType>;
    fn build_command(&self) -> String;          // "cargo build" / "pytest"
    fn test_command(&self) -> String;           // "cargo test" / "pytest"
    fn lsp_server(&self) -> LspServerConfig;    // rust-analyzer / pyright
    fn manifest_files(&self) -> Vec<Path>;      // Cargo.toml / pyproject.toml
}
```

加一门语言 = 实现一个 adapter + 一个 tree-sitter grammar/query（RepoMap 用），约 1～2 天/门。

---

## 6. 核心流程（一次写代码任务）

```
用户: "帮我在 lib 里加一个带缓存的 get_user 函数"

1. 生成/刷新仓库地图（RepoMap）→ 作为初始上下文
2. 理解需求 + 读相关代码（RipgrepTool 定位 + read_file）
3. 计划：改哪些文件、加什么（plan_runner）
4. 写/改代码（write_file / edit_file）
5. 验证：跑 cargo test / cargo build（shell）
6. 报错 → 解析失败摘要 → 精准定位 → 修 → 重跑（迭代循环）
7. 完成，给出改动 diff
```

> 关键：第 5–6 步的「失败喂回 → 迭代修」闭环，是编码 agent 的核心竞争力，也是本项目要重点打磨的地方。

---

## 7. 多 agent 大任务分解（核心难点）

> 本项目对 `phi-agent` **最有价值的压测点**。多 agent 的价值不在「spawn 得够多」，而在能否解决下面四个难问题。

### 7.1 四个难问题

1. **分解质量** —— 谁拆、怎么拆才能让子任务真正独立（不互相等待、不重叠）。
2. **上下文传递** —— 每个子 agent 该知道什么：给多了污染、给少了干不了。
3. **冲突处理** —— 两个子 agent 改到重叠文件怎么办。
4. **合并验证** —— 拼起来之后，谁验证「整体能编译、能跑通」。

### 7.2 分解策略（初步）

- 优先按**模块/文件边界**切，避免单文件内并发编辑冲突。
- 单文件改动过多时，按「互不重叠的文本区间」切。
- 分解结果建模为 `Plan`，用 `plan_runner` 执行。

### 7.3 合并与冲突（初步）

- 子 agent 输出改动 → 主 agent 按文件聚合。
- 重叠区间 = 冲突，标记交用户仲裁，不静默覆盖。
- 合并后跑一次验证（编译/测试），失败摘要喂回主 agent 迭代修。

> **设计原则：子 agent 只做「切片内实现」，主 agent 负责「拆、合、验」三件事。**

### 7.4 适用边界（诚实）

多 agent 在编码里的价值**不是「越大越强」，而是「越能并行越强」**：

| 任务类型 | 多 agent 表现 |
|---|---|
| 深链式推理（feature 内部 A 依赖 B 依赖 C） | 差 —— 不如一个强 agent 串行做 |
| 可并行切片（多个独立模块、批量迁移、审计） | 好 —— 天然并行，多 agent 的甜区 |

**分解器必须能判断「该不该并行」**：能切就扇出，不能切就单 agent 串行。这是分解质量的核心。

---

## 8. 验证闭环产品化（先验再交）

> Phase 6 主题：把 §6 第 5–6 步的「失败喂回 → 迭代修」从「靠 prompt 引导」升级成「产品保证 + 更快反馈」。拆两半：**6a 强制 verify 闸门**（保证）+ **6b LSP 快速内环**（加速）。

### 8.1 为什么写代码场景 LSP 价值大

开放式写新代码，不靠 diagnostics 会「盲写」：

| | 没有 LSP（跑编译） | 有 LSP（diagnostics） |
|---|---|---|
| 改完发现「第 42 行类型错了」 | 跑 `cargo build`，10–60 秒，自己解析报错 | 改完立刻拿到「42 行：类型不匹配」 |
| 修完再验证 | 再跑一遍编译 | 增量，快 |

### 8.2 先验再交的两半：权威闸门 + 快速内环

「永远不把编不过的代码交给你」靠两半拼起来：

- **权威闸门（已有，Phase 3）**：`verify` 跑 `cargo check`/`cargo test`，是「交出去之前必须过」的最终判定。
- **快速内环（Phase 6b）**：LSP diagnostics，改完**不重编译**立刻拿到 `file:line:col`。它不替代闸门，它让闸门「跑得起」——每次改完等 30 秒编译，agent 会本能地跳过验证；快了它才愿意每次都验。

即：LSP 让 agent **愿意**验，`verify` 让 agent **必须**验。

### 8.3 强制 verify 闸门（Phase 6a）——「必须验」的保证

「必须验」不是 prompt 劝告，而是框架层卡死：本 turn 动过文件（`write_file`/`edit_file`）又没跑过 `verify`，「报 done」就被拦下、强制先验。

**机制（框架已有，零改框架）**：`agent-base` 的 `Middleware` trait 有 `on_post_llm` 钩子，`PostLlmCtx` 的 `skip_push`（压掉本次结束）+ `follow_up_message`（注入下一条）就是「拦截结束 + 逼再走一轮」的通用能力，已有 `ToolEnforcementMiddleware` 先例（逼 agent 必须调工具，带 `max_nudges=3` 防死循环）。phiforge 写一个 `VerifyEnforcementMiddleware`：

```rust
struct VerifyEnforcementMiddleware { tracker: Arc<EditTracker> }

impl Middleware for VerifyEnforcementMiddleware {
    async fn on_post_llm(&self, ctx: &mut PostLlmCtx) -> AgentResult<()> {
        if !ctx.is_tool_call && self.tracker.edited_since_verify() {
            ctx.skip_push = true;
            ctx.follow_up_message =
                Some("CRITICAL: 你改过文件但还没 verify，先跑 verify 再汇报".into());
        }
        Ok(())
    }
}
```

`EditTracker` 是 phiforge 的共享状态：包装 `write_file`/`edit_file` → 置脏，`verify` → 清脏。**框架完全不知道「verify」「编译」是什么**——它只提供「响应可压掉、可注入下一条」这个通用能力；「编码必须验」是 phiforge 的策略，不是框架的默认。**底线：编码专属的强制策略不硬编码进框架**（别的业务不适用）。

三个兜底（防「绝对化」引出 bug）：

1. **死锁**：逼到无限循环让 agent 收不了尾。→ 学 `max_nudges`，强制 N 次（默认 3）后**降级**：不再压结束，改成最终回复打「⚠️ 未验证」交用户仲裁。
2. **误伤**：改 README/文档等非编译路径，逼跑 `cargo check` 无意义。→ 只对「编译相关路径」的改动计脏。
3. **安全**：强制跑 verify 不新增执行面——`verify` 本就经 shell、已过审批策略，闸门只是复用同一工具。

### 8.4 LSP 技术选型（Phase 6b）

- 底座：**`lsp-types`**（协议类型，事实标准，v0.95.1）
- 现成 client（很新，用前评估成熟度）：
  - **`nexo-lsp`**（2026-05）—— 进程内 client，包 rust-analyzer/pylsp/tsserver/gopls，暴露 go_to_def/hover/references/workspace_symbol/diagnostics
  - **`codive-lsp`**（2026-01）—— 面向 AI coding agent 的 LSP client 基础设施
- 兜底方案：`lsp-types` + 手写 ~150 行 stdio codec，自己封装需要的几个方法
- **范围**：首版只做 **diagnostics**（`didOpen`/`didChange` 同步 + 拉 `textDocument/publishDiagnostics`），不做 completion（agent 整文件写，不做行补全）；go-to-def/hover/references 后续再议。
- **集成**：`diagnostics` 工具（拉取式，复用 `Tool` trait + 审批），agent 写文件后主动调；不做后台常驻推送（YAGNI）。
- **语言**：Rust-only 起步（`LanguageAdapter` 抽象已备，§5.3），多语言后置。

---

## 9. TUI 设计（REPL + ratatui）

形态：**仅 REPL**，终端 TUI，风格类似 Claude Code。

### 9.1 技术选型

- **`ratatui`**（+ `crossterm` 后端）—— Rust TUI 事实标准，维护活跃、异步友好。
- 替换现有 REPL 的 `rustyline` 线性输入，改为 ratatui 的键盘事件处理。

### 9.2 事件流已经 TUI-ready

`agent-base` 的 `RuntimeEvent` 已细到覆盖 TUI 每个元素，**无需改 agent 循环**：

| TUI 元素 | RuntimeEvent |
|---|---|
| 逐字流式输出 | `TextDelta` |
| 思考过程 | `ThoughtDelta` |
| 工具调用（参数/结果） | `ToolCallStarted`(args_json) / `ToolCallFinished`(summary, denied) |
| 内联审批弹窗 | `AwaitingApproval`(ApprovalRequest) |
| 计划面板 | `PlanUpdated`(objective + plan) |
| 多 agent 归属 | 每个事件的 `agent_id` |

### 9.3 架构：独立的事件消费者

现有 `EventRenderer` 是 `Write` sink 式（线性输出），塞不下 full TUI。TUI 是**独立的 `RuntimeEvent` 消费者**：

```
agent 循环（phi-agent，不动）
   │  RuntimeEvent 经 mpsc channel 流出
   ▼
phiforge TUI（ratatui）
   ├─ 事件接收 task：消费事件 → 更新状态 → 画帧
   └─ 输入 task：键盘 → 用户输入 → agent；审批键 → ApprovalHandler
```

### 9.4 布局（初版）

```
┌─────────────────────────────┐
│  计划面板（PlanUpdated）      │
├─────────────────────────────┤
│  输出区（TextDelta / 工具调用）│
│  · 流式文本                  │
│  · 工具调用 + diff           │
├─────────────────────────────┤
│  （审批弹窗 AwaitingApproval）│
├─────────────────────────────┤
│  输入框（底部）               │
└─────────────────────────────┘
```

### 9.5 验证价值

- 压测事件模型：事件流够不够细、够不够驱动一个富 TUI。
- 压测 `agent_id`：多 agent 场景下，TUI 能否清楚展示「哪个子 agent 在干嘛」。
- 压测审批闭环：`AwaitingApproval` 能否支撑内联的 approve/reject 交互。

### 9.6 状态机与过程显示

自动化编程时，用户要能实时看到 agent **当前在干什么**（状态）和**已经干了什么**（过程）。二者都由事件流驱动：

**状态机（状态栏驱动）**

```
Idle ──用户输入──▶ Running ──RunFinished──▶ Idle
                     │
                     ├─ Thinking      （ThoughtDelta）
                     ├─ Streaming     （TextDelta）
                     ├─ ToolCalling   （ToolCallStarted → 具体工具名）
                     │    └─▶ AwaitingApproval（等待审批）
                     └─ （循环回到 Thinking，直到 LLM 停）
```

状态栏按当前状态渲染：

| 状态 | 触发事件 | 状态栏显示 |
|---|---|---|
| 思考中 | `ThoughtDelta` | 🤔 思考中… |
| 流式输出 | `TextDelta` | 💬 输出中… |
| 调用工具 | `ToolCallStarted` | 🔧 `read_file` → src/lib.rs |
| 跑命令 | `ToolCallStarted`(shell) | ⏳ 运行 `cargo test`… |
| 等待审批 | `AwaitingApproval` | ⚠️ 等待审批：edit_file |
| 完成 | `RunFinished` | ✅ 完成 |

**过程显示（工具调用日志）**：输出区累积展示已发生的工具调用（名称 + 参数摘要 + 结果摘要），让用户能回看 agent 干了什么。

**验证价值**：状态机是事件流的另一个消费者——如果事件不够细、时序不对，状态栏就会卡在错误的状态，直接暴露事件模型的问题。

---

## 10. 安全与审批模型

### 10.1 框架已有（复用，不重造）

- `ApprovalHandler` trait：`approve(request, cancel_token) -> ApprovalDecision`
- `ApprovalDecision` 三态：`AllowOnce` / `AllowAlways` / `Deny` —— 正好对应交互式审批的「y / 永久放行 / n」
- `ApprovalRequest.risk_level`：`Safe` / `Destructive` —— 让「智能审批」变成读一个字段
- `AutoApprovalHandler`（Auto / DenyAll）+ `AwaitingApproval` 事件

### 10.2 审批模式（三种）

| 模式 | 行为 | 实现 |
|---|---|---|
| `auto` | 全自动，什么都不问 | 复用 `AutoApprovalHandler(Auto)` ✅ |
| `ask`（默认） | 写操作逐条弹窗 | 新增 `TuiApprovalHandler`（唯一要写的）🔧 |
| `deny` | 只读，拒绝所有写 | 复用 `AutoApprovalHandler(DenyAll)` ✅ |

### 10.3 交互式审批的 TUI 交互

`ask` 模式下，`AwaitingApproval` → TUI 内联弹窗，三键：

- `y` → `AllowOnce`（仅本次放行）
- `a` → `AllowAlways`（此类操作本次会话永久放行，需 handler 维护会话级 allowlist）
- `n` → `Deny`

### 10.4 智能细化（可选，Phase 2）

`TuiApprovalHandler` 读 `request.risk_level` 分流：`Safe`（读文件/搜索/列）自动放行，`Destructive`（写文件/shell 危险命令）才弹窗。风险等级已在请求里，成本是一个 `if`。

### 10.5 其他安全底线

- **无工作区沙箱**：文件操作允许工作区外路径（绝对路径 + `..`），安全靠审批层（`auto`/`ask`/`deny`）而非路径边界，对齐 Claude Code。`resolve_path`（phi-kernel-tools）+ `validate_workspace_path`（phiforge）均已放宽，只拒绝空路径。
- **原子写**：`edit_file` 写临时文件再 rename，改坏可回滚。
- **diff 逐条审**：改动以 diff 呈现，用户逐条确认。

---

## 11. 验证目标（对 phi-agent 的能力压测）

| 能力点 | phiforge 如何压测它 |
|---|---|
| React/计划执行 | 需求 → 计划 → 多步执行 |
| 工具编排 | 读/写/搜索/跑测试的组合调用 |
| 失败恢复 | 编译报错 → 定位 → 修 → 重跑 的迭代循环 |
| **多 agent 协作** | 大任务分解 → 扇出 → 通信 → 合并 |
| 批量文件编辑 | 多文件、大仓库下的原子性 |
| 审批/权限 | diff 的逐条 approve/reject |
| 事件流/会话 | 全程可观测、可持久化 |

---

## 12. 里程碑

> 实际拆分比初版更细（初版把 RipgrepTool / RepoMap / verify 全压进 Phase 1）。进度见 §14。

### Phase 0 — 脚手架 ✅
- `Cargo.toml`（path deps + `[patch.crates-io]` 指向本地 sibling crates）+ 最小 `main.rs`，`cargo build` 通过。

### Phase 1 — 垂直切片 ✅
- 单 agent + file/shell 工具 + REPL，跑通「需求 → 写 → cargo check → 迭代修」闭环。
- 事件可观测：`session.log`（tracing）+ `turn_NNN.jsonl`（结构化事件流，`save_turn_log`）。
- smoke test 通过：从零脚手架 Rust 项目 + 带缓存的 `get_user`，8 轮 / 9 工具调用 / 0 失败。

### Phase 2 — 代码搜索 + 仓库地图（上下文架构 #3）✅
- 新增 `RipgrepTool`（按内容定位符号）+ `repomap.rs`（tree-sitter 结构索引）。
- **目标**：给 agent 一张结构化仓库地图 + 精准搜索，替代「用 shell 命令瞎探测」。

### Phase 3 — 验证闭环 + 审批 ask 模式 ✅
- `tools/verify.rs`（`verify` 工具：编译/测试失败摘要喂回 LLM，rustc 报错解析为 `file:line:col  code  message`）。
- 审批两层（`approval.rs`：`ApprovalPolicy` 闸门 + `CliApprovalHandler` 决策；`--approval auto/ask/deny`）。

### Phase 4 — 多 agent 大任务分解（核心验证点）✅
- `decompose.rs` / `merge.rs`，解决 §7 四个难问题。

### Phase 5 — TUI（ratatui）🔧
- REPL 升级为 ratatui TUI（§9）：计划面板 / 输出区 / 审批内联弹窗 / 输入框。
- 压测点：事件流够不够驱动富 TUI、`agent_id` 多 agent 归属、`AwaitingApproval` 内联审批。

### Phase 6 — 验证闭环产品化（先验再交）✅
- **6a 强制 verify 闸门** ✅：`VerifyEnforcementMiddleware`（consumer-side，零改框架）——动过代码文件未 verify 就拦下「报 done」逼先验，`max_nudges` 后降级为「⚠️ 未验证」标记（§8.3）。
- **6b LSP 诊断** ✅：`lsp-types` + rust-analyzer，`diagnostics` 工具拉 `publishDiagnostics`（不重编译的快速内环）。首版只 diagnostics，不做 completion/goto（§8.4）。
- **顺序**：6a 先（强需求 + 便宜 + 不依赖 LSP），6b 后（加速器）。

### Phase 7 — Skills 生态接入（复用 Claude Code skills）📋
- **7a 目录对齐 + 扫描配置口**（必做先做）：扫 `.claude/skills` + `~/.claude/skills`（+ 现有 `.phi/skills`），复用 Claude Code skill 生态。框架暴露 `with_skill_dirs()` 配置口（框架中立、consumer 决定）。
- **7b `/skill` 斜杠入口**（必做）：TUI/inline Composer 解析 `/skill-name args`，查 registry（`user-invocable`）+ `resolve_body` 参数化，注入上下文。
- **7c 执行语义**（可选，重，框架层）：`allowed-tools`/`context: fork`/`paths` 从「死字段」变真语义（工具门控 / 子 agent 隔离 / 路径触发）。
- **7d 自带 skills**（可选，内容）：`commit`/`code-review`/`explain` 三个开箱 skill。
- **7e 上下文硬截断**（可选，顺手）：开 `ContextWindowManager`（`has_context_window:false→true`）作 SummarizingMiddleware 的确定性兜底。
- 详见 `docs/phase7-plan.md`。

### Phase 8 — 斜杠命令系统（内置命令 + 分发器）📋
- Composer 识别 `/` 前缀 + 命令注册表 + 分发器（`CommandEffect` 三态：`Local`/`Run`/`NotACommand`），是 skills / memory / `@` 三者共用的地基。
- 首批内置命令：`/help` `/clear` `/status` `/model`（immediate 类）+ `/verify` `/diagnostics`（turn-triggering 类，直接调工具不经 LLM）。
- **明确不做**：自定义命令（并入 Phase 7 skills）、`@` 提及（Phase 10）、Memory（Phase 9）。
- 详见 `docs/phase8-plan.md`。

### Phase 9 — Memory（PHIFORGE.md + CLAUDE.md 兼容）📋
- 启动常驻注入：读自己的 `PHIFORGE.md`（项目根 / 全局 `~/.config/phiforge/`），无则兜底读 `CLAUDE.md`，让已写 CLAUDE.md 的项目开箱即用。命名待定（`PHIFORGE.md` vs `.phi/PHIFORGE.md`）。

### Phase 10 — `@` 文件提及 / 路径选择器 📋
- 输入 `@` 弹出路径选择器（TUI）：输路径（`..` 回退、`/` 下钻、绝对路径）+ 实时列目录，选中后把路径当普通文字插进输入，agent 自己 `read_file`/`repo_map` 去读。不注入内容、不做补全。
- 配套「读写都放开」：`read_file`/`write_file`/`edit_file`/`repo_map`/`search_content`/`list_files` 全部允许工作区外（无沙箱、不加 flag，安全靠审批层）。
- 详见 `docs/phase10-plan.md`。

---

## 13. 决策记录

### 已定

- **语言范围**：**多语言为目标**，首版 Rust 起步。`LanguageAdapter` 边界 + tree-sitter RepoMap（天生多语言；加语言 = 加配置 + 一个 tree-sitter query，约 1～2 天/门）。
- **形态**：仅 REPL + TUI（ratatui），类似 Claude Code，不做单次任务模式。
- **产品定位（2026-08-16 定）**：产品优先，「永远不把编不过的代码交给你」是招牌，框架验证是副产品（§1 已反转）。
- **强制 verify 闸门（6a）**：作为 phiforge 的 `Middleware` 实现（consumer-side），**不把「必须验」硬编码进框架**——框架只提供 `skip_push`/`follow_up_message` 通用能力，编码专属策略留在 consumer。防死锁走 `max_nudges` 降级。
- **Phase 6 顺序**：6a（强制闸门）先于 6b（LSP）。闸门是保证、不依赖 LSP；LSP 是加速。

### 开放

1. **多 agent 时机**：Phase 1 先单 agent 再上多 agent，还是因为「多 agent 是首要验证点」而提前？
2. **验证深度**：写代码的「对错」如何自动判定？方向：L1（编译）+ L2（现有测试）当骨架、L3（生成测试）当闭环压测器而非对错证明、diff 当真值。
3. **exec-policy 层（后置）**：codex 式「子 agent 提议 `prefix_rule` → 传播回父 → 合并进可变 exec-policy」需把 `ToolPolicy` 从无状态 `Arc` 改为可累积规则策略。当前用「委托审批 + 收窄 `action_key`」覆盖了 80% 安全价值，完整层待真需再上。

---

## 14. 进度记录

### Phase 1 完成（2026-08-15）

- `phiforge`（CLI + REPL）基于 `phi-agent` 0.11.0 本地源码构建。
- 可观测性：`session.log`（`log-core` tracing）+ `turn_NNN.jsonl`（`save_turn_log`），落在 `~/.phiforge/sessions/<id>/`。
- smoke test（空工作区 → 脚手架 + 带缓存 `get_user` + `cargo check`）：8 轮 / 9 工具调用 / 0 失败。

**压测发现（喂给后续 phase）**：

1. **上下文架构缺口实证**：agent 用 2 轮 shell 探测（`ls` / `git status` / `ls /target`）才搞清楚「工作区里有什么」——正是 §3 说的「差 coder 靠 shell 当劣质地图」。RepoMap 一次能给出的事实，被拆成多轮往返。→ Phase 2 优先级确认。
2. **失败迭代闭环只测了最弱形态**：唯一「失败」是 `dead_code` 警告，非编译错误。真 `编译报错 → 定位 → 修 → 重跑` 尚未压到。→ Phase 3 需用「故意类型错误」的任务打。
3. **事件流噪声**：单轮 1136 条 `ThoughtDelta` / 292 条 `TextDelta`（推理逐字流出）。终端无感，ratatui TUI 需合并/批量渲染。→ §9.5 压测点确认。

### Phase 2 完成（2026-08-15）

- 新增两个拉取式上下文工具：`search_content`（RipgrepTool，`rg --json`）+ `repo_map`（RepoMapTool，tree-sitter-rust 符号提取），4 条单元测试通过。
- p2 smoke test：agent **先调 `repo_map` 定位再读文件**，不再 shell 瞎探测 —— Phase 2 目标（拉取式上下文）达成。

**框架缺陷发现 + 修复（read_file 输出上限打架）**：

- **现象**：`read_file src/tools/repomap.rs` 被拒 —— "Tool 'read_file' output exceeds the 4000-char limit"。
- **根因（框架三处自相矛盾）**：
  1. `base_agent_builder` 设 `max_tool_output_chars = 4000`（`phi-agent/src/agent/builder.rs`），但 `read_file` 默认 `limit = 2000 行`（`phi-kernel-tools/src/file/read_file.rs`）。带行号格式化后 ~60 行就超 4000 字符，读任何正经源文件都硬失败。
  2. §6.5 写「工具应自己截断再返回」，但 `ToolContext` 无 output 预算字段，工具不知道上限，无法配合 —— 指引对框架自己的工具也无效。
  3. `builder.rs` 注释声称「截断 + TruncationInfo」，但该类型不存在，`pipeline.rs` 实际是 reject —— 契约未统一。
- **修复（改框架，非 phiforge 侧绕过）**：
  1. `ToolContext` 加 `max_output_chars: Option<usize>`，`tool_engine.rs` 注入引擎预算。
  2. `read_file` 按行边界自截断到预算，超界打 `...(truncated, use offset=N to continue)` 续读标记。
  3. 修 `builder.rs` 陈旧注释，统一「reject 作引擎兜底、能自界的工具自截断」契约。
- **验证**：read_file 12/12 通过（含自截断 + offset 续读）；agent-base 350 通过；phiforge 4 通过。
- **phiforge 侧**：`.max_tool_output_chars(16_000)` 从「绕过 bug」变为正当偏好（一次读更大块）。

**框架缺陷二 + 修复（list_files 无忽略逻辑 + 无自截断）**：

- **现象**：p2 smoke test 里 `list_files {"path": ".", "recursive": true}` 报 "output exceeds the 16000-char limit"，列了 9950 个文件——递归进了 `target/`。
- **根因**：`phi-kernel-tools/src/file/list_files.rs` 是裸 `read_dir` 递归，**零忽略逻辑**（无 .gitignore / target / node_modules / 隐藏文件），也无输出自截断（`_ctx` 未用），超限硬拒。
- **设计立场**：框架是通用 agent 框架，**不能**把 `target/`、`node_modules/` 这类代码生态专属目录硬编码进内核。
- **修复（通用化）**：
  1. `ListFilesTool` 加 `with_excludes(Vec<String>)` 构造 + 按名跳过（通用口子，默认空）。
  2. `list_files` 按 `ctx.max_output_chars` 自截断 + `...(truncated)` 标记（与 read_file 同一套）。
  3. `base_agent_builder_with_excludes(llm, excludes)` 变体；`base_agent_builder` 委托为默认空。
- **phiforge 侧**：`build()` 传 `["target", "node_modules"]`——代码专属名单只出现在 consumer。
- **验证**：list_files 13/13 通过（含 excludes + 自截断）；phiforge 4 通过；phi-agent builder 3 通过。

**框架缺陷二·补强（list_files 递归改走 gitignore-aware + limit 上限）**：

- **动机**：缺陷二的修复只做了「consumer 注入 excludes 名单」+「自截断」两层。但框架仍是裸 `read_dir` 递归，自己不懂 `.gitignore`——consumer 忘了传名单（或列的目录不是 git 仓库、却有自己的 `.gitignore`）时，递归照样进 `target/`/`node_modules/` 这类生成目录炸掉；且递归无上限，超大目录树一次吃满内存/输出预算。
- **修复（框架通用化，不掺代码专属逻辑）**：
  1. **P0 — gitignore-aware**：`collect_entries_recursive` 从裸 `read_dir` 递归改为 `ignore::WalkBuilder`（ripgrep 同款 ignore 逻辑）。`hidden(false)` 含点文件、`git_ignore/ignore/parents(true)` 吃 `.gitignore`/`.ignore`+父级、`require_git(false)` 非 git 仓库也生效、`follow_links(false)` 防环、`max_depth(64)`。`.git/.hg/.svn` 与 consumer `excludes` 经 `filter_entry` 在**下降前剪枝**（不再走进 target/ 再逐个跳过）。`git_global/git_exclude(false)` 跳过用户全局 ignore，保证行为确定。
  2. **P1 — limit 上限**：schema 加 `limit`（默认 500），收集阶段早停；触发时末尾追加 `[N entries limit reached. use limit=2N for more, or narrow path/pattern]`（范围提示，非翻页——文件无自然顺序，故不用 read_file 的 offset 式续读）。
- **验证**：list_files 15/15 通过（含 `test_list_files_recursive_respects_gitignore` + `test_list_files_limit`）；全 crate 133/133；phiforge 编译通过。

**P2 — 截断元数据 / 字节上限（评审通过，后置）**：

- **P2-a · `TruncationResult`**（`totalLines`/`totalBytes`/`truncatedBy`/`firstLineExceedsLimit`…）：即 §6.5 说「框架缺的那个类型」，demo/pi 的 `truncate.ts` 已有。收益在**渲染层**（TUI 显示「截断了 N 行」），对 agent 无增量——agent 只看文本，现有文本标记（`...(truncated, use offset=N)`、`[N entries limit reached]`）已把信息给足。→ **触发条件**：建 TUI（Phase 5）时做，走 `ToolContext::emit_user_event(UserEvent::Structured{..})` 侧信道（无需动 `Tool` trait 返回结构）。
- **P2-b · `max_output_chars` 字符 → 字节**：字节更贴近 token，但字符/字节都非真 token 数、都只是粗略兜底；单独换单位要横跨 agent-base + 3 工具 + phiforge + 测试重命名，churn 大收益小。→ **触发条件**：引入真 token 计数的整体改造时再议，不单独做。

**开发态依赖（path 依赖）**：

- 弃用 phiforge 的 `[patch.crates-io]`，改为给 sibling crates（agent-works / phi-tools / phi-telemetry / phi-kernel-tools / phi-agent）的依赖加 `path = "../xxx"`（保留 `version` 以便将来 crates.io 发布）。
- 效果：任一 crate 目录里 `cargo test` 直接吃本地源码，无需临时 patch；发布时 cargo 自动用 `version` 字段。

**可观测性修复（turn log 跨进程混跑）**：

- **现象**：复用 `--session p2` 跑两次，两次的 `turn_001.jsonl` 追加进同一个文件，无法一眼区分哪次是旧二进制。
- **根因**：`turn_number` 每个进程都从 0 起算（phiforge `run_repl` 与 phi CLI `run.rs` 都是 `let mut turn_number = 0`），而 `save_turn_log` 用 `.append(true)` 写 `turn_NNN.jsonl`——同 session 复用时 turn 1 撞车。
- **修复**：`SessionContext` 加 `last_turn_number()`（扫描现有 `turn_NNN.jsonl` 取最大 N）；两处 consumer 用它初始化计数器，复用 session 时续号而非归零；phi CLI 的 `reset` 同理续号。
- **验证**：session 21/21 通过（含新 `test_last_turn_number_scans_existing_turns`）；phi CLI + phiforge 均编译通过。

**框架缺陷三 + 修复（execute_command 输出不自截断）**：

- **现象**：p3 smoke test 里 `list_files` 修复已生效（12 文件、`target/` 排除、无报错），但 agent 紧接着用 shell 跑 `find . -not -path './.git/*' | sort`，`find` 不认 .gitignore 递归进 `target/`，产出 1,071,869 字符，被引擎按 16000 硬拒 —— "Tool 'execute_command' output exceeds the 16000-char limit"。
- **根因**：`phi-kernel-tools/src/local_shell.rs` 的 `call` 用 `_ctx`（预算字段没读），`format_result` 拼完 stdout+stderr 直接返回，无自截断——与 read_file / list_files 同一类缺陷（能产出无界文本 + 不自截断），只是换到了 shell 工具。
- **修复（通用化，不掺代码专属逻辑）**：`call` 读 `ctx.max_output_chars`，对 `format_result` 的最终字符串做 `truncate_output` 自截断——**head + tail 双端保留**（头部 1/3、尾部 2/3，中间打 `...[output truncated]` 标记）。理由：shell 输出不像文件/目录那样「从头读就够」，`cargo build/test` 的报错在**尾部**、`ls/find` 的开头也有意义，双端保留对命令输出最通用。
- **验证**：local_shell 18/18 通过（含新增 `test_truncate_output_keeps_head_and_tail` + `test_call_self_truncates_large_output`）；phiforge 编译通过。
- **备注**：agent 用裸 `find` 探测结构本身是「拿 shell 当劣质地图」——正是 §3/Phase 2 要解决的上下文架构缺口，`repo_map`/`search_content` 落地后应减少这类调用；框架侧只保证「超限不自截断」这层兜底不再炸。

### Phase 3 完成（2026-08-15）

- **verify 工具**（`tools/verify.rs`）：跑 `sh -c <command>`（默认 `cargo check`），把 rustc 报错解析成紧凑摘要 `N errors:` + `file:line:col  code  message`（成功则单行 `✓ passed`；非 rustc 输出回退为 stderr 尾部 20 行）。注册进 agent + SYSTEM_PROMPT 第 3 步改成「用 verify 拿精简错误摘要」。5 条单测（解析、摘要、非 cargo 兜底、schema）。
- **审批两层**（`approval.rs`）：`ApprovalPolicy`（`ToolPolicy` 闸门——读工具/`verify` 放行、`write_file`/`edit_file` 弹窗、`execute_command` 按 `classify_command` 分级）+ `CliApprovalHandler`（终端 y/a/n，`read_stdin_line_cancellable` 竞速 `cancel_token`）。`build_approval(mode)` 返回 `(handler, Option<policy>)`；`--approval` 现为 `auto/ask/deny` 三态。
- **框架 re-export 缺口修复**：`phi-agent/src/lib.rs` 补 `ApprovalDecision`/`RiskLevel`（consumer 实现 handler/policy 时拿不到这两个类型就没法写）。

**框架缺陷发现（审批 `deny` 是空操作）**：

- **现象**：phiforge 此前只设了 `ApprovalHandler`（Auto/DenyAll），没设 `ToolPolicy`。而 agent-base `process_approval` 里「无 policy → 直接 `return Ok(())`」根本不进 handler → **`--approval deny` 实际什么都没拒**。
- **根因**：审批是两层——`ToolPolicy`（闸门：决定「要不要审」+ 赋 `risk_level`/`action_key`，返回 `None` = 放行）+ `ApprovalHandler`（决策：AllowOnce/AllowAlways/Deny）。只设 handler 不设 policy = 闸门永远放行。
- **修复（phiforge 侧补 policy，不动框架默认）**：`ApprovalPolicy` 给写操作/危险命令返回 `Some(request)` 才触发 handler；`deny`/`ask` 都带上它。读工具/安全命令（`cargo check`/`git status`/`ls`…）仍放行，维持「只读模式」语义。
- **验证**：phiforge 19/19 通过（policy 分级 + `build_approval` 三态 + `classify_command` + `map_input` + verify 解析器）；`cargo build` 通过。

**live smoke（真机）**：

- 审批三态 + verify 各自跑通：`deny` 写被拒、`auto` 写成功、`ask` 弹窗 y/n 双向、`verify` 返回精简报错摘要。
- 三次编码任务递增（单行 bug → 泛型化 → 跨文件签名变更）全绿；第三次完整走通「编译错 → 读报错 → 修 → 重跑」迭代闭环（`tests/integration.rs` 隐藏调用方被 `verify` 的 `E0061` 摘要兜住，回头读文件再修）。

### Phase 4 完成（2026-08-15）

- **共享基础设施**（`tools/workspace.rs`）：`WorkspaceTracker`（`Arc<Mutex<TrackerState>>`）+ `snapshot`/`diff`/`normalize_path` 纯函数。`decompose` 时记内容 hash 快照（跳过 `target`/`node_modules`/`.git`/`.hg`/`.svn` 及隐藏项），`merge` 时重扫 diff 出 `Added`/`Modified`/`Removed`——不依赖 git，覆盖裸目录工作区（§10）。
- **`decompose` 工具**（`tools/decompose.rs`）：持 `Arc<dyn StreamClient>`，`call()` 内做一次**嵌套 LLM 调用**（`response_format=JsonObject`），产出结构化 `Decomposition { strategy: Serial|Parallel, slices: [{name, files, context, task}] }`。`strategy` 落地 §7.4「分解器判断该不该并行」；`slice.files` 是 merge 做冲突归属的结构化边界。复用 `repomap::build_repo_map` 喂结构。
- **`merge` 工具**（`tools/merge.rs`）：从共享 `WorkspaceTracker` 读上次 decompose 的快照 + slices（LLM 不回传列表，接口最简），diff 出变更、算冲突（① 一文件被 ≥2 slice 声明 = 重叠；② 改动落在任何 slice 边界外 = 越界编辑），再复用 `verify::run_and_summarize` 跑 `cargo check` 折叠错误摘要。无快照时提示「先 decompose」。
- **接线**：`Cargo.toml` 开 `multi-agent` feature（拉起 6 个框架工具 `spawn_agent`/`send_message`/`followup_task`/`wait_agent`/`list_agents`/`close_agent`）；`agent.rs` 注册两工具 + SYSTEM_PROMPT 补「大任务 decompose → parallel 则每 slice 一个 `spawn_agent` + `wait_agent` → merge → 修再 verify」；`approval.rs` 放行名单补 `decompose`/`merge`；子 agent 权限随审批模式走（auto=`Full`，ask/deny=`None`，见下）。
- **验证**：phiforge 40/40 单测通过（workspace diff 三态 / decompose JSON 解析+围栏剥离+未知策略回退 serial / merge 冲突四类 / normalize_path 折叠 `./`+反斜杠）；`cargo build` 通过——`multi-agent` feature 使 `base_agent_builder_with_excludes` 的 `with_multi_agent` 块（`phi-agent/src/agent/builder.rs:103-111`）生效，6 工具注册。

**压测发现（框架观察，非修复）**：

1. **子 agent 继承编排工具**：`register_tool` 一律进 `business_tools`，子 agent 按 Arc clone 继承——`decompose`/`merge` 也随之进了子 agent 的工具面。`PhiAgent` 不暴露 post-build `tools_mut`，无法注册「父专用」工具。实践上子 agent 任务是「实现切片」不会调编排工具；即便误调，父 agent 重跑 `decompose` 会重录快照。→ 列为已知 caveat，未为此绕过 `PhiAgent`。

**框架缺陷四 + 修复（受限子 agent 本地硬拒，而非上抛父）**：

- **现象**：`--approval deny` 下，`spawn_agent(full_permission=true)` 的子 agent 仍能写文件——「deny 只读」语义在多 agent 场景不闭合。
- **根因（两层）**：① phiforge 用 `MultiAgentConfig::default()`（`ChildPermissionMode::Full`，`effective_permission` 无条件放行），LLM 传的 `full_permission` flag 被覆盖；② 即便切到 `ChildPermissionMode::None`，框架的 `build_child_runtime` 受限分支给子 agent 挂的是 `DenyAllApprovalHandler`——**本地硬拒**，审批请求既上抛不到父、也到不了人，`ask` 模式下子 agent 写文件无法交互审批。
- **参考 codex 的解法**：`codex-rs/core/src/codex_delegate.rs:443-523` 的 `handle_exec_approval` 把子 agent 的 shell/patch 审批请求转成 `parent_session.request_command_approval(...)`（来源标 `GuardianApprovalRequestSource::DelegatedSubagent`），**上抛给父 session 统一裁决**；子 agent 从不「本地自动通过」或「本地硬拒」，另有逐 agent 的 `permissions.approval_policy` + `prefix_rule` 修订传播回父（`approvals.rs:2081`）。
- **修复（改框架，学 codex）**：`MultiAgentRuntime` 增 `approval_handler` 字段，`build_child_runtime` 受限分支把子 agent 的 `ApprovalHandler` 从硬编码 `DenyAllApprovalHandler` 改为**委托父的 handler**（父无 handler 时回退 `DenyAll` 保住「无策略→只读」不变量）；`setup_multi_agent` 传入 `runtime.approval_handler()`。
- **phiforge 侧**：`agent.rs::build` 按审批模式设 `child_permission_mode`——`auto`（无 policy）=`Full`，`ask`/`deny`（有 policy）=`None`；SYSTEM_PROMPT 不再硬编码 `full_permission=true`，改提示「子 agent 写权限随审批模式，被拒则该切片自己写」。另把 `approval.rs` 的 `action_key` 从工具名收窄为「`write_file:<path>` / `execute_command:<命令>`」，使 `AllowAlways` 只放行具体对象而非整个工具（codex `prefix_rule` 的收窄语义，复用 `tool_engine` 的按 key 精确缓存，无需改框架）。
- **验证**：agent-works 97/97（新增 `build_child_runtime_none_delegates_to_parent_approval_handler` + `build_child_runtime_none_denies_when_parent_has_no_handler`）；phi-agent builder 5/5；phiforge 40/40。
- **仍存的窄限制（记录，不阻塞）**：`ask` 模式下子 agent 的写审批走父的 `CliApprovalHandler`（读 stdin），多个并行子 agent 同时弹窗会**交错**——REPL 里可逐个回答，但交互体验不如 codex 的父 session 统一审批队列。属 TUI（Phase 5）要处理的交互问题，非正确性缺口。

**已真机验证（2026-08-15，deepseek-v4-pro）**：跑了一次「三模块 + 单测」的较大任务，完整走通 `decompose`（判 `parallel`，4 切片文件边界清晰，主动把共享 `lib.rs` 编辑单列成独立切片）→ `spawn_agent ×4`（`full_permission=true`，各写各文件）→ `wait_agent ×4` 全 `ok`（0 denied）→ `merge`（报 changed files + 越界检测 + `✓ passed`），最终 `cargo test` 24 单测绿、`session.log` 0 ERROR/WARN。§7 四个难问题（分解质量/上下文传递/冲突处理/合并验证）真机跑通。

- **发现并修复**：子 agent 跑 `cargo check` 生成的 `Cargo.lock` 被 `merge` 误报为 out-of-scope 冲突——snapshot 排除列表只排目录（`target` 等）没排文件。补 `EXCLUDED_FILES = ["Cargo.lock"]`（`workspace.rs`，附 `snapshot_skips_cargo_lock` 单测），现单测 43/43。
- **另记两条观察**：① `decompose` 对小任务（两模块）欠触发——模型理性判断「直接写更快」就跳过编排，说明 SYSTEM_PROMPT 的「大任务先 decompose」是建议非强制；② `auto` 模式下子 agent 走 `Full`，codex 式审批上抛（`ChildPermissionMode::None`）只在 `ask`/`deny` 触发——后已在 ask 模式下真机验（见 Phase 5 记录）。

### Phase 5 完成（2026-08-15）

- **TUI（ratatui + crossterm）**：`src/ui/`（`input.rs` Composer 多行输入 / `app.rs` 状态机 / `render.rs` 画帧 / `mod.rs` run_tui 编排）。默认 UI，固定底部输入栏（Claude Code 风格 `> ` 前缀 + 圆角框，光标留框内、内容在上方滚动），`--inline` 降级 opt-in（`--no-tui`/rustyline REPL 已删）。
- **审批内联（5b）**：`QueuedApprovalHandler`（mpsc 队列 + 每请求 oneshot），一次渲染一个 y/a/n 弹窗，解决并行子 agent 审批交错。
- **inline 变体（5c，`src/inline.rs`）**：不进 alternate screen、`TextDelta` 直写 stdout 无缓冲（解决长答案「卡死」）、reasoning 折叠进 spinner 状态行（`\r\x1b[K` 原位覆写）、内联 y/a/n。
- **帧日志（观测 UI）**：`run_tui` 把重绘脱机渲染成文本（`render::snapshot_text` 去色留布局），去重写 `<session>/frames.txt`（cap 2000 帧，`dirty` 门 + 100ms 节流）；inline 用 `Tee` 镜像字节流到 `inline.raw`，`src/bin/replay.rs` 迷你 ANSI 终端离线回放。用途＝让 Claude 能看到用户真实跑的 UI、离线评估。
- **观感修复**：CJK 双宽（`unicode-width`：`wrap` 按显示宽度、`buffer_to_text` 跳宽字符续列、光标列按 `display_width`）；工具内联进 transcript（`LineKind::Tool`/`ToolResult`，删独立工具 pane）。
- **修两个 TUI 老 bug**：① 流式「卡死」——`pending_text` 只在结构事件 flush，加 `App::streaming_tail()` 尾部实时渲染；② 滚动坏——`scroll_y` 首可见行语义 → `scroll_offset` 离底行数 + `EnableMouseCapture` 处理滚轮。
- **inline 两 bug**：① 右漂移（raw mode 裸 `\n` 不回车 → 全改 `\r\n`）；② 冻结 spinner（`stream`/`line` 遇 `LiveStatus` 用 `\r\x1b[K` 原位擦除而非提交）。
- 单测 97 全绿。

**压测/测试发现**：

1. **TUI 真机测试难**：ratatui 在 PTY 里默认 winsize 0x0 → 渲染空屏（frames.txt 空，易误判为 bug）。用 Python `pty.fork()` + `fcntl.ioctl(TIOCSWINSZ)` 设 winsize 驱动真实 TUI；完成信号＝轮询 `turn_NNN.jsonl` 出现 `run_finished`，审批弹窗＝轮询 PTY 字节流出现 `Approval required`。
2. **顶层审批 + 子 agent 审批上抛真机验**：`--approval ask` 下 `write_file`→Sensitive 弹窗，`y`=放行（文件建）/`n`=拦截（文件未建）；`--approval deny` 无弹窗自动拒（`denied:true`）。多 agent：decompose 拆 3 文件→parallel→spawn 3 子 agent→各自 `write_file` 一路上抛到父 TUI 弹窗（子 agent 工具调用带 `[root/create-doc-X]` 前缀渲染在父 transcript）→逐条 `y`→3 文件全建、merge 跑完。小瑕疵：`decompose.rs` 的 `format_plan` 让 spawn 传 `full_permission=true`，被 `ChildPermissionMode::None` 无视（无害但文案误导）。

### Phase 6a 完成（2026-08-16）

- **强制 verify 闸门**：`src/gate.rs` 的 `VerifyEnforcementMiddleware`（consumer-side `Middleware`，零改框架，见 §8.3）。dirty 跟踪＝`write_file`/`edit_file` 落在 `.rs`/`Cargo.toml`/`Cargo.lock` 置位，`verify`/`merge` 清位，文档写不置位；`on_post_llm` 纯文本 + dirty → `skip_push` + `follow_up_message` 逼循环再来一轮，`max_nudges`(3) 后降级追加「⚠️ Unverified」；`on_user_message` 每轮重置；`deny` 模式（`writes_possible=false`）整体关闭不误伤只读 agent。
- 接线：`agent.rs::build` 加 `.middleware(...)`；`main.rs` 传 `cli.approval != "deny"`。
- 单测 114 全绿（+17 gate：veto / verify+merge 清位 / 文档写不置位 / 降级 / 每轮重置 / deny 关闭）。
- **真机 smoke**（PTY + 临时 crate）：`middleware_count` 1→2；`write_file extra.rs`→「done」被拦（`verify gate: suppressing unverified done`，nudge 1）→`verify cargo check` ✓→done。一次 nudge 自纠，无死锁、无误报。

**已知边界**：① `execute_command cargo check` 不清 dirty（只认 `verify`/`merge`）——agent 用 shell 验证会多 nudge 一次，无害；② 路径过滤 Rust-first（`.rs`+Cargo 文件），多语言时需扩展。

### Phase 6b 完成（2026-08-16）

- **LSP 诊断**：`src/lsp.rs` 手写最小 LSP 客户端（只 diagnostics，不做 completion/goto）+ `src/tools/diagnostics.rs` pull 工具。依赖 `lsp-types` 仅作协议类型基础；JSON-RPC 帧（Content-Length）、`initialize`/`initialized` 握手、`didOpen`/`didChange`/`didSave`、`publishDiagnostics` 缓存全手写，零重依赖（§8.4）。
- 架构：reader/driver 两个后台线程读 stdout / 写 stdin，诊断缓存进 `Arc<Mutex<HashMap<path, Vec<DiagnosticEntry>>>>`；启动/握手失败记 `state.error`，工具层经 `health()` 感知并降级到 `verify`。`didSave` 触发 checkOnSave → cargo check → publish。
- 接线：`agent.rs` 注册 `diagnostics` 工具 + 系统提示加说明；`main.rs` 加 `mod lsp`。进程级单例 rust-analyzer，`Drop` 杀进程收尾。
- 单测 133 全绿（+18：lsp 帧编解码 / 握手消息构造 / publishDiagnostics 解析 / 诊断扁平化，diagnostics 摘要格式 / 文件收集）。
- **真机 smoke**（PTY + 临时 crate）：改坏 `src/main.rs`（`let x: u32 = "..."`）→ agent 调 `diagnostics` → 秒级返回 `2 error(s):\n  src/main.rs:2:22  E0308  mismatched types …`。`tool_count` 10→11。

**已知边界**：① rust-analyzer 对同一处类型错误可能发多条 error 级诊断（如「expected u32, found &str」+「mismatched types」），`diagnostics` 忠实上报、不 dedup（`verify`/`cargo check` 只报 1 条）；② 只 Rust（rust-analyzer）；③ hint/info 级诊断被 `format_diagnostics` 舍弃（对「编不过」无意义）；④ 依赖 rust-analyzer 在 PATH，缺失时工具优雅报错并建议 `verify`。
