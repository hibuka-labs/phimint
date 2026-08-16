# Phase 8 实施计划（斜杠命令系统：内置命令 + 分发器）

把 phiforge 从「`/xxx` 当普通文本提交」接到「打 `/` 触发动作」，对标 Claude Code 的斜杠命令，并顺手把分发器造成 skills / memory / `@` 三者共用的地基。

## 背景（现状盘点，2026-08-16 查证）

- **输入链路**：`src/ui/input.rs` 的 `Composer` 是纯多行文本缓冲，无任何 `/` 感知；`run_tui`/`run_inline` 收到 Enter 就 `Cmd::Run(text)` 把整段文本发给 agent。`/xxx` 会被原样当一句普通话喂给 LLM。
- **工具已存在但无手动入口**：`verify`（编译/类型检查）、`diagnostics`（LSP 诊断）是 agent 工具，只能由 LLM 决定调用；用户想「现在就看一眼编译状态」没有直接口子。
- **会话控制缺失**：`--model` / `LLM_MODEL` 只在启动时定死；TUI 里没有「清历史 / 看状态 / 切模型 / 看帮助」这些会话控制。
- **phase7 已规划 `/skill` 入口（7b）**，但它的斜杠分发地基尚不存在——本 phase 先造地基，7b 复用。

## 范围（做 / 不做）

**做**：
- 一个斜杠**分发器**（Composer 识别 `/` 前缀 + 命令注册表 + 分发语义）。
- 首批**内置命令**：`/help` `/clear` `/status` `/model`。
- 把现有工具暴露成命令：`/verify` `/diagnostics`。
- 分发器预留「注入模板」的口子，供 skills / memory / `@` 复用。

**不做（明确边界）**：
- **自定义命令（markdown 模板）**：并入 phase7 的 skills，不单独设计。skills 的 `user-invocable` 字段天生就是「自定义命令」机制，现在建两套是重复劳动、违反高内聚低耦合。
- **`@` 上下文提及**：独立 Phase 10（语义是「往上下文塞内容」，非「触发动作」，UX 重）。
- **Memory（PHIFORGE.md + CLAUDE.md 兼容）**：独立 Phase 9（启动常驻注入，非按需触发）。
- 不做 `/cost`（token 用量并进 `/status`，YAGNI）、`/compact`（`SummarizingMiddleware` 已 30k 自动压缩，手动压缩低价值）、`/mcp` `/login` `/init` `/add-dir`（不适用）。

## 全局路线图（输入/上下文面）

```
Phase 8  斜杠命令（内置 + 分发器）   ← 本 phase，先做，造分发器地基
Phase 9  Memory（PHIFORGE.md + CLAUDE.md 兼容）   ← 小、独立、紧随
Phase 10 @ 上下文提及               ← 中成本、UX 重、独立
Phase 7  Skills 生态（已有 plan）   ← 重、最后；7b 复用 Phase 8 的分发器
```

**共享地基**：一个「把指定内容按结构化方式注入本轮上下文」的注入层。Phase 8 先立起最小版本（分发器产出 `CommandEffect::Run { prompt }`），Phase 9 / 10 / 7 都来复用，避免三处各写一套注入逻辑。

**执行顺序注记**：Phase 7 的 `7b /skill 入口` 依赖本 phase 的分发器，因此 7b 实际应在 Phase 8 之后执行（7a 目录对齐、7c/7d/7e 与 8 无依赖，可并行）。

## 架构设计

### 分发器（核心，`src/ui/command.rs` 新增）

输入缓冲首个非空字符为 `/` 时，解析为命令；否则走原「普通文本」路径。分发器输出三态，这是 skills / `@` / 未来自定义命令的接缝：

```rust
enum CommandEffect {
    /// 本地处理，不触发 turn（/help /clear /status /model）
    Local(Command),
    /// 把（可能重写的）prompt 作为一次普通 turn 提交（未来 skills 注入 body、@ 展开）
    Run { prompt: String },
    /// 不是命令 —— 整段当普通文本转发给 agent（/未知 也走这里，保持宽松）
    NotACommand,
}
```

- `parse(text: &str) -> CommandEffect`：取 `/` 后第一个 token 匹配注册表；命中 → 对应 effect；未命中 → `NotACommand`（不打断现有 `/xxx` 文本的宽松性，与 phase7 决策一致）。
- **注册表**：`BuiltinCommand` 枚举 + `name/description/args` 元数据。`/help` 遍历注册表渲染清单；未来 `@` 自动补全也查它。
- **两类命令**：
  1. **immediate（本地）**：`/help` `/clear` `/status` `/model` —— 不 spawn turn，直接在 UI 层执行并往 transcript 写一条 `LineKind::System`。
  2. **turn-triggering（跑工具）**：`/verify` `/diagnostics` —— 直接调工具函数、把结果摘要写进 transcript，不经过 LLM（确定性、零 token）。

### 接线点

- `Composer` 保持纯文本（不动它），解析放在**提交点**：`run_tui`/`run_inline` 收到 `Cmd::Run(text)` 前先过 `command::parse`。
- `CommandEffect::Local` 通过现有 `App::push_system` / `App` 事件通道回显；`Run { prompt }` 走现有 `Cmd::Run`；`NotACommand` 走 `Cmd::Run` 原文。
- `/clear` 需重置 agent 对话历史（调用 agent 运行时的 reset 接口；若无，标记为框架依赖，见「待定决策」）。

## 命令清单（首批）

| 命令 | 作用 | 类型 | 备注 |
|---|---|---|---|
| `/help` | 列出所有命令 + 快捷键 | immediate | 遍历注册表 |
| `/clear` | 清空对话历史（重置上下文） | immediate | 依赖 agent reset 接口 |
| `/status` | 会话状态：模型 / 工作区 / session 目录 / 运行时长 / token 用量 | immediate | token 用量并进来，不单做 `/cost` |
| `/model <name>` | 切换模型，下一轮生效 | immediate | 依赖 phi-agent 支持中途换模型（见决策 1） |
| `/verify` | 手动触发 verify（编译/类型检查），展示摘要 | turn-triggering | 复用 `tools/verify.rs` 核心，不经 LLM |
| `/diagnostics` | 手动触发 LSP 诊断，展示摘要 | turn-triggering | 复用 `tools/diagnostics.rs` 核心，不经 LLM |

**不做**：`/decompose` `/merge`（niche，后置，真需再加，注册表加一项即可）。

## 分阶段任务

### 8a 分发器 + 注册表（必做先做，低成本）

1. 新增 `src/ui/command.rs`：`BuiltinCommand` 枚举 + 注册表（`name`/`description`）+ `parse` + `CommandEffect`。
2. 接线：`run_tui`/`run_inline` 提交点先过 `parse`，三态分流。
3. 验证：输入 `/help` 出现命令清单；`/不存在的` 当普通文本转发。

### 8b 内置命令（immediate 类）

1. `/help` —— 遍历注册表 + 快捷键，写 `LineKind::System`。
2. `/status` —— 读当前 agent 配置（模型/工作区/session 目录/运行时长/token 计数），写系统行。
3. `/clear` —— 调 agent reset 接口清历史（框架无接口则按决策 1 处理）。
4. `/model <name>` —— 设「下一轮生效」的模型覆盖（见决策 1）。

### 8c 工具命令（turn-triggering 类）

1. `/verify` —— 直接调 `verify.rs` 的 verify 核心（按工作区语言选命令、解析报错），摘要写 transcript。
2. `/diagnostics` —— 直接调 `diagnostics.rs` 的 pull 核心（按语言分组 sync + 合并快照），摘要写 transcript。

## 待定决策

### 已查证（8/16，零框架改动）

1. **`/model` —— 框架已支持，零改动**：`OpenAiClient::with_model(model)`（`openai.rs:74`）克隆出同 key/base_url 换 model 的新 client；`AgentRuntime::set_client(&mut self, Arc<dyn StreamClient>)`（`runtime/mod.rs:99`）换掉底层 `LlmEngine.client`（`RwLock<Arc<dyn StreamClient>>`，内部可变、全局可见）。`AgentRuntime` 是 `#[derive(Clone)]`（`Arc<RuntimeCore>`），故 `let mut rt = agent.runtime().clone(); rt.set_client(...)` 即切共享状态。**phiforge 侧唯一小事**：`main.rs` 需保留 `Arc<OpenAiClient>` 引用以便 `with_model` 重建。语义定为「下一轮生效」。
2. **`/clear` —— 框架已支持，零改动**：`AgentRuntime::with_session_mut(&self, &session, f)`（`mod.rs:61`，`&self` 即可）+ `AgentSession::chat_messages_mut()`（`session.rs:80`，公开）。实现为 `chat_messages_mut().retain(仅 System)`：清 User/Assistant/Tool、保留 System，`turn_count()` 按 User 派生自动归零，session 目录与 turn 日志续号、approval 缓存保留。

### 仍待定（实施时定）

3. **`/verify` `/diagnostics` 是否直接跑工具 vs 注入指令走 LLM**：倾向「直接跑工具」（确定性、零 token）；若工具核心与 agent 运行态耦合过紧（如依赖脏位状态），退化为注入「请运行 verify 工具」走普通 turn。
4. **注入口子形态**：`CommandEffect::Run { prompt }` 是否够 phase7 skills 用（`resolve_body` 产出 body 后作 prompt）？若 skills 需要「system 注入」而非「user 消息」，口子需扩一态（phase7 决策 2 已倾向 user 消息）。

## 依赖 / 风险

- **框架暴露口**：✅ 已查证（见「待定决策·已查证」）——`/model` 用 `set_client`+`with_model`、`/clear` 用 `with_session_mut`+`chat_messages_mut`，均零框架改动。唯一 phiforge 侧改动是 `main.rs` 保留 `Arc<OpenAiClient>` 引用供 `/model` 重建。
- **token 计数来源**：`/status` 要展示 token 用量，需确认 `turn_NNN.jsonl` / 事件流里是否已带 usage 字段（memory 提到有 usage 观测，但需查是否暴露给 TUI 侧）；没有就只在 `/status` 展示「无 token 数据」或从 jsonl 累加。
- **工具命令复用边界**：`/verify` `/diagnostics` 直接调工具核心，需确认核心函数与 agent 运行态（脏位、LSP 单例）解耦到可直接调用；若耦合，抽一个无状态入口（符合高内聚低耦合）。

## 验证

- 8a：`/help` 出清单；`/不存在的` 当普通文本（不报错、不吞字）。
- 8b：`/clear` 后 agent 上下文清空（新问题不带旧上下文）；`/status` 显示正确模型/目录/时长；`/model gpt-4o` 后下一 turn 用新模型（`turn_NNN.jsonl` 或 session.log 可见）。
- 8c：`/verify` 在干净工作区秒级返回「通过」，在改坏代码后返回 `file:line:col` 报错；`/diagnostics` 秒级返回 LSP 诊断（缺失 server 时优雅降级）。
- 回归：普通多行输入、粘贴、中文、审批弹窗不受影响（单测全绿 + 真机 TUI smoke）。
