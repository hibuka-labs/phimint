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

每个子 agent 完成 → 通知主 agent 一次 → 主 agent 根据情况决定"继续等"还是"开始干活"。

两个注入点，取决于主 agent 当前状态：

- **主 agent 在跑**（内层 loop 中）→ watcher push 到 follow-up queue → 内层 loop 结束后自动 drain
- **主 agent 空闲**（等用户输入）→ watcher 通知 TUI 事件循环 → 构造 synthetic message 喂给主 agent → 触发新 run

### 数据流

```
时间线 ──────────────────────────────────────────────────────────►

主 agent                           子 agent A           子 agent B         Watcher task
(内层 loop 运行中)                                              (后台并发运行)
    │                                 │                      │
    │  ┌─ LLM 调用 ─┐                │                      │
    │  │            │                │                      │
    │  └────────────┘                │                      │
    │                                 │                      │
    │  ┌─ 执行工具 ─┐          完成!  │                      │
    │  │            │          post_result()                 │
    │  └────────────┘                │ ── bump seq ───►     │
    │                                 │             seq.changed() 醒了
    │  ┌─ LLM 调用 ─┐                │                      │
    │  │            │                │           try_recv_any() → 取一个
    │  └────────────┘                │                      │
    │                                 │           push 到 follow-up queue
    │  ┌─ 执行工具 ─┐                │                      │
    │  │            │                │                      │
    │  └────────────┘                │                      │
    │                                 │                      │
    │  ┌─ LLM 回复文本 ─┐            │                完成!  │
    │  │  → Done       │            │          post_result() │
    │  └───────────────┘            │                ── bump seq ──►
    │                                 │                      │       ...
    ╔═══════════════════════════════════════════════════════════════════╗
    ║  内层 loop 结束                                                   ║
    ╚═══════════════════════════════════════════════════════════════════╝
    │                                 │                      │
    │  run_managed() drain            │                      │
    │  follow-up queue ◄── 子 agent A 结果 ──────────────────┘
    │                                 │
    │  有 follow-up → 开新内层 loop    │
    │                                 │
    │  LLM 看到 A 的结果               │
    │  "还差 B，继续等"                │
    │  ... 或者继续执行工具             │
    │                                 │
    │  LLM 回复文本 → Done             │
    │                                 │
    ╔═══════════════════════════════════════════════════════════════════╗
    ║  内层 loop 结束，run_managed() 返回，主 agent 空闲               ║
    ╚═══════════════════════════════════════════════════════════════════╝
    │                                 │                      │
    │  TUI 事件循环 ←── child_result_rx ◄── B 的结果到达 ────┘
    │                                 │
    │  构造 synthetic message          │
    │  触发新一轮 run_managed()        │
    │                                 │
    │  LLM 看到 B 的结果               │
    │  "所有子 agent 都完成了，继续"    │
```

### 与现有基础设施的关系

| 组件 | 状态 | 说明 |
|------|------|------|
| `MailboxHub::seq` (watch channel) | **已有** | `post_result` 已 bump seq，`wait_for_result` 靠 `seq.changed()` 唤醒 |
| `run_managed()` follow-up queue | **已有** | 内部 loop 结束后自动 drain，不需要改 turn loop |
| `try_recv_any()` | **已有** | 非阻塞获取任意 pending result |
| `extract_last_assistant_message` | **已有** | 子 agent 结果精简，只返回最终回复 |
| TUI 事件循环 | **已完成** | `child_result_rx` 分支 + 通知显示（idle: system message, running: notice） |
| Watcher task | **已完成** | `runtime/watcher.rs`，监听 seq 变化，双通道投递 |
| Prompt 改造 | **已完成** | 告诉主 agent "结果会自动到达" |

### 需要改动的组件

#### 1. Watcher Task（agent-works，新增）

在 `MultiAgentRuntime` 启动时（或首次 spawn 时）启动一个后台 task：

```rust
// 伪代码
async fn child_result_watcher(
    mailbox: Arc<MailboxHub>,
    follow_up_tx: FollowUpSender,     // run_managed 的 follow-up queue
    child_result_tx: Sender<MailboxResult>,  // 通知 TUI（agent 空闲时）
    mut seq_rx: watch::Receiver<u64>,
) {
    loop {
        // 等待序列号变化（已有信号）
        if seq_rx.changed().await.is_err() { break; }

        // 每次只取一个结果（逐个通知）
        let Some(result) = mailbox.try_recv_any() else { continue };

        // 注入 follow-up queue（如果主 agent 在跑，loop 结束后 drain）
        let message = format_child_result(&result);
        follow_up_tx.send(message).await;

        // 通知 TUI 事件循环（如果主 agent 空闲，触发新 run）
        let _ = child_result_tx.send(result).await;
    }
}
```

关键设计：
- **逐个通知**：每次 seq 变化只取一个结果，主 agent 的 LLM 推理决定"继续等"还是"开始干活"
- **双通道投递**：follow-up queue（agent 在跑时）+ TUI channel（agent 空闲时），两者互补
- **复用已有信号**：不新建通知通道，直接用 `MailboxHub::seq`

#### 2. TUI 事件循环改造（phimint）

在 TUI 的事件循环中新增一个分支，处理 agent 空闲时的子 agent 结果到达：

```rust
// TUI 事件循环（简化）
loop {
    tokio::select! {
        // 用户输入
        input = user_input_rx.recv() => {
            agent.run_managed(input).await;
        }

        // 子 agent 结果到达（agent 空闲时）
        child_result = child_result_rx.recv() => {
            // 构造 synthetic message
            let msg = format!("[子 agent {} 已完成]\n{}", child_result.path, child_result.text);
            // 触发新一轮 run_managed
            agent.run_managed(msg).await;
        }
    }
}
```

这样就覆盖了两种情况：
- **agent 在跑**：follow-up queue 在内层 loop 结束后自动 drain（不需要 TUI 介入）
- **agent 空闲**：TUI select 分支捕获，构造 synthetic message 触发新 run

#### 3. Prompt 改造（phimint，Phase 1 优先）

修改 `phimint/src/prompt/mod.rs` 中的 MULTI_AGENT prompt：

- 告诉主 agent："子 agent 结果会自动到达你的上下文，不需要调用 `wait_agent` 等待"
- "`wait_agent` 仅用于需要立即查看子 agent 完整输出的场景"
- "spawn 多个子 agent 时不需要逐个 wait，结果到了会通知你"
- 主 agent 收到一个子 agent 结果后，自己判断"还差几个，继续等"还是"够了，开始干活"

#### 4. wait_agent 改造（phi-kernel-tools）

保留阻塞模式（兼容），新增非阻塞查询：

```rust
enum WaitMode {
    Blocking { timeout_ms: u64 },     // 当前行为不变
    NonBlocking,                       // 只检查是否已有结果
}
```

非阻塞模式直接调用 `mailbox.try_recv_result(path)`，无结果返回 "pending"。

### 与 codex 的差异

| 维度 | codex | phi |
|------|-------|-----|
| 线程模型 | 每个 agent 独立 thread | 父子共享 runtime |
| 输入队列 | `input_queue` + `watch channel` | mailbox `watch::channel<u64>` + follow-up queue |
| 触发机制 | `trigger_turn` 标记 | `seq.changed()` → watcher → 双通道投递 |
| agent 在跑时 | 注入 input_queue | push 到 follow-up queue（内层 loop 结束 drain） |
| agent 空闲时 | `maybe_start_turn_for_pending_work()` | TUI 事件循环 select，构造 synthetic message |
| 通知粒度 | 每个 mailbox 消息一次 | 每个子 agent 完成一次 |

### 待定问题（已解决）

**Q1: 主 agent 正在执行工具时，子 agent 结果到达怎么办？**

不需要特殊处理。`post_result()` 只是往 `Vec<MailboxResult>` push 数据 + bump seq。主 agent 当前工具执行完 → 内层 loop 结束 → `run_managed()` drain follow-up queue → 拿到子 agent 结果 → 开新 inner loop 处理。这是 mailbox + follow-up queue 的自然行为。

**Q2: 多个子 agent 同时完成，逐个注入还是批量注入？**

**逐个注入**。每个子 agent 完成触发一次通知，主 agent 的 LLM 推理自主决定：
- "还差 2 个结果，继续执行其他任务"或"原地等待"
- "所有子 agent 都完成了，开始汇总"

理由：让 LLM 做决策比硬编码批量策略更灵活，实现也更简单（watcher 每次只取一个）。

> **⚠️ Phase 7 已推翻此策略**：逐个唤醒让父 agent 在"还差一个"的状态下反复醒来，每个中间醒来点都是一次偏离机会（session 20260903_0cf95e79 即在第一个结果到达后自己干了 30+ 次读操作）。fan-in 设计改为"全部返回才唤醒一次"，见 Phase 7。

**Q3: 子 agent 结果太长（超过 context window）怎么处理？**

复用现有 `max_tool_output_chars`（16,000）限制。`extract_last_assistant_message` 已做精简，超过限制的截断 + 提示"结果已截断，完整输出请用 `wait_agent` 查看"。

**Q4: 要不要在 prompt 里告诉主 agent？**

**要，且是 Phase 1 优先项**。不改 prompt 的话，主 agent 仍然会 spawn + wait_agent，形成双重等待。改造内容：
- "子 agent 结果会自动到达你的上下文"
- "`wait_agent` 仅用于需要立即查看完整输出的场景"
- "spawn 多个子 agent 时不需要逐个 wait"

### 错误与取消场景

| 场景 | 行为 |
|------|------|
| 子 agent 正常完成 | `post_result(Ok)` → watcher 取一个 → follow-up queue + TUI channel |
| 子 agent 执行出错 | `post_result(Error)` → 同上，主 agent 看到错误信息并决定下一步 |
| 子 agent 被取消 | `ChildCleanup::drop` → `post_result(Closed)` → 同上，主 agent 知道子 agent 已关闭 |
| 子 agent 超时 | `post_result(Error, "timeout")` → 同上 |
| 主 agent 被取消 | `root_cancel` 级联取消所有子 agent（已有机制） |
| watcher task panic | 需要 watchdog 重启，或 fallback 到 `wait_agent` 阻塞模式 |

## 实现步骤

### Phase 1：Prompt 改造 + 非阻塞 wait_agent ✅（2026-09-03）

**目标**：让主 agent 知道结果会自动到达，减少不必要的阻塞等待。

- ✅ 修改 `phimint/src/prompt/mod.rs` MULTI_AGENT prompt：告知结果自动到达、wait_agent 仅用于调试
- ✅ `wait_agent` 新增 `blocking` 参数（默认 `true` 兼容），`blocking=false` 时调用 `try_wait` 立即返回
- ✅ `agent-works` 新增 `MultiAgentRuntime::try_wait()` 非阻塞方法
- ✅ 新增 4 个 lifecycle 测试覆盖 try_wait 各路径
- 修改文件：`phi-kernel-tools/src/multi_agent/wait_agent.rs`、`agent-works/src/multi_agent/runtime.rs`、`agent-works/src/multi_agent/runtime/tests/lifecycle.rs`、`phimint/src/prompt/mod.rs`

### Phase 2：Watcher Task + 双通道注入 ✅（2026-09-03）

**目标**：核心自动推送机制。

- ✅ 新增 `agent-works/src/multi_agent/runtime/watcher.rs`：watcher task 监听 `mailbox.seq_rx.changed()`，每次取一个结果，双通道投递
- ✅ 新增 `ChildResultEvent` 结构体（agent_path、status、result、message）
- ✅ 新增 `format_child_result()` 将 `MailboxResult` 格式化为事件
- ✅ `MultiAgentRuntime::start_watcher()` 启动 watcher，返回两个接收通道
- ✅ `AgentBuilder::build_with_ma()` 返回 `MultiAgentRuntime` 供外部访问
- ✅ `PhiAgent` 存储并暴露 `multi_agent_runtime()`
- ✅ phimint TUI 接入：agent idle 时 child_result_rx 触发 synthetic run
- ✅ watcher 支持 `CancellationToken` 优雅退出
- ✅ 新增 7 个测试（watcher 3 + format_child_result 4）
- 修改文件：`agent-works/src/multi_agent/runtime/watcher.rs`（新增）、`agent-works/src/multi_agent/runtime/outcome.rs`、`agent-works/src/multi_agent/runtime.rs`、`agent-works/src/builder.rs`、`phi-agent/src/agent/factory.rs`、`phi-agent/src/lib.rs`、`phimint/src/ui/mod.rs`

### Phase 3：TUI 事件循环适配 ✅（2026-09-03）

**目标**：agent 空闲时能被子 agent 结果唤醒，用户能看到通知。

- ✅ agent idle 时：`push_system("子 agent {name} 已完成，结果已注入上下文")` + `Cmd::Run`
- ✅ agent running 时：`set_notice("子 agent {name} 已完成")` 状态栏通知，不打断用户输入
- ✅ 使用 `agent_path.rsplit('/')` 提取短名称，避免显示完整路径
- 修改文件：`phimint/src/ui/mod.rs`

### Phase 4：健壮性 + 兼容性收尾 ✅（2026-09-03）

- ✅ watcher task panic 恢复：`spawn_watcher_with_watchdog()` 自动重启 panicked watcher（100ms backoff）
- ✅ `wait_agent` 阻塞/非阻塞模式完整测试：新增 7 个测试（try_wait closed、try_wait any、wait_for_result basic/any/timeout/has_more）
- ✅ 多层 agent 推送验证：新增 2 个测试（独立结果投递、并发结果投递）
- 修改文件：`agent-works/src/multi_agent/runtime/watcher.rs`、`agent-works/src/multi_agent/runtime/tests/lifecycle.rs`、`agent-works/src/builder.rs`

### Phase 5：wait_agent 删除 + 运行中投递接通 ✅（2026-09-03）

**背景**：session 20260903_51de29ef 暴露三个问题——(1) mimo 对 wait_agent 参数截断 6/6 次，熔断器拉黑后父 agent "can't wait" 杀掉 4 个运行中子 agent；(2) 运行中收到的结果在 TUI 里被 `try_recv` 消费后丢弃，只弹状态栏通知；(3) watcher 的 `follow_up_rx` 通道在 agent_loop 里从未被读取（`_follow_up_rx` 死参数），结果推送实际上有去无回。双通道（wait + push）并存还导致模型在收不到结果时回退到 wait_agent，正好掉进截断陷阱。

**决策**：删除 LLM 侧 wait_agent 工具（推送是唯一结果通道；父 agent "等待" = 结束回合，零 LLM 输出、零截断风险）。保留 agent-works 内部 `wait_for_result`/`try_wait` API（测试与运行时内部使用）。

- ✅ 删除 `phi-kernel-tools/src/multi_agent/wait_agent.rs` 工具，create_all_tools 5→4
- ✅ watcher 简化单通道：删除从未接线的 `follow_up_tx`（含 `start_watcher` 返回值 3→2）
- ✅ TUI 运行中投递：agent running 时结果 stash 到 `pending_child_results` + 状态栏通知；turn 结束（TurnDone/TurnError 后 running=false）→ 批量注入一次 `Cmd::Run`
- ✅ `list_agents` 输出补 `task` 字段（registry 记录首次 send_task 的任务描述），支持用户主动问"某任务什么状态"
- ✅ 挂死兜底：启用 `ControlConfig.task_timeout`（phimint 配置 10 分钟）——超时硬停子任务 + Error 结果走推送管道唤醒父 agent（机制 §9.2 已有，此步只是配置）
- ✅ 框架层 `build_multi_agent_system_prompt`（agent-works）与 phimint MULTI_AGENT prompt 同步改为推送语义："To wait, simply end your turn"
- 修改文件：`phi-kernel-tools/src/multi_agent/{mod.rs,wait_agent.rs(删),list_agents.rs}`、`agent-works/src/multi_agent/{runtime.rs,runtime/watcher.rs,runtime/registry.rs,child_builder.rs,builder.rs}`、`phi-agent/src/lib.rs`、`phimint/src/{ui/mod.rs,agent.rs,prompt/mod.rs}`

### Phase 6：首次端到端验证 + 两处框架修复 ✅（2026-09-03）

**背景**：session 20260903_2438d139 是 Phase 5 后的首次真实多 agent 会话（4 子 agent 分析 4 个工程）。验证结果：spawn 4/4 成功且参数无截断；"结束回合即等待"生效（全程零 wait_agent）；推送 3/4 到达（idle 即时注入 + busy stash→turn 末批量注入均工作）；list_agents task 字段正常。**但 pi 的报告彻底丢失**，且父 agent 误判子 agent 卡死后自己干了活并 close ×4。

**测试驱动定位**：按"先写必失败单测钉死问题，再修复到绿"流程，4 个新测试在修复前 4/4 失败，修复后全绿。

- ✅ **P1 状态失真**：子 agent 交付结果后 registry 状态停在 Running（registry 文档声明 `set_status(Done) on completion` 但运行时从未实现）→ list_agents 撒谎，父 agent 看到"刚交付结果的 agent 仍 running"，结合 2 分钟无新推送，误判"卡死"。修复：`run_child_loop` 在 post_result(Ok/Error) 后 `set_status(Done)`（新任务由 send_task 重新置 Running，状态机闭合 Idle→Running→Done→Running）。测试：`test_list_agents_shows_done_after_result_delivered`
- ✅ **P2 cleanup 丢结果**：`ChildCleanup::drop` 的 `post_result(Closed)` → `unregister()` 在同一同步 poll 内（无 yield 点），watcher 无法在间隙被调度，而 `unregister` 会**丢弃排队结果** → Closed 通知 4/4 结构性丢失；close 后才完成的子任务 Ok 结果同样可能被吃（pi 即如此——close 不打断 in-flight 任务，任务完成后 Ok+Closed 一起被丢弃）。修复：mailbox 墓碑机制——`unregister` 把排队结果搬进 `tombstones`（不丢弃），`try_recv_any`/`try_recv_result`/`has_results`/`total_pending_results` 连墓碑一起读（锁序统一 entries→tombstones）。测试：`unregister_preserves_queued_results`、`test_close_idle_child_delivers_closed_event`、`test_close_running_child_result_still_delivered`
- ⚠️ **P3 行为问题（未修，观察项）**：父 agent 收到第一个结果后未结束回合，自己做了 30+ 次读操作分析三个仓库，然后 close 掉 3 个运行中的 agent。prompt 的 "Keep working on other things instead of waiting on them" 可能被解读为"自己去干分析的活"。P1 修复后 list_agents 如实显示 done/running，误判原料已消除；如复发，考虑加硬规则："仍有子 agent 运行时收到结果→只记录并结束回合；禁止代替子 agent 干活；禁止主动 close 运行中的 agent"
- 修改文件：`agent-works/src/multi_agent/mailbox.rs`（墓碑）、`agent-works/src/multi_agent/runtime/spawn.rs`（set_status(Done) + registry 参数）、`agent-works/src/multi_agent/runtime/tests.rs`（DelayedStub + make_ma_runtime_with）、`agent-works/src/multi_agent/runtime/tests/lifecycle.rs`（3 个回归测试）

#### Phase 6 收尾：TUI 投递决策解耦为可单测的 router ✅（2026-09-03）

Phase 6 复盘（P1-P4 分级）指出：TUI loop 里的投递决策（idle 即时注入 / busy stash→turn 末批量注入）是内联代码、零测试覆盖——Phase 2-4 的"死通道" bug（follow_up_rx 从未被读取）正是藏在这类不可测的内联循环里。此步把决策逻辑抽出为纯结构：

- ✅ 新增 `phimint/src/ui/child_results.rs`：`ChildResultRouter`（`on_result(agent_running, event) -> Hold | Inject` + `flush_when_idle() -> Option<Inject>`），通知文案与原内联代码逐字一致
- ✅ 新增 `phimint/src/ui/child_results_tests.rs`：6 个单测（hold→flush、idle 即时注入、批量保序合并、短名称、空 flush no-op、跨回合混合）
- ✅ `phimint/src/ui/mod.rs`：run_tui 循环只执行副作用（`set_notice`/`push_system`/`Cmd::Run`），时序策略全部在 router 内
- 测试：phimint 267 → 273 全绿

### Phase 7：fan-in 重设计——全部返回才唤醒一次 ✅（2026-09-03）

**背景**：session 20260903_0cf95e79（3 子 agent 分析 4 工程）失败根因：第一个结果到达后父 agent 没有结束回合，自己做了 30+ 次读操作分析三个仓库；guard 的静态 tool_count（spawn 时的 inventory 快照）把父 agent 的活跃干活误判为"子 agent 卡死"；guard `judge_fail_open=false` 又把父 agent 的合法回合结束拦下。逐个推送 + 状态撒谎 + 静态库存三类问题叠加。

**决策（用户批准）**：watcher 升级为 **fan-in 协调器**——

- **Progress**：单个子 agent 返回 → 只发用户可见进展通知（Focus 摘要，仅给用户看；**绝不唤醒父 agent**，父 agent 上下文零污染）
- **Batch**：**全部**子 agent 都返回（成功或失败都算）→ 才发一次批次事件唤醒父 agent，附带所有完整报告
- **完整报告 = 最终结论**：`extract_last_assistant_message`，只含最终结论，不含子 agent 的中间过程
- Focus 不做压缩：摘要只服务用户；Focus 失败 → 纯状态词降级（"子 agent X 已完成"）
- 挂死兜底：`task_timeout`（10 分钟）；heartbeat reaper 推迟（等 activity 数据积累后观察再定）

**两条时序不变式**（fan-in 正确性的基石）：

1. **Done-before-post**：子 loop 先 `set_status(Done)` 再 `post_result`。裸 `set_status` 不 bump seq，反过来的顺序会让 watcher 拿着最后一批结果却看到 producer 仍 Running → 批次永远凑不齐 → 死锁。由此确立守恒律："结果已被取走 ⇒ producer 已 Done"
2. **unregister bump seq**：close 路径（`ChildCleanup::drop` → post Closed → registry.close → unregister）全程同一同步块，unregister bump seq 保证 watcher 能被唤醒读取（P2 墓碑机制保证结果不丢）

**批次判定**（静止规则）：`registry.running_count() == 0 && batch 至少含一个非 Closed` → 发 Batch。冗余 Closed（agent 已有 Ok/Error 在批次里）去重；纯 Closed 批次（close 空闲 agent）不唤醒父 agent 且清空批次，避免泄漏进下一代 spawn。

- ✅ **P-A registry 真数据**：`tool_calls`（事件桥 `ToolCallStarted` → `record_tool_call`，单调递增）+ `last_activity`（`send_task` → `touch`）替换静态 tool_count 库存；新增 `running_count()` 直通
- ✅ **P-B watcher fan-in 协调器**：重写 `runtime/watcher.rs`，`ChildResultEvent::{Progress, Batch}`；批次聚合 + 静止判定 + 去重，12 个测试
- ✅ **P-C Focus 摘要**：新增 `focus/progress.rs` `ProgressSummarizer`（中文一句话播报 ≤60 字，只报结论；1200 字尾部保留截断；超时/LLM 错误/解析失败一律降级 None）；`Focus` derive Clone；`multi_agent = ["focus"]` 特性统一
- ✅ **P-D phimint 路由适配**：`ChildResultRoute::{Notice, Hold, Inject}` 三态——Progress → Notice（仅转录显示）；Batch + running → Hold（turn 末 flush）；Batch + idle → Inject（立即合成 run）；router 9 个单测
- ✅ **P-E 文档 + 记忆**：本节 + 记忆更新
- 测试：agent-works 389 lib + 21 integration + 4 stress；phi-kernel-tools 40+5；phi-agent 220（顺带修复陈旧的 wait_agent 断言——Phase 5 删了工具但测试没跟上）；phimint 276
- 修改文件：`agent-works/src/multi_agent/{registry.rs,runtime.rs,runtime/spawn.rs,runtime/watcher.rs(重写),runtime/outcome.rs,runtime/tests/*,mod.rs}`、`agent-works/src/focus/{mod.rs,progress.rs(新增),core.rs}`、`agent-works/Cargo.toml`、`agent-works/tests/multi_agent_stress.rs`、`phi-kernel-tools/src/multi_agent/{list_agents.rs,spawn_agent.rs,mod.rs}`、`phi-agent/src/lib.rs`、`phi-agent/src/agent/builder.rs`、`phimint/src/ui/{child_results.rs(重写),child_results_tests.rs(重写),mod.rs}`

### Phase 8：9255c25e 实战复盘修复——机制对但旅程崩 ✅（2026-09-03）

**背景**：session 20260903_9255c25e（4 子 agent 分析 4 工程）。fan-in 机制本身**工作正常**（4/4 报告 2204 行 89KB 一次批次注入，turn 2 零轮询合成），但过程全面劣化：父 agent 轮询 `list_agents` 65 次（每次 = 完整 LLM call + 4 条完整任务文本 ≈ 2.5KB）→ 19 连发轮询 → 误判子 agent 卡死 → close ×4（previous_status 全是 running，强杀在飞任务）→ 自己做 132 次 read_file 分析 → turn 1 无报告作答 → 子 agent 在飞完成后批次注入成 turn 2 → 连续两次回答，UX 混乱。Focus 摘要 4/4 全部超时降级。

根因四件套，逐一修复：

1. **prompt 接线不统一（最关键）**：prompt 文本存在两份——`phimint/src/agent.rs` 内联 `SYSTEM_PROMPT`（**活的**，模型实际看到的）和 `src/prompt/mod.rs`（**死代码**，从未被 import）。Phase 1 改的是死的那份，活的那份还带着 fan-in 前语义 "wait for them before yielding" → 模型被指示等待 → 没有 wait 工具 → 只能轮询。
   - **修复**：活 prompt 改写为 fan-in 语义（"Results are pushed to you automatically" / "To wait, simply end your turn" / "Do NOT loop `list_agents`"）；**删除死模块** `src/prompt/mod.rs`（单一事实源）；`main.rs` 去掉 `mod prompt;`
   - **防复发**：`agent.rs` 新增 `prompt_guard_tests`——断言活文本包含关键 fan-in 短语、不包含 "wait for them before yielding"。字符串常量无法测接线，但能测语义漂移；这是 string prompt 可测性的极限
2. **list_agents 瘦身**（用户硬要求"必须廋身"）：`task` 字段从完整任务文本改为首行 60 字符截断 + `…`（`task_excerpt`）。任务全文是 spawn 者自己的输入，每次轮询重发纯属燃烧 token（该 session 65×4 全文 ≈ 160KB）。description 同步改为"spot-check, never loop"。+3 单测
3. **Focus 摘要超时 10s → 30s + 降级可观测**：摘要与运行时共享 client，推理模型一句中文 >10s 很正常；4/4 降级但无任何日志。`DEFAULT_SUMMARY_TIMEOUT` 提到 30s；静默 `.ok()?` 改为 `tracing::warn!`（失败与空摘要两个分支），降级不再静默
4. **TUI 等待态（用户要求"不能显示 done"）**：`AgentStatus::Waiting { running: usize }` 新状态——回合结束但仍有子 agent 运行时：不显示 ✅ done、不清任务面板，显示 "⏳ 等待子 agent 返回（N 个运行中）"；Progress 事件翻转面板条目（`mark_sub_agent_finished`）并刷新计数；批次注入时 `mark_all_sub_agents_finished` 全部翻绿。`settle_after_turn` 统一 TurnDone/TurnError/RunFinished 三条路径的结算逻辑。+5 UI 测试
   - 顺带：**guard 接 LLM client + judge_fail_open=true**（0cf95e79 遗留项，Phase 7 背景——`DefaultGuard::new()` 无 client 时 LLM judge 永远无法裁决，fail-closed 会拦截一切合法回合结束包括"结束回合以等待"；fail-open 让个别未验证回答通过，下一回合自纠，代价远小于错误拦截）

**验证**：phimint 279；agent-works 389 lib + 21 + 4 + 5；phi-kernel-tools 20+5；release 二进制重建。

**修改文件**：`phimint/src/{agent.rs,main.rs}`（prompt 统一 + guard 接线 + 守卫测试）、`phimint/src/prompt/mod.rs`（删除）、`phimint/src/ui/{app.rs,render.rs,handlers/runtime.rs,mod.rs,app_tests.rs,task_panel_tests.rs}`（等待态）、`phi-kernel-tools/src/multi_agent/list_agents.rs`（瘦身）、`agent-works/src/focus/progress.rs`（超时+日志）。

**遗留**：`--features file/full` 的 `Content::detail` 编译断（历史遗留，phimint 不用，下轮清理）；heartbeat reaper 继续推迟观察；硬规则 prompt 行（P3）暂不加——fan-in + prompt 修复应消除轮询动机，复发再加。

### Phase 9：d8fc41dc 实战验证通过 + 两个 Focus 路径修复 ✅（2026-09-03）

**验证结果（4 子 agent 分析 4 工程，mimo-v2.5-pro）**：Phase 8 的全部修复实战生效——list_agents **0 次**轮询（上轮 65）、close_agent **0 次**强杀、read_file 洪流 **0 次**；全程 8 次 LLM call；父 agent 发进度说明后结束回合，TUI 显示 `⏳ 等待子 agent 返回（N 个运行中）` 且计数随完成递减；4/4 报告（76,886 字符）一次批次注入，单次合成 7,673 字符；总 input 27K tokens。fan-in 机制端到端成立。

**新暴露的两个问题 + 修复**：

1. **spawn 路径的 `expand_task_via_focus`（独立 10s 超时，Phase 8 漏改的第二个 Focus 调用点）**：4/4 超时降级 fallback，且在父 agent tool call 里同步阻塞，4×10s 纯死等。**修复：整段砍掉**——spawn 不再做任何 LLM 调用，子 agent 用静态 `CHILD_SYSTEM_PROMPT`（自主规划 + 最终消息即交付物）+ 完整任务文本；工具描述从"task 保持一句话"**反转为"task 必须完整自包含"**（原指引只为配合 Focus 扩写）；`DESCRIPTION` 提为 const + 守卫测试防语义漂移；kernel-tools 摘掉 `focus` feature 依赖。依据：本次 4 个子 agent 全在 fallback 模板上产出高质量报告——能干的子 agent 只需要角色说明 + 完整任务
2. **watcher 摘要串行阻塞批次唤醒（违反"Progress 绝不阻塞"设计意图）**：`summarize().await` 在 watcher 循环里同步执行，每个摘要的超时全额加在 `batch.push` 和静止判定之前（本次最后一个子 agent 返回后父 agent 干等 21s；4 个挤在一起完成则最坏 N×30s 排队）。**修复：Ok/Error 摘要改为 `tokio::spawn` detached**，结果立即入批次、批次判定照常；Closed 仍同步发（无 LLM 调用）。语义后果：Progress 事件可能晚于 Batch 到达，消费方必须幂等（phimint UI 的 mark-finished 对已完成条目是 no-op，天然满足）。mimo 摘要实测 21-30s/句，30s 超时 2/4 成功——解耦后摘要快慢只影响用户看到句子的时刻

**测试**：phi-kernel-tools 22+5（+2 守卫）；agent-works 390 lib（+1 `slow_summary_does_not_delay_batch`：500ms 慢摘要下 Batch 必须在 400ms 内到达）+ 21 + 4 + 5（`test_close_running_child_result_still_delivered` 顺序断言放开为守恒断言）；phimint 279；phi-agent 全绿。release 已重建。

**修改文件**：`phi-kernel-tools/src/multi_agent/spawn_agent.rs`（砍 expand + 静态 prompt + 守卫测试）、`phi-kernel-tools/Cargo.toml`（摘 focus feature）、`agent-works/src/multi_agent/runtime/watcher.rs`（摘要 detached + 模块文档 + 测试）、`agent-works/src/multi_agent/runtime/tests/lifecycle.rs`（守恒断言）。

**下一步**：框架级 agent 生命周期状态机（Running/Waiting{since}/Done/未来 Paused），状态由事实推导（turn 结束 + running_count）而非模型声明，单一事实源广播事件，UI/metrics/guard 全部改消费方。先出设计文档再动工。

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
| `phi-kernel-tools/src/multi_agent/list_agents.rs` | list_agents 工具（task 首行 60 字符截断；wait_agent 删除） |
| `agent-works/src/multi_agent/runtime.rs` | MultiAgentRuntime：mailbox、registry、wait_for_result（内部 API） |
| `agent-works/src/multi_agent/runtime/watcher.rs` | fan-in 协调器（Progress 单发 + Batch 全返才唤醒） |
| `agent-works/src/multi_agent/runtime/outcome.rs` | 子 agent 结果格式化（build_child_result + format_child_result） |
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
