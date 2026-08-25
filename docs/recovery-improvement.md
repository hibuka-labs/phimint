# ConsecutiveFailureRecovery 改进方案 (方案 B)

## 背景

日志：`~/.phimint/sessions/20260824_c18888ee/session.log`

两次 run，两种结束方式：

**Session 1（正确）**：4 个 turn，guard 触发。Turn 4 返回 1371 字符分析文本，无工具调用，`on_text_only` guard 判断 `response_chars >= judge_skip_threshold(256)` → `GuardAction::Done`。符合预期。

**Session 2（问题）**：18 个 turn，ConsecutiveFailureRecovery 触发，guard 从未生效。
- Turn 16-18：LLM 连续 3 次调用 `write_file`，参数为空 `{}`
- 错误消息：`"Tool 'write_file' argument parsing failed: "`（空的，serde 错误未捕获）
- `ConsecutiveFailureRecovery` 达到 `max_consecutive_failures=3` → `ToolErrorAction::Stop` → Run 结束
- LLM 不知道为什么被停了，无法换方案或告知用户

## 问题

`ConsecutiveFailureRecovery` 在同一工具连续失败 N 次后，直接返回 `ToolErrorAction::Stop` 结束 run：

- **LLM 不知道为什么被停了** — 没有机会换方案或告知用户
- **TUI 没有提示** — 用户看不到是工具失败导致的结束，以为任务正常完成
- **没有错误历史** — LLM 每次只看到当前错误，看不到"已经连续失败了 N 次"

## 现状

```
工具失败 1..N 次 → Retry（推当前错误给 LLM，继续）
工具失败第 N 次 → Stop（直接结束 run，LLM 不知道发生了什么）
```

## 实际案例

Session `20260824_c18888ee` 中，write_file 连续失败 3 次触发 ConsecutiveFailureRecovery：

- **LLM 行为**：生成了 `write_file` 工具调用，但参数为空 `{}`
- **错误消息**：`"Tool 'write_file' argument parsing failed: "` — 冒号后面是空的
- **根本原因**：`tool_engine.rs` 的 serde 错误没有捕获实际报错信息，`args_str` 是 `"{}"`，解析失败但不知道缺哪些字段
- **LLM 无法自愈**：Stop 后 LLM 不知道发生了什么，无法换方案或告知用户

**额外问题**：错误消息质量差，即使告诉 LLM 错误历史，它也看不到具体原因。需要同时修复错误捕获。

## 涉及代码

| 文件 | 说明 |
|------|------|
| `agent-base/src/engine/recovery.rs` | `ConsecutiveFailureRecovery`，计数和 `Stop` 决策 |
| `agent-base/src/engine/runtime/react_loop/tools.rs` | `ToolErrorAction` 分发：`Stop`/`Retry` 处理逻辑 |
| `agent-base/src/engine/runtime/tool_engine.rs:357` | 工具参数解析，serde 错误捕获（丢失实际报错） |
| `agent-base/src/engine/runtime/event.rs` | `RuntimeEvent` 定义（`UserEvent` 复用） |
| `agent-base/src/types/error.rs:26` | `ToolArgsInvalid` 错误类型定义 |

## 方案

```
工具失败 1..N 次 → Retry（推当前错误给 LLM，继续）
工具连续失败第 N 次 → RetryWithHistory（推完整错误栈给 LLM + 通知 TUI）
                      ↓
                      LLM 评估错误，决定：
                      ├─ 换方案继续（换路径、换写法等）
                      └─ 停下告诉用户失败原因
                      ↓
同一工具再次连续失败 N 次 → Stop（真正停掉 + 通知 TUI）
```

**核心思路**：LLM 是第二道决策者。我们不硬停，而是把错误信息给 LLM，让它评估后决定继续还是告知用户。`RetryWithHistory` 只触发一次。

## 实现细节

### 1. ToolErrorAction 新增变体

```rust
pub enum ToolErrorAction {
    /// 立即停止 run
    Stop,
    /// 推当前错误给 LLM，继续（已有）
    Retry,
    /// 推完整错误历史给 LLM，让它评估（新增）
    RetryWithHistory {
        errors: Vec<String>,
        max_retries: usize,
    },
}
```

`max_retries` 语义：表示这是第几轮 grace（目前固定为 1，即只给一次机会）。

### 2. ConsecutiveFailureRecovery 改动

```rust
pub struct ConsecutiveFailureRecovery {
    max_consecutive_failures: usize,
    max_error_stack_size: usize,         // 错误栈最大字符数（新增）
    failure_counts: Mutex<HashMap<u64, HashMap<String, usize>>>,
    error_messages: Mutex<HashMap<u64, HashMap<String, Vec<String>>>>,  // 新增
    grace_used: Mutex<HashMap<u64, HashMap<String, bool>>>,             // 新增
}
```

`on_error` 逻辑：
1. 记录错误消息（截断到 `max_error_stack_size`）
2. 连续失败 < N 次 → `Retry`
3. 连续失败 = N 次 且 grace 未用 → `RetryWithHistory`，标记 `grace_used = true`
4. 连续失败 = N 次 且 grace 已用 → `Stop`

### 3. tools.rs 处理 RetryWithHistory

```rust
ToolErrorAction::RetryWithHistory { errors, max_retries: _ } => {
    // 1. 构建错误历史消息
    let error_history = format!(
        "The tool `{tool_name}` has failed {} consecutive times. Error history:\n\n{}",
        errors.len(),
        errors.iter().enumerate()
            .map(|(i, e)| format!("{}. {}", i + 1, e))
            .collect::<Vec<_>>()
            .join("\n\n"),
    );

    // 2. 发 UserEvent 通知 TUI
    runtime_context.emit_event(RuntimeEvent::UserEvent(UserEvent {
        content: format!("⚠️ {tool_name} 连续失败 {} 次，已将错误历史发给 LLM", errors.len()),
        attachments: None,
    })).await;

    // 3. 推错误历史到 session（作为 User 消息）
    if let Some(session) = ctx.session.upgrade() {
        let mut session = session.lock().await;
        session.push_message(Message::user(&error_history));
    }

    // 4. 恢复 dangling tool calls
    // ...（和现有 Retry 逻辑相同）

    // 5. 继续 turn
    Ok(TurnFlow::Continue)
}
```

### 4. tools.rs 处理 Stop（补充通知）

现有 Stop 逻辑不变，但增加 `UserEvent` 通知：

```rust
ToolErrorAction::Stop => {
    // 现有逻辑...

    // 补充：发 UserEvent
    runtime_context.emit_event(RuntimeEvent::UserEvent(UserEvent {
        content: format!("❌ {tool_name} 连续失败已停止，请检查工具执行环境"),
        attachments: None,
    })).await;

    // ... 继续现有逻辑
}
```

### 5. 默认值

```rust
impl Default for ConsecutiveFailureRecovery {
    fn default() -> Self {
        Self {
            max_consecutive_failures: 3,
            max_error_stack_size: 2000,  // 错误栈最多 2000 字符
            failure_counts: Mutex::new(HashMap::new()),
            error_messages: Mutex::new(HashMap::new()),
            grace_used: Mutex::new(HashMap::new()),
        }
    }
}
```

## 设计决策

| 决策点 | 选择 | 理由 |
|--------|------|------|
| RetryWithHistory 触发次数 | 1 次 | LLM 评估后要么成功要么告知用户，不需要多轮 |
| 错误栈大小限制 | 2000 字符 | 防止过长错误消息消耗 token |
| TUI 通知方式 | UserEvent | 复用现有事件机制，TUI 自行决定显示方式 |
| LLM 消息类型 | User 消息 | 模拟"用户告知 LLM 工具失败"的语义 |
| 错误计数重置 | RetryWithHistory 成功后重置 | LLM 换方案成功，计数从 0 开始 |

## 需要修改的文件

| 文件 | 改动 |
|------|------|
| `agent-base/src/engine/recovery.rs` | 新增 `RetryWithHistory` 变体，改 `ConsecutiveFailureRecovery` |
| `agent-base/src/engine/runtime/react_loop/tools.rs` | 处理 `RetryWithHistory`，补充 Stop 通知 |
| `agent-base/src/engine/runtime/tool_engine.rs:357` | 捕获 serde 错误详情（附录） |
| `agent-base/src/engine/runtime/event.rs` | 无需改动（复用 `UserEvent`） |

## 附录：错误消息质量修复

### 问题

`agent-base-0.1.14/src/engine/runtime/tool_engine.rs:357` 的错误捕获丢失了 serde 的实际报错：

```rust
// 现在：raw 是 args_str 本身，如果是 "{}" 则错误消息没有有用信息
serde_json::from_str(args_str).map_err(|_| AgentError::ToolArgsInvalid {
    name: name.clone(),
    raw: args_str.clone(),
})?;
```

输出：`Tool 'write_file' argument parsing failed: ` （空的）

### 修复

```rust
// 改成：捕获 serde 错误详情
serde_json::from_str(args_str).map_err(|e| AgentError::ToolArgsInvalid {
    name: name.clone(),
    raw: format!("{} (args: {})", e, args_str),
})?;
```

输出：`Tool 'write_file' argument parsing failed: missing field 'path' at line 1 column 2 (args: {})`

### 影响

- 方案 B 的错误历史质量直接受益 — LLM 能看到具体缺哪些字段
- Retry 现有的单次错误推送也受益
- 不影响其他逻辑，只改错误消息格式

### 修改文件

| 文件 | 改动 |
|------|------|
| `agent-base-0.1.14/src/engine/runtime/tool_engine.rs:357` | 捕获 serde 错误详情到 `raw` 字段 |

## 测试场景

1. write_file 连续失败 3 次 → LLM 收到错误栈（含具体原因）→ 换方案成功 → 正常完成
2. write_file 连续失败 3 次 → LLM 收到错误栈 → 换方案仍然失败 → Stop + TUI 通知
3. read_file 失败 2 次后成功 → 计数重置 → 不触发 RetryWithHistory
4. 不同工具独立计数：write_file 失败 3 次不影响 read_file 的计数
5. 空参数场景：错误消息包含 `missing field 'path'` 而非空消息
