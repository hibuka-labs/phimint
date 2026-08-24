# phimint Session 性能分析报告：20260824_51046ddb

> **本文档用途**：一次 agent 端到端执行的日志分析结果。文档自包含 —— 包含完整背景、证据、根因与修复建议，可直接交给其他 AI/工程师独立评审，无需追问上下文。
>
> **分析日期**：2026-08-24
> **分析对象**：`~/.phimint/sessions/20260824_51046ddb/`
> **测试时机**：本 session 在 recovery-improvement 修复**之前**运行（session开始于 06:28，recovery-improvement commit 于 10:40）

---

## 修复状态跟踪

| 问题 | 优先级 | 状态 | 修复时间 | 备注 |
|------|--------|------|----------|------|
| [P0-2](#p0-2) stop_reason 解析 | 🔴 P0 | ✅ 已修复 | 2026-08-24 15:30 | 激活截断守卫链，454 测试通过 |
| [P0-1](#p0-1) 空工具参数 | 🔴 P0 | ⏳ 待修复 | - | P0-2 已解决根本原因，可选兜底 |
| [P0-1b](#p0-1b) 错误信息误导 | 🔴 P0 | ✅ 已修复 | 2026-08-24 15:30 | 随 P0-2 一起解决 |
| recovery-improvement | 🟠 P1 | ✅ 已修复 | 2026-08-24 10:40 | RetryWithHistory 机制 |
| [P1-3](#p1-3) perf.log | 🟡 P2 | ⏳ 待修复 | - | 简单优化 |
| 其他问题 | 🟡 P2 | ⏳ 待修复 | - | 逐步推进 |

---

## 目录

- [第一部分：上下文](#第一部分上下文)
- [第二部分：本次会话发生了什么](#第二部分本次会话发生了什么)
- [第三部分：性能基线](#第三部分性能基线)
- [第四部分：问题清单](#第四部分问题清单)
- [第五部分：修复建议](#第五部分修复建议)
- [第六部分：给评审者的开放问题](#第六部分给评审者的开放问题)
- [附录 A：分析所用命令](#附录-a分析所用命令)

---

# 第一部分：上下文

## 1.1 项目是什么

**phimint** 是一款终端 AI 编码 agent（Rust + ratatui TUI），产品定位是「永远不把编不过的代码交给你」—— 改完先跑验证，验过才报完成。它同时是 `phi-agent` 框架的真实压测场。

关键特性（与本次分析相关的部分）：

- **流式 TUI**：ratatui + crossterm，固定底部输入栏，逐字流式输出、工具调用内联展示。
- **「先验再交」闸门**：动过代码文件但没跑 `verify` 就拦下「报 done」（`VerifyEnforcementMiddleware`）。
- **拉取式上下文架构**：`repo_map`（tree-sitter 符号索引）+ `search_content`（ripgrep），agent 主动搜主动读，不把整仓塞进 context。
- **ReAct 循环**：每个用户输入触发一次 run，run 内部循环若干 "turn"，每 turn = 1 次 LLM 调用 + 0..n 次工具调用，直到 guard 判定完成。

## 1.2 代码结构与依赖

工作区根目录：`~/source/buka/buka-works/`（多 crate 平铺，非 cargo workspace，靠 path 依赖互联）

```
phimint/          0.1.0   ← 本项目（TUI + 工具 + 审批 + LSP）
├── phi-agent             agent 框架上层（features: shell, multi-agent）
├── phi-kernel-tools      内核工具集（read_file/edit_file/local_shell 等）
├── agent-base    0.3.1   ← ReAct 运行时 + LLM provider 适配层（本次缺陷所在）
├── agent-works           中间件（compression / guard / skill）
└── log-core              日志
```

phimint 自身源码布局（本次任务改动的就是 `src/ui/`）：

```
src/
├── main.rs, agent.rs, approval.rs, banner.rs, gate.rs, lang.rs, lsp.rs, skills.rs
├── tools/
└── ui/
    ├── mod.rs, app.rs, render.rs, input.rs, mention.rs
    └── app_mention.rs, app_slash.rs, render_markdown.rs, render_popup.rs  ← 本次新建
```

本次分析涉及的 `agent-base` 关键文件：

| 文件 | 职责 |
|---|---|
| `src/llm/anthropic.rs` | Anthropic 协议适配：构造请求体、解析 SSE 流 |
| `src/engine/runtime/llm_engine.rs` | 消费 `StreamChunk`，累加文本/思考/工具参数 |
| `src/engine/runtime/react_loop/turn.rs` | 单 turn 编排、finish_reason 归一化与分支 |
| `src/engine/runtime/react_loop/tools.rs` | 工具轮执行 + **截断守卫** |
| `src/types/finish_reason.rs` | provider stop_reason → 语义枚举 |
| `src/engine/auto_continue.rs` | 纯文本被截断时注入续写提示 |

## 1.3 模型与 Provider 配置（重要）

来自 `phimint/.env`：

```
LLM_PROVIDER=Anthropic
LLM_MODEL=mimo-v2.5-pro
LLM_BASE_URL=https://token-plan-cn.xiaomimimo.com/anthropic
```

**关键事实：这不是 Anthropic 官方 API。** 是小米 MiMo 模型通过一个 **Anthropic 兼容协议中转**提供服务。phimint 用 `anthropic.rs` 适配器与之通信。

实际请求体（从日志 `Anthropic chat_stream request` 提取）：

```json
{
  "max_tokens": 8192,
  "model": "mimo-v2.5-pro",
  "thinking": { "type": "enabled" },
  "tools": [ ... 17 个 ... ],
  "system": "...",
  "messages": [ ... ]
}
```

两个值得注意的点，后文会展开：

- `max_tokens: 8192` 是**硬编码**的（`agent-base/src/llm/anthropic.rs:233`）
- `thinking` 只发了 `{"type":"enabled"}`，**没有 `budget_tokens`**（`anthropic.rs:251-258`：仅当 `config.budget_tokens` 为 `Some` 时才写入）

## 1.4 会话目录与日志布局（分析前必读）

```
~/.phimint/sessions/20260824_51046ddb/
├── session_meta.json     147B    created_at / last_active_at / session_id
├── session_id             17B
├── session.log           5.2MB  3,460 行  ← 当前（最后一段）
├── session.1.log         9.7MB  2,989 行
├── session.2.log         9.8MB  2,716 行
├── session.3.log         9.9MB 16,877 行  ← 最早
├── perf.log             94.2MB  2,963,868 行  UI 逐帧 CSV
├── frames.txt            6.4MB
├── turn_001.jsonl        88.8KB    734 行  第 1 个用户轮次的事件流
├── turn_002.jsonl       434.8KB  2,683 行  第 2 个用户轮次的事件流
├── composer.log             0B
└── session.lock             0B
```

⚠️ **日志轮转顺序是反直觉的**，按时间正序应为：

```
session.3.log  06:28:26 → 06:39:58   （最早）
session.2.log  06:39:58 → 06:44:54
session.1.log  06:44:54 → 06:47:31
session.log    06:47:31 → 08:12:42   （最新）
```

**只看 `session.log` 会漏掉 turn 1–78**（它只覆盖 turn 79–88）。而且文件内没有任何轮转序号标记，只能靠时间戳推断顺序 —— 这本身是个可改进点（见 [F16](#f16-fix)）。

日志行格式：

```
[YYYY-MM-DD HH:MM:SS] [LEVEL] [module::path] message {json_fields}
```

`turn_00N.jsonl` 事件类型：`thought_delta` / `text_delta` / `tool_call_started` / `tool_call_finished` / `checkpoint` / `plan_updated` / `run_finished` / `turn_end`。首行是 `{timestamp, turn, user_input}`。

---

# 第二部分：本次会话发生了什么

## 2.1 时间线总览

| 时间 | 事件 |
|---|---|
| 06:28:26 | 会话启动（2 条 skill 目录 WARN，见 [F15](#f15)） |
| 06:28:45 – 06:30:17 | **用户轮次 1**：17 个 react turn，92s |
| 06:30:28 – 06:49:07 | **用户轮次 2**：88 个 react turn，18m39s |
| 06:49:07 | `agent turn completed {"turn_count":88}` —— 任务完成 |
| 06:49:07 – 08:12:42 | **83 分钟纯 UI 活动**：用户滚动阅读输出、Ctrl+C 复制（1,783 行日志，其中 957 行 `scroll_up`） |

**Agent 实际工作时长 = 20m22s**（06:28:45 → 06:49:07）。后面 83 分钟是人在读结果，不是 agent 卡住。

## 2.2 两个用户轮次

| # | 用户输入 | react turns | 耗时 | tokens | messages | 结束方式 |
|---|---|---|---|---|---|---|
| 1 | 「你好，我想重构下这个构成的 ui 模块，怎么重构，我们讨论下」 | 17 | 92s | 8.2k → 16.2k | 2 → 36 | guard 判定完成 |
| 2 | 「按照你的推荐重构吧」 | 88 | 18m39s | 16.2k → 105.4k | 36 → 220 | guard 判定完成 |

**行为正确性检查**：用户轮次 1 明确说「我们讨论下」，agent 确实只做了探索 + 给方案（`repo_map` → `list_files`×3 → `read_file`×7 → `search_content`×4），**没有擅自动手改代码**。轮次 2 用户批准后才开始执行。这一点值得肯定。

## 2.3 任务结果（已交叉验证，真实有效）

轮次 2 的目标：拆分臃肿的 `src/ui/` 模块。

最终产出（agent 自述 ↔ `git status` ↔ 磁盘实际文件，三方一致）：

| 文件 | 原行数 | 新行数 | 变化 |
|---|---|---|---|
| `src/ui/render.rs` | 1695 | 810 | **-52%** |
| `src/ui/app.rs` | 2895 | 2622 | -9% |

新增 4 个文件：`render_markdown.rs`(675) / `render_popup.rs`(262) / `app_mention.rs`(149) / `app_slash.rs`(139)。
为支持跨文件 `impl App`，`mention` / `slash` / `skill_summaries` 三个字段改为 `pub(super)`。
**`cargo test` 227 个测试全部通过。**

`git status` 确认：`M src/ui/app.rs`、`M src/ui/mod.rs`、`M src/ui/render.rs`，`?? app_mention.rs`、`?? app_slash.rs`、`?? render_markdown.rs`、`?? render_popup.rs`。

**结论：任务成功，产出质量高，agent 的自我报告没有夸大或幻觉。**

## 2.4 工具调用统计

两轮合计 **103 次**工具调用（成功 101，失败 2，错误率 **1.9%**）：

```
read_file       33
edit_file       21
execute_command 18   （退出码：17×0，1×1）
search_content  12
update_plan      8
write_file       4   （+2 次失败）
list_files       3
repo_map         1
diagnostics      1
```

## 2.5 模型行为质量（整体优秀）

- **reasoning 简洁克制**：每 turn 思考文本 30–650 字符，峰值 1008（T60）。无冗长空转、无自我怀疑循环。
- **零卡死、零重试风暴**：无重复的相同调用，无死循环。
- **失败自愈**：两次 `write_file` 拿到空参数后，主动改用 `edit_file` 策略并成功（详见 [P0-1](#p0-1)）。
- **计划纪律好**：`update_plan` 调用 8 次，10 个步骤状态单调推进、全部 completed，无反复摇摆。
- **最终输出质量高**：1662 字符结构化中文报告，含文件结构树、行数变化表、字段可见性变更说明。

唯一的计划瑕疵：计划里写的是拆分 `app_handlers.rs` / `app_features.rs`，实际产出叫 `app_mention.rs` / `app_slash.rs` —— 命名漂移，不影响结果。

---

# 第三部分：性能基线

## 3.1 延迟分布（n = 105 次 LLM 调用）

| 指标 | min | p50 | p90 | p99 | max | mean | 总计 |
|---|---|---|---|---|---|---|---|
| **TTFT** | 0.3ms | 66.8ms | 2.66s | 8.62s | 9.32s | 930ms | 97.6s |
| **Stream 时长** | 547ms | 1.88s | 17.3s | 83.1s | 84.0s | 7.6s | 798.8s |

**解读：**

1. **LLM 流式耗时占 agent 总时长 65%**（798.8s / 1222s），其余是工具执行与 `cargo` 编译。这是主要优化战场。
2. **TTFT 双峰分布**：p50 仅 66.8ms（明显是 prompt cache 命中），但 p90 跳到 2.66s、p99 到 8.62s（cache miss）。**缓存命中率不稳定**，值得单独调查（见 [Q3](#q3)）。
3. **10 次超长流**：84.0s / 83.1s / 81.2s / 78.8s / 59.1s / 27.5s / 26.8s / 25.7s / 22.5s / 20.3s。全部对应大参数的 `write_file` / `edit_file`（args 9k–26k 字符）。其中 **84.0s 和 81.2s 两次是纯浪费**（见 [P0-1](#p0-1)）。

## 3.2 上下文管理（健康，但未被压测）

- **压缩从未触发**：峰值 105,440 tokens vs 触发阈值 160,000 —— **只到阈值的 66%**
- 消息数线性增长 +2/turn（伴随 assistant 文本时 +3），最终 220 条
- `[compression-timing] threshold check` 执行 105 次，开销可忽略
- 无截断、无信息丢失

**结论：本次上下文管理健康，但也意味着压缩路径完全没被覆盖验证**（见 [F16](#f16)）。

## 3.3 UI 渲染性能（很好）

来自 `perf.log`（2,963,868 行 CSV）：

```
平均 draw_ms = 0.007
最大 draw_ms = 21
draw_ms ≥ 8 的帧 = 759
```

渲染本身完全不是瓶颈。但这个文件本身是个问题（见 [P1-3](#p1-3)）。

---

# 第四部分：问题清单

## <a id="p0-1"></a>P0-1：165 秒浪费在空工具参数上，且完全静默

### 现象

turn 21 与 turn 22 连续两次 `write_file` 拿到**完全空的参数字符串**：

```
[2026-08-24 06:36:30] [DEBUG] [agent_base::engine::runtime::tool_engine] tool args JSON parse failed
    {"args":"","error":"EOF while parsing a value at line 1 column 0","session_id":1,"tool":"write_file"}
[2026-08-24 06:37:55] [DEBUG] [agent_base::engine::runtime::tool_engine] tool args JSON parse failed
    {"args":"","error":"EOF while parsing a value at line 1 column 0","session_id":1,"tool":"write_file"}
```

代价：**84.0s + 81.2s = 165.1s**，占全部 LLM 流式时间的 **13.6%**。

### 证据：h2 帧级时间线（turn 21）

```
06:35:01  calling LLM {"msg_count":77,"tool_count":17,"turn":21}
06:35:06  LLM chat_stream: API response received
06:35:08  LLM first token received {"ttft":"2.129619583s"}
06:35:06–06:35:09   ~40 个 h2 Data 帧  ← thinking + 78 字符 text + tool_use content_block_start
06:35:09  LLM stream: first tool_call chunk received
          ┌─────────────────────────────────────────────────┐
06:35:41  │ 1 个 Data 帧              （32 秒后）           │  81 秒近乎完全静默
06:36:30  │ 2 个 Data 帧 + END_STREAM （又 49 秒后）        │  零个 input_json_delta
          └─────────────────────────────────────────────────┘
06:36:30  LLM stream done {"elapsed_ms":"83969","reasoning_len":262,"text_len":78,"tool_calls":1}
06:36:30  handle tool calls start {"tool_names":"[\"write_file\"]"}
06:36:30  tool args JSON parse failed {"args":"", ...}
```

那 3 个稀疏帧几乎肯定是 SSE `ping` 保活 + 最后的 `message_delta`/`message_stop`。

### 根因（两层）

**第一层：上游中转行为异常。** 中转打开了 `tool_use` content block（所以 `tool_calls=1`），然后**一个 `input_json_delta` 都没发**就结束了流。客户端累加器 `entry.2.push_str(args)`（`llm_engine.rs:214`）因此从未被调用，只剩 `content_block_start` 创建的空条目 —— 这解释了为什么 args 是**完全空**而不是「截断的半个 JSON」。

**触发条件：撞上 `max_tokens: 8192` 天花板。** 强证据：

| turn | 工具 | args_len | stream | 结果 |
|---|---|---|---|---|
| T10 | `write_file` | 25,655 | 78.8s | ✅ 勉强成功（≈8k output tokens，正好卡线） |
| T16 | `edit_file` | 24,030 | 83.1s | ✅ 勉强成功 |
| **T21** | `write_file` | **0** | **84.0s** | ❌ 上游摆烂 |
| **T22** | `write_file` | **0** | **81.2s** | ❌ 上游摆烂 |
| T23 | `edit_file` | 23,534 | 59.1s | ✅ 模型改用 edit_file 后成功 |

模型当时想整文件重写 `render.rs`（1670 行 ≈ 25k+ 字符），超过了 8192 output token 预算。`thinking` 又与正文共享同一预算且未设 `budget_tokens`，进一步压缩了可用输出空间。

**第二层：phimint 完全没识别出这是截断。** 见 [P0-2](#p0-2)。

### 影响

- 直接损失 165s（13.6% 的 LLM 时间）
- 日志里只有一行 **DEBUG** 级记录，生产环境默认不可见 —— 一个 P0 级故障几乎不留痕迹
- **模型收到了错误，但归因是错的，导致它重复踩同一个坑**（详见下节 [P0-1b](#p0-1b)）

---

## <a id="p0-1b"></a>P0-1b：错误信息误导模型，直接造成第二次 81 秒浪费

这是 [P0-1](#p0-1) 的延伸，也是**实际造成损失翻倍的直接原因**。

### 先厘清两件常被混淆的事

**(a) `write_file` 并没有报错 —— 它根本没被调用。**

失败发生在**参数解析阶段**，早于工具执行（`tool_engine.rs:382-396`）：

```rust
tracing::debug!(..., "tool args JSON parse failed");
failures.push(ToolFailure {
    id: id.clone(),
    error: AgentError::ToolArgsInvalid {
        name: name.clone(),
        raw: format!("{} (args: {})", e, args_str),
    },
});
continue;          // ← 跳过 process_approval 与 execute_tool
```

日志侧印证：T21/T22 **缺少**成功 turn 都有的三行 —— `execute tool start`、`looking up tool in registry`、`tool found, executing via pipeline`，也没有 `tool calls done, continuing loop`。

**(b) 模型确实收到了错误反馈，机制工作正常。**

`tools.rs:460-474` 会为每个失败推一条 `tool_result`，再返回 `Err` 触发 `handle_tool_error` 追加一条 recovery prompt。从 T22 的**实际请求体**可以确认模型收到了什么：

```json
[assistant/tool_use]  {"name":"write_file","input":{}}
                       ↑ input 是空对象，证实参数从未到达客户端
[user/tool_result]    "Tool 'write_file' argument parsing failed:
                       EOF while parsing a value at line 1 column 0 (args: )"
[user/text]           "Tool calls failed: write_file
                       Error: Tool 'write_file' argument parsing failed:
                       EOF while parsing a value at line 1 column 0 (args: )
                       Please analyze the error and adjust your approach."
```

这解释了 `msg_count` 的 **+4** 增长（正常 +2）：assistant text + assistant tool_use + tool_result + recovery prompt。

### 真正的问题：归因完全错误

模型被告知的是 **「你生成的 JSON 语法非法（EOF while parsing）」**。
实际发生的是 **「响应撞上 8192 output token 上限，参数被上游截断/丢弃」**。

对一个收到「JSON 语法错了」的模型来说，最合理的反应就是**照原样再写一遍**。它就是这么做的：

| turn | assistant text | 动作 | 结果 |
|---|---|---|---|
| T21 | "Now let me rewrite `render.rs` cleanly, removing the duplicated markdown code:" | `write_file` | ❌ 84.0s |
| T22 | "Let me rewrite `render.rs` cleanly, removing the extracted markdown code:" | `write_file` **重试同策略** | ❌ 81.2s |
| T23 | "Let me use `edit_file` to remove the duplicated markdown code block from render.rs." | `edit_file` | ✅ 59.1s |

**全程模型从未提及 token 限制 —— 它对此毫不知情。**

### 影响与定性

- **误导性错误信息直接造成第二次 81 秒浪费**。若首次就告知「输出超限，请改用增量编辑或分块写入」，模型极可能一次就切换策略，165s 损失可减半甚至避免。
- 这让 [P0-2](#p0-2) 的危害升级：`stop_reason` 从不解析，不只是「缺少守卫」这种**遗漏**，而是**主动向模型灌注了错误归因**。守卫缺失让系统沉默，错误文案让模型走错路。
- 对比 `tools.rs:43` 那个本该触发的守卫，它的文案是正确的：
  > "Tool call was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments."
  
  **正确的诊断文案早就写好了，只是因为 `finish_reason` 恒为 `None` 而永远送不到模型面前。**

---

## <a id="p0-2"></a>P0-2：Anthropic 适配器从不解析 `stop_reason`，导致整条截断守卫链失效

这是**单点根因**，也是本报告最重要的发现。

### 好消息：下游守卫链是完整且正确的

我逐层验证了截断处理链路，**除了第一层，其余全部实现正确**：

```
1. anthropic.rs 解析 SSE → finish_reason: Option<String>     ❌ 永远是 None
2. turn.rs:409  FinishReason::from_raw(...)                   ✅ 正确
3. finish_reason.rs:70-89  "max_tokens"/"length" → Truncated   ✅ 正确
4. turn.rs:562  tool-call 分支，把 &finish_reason 传下去       ✅ 正确
5. tools.rs:43  if finish_reason.is_truncated() { ... }        ✅ 正确
```

第 5 层（`tools.rs:43-65`）的设计完全对症 —— 它会拒绝执行工具、给模型一条明确的重试指令：

```rust
// Truncation guard — when the LLM response hit the token limit,
// tool call arguments may be incomplete. Fail all tool calls
// without executing them, so the LLM can retry with complete args.
if finish_reason.is_truncated() {
    tracing::warn!(..., "LLM response truncated (finish_reason=length) — \
        tool calls may have incomplete arguments, marking as errors");
    for (tc_id, tc_name, _) in &tool_calls {
        // push_tool_result:
        // "Tool call was not executed: the response hit the output token limit,
        //  so its arguments may be truncated. Re-issue the tool call with
        //  complete arguments."
    }
    return Ok(TurnFlow::Continue);
}
```

**如果这个守卫能触发，T21/T22 会立刻拿到清晰的错误反馈并重试，而不是白等 165 秒然后静默失败。**

### 坏消息：第一层把整条链饿死了

`agent-base/src/llm/anthropic.rs` 有两处协议理解错误：

**(a) `message_stop` 分支读了一个不存在的字段**（`anthropic.rs:388-394`）：

```rust
"message_stop" => {
    let finish_reason = data
        .get("message")                    // ← 真实协议里 message_stop 没有这个字段
        .and_then(|m| m.get("stop_reason"))
        .and_then(Value::as_str)
        .map(String::from);
    Ok(StreamChunk::Stop { finish_reason })
}
```

真实 Anthropic SSE 协议中，`message_stop` 事件是**裸的** `{"type":"message_stop"}`。

**(b) `message_delta` 分支丢掉了真正的 `stop_reason`**（`anthropic.rs:375-387`）：

```rust
"message_delta" => {
    let output_tokens = data.get("usage")...;   // ← 只取了 usage
    Ok(StreamChunk::Usage(UsageInfo { ... }))   // ← delta.stop_reason 被丢弃
}
```

而 `stop_reason` 实际就在这里：

```json
{"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null},"usage":{"output_tokens":8192}}
```

**净效果：`finish_reason` 恒为 `None` → `from_raw(None)` 返回 `Stop` → `is_truncated()` 恒为 false → 守卫永不触发。**

日志侧交叉验证：全部 4 个日志文件里 `grep -o 'stop_reason'` **零命中**，从未记录过任何 stop reason。

### 为什么这个 bug 能长期存活：单元测试固化了错误的协议格式

`anthropic.rs:806` 的测试用例：

```rust
r#"{"type":"message_stop","message":{"stop_reason":"end_turn"}}"#
```

这个 wire format **线上根本不存在**。测试喂给自己一个虚构的输入，于是测试通过、bug 隐藏。这是典型的「测试验证了实现，而非验证了协议」。

### 附带发现：三个 provider 归一化函数是死代码

`finish_reason.rs` 里的 `from_openai` / `from_anthropic` / `from_responses` **只被它们自己的单元测试引用**，运行时路径（`turn.rs:409`）用的是通用的 `from_raw`。

```
$ grep -rn "from_anthropic\|from_openai\|from_responses" src --include="*.rs" | grep -v "fn from_"
→ 全部命中都在 finish_reason.rs 的文档注释和 #[test] 里
```

`from_raw`（`finish_reason.rs:70-89`）已正确覆盖 `"length"`、`"max_tokens"`、`"incomplete:max_output_tokens"`，功能上不缺东西。但**两套并行实现**是维护陷阱：将来有人改 `from_anthropic` 会以为生效了，其实没有。

---

## <a id="p1-3"></a>P1-3：perf.log 99.88% 是无效写入

```
总行数                      2,963,868   （94.2 MB）
dirty=true（真实绘制）           3,491   （ 0.12%）
dirty=false（空转帧）        2,960,377   （99.88%）
```

104 分钟写了 296 万行 CSV，约 **474 行/秒**，只为记录 3,491 次真实绘制。

典型空转行（`draw_ms=0, dirty=false, slept=true`）：

```csv
frame_id,draw_ms,capture_ms,loop_ms,dirty,scroll_offset,follow_bottom,output_lines,crossterm_events,slept,event_types
1,0,0,2,false,0,true,9,0,true,
```

讽刺的是：**UI 渲染性能本身很好**（avg 0.007ms / max 21ms），这 94MB 数据除了证明「没事发生」之外没有信息量。这是全部问题里**最容易拿到的收益**。

---

## P1-4：日志信噪比过低

`session.log` 3,460 行的构成：

| 模块 | 行数 | 占比 |
|---|---|---|
| `phimint::ui::app` (DEBUG) | 1,648 | 47.6% |
| `phimint::ui::render` (DEBUG) | 1,146 | 33.1% |
| `h2::codec::framed_read/write` (DEBUG) | 362 | 10.5% |
| **engine INFO（真正有价值的部分）** | **149** | **4.3%** |

具体问题：

- **逐帧 UI DEBUG 淹没一切**：`draw layout` / `render_composer` / `window_range` 每帧各一行。
- **任务完成后还在刷**：06:49:07 之后又写了 1,783 行，其中 **957 行是 `scroll_up`**（用户翻阅输出时产生）。
- **h2 帧级日志常开**：本次排障靠它锁定了根因（很有价值），但日常应按需开启。
- `session.3.log` 更极端：11 分钟 16,877 行。

后果：重建一次执行时间线需要跨 4 个文件、过滤 96% 的噪音。

---

## P1-5：工具调用效率问题

### (a) 路径幻觉浪费 3 个 turn

```
T25  execute_command: cd /home/user/repos/phimint && cargo check   → exit_code=1
T26  execute_command: pwd && ls                                     ← 探路恢复
T27  execute_command: cargo check                                   ← 重试成功
```

`/home/user/repos/phimint` 是容器风格路径，本机根本不存在（实际在 `~/source/buka/buka-works/phimint`）。这是全会话**唯一的非零退出码**。系统提示里显然没有给出明确的绝对工作目录。

### (b) 「行号狩猎」：33 次 read_file 里约 20 次在反复读同一个文件

`src/ui/render.rs` 被反复读取，窗口越来越小：

```
limit=200 → 300 → 200 → 1300 → 1670 → 500 → 400 → 62 → 200 → 20 → 20 → 10 → 4 → 4
```

`limit=4` 意味着模型在为 `edit_file` 精确定位锚点文本而逐行试探。这是 **`edit_file` 易用性不足的强信号** —— 模型不得不用大量廉价读取来换取一次编辑成功。

### (c) 模型主动绕过 edit_file

T47 和 T66 两次改用 shell 手工改文件：

```python
python3 -c "
lines = open('src/ui/render.rs').readlines()
# Remove lin...
```

```python
python3 -c "
lines = open('src/ui/app.rs').readlines()
# Find the line...
```

模型选择「读全文 → 按行切片 → 重写」而不用正规编辑工具，是工具体验有摩擦的直接证据。

### (d) 18 次 shell 里 14 次是全量编译

`cargo check` × 11 + `cargo test` × 3。对重构任务算合理，但每次都是完整编译周期。phimint 号称有「LSP 快速内环」（改完不重编译秒级拿诊断），但全程只调用了 **1 次 `diagnostics`** —— 这个卖点特性基本没被用上。

---

## <a id="f15"></a>P2-6：启动期 skill 目录 WARN

```
[06:28:26] [WARN] [agent_works::skill::prompt_skill] skipping invalid skill directory
    {"dir":"/Users/kangzengchen/.claude/skills/connect-chrome","error":"Directory name 'connect-chrome' does not match skill na..."}
[06:28:26] [WARN] ... {"dir":"/Users/kangzengchen/.claude/skills/_gstack-command", ...}
```

属用户本地 skill 配置问题（目录名与 skill 名不匹配），**非 phimint 缺陷**。但错误信息被截断了，没告诉用户「期望的名字是什么」，可读性可以改进。

---

## <a id="f16"></a>P2-7：压缩路径零覆盖

峰值 105,440 tokens vs 阈值 160,000，压缩逻辑本次**完全没执行过**。这条路径的正确性在本次测试中未获任何验证。

---

# 第五部分：修复建议

按「投入产出比」排序。标注了精确的 `file:line`。

## 🔴 修复 1（最高优先级）：让 Anthropic 适配器正确解析 stop_reason

**这是单点修复，能一次性激活整条已经写好的截断守卫链。**

**F1.** `agent-base/src/llm/anthropic.rs:375-387` —— `message_delta` 分支补读 `delta.stop_reason`：

```rust
"message_delta" => {
    let stop_reason = data
        .get("delta")
        .and_then(|d| d.get("stop_reason"))
        .and_then(Value::as_str)
        .map(String::from);
    let output_tokens = data.get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_u64)
        .map(|v| v as u32);
    // 需要同时携带两者
}
```

⚠️ **实现注意**：`StreamChunk` 目前是枚举，一个事件只能返回一种 chunk。这里需要设计决策 —— 三个选项：
1. 让 `parse_sse` 返回 `Vec<StreamChunk>`（`openai_responses.rs:246-256` 已经在用 `chunks.push(...)` 的模式，可对齐）
2. 给 `StreamChunk::Usage` 增加可选的 `finish_reason` 字段
3. 在适配器内部缓存 stop_reason，到 `message_stop` 时再吐出

推荐**选项 1**，与 `openai_responses.rs` 现有风格一致。

**F2.** `anthropic.rs:388-394` —— `message_stop` 不再尝试读 `message.stop_reason`（该字段不存在）。改为直接 `Ok(StreamChunk::Stop { finish_reason: None })`，或配合 F1 选项 3 吐出缓存值。

**F3.** `anthropic.rs:806` —— **测试用真实 wire format**，这是防止回归的关键：

```rust
// message_delta 携带 stop_reason（真实格式）
r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null},"usage":{"output_tokens":8192}}"#
// message_stop 是裸的（真实格式）
r#"{"type":"message_stop"}"#
```

再补一条**端到端 SSE 测试**：喂入完整的 `message_start → content_block_start(tool_use) → input_json_delta → message_delta(stop_reason=max_tokens) → message_stop` 序列，断言 `LlmTurnResult.finish_reason == Some("max_tokens")`，并断言 `tools.rs:43` 的守卫被触发。

**F4.** `agent-base/src/types/finish_reason.rs` —— 处理死代码 `from_openai` / `from_anthropic` / `from_responses`：要么删除（运行时统一走 `from_raw`），要么让各 provider 适配器真正调用它们。**保留两套并行实现是维护陷阱。** 建议直接删除，并在 `from_raw` 上加注释说明它是唯一入口。

## 🔴 修复 2：让空/非法工具参数不再静默

**F5.** `agent-base/src/engine/runtime/tool_engine.rs` —— `tool args JSON parse failed` 目前是 **DEBUG** 级，升为 **WARN**，并补充上下文字段：`finish_reason`、`stream_elapsed_ms`、`args_len`、`turn`。

> 本次一个 P0 级故障（165s 损失）在日志里只有一行 DEBUG，生产默认不可见。这是可观测性的严重缺口。

**F6.** 增加**空参数专项检测**：`args` 为空字符串但 `tool_calls` 非空，是明确的异常态（正常至少是 `{}`）。应独立报警并直接给模型明确反馈，不依赖 JSON 解析器的 `EOF while parsing` 这种间接信号。

**F7.** 增加**流式空闲看门狗**：收到 `first tool_call chunk` 之后，若 N 秒（建议 20–30s）内无任何 delta 到达，主动断流重试。本次两次故障各干等 81s，看门狗可省下约 130s。

## 🟠 修复 3：调整输出预算，从根上避免触发

**F8.** `agent-base/src/llm/anthropic.rs:233` —— `max_tokens: 8192` 硬编码，**改为可配置**（按 provider/model）。

依据：整文件重写 1670 行需 ≈8k+ output tokens，正好撞线。T10 的 `args_len=25655` 是勉强通过的临界样本，T21/T22 越线即崩。8192 对现代编码 agent 明显偏小。

**F9.** `anthropic.rs:251-258` —— `thinking` 启用时应显式设置 `budget_tokens`。当前只发 `{"type":"enabled"}`，而 thinking 预算与正文共享 `max_tokens`，不设定会让「留给工具参数的实际预算」变得不可控，直接加剧 F8 的问题。

## 🟠 修复 4：提示词与工具易用性（削减无谓开销）

**F10.** **抑制整文件重写**：系统提示中明确「超过约 300 行的文件禁止用 `write_file` 全量重写，必须用 `edit_file` 增量修改」。这条直接消除 [P0-1](#p0-1) 的触发条件，也是最低成本的缓解手段。

**F11.** **注入绝对工作目录**：系统提示中显式给出 cwd，消除 T25 的 `/home/user/repos/...` 幻觉（P1-5a，浪费 3 turn）。

**F12.** **降低 edit_file 定位成本**：让 `read_file` 返回带行号的内容，或让 `edit_file` 支持行号区间/模糊锚点匹配。可显著削减那 20 次 `render.rs` 小窗口重读（占 33 次 `read_file` 的大头，P1-5b），同时消除模型绕道 `python3 -c` 的动机（P1-5c）。

**F13.** **推广 LSP 内环**：phimint 的卖点特性「改完秒级拿诊断」全程只用了 1 次 `diagnostics`，却跑了 11 次 `cargo check`。应在系统提示中引导：改完先用 `diagnostics`，只在需要跑测试时才 `cargo test`。

## 🟡 修复 5：日志与观测

**F14.** `perf.log` **只在 `dirty=true` 时写行**（或聚合为每秒一行摘要）。立即消除 **99.88%** 的写入量：94MB → 约 0.1MB。**投入最小、收益最直观的一条。**

**F15.** `phimint::ui::app` / `phimint::ui::render` 的逐帧 DEBUG **降级为 TRACE**；`h2::codec` 默认关闭、按需开启（本次排障靠它，但不该常开）。目标：让 engine INFO 从 4.3% 提升到可读比例。

**F16.** <a id="f16-fix"></a>**日志轮转加序号/边界标记**，或在 `session_meta.json` 里维护文件顺序。当前必须靠时间戳猜 `session.3 → session.2 → session.1 → session.log`，极易误判（只看 `session.log` 会漏掉 turn 1–78）。

**F17.** 补 **turn 级结构化摘要**：每 turn 一行 JSON，含 `turn` / `tools` / `tokens` / `msg_count` / `ttft_ms` / `stream_ms` / `finish_reason` / `error`。避免像本次这样必须跨 4 个文件、用 Python 重建时间线。

**F18.** 记录 `stop_reason` / `finish_reason` 到日志（修复 F1 后自然可得）。当前全部日志 `grep stop_reason` 零命中。

## 🟡 修复 6：测试覆盖

**F19.** 压缩路径本次零覆盖（峰值 105k vs 阈值 160k）。增加长会话测试，或提供可调低阈值的测试开关，验证压缩正确性。

**F20.** 增加**大参数工具调用**的集成测试：构造接近/超过 `max_tokens` 的 `write_file` 调用，断言截断被正确识别、守卫触发、模型收到可操作的错误反馈。

---

# 第六部分：给评审者的开放问题

以下问题我无法仅从日志判定，需要人工或进一步实验确认：

**Q1. 上游中转的行为该如何归因？**
`mimo-v2.5-pro` 中转在超预算时「打开 tool_use 块但不发任何参数、静默结束流」—— 这是中转 bug、模型 bug，还是某种预期内的降级？建议直接用 curl 复现：构造一个必然超 8192 output token 的 tool_use 请求，抓完整 SSE 流，确认 `stop_reason` 到底是什么值（甚至是否发送）。**这决定了 F1 修好后能否真正兜住这个 case。**

**Q2. 若上游根本不发 `stop_reason`，F1 就不够。**
那种情况下需要额外的启发式兜底：`tool_calls` 非空但所有 `arguments` 为空 → 直接判定为异常并反馈重试（即 F6 的独立价值）。建议 F1 与 F6 都做，不要只做 F1。

**<a id="q3"></a>Q3. TTFT 双峰（p50 66.8ms vs p90 2.66s）的成因？**
是 prompt cache 命中率波动、中转排队，还是 `thinking` 开启导致的首 token 延迟抖动？若是缓存问题，稳定命中可显著改善交互体感（TTFT 总计 97.6s 有压缩空间）。

**Q4. 88 个 react turn 完成一次 UI 模块拆分，是否偏多？**
其中 33 次 `read_file`、11 次 `cargo check`。若 F12（编辑工具易用性）+ F13（LSP 内环）落地，能压缩到多少？值得设一个基线指标持续跟踪。

**Q5. `max_tokens` 提到多少合适？**
需要结合 `mimo-v2.5-pro` 的实际上限、`thinking` 预算切分策略、以及成本一起定。是否该按工具类型动态调整（如 `write_file` 时临时提高）？

---

# 附录 A：分析所用命令

日志文件极大（单行可达 335KB，含完整 JSON body），**不要直接读取整个文件**。以下命令可复现本报告的全部结论。

```bash
S=~/.phimint/sessions/20260824_51046ddb

# 0) 确认轮转顺序（关键第一步）
for f in session.3.log session.2.log session.1.log session.log; do
  echo "== $f"
  awk 'NR==1{print "  first: " substr($0,1,21)} END{print "  last:  " substr($0,1,21); print "  lines: " NR}' $S/$f
done

# 1) 按模块/级别统计信噪比
python3 -c "
import re; from collections import Counter
pat=re.compile(r'^\[([\d\- :]+)\] \[(\w+)\] \[([\w:]+)\] (.*)\$')
c=Counter()
for line in open('$S/session.log',errors='replace'):
    m=pat.match(line)
    if m: c[(m.group(2),m.group(3))]+=1
for k,v in c.most_common(20): print(v,k)"

# 2) 提取 engine INFO 时间线（过滤 UI/h2 噪音）
python3 -c "
import re
pat=re.compile(r'^\[([\d\- :]+)\] \[(\w+)\] \[([\w:]+)\] (.*)\$')
for f in ['session.3.log','session.2.log','session.1.log','session.log']:
    for line in open('$S/'+f,errors='replace'):
        m=pat.match(line)
        if not m or m.group(2)!='INFO': continue
        if m.group(3).startswith(('phimint::ui','h2::','hyper')): continue
        print(m.group(1)[11:], m.group(3).split('::')[-1],'|',m.group(4)[:200])"

# 3) 工具调用序列 + 参数长度（含失败）
grep -h 'execute tool start\|tool args JSON parse failed' $S/session.3.log \
  $S/session.2.log $S/session.1.log $S/session.log | cut -c1-200

# 4) 本次核心缺陷：空参数失败
grep -h 'tool args JSON parse failed' $S/session.*.log $S/session.log

# 5) h2 帧级证据（锁定 81 秒静默）—— 把时间窗替换成任意可疑 turn
python3 -c "
import re
pat=re.compile(r'^\[([\d\- :]+)\] \[(\w+)\] \[([\w:]+)\] (.*)\$')
for line in open('$S/session.3.log',errors='replace'):
    m=pat.match(line)
    if not m: continue
    ts=m.group(1)[11:]
    if not ('06:35:05'<=ts<='06:36:35'): continue
    if 'h2::codec' in m.group(3) or 'llm_engine' in m.group(3):
        print(ts, m.group(3).split('::')[-1], m.group(4)[:150])" | grep -v 'thought chunk'

# 6) TTFT / stream 分位数
python3 -c "
import re,json,statistics as st
pat=re.compile(r'^\[([\d\- :]+)\].*(LLM stream done|LLM first token received)[^{]*(\{.*\})\s*\$')
def ms(s):
    s=s.strip()
    if s.endswith('ms'): return float(s[:-2])
    if s.endswith('µs'): return float(s[:-2])/1000
    if s.endswith('s'): return float(s[:-1])*1000
    return 0.0
t=[];x=[]
for f in ['session.3.log','session.2.log','session.1.log','session.log']:
    for line in open('$S/'+f,errors='replace'):
        m=pat.match(line)
        if not m: continue
        d=json.loads(m.group(3))
        if m.group(2)=='LLM first token received': t.append(ms(d['ttft']))
        else: x.append(int(d['elapsed_ms']))
for name,a in (('TTFT ms',t),('stream ms',x)):
    a=sorted(a); n=len(a)
    print(f'{name}: n={n} p50={a[n//2]:.1f} p90={a[int(n*.9)]:.1f} p99={a[int(n*.99)]:.1f} max={a[-1]:.1f} sum={sum(a)/1000:.1f}s')"

# 7) 上下文增长与压缩阈值
grep -h 'threshold check' $S/session.3.log $S/session.2.log \
  $S/session.1.log $S/session.log | grep -o '{.*}' | tail -5

# 8) perf.log 浪费量化（94MB，用 awk 不要用 python）
awk -F, 'NR>1{tot++; if($5=="true")d++; sum+=$2; if($2+0>mx)mx=$2+0}
  END{printf "rows=%d dirty=%d (%.2f%%) idle=%.2f%% avg_draw=%.3fms max=%dms\n",
  tot,d,100*d/tot,100*(tot-d)/tot,sum/tot,mx}' $S/perf.log

# 9) 用户输入与最终输出（turn_00N.jsonl）
python3 -c "
import json
for f in ['turn_001.jsonl','turn_002.jsonl']:
    L=open('$S/'+f).readlines()
    print('==',f,'| USER:',json.loads(L[0]).get('user_input'))
    s=''.join(json.loads(l).get('delta','') for l in L
              if json.loads(l).get('type')=='text_delta')
    print('assistant text chars:',len(s)); print(s[-800:])"

# 10) 全局 WARN/ERROR 扫描
grep -h '\[WARN\]\|\[ERROR\]' $S/session.3.log $S/session.2.log \
  $S/session.1.log $S/session.log | cut -c1-220 | sort -u
```

---

## 修复记录

### P0-2: stop_reason 解析修复 ✅

**修复时间**：2026-08-24 15:30
**修复文件**：`agent-base/src/llm/anthropic.rs`
**测试结果**：454 个测试全部通过

#### 问题根源

Anthropic 适配器从不解析 `message_delta` 中的 `stop_reason` 字段：
- `message_delta` 分支只读取 `usage`，丢弃 `delta.stop_reason`
- `message_stop` 分支读取不存在的 `message.stop_reason` 字段
- 结果：`finish_reason` 永远是 `None`，截断守卫永不触发

#### 修复方案

**采用方案：返回 Vec<StreamChunk>**

1. **修改 `parse_sse` 返回类型**：
   - 从 `AgentResult<StreamChunk>` 改为 `AgentResult<Vec<StreamChunk>>`
   - 可以在一个事件中返回多个 chunks

2. **修复 `message_delta` 分支**：
   - 正确提取 `delta.stop_reason`
   - 如果有 `stop_reason`，返回 `[Usage, Stop]` 两个 chunks
   - 如果没有 `stop_reason`，只返回 `[Usage]`

3. **修复 `message_stop` 分支**：
   - 返回空 vec（因为 `stop_reason` 已经在 `message_delta` 中处理）

4. **修改调用方**：
   - 使用 `flat_map` 处理多个 chunks
   - 与 `openai_responses.rs` 风格一致

5. **更新测试用例**：
   - 修正为真实的 Anthropic 协议格式
   - 添加 6 个新的单元测试
   - 添加 1 个端到端集成测试

#### 验证结果

**新增的集成测试 `chat_stream_parses_truncated_response`** 验证了完整的截断检测流程：

```
chunks[0]: Usage (from message_start)
chunks[1]: ToolCall (from content_block_start)
chunks[2]: ToolCall (from content_block_delta)
chunks[3]: Usage (from message_delta, output_tokens=8192)
chunks[4]: Stop { finish_reason: Some("max_tokens") }  ← 关键！
```

**修复前**：
- `finish_reason` 永远是 `None`
- 截断守卫（`tools.rs:43`）永不触发
- LLM 收到"JSON语法错误"，重试相同策略，浪费 165 秒

**修复后**：
- `finish_reason` 正确返回 `"max_tokens"`
- 截断守卫现在能触发
- LLM 将收到"输出超限，请用增量编辑"的正确反馈

#### 影响范围

- ✅ 激活已写好的截断守卫链（`tools.rs:43-65`）
- ✅ P0-1b（错误信息误导）随 P0-2 一起解决
- ✅ 454 个测试全部通过，无回归
- ✅ 改动集中在一个文件，风险可控

#### 技术细节

**修改前**：
```rust
// anthropic.rs:375-386 - message_delta 分支
"message_delta" => {
    let output_tokens = data.get("usage")...;
    Ok(StreamChunk::Usage(UsageInfo { ... }))
    // ❌ delta.stop_reason 被丢弃
}

// anthropic.rs:387-394 - message_stop 分支
"message_stop" => {
    let finish_reason = data.get("message")...;  // ← 读不存在的字段
    Ok(StreamChunk::Stop { finish_reason })       // ← 永远是 None
}
```

**修改后**：
```rust
// anthropic.rs:375-400 - message_delta 分支
"message_delta" => {
    let stop_reason = data.get("delta").and_then(|d| d.get("stop_reason"))...;
    let output_tokens = data.get("usage")...;
    
    let mut chunks = vec![StreamChunk::Usage(UsageInfo { ... })];
    if let Some(finish_reason) = stop_reason {
        chunks.push(StreamChunk::Stop { finish_reason: Some(finish_reason) });
    }
    Ok(chunks)
}

// anthropic.rs:401-405 - message_stop 分支
"message_stop" => {
    // stop_reason 已在 message_delta 中处理，返回空
    Ok(vec![])
}
```

---

## 一页纸结论

**任务本身：成功。** 105 次 LLM 调用、103 次工具调用、20m22s，把 `render.rs` 从 1695 行降到 810 行（-52%），拆出 4 个新模块，227 个测试全绿。模型行为稳健：reasoning 简洁、无卡死、计划纪律好、失败能自愈、最终报告与实际改动完全一致（无幻觉）。

**~~但存在一条 P0 静默失败链路。~~** ~~上游中转在超 `max_tokens: 8192` 时打开 `tool_use` 块却不发任何参数就断流；phimint 因 `anthropic.rs` 从不解析 `stop_reason`（读了不存在的 `message_stop.message.stop_reason`，又丢掉了 `message_delta.delta.stop_reason`）而无法识别截断；于是 `tools.rs:43` 那个**本来完全写对了**的截断守卫永不触发，故障只在 DEBUG 级留下一行 `parse failed`。代价 165 秒（占 LLM 时间 13.6%），且被一条固化了虚构 wire format 的单元测试长期掩盖。~~

**✅ P0-2 已修复（2026-08-24 15:30）。** 截断守卫链现在能正常工作：`anthropic.rs` 正确解析 `message_delta.delta.stop_reason`，`finish_reason` 不再是 `None`，`tools.rs:43` 的守卫能正确触发并给 LLM 发送"输出超限"的反馈。

**剩余可优化项**：
- P1-3：`perf.log` 99.88% 是空转帧 —— 94MB 换 3,491 条有用记录，改成只在 `dirty=true` 时落盘即可
- P0-1：可选的空参数兜底检测（P0-2 已解决根本原因）
- 其他 P1/P2 问题：逐步推进
