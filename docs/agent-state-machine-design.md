# 子 Agent 状态机设计（状态单一事实源）

> 状态：设计已 review（2026-09-04 晚），可直接实施。源于 session 20260904_c6559510
> 复盘（见 `docs/child-result-push-design.md` Phase 8-9 之后的第四次实战回归）。
> 标记层修复批次已落地（注入限幅 + 出队标 Running），本设计是根治层。
>
> **P0/P1 标签消歧**：本文档说的 P0/P1 指 **c6559510 批次**（bug ② 注入限幅、
> bug ① 出队标 Running）。20260904_efad759c 复盘批次（agent-base 53a9629 事件门、
> c8c75db+37f6b03 guard 判据）与状态机**正交**——guard 判据在回合完成判定层，
> 不读子 agent 状态，不受本设计影响。

## 问题

### 四方各有一个 "Done"

同一个子 agent 的"完成"，在四处代码里有四种互不一致的解释：

| 消费方 | 位置 | "完成"的判据 | 与现实的偏差 |
|--------|------|--------------|--------------|
| agent-works registry | `registry.rs` `AgentStatus::{Idle,Running,Done}` | **存储标记**，由调用方在关键时刻 `set_status` | 标记的语义是"刚发生了什么"，不是"现在是什么状态" |
| agent-works watcher | `runtime/watcher.rs:218` | `registry.running_count() == 0` && batch 非空 | quiescence 判断依赖标记的**及时性**——标记晚一步就是 phantom-idle（bug ① 根因） |
| phimint UI | `ui/app.rs` `SubAgentStatus::{Running,Done}` | 从 Progress/spawn 事件**推断**，与 registry 完全无关 | 面板显示与框架状态可能矛盾（用户看到 done 但 list_agents 说 running） |
| 模型（list_agents） | `phi-kernel-tools/multi_agent/list_agents.rs` | registry 标记的字符串化 | "done" 曾表示"刚交付了结果"而结果还在批次里没投递——**状态撒谎**，喂给了焦虑轮询 |

再加上两套辅助判据，实际是六方：

- metrics：`tool_calls`（单调递增）+ `last_activity`（最后活动时间）——另一个 "stall" 定义；
- phimint 根 agent 状态：`ui/app.rs` `AgentStatus::{Idle, Waiting{running}, Running{phase}}`——`running` 数来自 UI 自己维护的 `sub_agents` 表。

### 20260904_c6559510 里这如何爆发

```
模型行为偏离（52× list_agents + 7× send_message(trigger=true) + 4× 中途 close）
    → 排队出第 2/3 轮任务
    → bug ①：post 标 Done / 出队不标 Running → running_count()==0 而队列非空
    → phantom-idle → 批次提前/碎裂 → 7 报告 53,276 token
    → bug ②：>50,000 安全阀静默 pop → 模型凭空合成
    → 4 个掉队单报告注入 → 2.5 分钟 6 轮回答 = 用户看到的"紊乱"
```

bug ① 的本质：**状态是标记，不是事实**。任何一个调用方忘记 `set_status`，
全局 quiescence 判断就错。P1 修复（spawn.rs 出队前 `try_recv` peek，
有队列任务标 Running、无则标 Done）把正确的转移点补上了，但它仍然是
"调用方负责标状态"——只是把标记打对了，没有消除"忘记打标记"这个故障类别。

## 目标

1. **状态从可观察事实推导**，不靠散落各处的 `set_status` 调用；
2. **单一事实源**在 agent-works `MultiAgentRuntime`，以生命周期事件广播；
3. UI / metrics / guard / list_agents / watcher 全部降级为**消费者**；
4. 为 heartbeat reaper（stall 收割）和 Paused 状态留好自然位置。

## 设计方案

### 核心思路：状态 = 纯函数(事实)

每个 agent 的事实只有两个，由 runtime 的结构位置天然维护：

```
queue_len  : 该 agent 的 mailbox 里还有几个未执行任务
in_flight  : 是否有一个已出队、未交付结果的任务正在执行
```

事实的维护点唯一且无法遗忘：

| 事实变化 | 维护点 | 时机 |
|----------|--------|------|
| `queue_len += 1` | `runtime.send_task` | 入队时（已有，`runtime.rs:269` 附近） |
| `queue_len -= 1` | `run_child_loop` 出队 | `task_rx.recv()` 返回时 |
| `in_flight = true` | 同上 | 同一次出队 |
| `in_flight = false` | `mailbox.post_result` 返回前 | 交付时 |

状态是这两个事实的纯函数：

```rust
fn derive_status(queue_len: usize, in_flight: bool, registered: bool) -> AgentStatus {
    match (registered, in_flight, queue_len) {
        (false, _, _)      => AgentStatus::Closed,          // 已注销
        (_, true, _)       => AgentStatus::Running,          // 有在飞任务（队列还有排队的也一样在跑）
        (_, false, 0)      => AgentStatus::Done,             // 交付完且无排队 —— 真正的 Done
        (_, false, _)      => AgentStatus::Queued,           // 有排队任务但还没开始执行第一个
    }
}
```

注意派生规则消除了旧协议的三个坑：

- **没有 Idle**。旧 `Idle`（"spawn 了但没收到任务"）实际是
  `queue_len==0 && !in_flight` 的一个真子集——和 `Done` 事实相同，纯属两个名字。
  保留 `Idle` 会迫使模型区分"从没干过活"和"干完了"，徒增轮询理由。
- **`send_task` 入队时不再需要标 Running**。排队期是 `Queued`，第一次出队才
  `Running`。"Queued 但从未 Running"和"Running 中又排队了"由事实自然区分。
- **watcher 的 quiescence 基础是事实**，与"谁记得打标记"无关。完整谓词见下节。

### quiescence 谓词：三个事实（review 补强）

只有 `in_flight`/`queue_len` 两个事实，quiescence 有一个结构性盲区：
`spawn_agent` 工具里 `spawn_child_with_history` 和 `send_task` 是**两次 await**，
中间 register 的 seq bump 会唤醒 watcher，此刻新 agent 派生为 Done（无队列、
无在飞）——quiescence 对它成立。窗口内 batch 恰有兄弟结果时，会提前发出
缺人批次的唤醒（c6559510 提前批次的近亲）。今天不炸靠两个偶然：窗口内
batch 通常为空（`batch.is_empty() → continue` 挡住）、父 agent 已结束回合
不会并发 spawn。状态机不靠偶然。

**修正**：quiescence 谓词加第三个事实——**批次交付完备性**：

```rust
// 批次 quiescence = 三条同时成立
//  1. 所有 agent: !in_flight && queue_len == 0        （没人干活）
//  2. 批次内每个成员都交付过 ≥1 次结果                 （人人交齐）
//  3. batch 非空                                       （有东西可唤醒）
```

事实 2 物理上现成：`ChildMailbox` 已有 `has_results` 访问器（`mailbox.rs`），
`has_pending` 即 queue_len>0 的查询。**这才是 fan-in 的真实语义——
"所有人交齐才唤醒"，而不是"没人干活了就唤醒"。** 它同时结构性封死
spawn→send 窗口（新成员没交付过 → 谓词不成立 → 不提前唤醒）。

### 状态定义

```rust
pub enum AgentStatus {
    /// 任务已入队但尚未开始执行（第一次出队前）。
    Queued,
    /// 正在执行一个已出队的任务（从出队到 post_result 之间）。
    Running,
    /// 队列空且无在飞任务——结果已交付，等待新任务或 close。
    Done,
    /// 已注销（close 或清理）。
    Closed,
    /// （Phase 6+）用户/守卫暂停。保留位，本设计不实现。
    Paused,
}
```

`Running` 附属 `since: Instant`（进入时刻），供 stall 检测使用
（见 Phase 6）。`last_activity` / `tool_calls` 保留为 metrics 事实，
不参与状态推导。

**枚举变更影响面（已核实 2026-09-04）**：删除 Idle / 新增 Queued 的编译
影响被锁在 agent-works 内部——phimint UI 用自己的 `ui::AgentStatus` /
`SubAgentStatus`（Phase 5 才映射）、phi-agent 零引用、phi-kernel-tools
`list_agents` 只消费字符串快照。Phase 1 可以放心动枚举。

### 单一事实源 + 生命周期广播

registry 是唯一持有事实的地方，`set_status` 删除。事实变化时广播：

```rust
pub struct AgentLifecycleEvent {
    pub path: AgentPath,
    pub from: AgentStatus,
    pub to: AgentStatus,
    /// 人类可读的原因，透传到日志与 UI tooltip。
    pub reason: &'static str,   // "task_enqueued" / "task_dequeued" / "result_posted" / "unregistered"
}
```

广播通道用 `tokio::sync::watch`（只关心最新全局状态）或 `broadcast`
（需要事件历史，容量沿用 agent-base 事件总线的 2048）。倾向 **watch +
最新快照**：所有消费者（watcher quiescence、UI 面板、list_agents）要的都是
"现在每个 agent 什么状态"，不是事件回放；事件流只用于日志。

### 消费者改造

| 消费者 | 现状 | 改造后 |
|--------|------|--------|
| watcher quiescence | `registry.running_count() == 0`（读标记） | 三事实谓词：`!in_flight && queue_len==0` + 批次交付完备（人人 ≥1 结果，见上节）；时序不变式（Done-before-post 等）由派生规则**结构性保证**，不再依赖注释约束 |
| list_agents | status 字符串来自标记 | 来自派生状态；`Running` 附 `running_secs`（替代裸 `last_activity_secs`）；`Queued` 显示"排队中，结果稍后随批次到达" |
| phimint UI 面板 | 自己从 Progress 事件推断 `SubAgentStatus` | 订阅同一快照（经 phimint 已有的事件桥）；映射 `Running/Queued→Running`、`Done→Done`、`Closed→移除`。Phase 5 可选——现状 UI 未出错，优先级最低 |
| phimint 根状态 `Waiting{running}` | `running = sub_agents 表 Running 数` | `running = 快照中 in_flight‖queue 非空 数`（语义不变，来源变准） |
| metrics/telemetry | tool_calls/last_activity | 不变；额外可从 `since` 算 Running 时长分布 |
| guard | 不读子 agent 状态 | 不变（DefaultGuard 只判 text-only 结束） |
| heartbeat reaper | 不存在（deferred） | Phase 6：`Running{since}` 超过 `max_wait` → 收割（现有 `task_timeout` 10min 的框架级推广） |

### 与标记层修复（c6559510 批次）的关系

`spawn.rs` 已落地的出队 peek（`try_recv` → 有任务标 Running / 无则标 Done，
然后才 `post_result`）是本设计的第一块砖：它把"正确事实"写进了正确时点。
状态机改造把这一步**从调用方义务变成结构性必然**——`run_child_loop` 不再
`set_status`，只做"出队（事实）"和"post（事实）"，状态自己长出来。
回归测试 `test_queued_task_does_not_fire_premature_batch` 原样保留，
是本次改造最重要的验收用例。

### 错误与取消场景

- **任务超时**（`task_timeout` 到期）：`in_flight=false` + `queue_len--`（该任务
  作废）→ 派生为 Done/Error 交付，照常走 mailbox。事实不变，只是交付内容是 Error。
- **close running child**（20260903_2438d139 场景）：close 只置 `closing` 意愿位，
  不改状态；子 agent 完成在飞任务后正常交付、注销 → Closed。中途 cancel 只在
  任务边界生效，状态机不需要知道。
- **进程级 cancel_all**：逐个走注销 → Closed。

## 已落地的前置工作（2026-09-04）

- [x] **标记层修复批次（c6559510，注意与 efad759c 批次的 P0/P1 标签区分，见文首）**：
      **注入限幅**——phimint `child_results.rs` 单报告 24k 字符截断（带可见标记）、
      phi-agent builder `max_message_tokens` 50k→120k（bug ② 双侧封堵）；
      **出队标 Running**——`spawn.rs` `run_child_loop` 出队 peek（`try_recv`），
      有排队任务 post 前标 Running、无则 Done（bug ① 的标记层修复，
      回归测试 `test_queued_task_does_not_fire_premature_batch`）；
      **list_agents 描述重写** + **prompt 软约束修补**（无条件 end-turn、禁轮询/
      禁 nudge/禁中途 close，带 guard 测试）
- [x] **efad759c 复盘批次（与本设计正交）**：agent-base 53a9629（tool_call 后
      text/thought 事件不再吞）、phi-kernel-tools 2abfcdb（spawn task 3-5 句 +
      静态报告脚手架）、guard 判据 c8c75db+37f6b03（未执行的 tool_call 不得判完成）
- [x] **实战验证 d1020cc6（2026-09-04 晚）**：同 prompt 重跑，机制+行为双零故障
      （4/4 spawn、0 轮询、fan-in 一次注入，双零详见记忆 child-result-push）——标记层修复全数生效

标记层仍是"调用方负责打对标记"。以下 Phase 把它变成结构性必然。

## 实现步骤

### Phase 1：registry 事实化
- [ ] `AgentEntry` 增加 `queue_len: usize`、`in_flight: bool`（`since: Instant`）
- [ ] `status()` 改为派生纯函数；`set_status` 删除，改为 `note_enqueued/dequeued/posted/unregistered`
- [ ] 三处生产调用点接线：`runtime.rs:269`（send_task→Running 改 note_enqueued）、
      `spawn.rs:613`（post→Running/Done 改 dequeued+posted）、
      `spawn.rs:231`（spawn→Idle **直接删**——派生规则下 spawn 不产生状态转移）
- [ ] 回归：`test_queued_task_does_not_fire_premature_batch` 继续绿；registry 单测改写

### Phase 2：生命周期广播
- [ ] `AgentLifecycleEvent` + watch 通道（快照 + 事件日志双输出）
- [ ] `list_agents` 从快照读；`running_secs` 字段
- [ ] 文档不变式更新（watcher.rs 模块注释从"调用方必须…"改为"派生保证…"）

### Phase 3：watcher 改读事实
- [ ] quiescence 换成三事实谓词：`running_count()` → `!in_flight && queue_len==0`
      **+ 批次交付完备**（每成员 ≥1 结果，`has_results` 现成）——封死 spawn→send 窗口
- [ ] 全量 watcher 测试不改动语义（现测试即验收）

### Phase 4：状态语义透出（phi-kernel-tools）
- [ ] `list_agents` 输出 `Queued`（"排队中"）与 `running_secs`
- [ ] 描述文字与状态机对齐（"done = 结果已投递或即将随批次到达"）

### Phase 5：phimint UI 订阅快照（可选）
- [ ] 事件桥透传 lifecycle 快照；`SubAgentStatus` 映射
- [ ] 根状态 `Waiting{running}` 改读快照

### Phase 6：heartbeat reaper（stall 收割）
- [ ] `max_wait`（默认待定，建议 15min > task_timeout 10min）到期收割
- [ ] 与 `task_timeout` 的关系梳理：per-task 硬限 vs per-agent 软限

## 待定问题

1. **Paused 的范围**：用户在 TUI 按 ESC 中断子 agent？审批等待算不算独立状态？
   现在审批走 bridge 上抛父级，子 agent 侧表现为长时间 Running——如果做
   `Waiting{since}`（等审批）能改善 UI 显示，但会引入"审批等待"与"卡死"
   的区分问题。**建议**：Phase 6 之后再定。
2. **max_wait 默认值**：task_timeout 已是 10min 硬限，reaper 是兜底的兜底。
   15min 起步，等实战数据。
3. **close_agent / trigger=true 硬门**：子 agent Running 中拒绝 close/追加任务
   （现在是 prompt 软约束 + ChildCleanup 兜底）。框架加门会损失灵活性
   （close-running 正是 2438d139 修复支持的合法场景）；不加门则模型仍可能
   制造排队。**不加硬门，靠 Queued 状态透出让模型看见后果；
   若再实战回归一次同型故障，升级为"close 排队任务的警告返回值"。

## 关键文件索引

### agent-works（状态机本体）
- `src/multi_agent/registry.rs` — AgentEntry/AgentStatus，Phase 1 主战场
- `src/multi_agent/runtime.rs` — send_task 入队事实（:269 现标 Running 处）
- `src/multi_agent/runtime/spawn.rs` — run_child_loop 出队/post 事实（P1 已改）
- `src/multi_agent/runtime/watcher.rs` — quiescence 消费者（:242，2026-09-04 核实）
- `src/multi_agent/mailbox.rs` — 队列本体（事实的物理来源）
- `src/multi_agent/runtime/tests/lifecycle.rs` — 回归测试

### phi-kernel-tools（透出层）
- `src/multi_agent/list_agents.rs` — 状态字符串/描述（本次已重写，Phase 4 再对齐）

### phimint（消费层）
- `src/ui/app.rs` — 根 AgentStatus / SubAgentStatus（Phase 5）
- `src/ui/child_results.rs` — 批次投递路由（不受本设计影响，已稳定）

### agent-base（相邻但不动）
- `src/engine/runtime/session_manager.rs` — max_message_tokens 安全阀
  （bug ② 侧，P0 已把 phimint 侧阈值提到 120k；框架侧默认值是否跟调另议）
