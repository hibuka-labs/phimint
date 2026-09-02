# 子 Agent 结果推送机制设计

## 问题

当前 `wait_agent` 是阻塞式的：主 agent spawn 子 agent 后必须调 `wait_agent` 等待结果，期间主 agent 不能做其他事，用户也不能继续聊天。

## 目标

子 agent 完成后，结果**自动推送**到主 agent 的上下文，主 agent 不需要主动等待。

## 参考：codex 的实现

codex 使用 `InterAgentCommunication` + `input_queue` 实现：

1. 子 agent 完成 → `enqueue_mailbox_communication(trigger_turn: true)` 注入父 agent 的 mailbox
2. 父 agent 空闲时 → `maybe_start_turn_for_pending_work()` 检测 mailbox → 自动开新 turn
3. 父 agent 的新 turn 以子 agent 结果为输入，继续推理

关键设计点：
- `trigger_turn: true` 标记：只有带此标记的 mailbox 消息才会触发新 turn
- `watch::Receiver<InputQueueActivity>`：activity channel 通知父 agent 有新消息
- 父 agent 只在空闲时（无 active turn）才响应 mailbox

## phi 现状

```
主 agent → spawn_agent (异步) → wait_agent (阻塞) → 拿到结果 → 继续
```

子 agent 结果存在 mailbox 里（`MailboxHub.post_result`），但主 agent 必须主动 `wait_agent` 才能拿到。

## 设计方案

### 核心思路

复用现有 mailbox，增加"结果到达时自动注入主 agent 上下文"的能力。

### 数据流

```
子 agent 完成
    ↓
mailbox.post_result(result)
    ↓
通知主 agent（新增：通过 event_bus 或 input channel）
    ↓
主 agent 当前 turn 结束后，检查 pending results
    ↓
自动开新 turn，把子 agent 结果作为输入
    ↓
主 agent 继续推理（看到子 agent 的结果）
```

### 需要改动的组件

#### 1. MultiAgentRuntime（agent-works）

新增方法：`notify_parent_on_child_complete(agent_path, result)`

- 子 agent 完成时调用
- 往父 agent 的输入队列注入子 agent 结果
- 标记为 `trigger_turn`

#### 2. 父 agent 的 turn loop（agent-base）

在 turn 结束后增加检查：

```rust
// turn 结束后检查
if has_pending_child_results() {
    // 注入子 agent 结果到上下文
    // 自动开新 turn
}
```

#### 3. wait_agent（phi-kernel-tools）

保留但改为可选：
- 阻塞模式：当前行为不变，用于"我一定要等这个结果"的场景
- 非阻塞模式：只检查是否已有结果，没有就立即返回

#### 4. UI 层（phimint）

子 agent 结果到达时：
- 在 TUI 显示通知（如"子 agent xxx 已完成"）
- 不打断用户当前输入

### 与 codex 的差异

| 维度 | codex | phi |
|------|-------|-----|
| 线程模型 | 每个 agent 独立 thread | 父子共享 runtime |
| 输入队列 | input_queue + watch channel | mailbox + wait 阻塞 |
| 触发机制 | trigger_turn 标记 | 待实现 |
| 用户交互 | 父 agent 空闲时自动处理 | 需要适配 TUI |

### 实现步骤

**Phase 1：mailbox 通知**
- 子 agent 完成时，往父 agent 的 event_bus 发送通知
- 通知包含：子 agent path、结果摘要、状态

**Phase 2：自动 turn 触发**
- 主 agent turn 结束后检查 pending child results
- 有结果就自动开新 turn，注入结果

**Phase 3：wait_agent 改造**
- 保留阻塞模式（兼容）
- 新增非阻塞查询模式

**Phase 4：TUI 适配**
- 子 agent 结果到达时显示通知
- 支持用户继续输入

### 待定问题

1. 主 agent 正在执行工具时，子 agent 结果到达怎么办？（排队等当前工具完成）
2. 多个子 agent 同时完成，是逐个注入还是批量注入？
3. 子 agent 结果太长（超过 context window）怎么处理？
4. 要不要在 prompt 里告诉主 agent"子 agent 结果会自动到达，不需要 wait_agent"？

## 已完成的前置工作

### wait_agent 输出精简（2026-09-02）

**改动文件**：`agent-works/src/multi_agent/runtime/outcome.rs`

- `extract_assistant_text` → `extract_last_assistant_message`
- 只返回最后一次工具调用之后的 assistant 文本（最终回复），不返回完整对话
- 解决了 `wait_agent output exceeds 16000-char limit` 问题

**测试文件**：`agent-works/src/multi_agent/runtime/tests/outcome.rs`

### provider truncation guard（2026-09-02）

**改动文件**：`agent-base/src/engine/runtime/react/tools.rs`

- Case 4：空 `{}` 参数 + 工具有 required fields → 重新发出而非报错
- 回归测试：`agent-base/src/engine/runtime/react/tests.rs`

## 关键文件索引

### phi 多 agent 系统

| 文件 | 职责 |
|------|------|
| `phi-kernel-tools/src/multi_agent/spawn_agent.rs` | spawn_agent 工具定义 |
| `phi-kernel-tools/src/multi_agent/wait_agent.rs` | wait_agent 工具定义 |
| `agent-works/src/multi_agent/runtime.rs` | MultiAgentRuntime：mailbox、registry、wait_for_result |
| `agent-works/src/multi_agent/runtime/outcome.rs` | 子 agent 结果格式化（build_child_result） |
| `agent-works/src/multi_agent/runtime/spawn.rs` | 子 agent 生命周期管理 |
| `agent-works/src/multi_agent/mailbox.rs` | MailboxHub：post_result、try_recv_result |
| `agent-works/src/multi_agent/child.rs` | 子 agent 执行逻辑 |

### agent-base 框架

| 文件 | 职责 |
|------|------|
| `agent-base/src/engine/runtime/react/tools.rs` | 工具调用处理、truncation guard |
| `agent-base/src/engine/runtime/react/turn_loop.rs` | 主 turn loop |
| `agent-base/src/engine/pipeline.rs` | 工具输出限制（max_tool_output_chars） |
| `agent-base/src/engine/runtime/tool_engine.rs` | 工具引擎：解析、审批、执行 |
| `agent-base/src/types/events.rs` | RuntimeEvent 定义（TextDelta、ToolCallStarted 等） |

### phimint 应用

| 文件 | 职责 |
|------|------|
| `phimint/src/agent.rs` | agent 构建：max_tool_output_chars(16_000)、工具注册 |
| `phimint/src/prompt/mod.rs` | MULTI_AGENT prompt（鼓励并行 spawn） |

### codex 参考实现

| 文件 | 职责 |
|------|------|
| `demo/codex/codex-rs/core/src/tools/handlers/multi_agents/wait.rs` | wait_agent：只返回 AgentStatus，不返回内容 |
| `demo/codex/codex-rs/core/src/tools/handlers/multi_agents/spawn.rs` | spawn_agent：返回 agent_id + nickname |
| `demo/codex/codex-rs/core/src/agent/control.rs` | subscribe_status + watch channel |
| `demo/codex/codex-rs/core/src/session/input_queue.rs` | mailbox 输入队列 + trigger_turn 机制 |
| `demo/codex/codex-rs/core/src/tasks/mod.rs` | maybe_start_turn_for_pending_work：空闲时自动处理 mailbox |
| `demo/codex/codex-rs/protocol/src/protocol.rs` | AgentStatus::Completed(Option<String>)：包含最后一条消息 |

## 多层 agent 架构说明

phi 当前的 agent 层级：`root → 子 agent → 孙 agent`（通过 session.log / session.1.log 区分）

TUI 显示 `[root/xxx]` 前缀标识 agent 路径。并行子 agent 的输出交错显示，不是嵌套关系。
