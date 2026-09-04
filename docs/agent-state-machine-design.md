# 子 Agent 状态机设计（状态单一事实源）

> 状态：**Phase 1-4 已实施（2026-09-04 晚）+ review 批次已合入，全家族测试绿**
> （agent-works 402 lib + 21 集成、phi-kernel-tools 132+5、phimint 295）。
> Phase 5（可选）与 Phase 6（deferred）未做。
> 源于 session 20260904_c6559510 复盘（见 `docs/child-result-push-design.md`
> Phase 8-9 之后的第四次实战回归）。标记层修复批次已落地（注入限幅 + 出队标
> Running），本设计是根治层。
>
> **P0/P1 标签消歧**：本文档说的 P0/P1 指 **c6559510 批次**（bug ② 注入限幅、
> bug ① 出队标 Running）。20260904_efad759c 复盘批次（agent-base 53a9629 事件门、
> c8c75db+37f6b03 guard 判据）与状态机**正交**——guard 判据在回合完成判定层，
> 不读子 agent 状态，不受本设计影响。
>
> **实施修正（2026-09-04，代码核实发现）**：
> 1. **事实 2 不能用 `has_results`**——watcher 把结果 drain 进本地 batch，
>    未读计数在 drain 后恒为 false。落地为 registry 单调计数器
>    `results_posted`（`note_posted` 时递增），且谓词检查**所有在册
>    agent** 而非仅 batch 成员（新 spawn 的 agent 还不是 batch 成员，
>    只查成员封不住 spawn→send 窗口）。
> 2. **`run_child_loop` 的 peek 前瞻循环整体删除**（而非翻译成 note 调用）。
>    peek 存在的唯一原因是旧标记模型没有 Queued；事实模型下 queue_len>0
>    直接挡住 quiescence，自然循环（recv→执行→note_posted→post）即可派生出
>    正确状态：带排队的 post → Queued（不发批），末次 post → Done（发批）。
>    "调用方负责打标记"的最后一处义务随之消失。
> 3. **`has_pending` ≠ queue_len**（它数的是 send_message 的 pending 消息，
>    不是排队任务）。queue_len 按"事实维护点"表落地为 registry 计数器。
>
> **review 批次（2026-09-04 晚，提交前审查发现，3 specialist 并行 + 主审）**：
> 4. **入队事实必须先于投递**（critical，实测复现）。原实现先
>    `mailbox.send_task`（唤醒 child）后 `note_enqueued`，child 的
>    `note_dequeued` 可抢先进 registry 锁——迟到的入账让 queue_len 永久
>    虚高 1 → quiescent() 永假 → fan-in 挂死且无自愈。修法：`send_task`
>    先记账再投递，投递失败 `note_send_failed` 回滚（账实一致是
>    quiescence 存活的前提，已入"事实维护点"不变式）。
> 5. **交付完备条款的配套义务**（critical）：quiescent() 要求所有在册
>    agent `results_posted ≥ 1`，因此"已注册但永无任务"的孤儿会永久
>    卡死批次。spawn_agent 原以 `let _ =` 吞掉 send_task 失败并谎报
>    成功——已改为失败即 close 孤儿 + 如实返回（phi-kernel-tools）；
>    spawn_inner 的 mailbox.register 失败路径补上 registry.close 回滚。
> 6. 维护性清理：OnceLock→直接字段、死代码 count_by_status 删除、
>    snapshot/AgentInfo 文档修正（"closed" 不会出现在快照里）、watch
>    消费者文档注明 metric 字段 transition-frozen；补 5 个测试
>    （send 失败回滚、watcher 级 spawn→send 窗口、close-during-queued、
>    running_secs/watch 唤醒、disabled_config 恢复）。

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

事实 2 的来源（实施修正，见文首）：**不能用 `has_results`**——watcher 会把
结果 drain 进本地 batch，未读计数在 drain 后恒为 false。落地为 registry 的
单调 `results_posted` 计数器（`note_posted` 递增），且检查**所有在册 agent**：
新 spawn 的 agent 还不是 batch 成员，只查"batch 成员"封不住 spawn→send 窗口。
**这才是 fan-in 的真实语义——"所有人交齐才唤醒"，而不是"没人干活了就唤醒"。**

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
| watcher quiescence | `registry.running_count() == 0`（读标记） | 三事实谓词 `quiescent()`：`!in_flight && queue_len==0` + 交付完备（每在册 agent `results_posted ≥1`，见上节修正）；时序不变式（Done-before-post 等）由派生规则**结构性保证**，不再依赖注释约束 |
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

### Phase 1：registry 事实化（✅ 2026-09-04）
- [x] `AgentEntry` 增加 `queue_len: usize`、`in_flight: bool`、
      `running_since: Option<Instant>`（`since` 落为字段而非变体参数，枚举保持
      unit 变体，`Paused` 预留）、`results_posted: usize`（事实 2 的载体）
- [x] `status()` 改为派生纯函数（`derive_status(registered, in_flight, queue_len)`）；
      `set_status` 删除，改为 `note_enqueued/dequeued/posted`；
      `running_count()` 更名 `busy_count()`（在飞+排队计数，quiescence 不再走计数）
- [x] 三处生产调用点接线：`runtime.rs` send_task（→note_enqueued）、
      `spawn.rs` run_child_loop（自然循环：出队 note_dequeued、post 前 note_posted）、
      `spawn.rs` spawn_ready（标 Idle **已删**——派生规则下 spawn 不产生状态转移）
- [x] 回归：`test_queued_task_does_not_fire_premature_batch` 绿；registry 单测改写
      （新增 derive/事件/快照/quiescence 窗口单测）

### Phase 2：生命周期广播（✅ 2026-09-04）
- [x] `AgentLifecycleEvent` + `watch` 快照通道（`RegistrySnapshot`；
      `send_replace` 发布——`send` 在无接收者时会静默丢弃更新，测试抓到过）+
      2048 容量事件环（`recent_events`）；runtime 侧
      `subscribe_lifecycle` / `recent_lifecycle_events` 访问器（Phase 5 的桥接钩子）
- [x] `list_agents` 从快照读；`AgentInfo` 增 `running_secs`（`last_activity_secs`
      按"metrics 不变"一行保留）
- [x] 文档不变式更新（registry.rs / spawn.rs / watcher.rs 模块注释改为
      "派生保证"语义）

### Phase 3：watcher 改读事实（✅ 2026-09-04）
- [x] quiescence 换成 `AgentRegistry::quiescent()` 三事实谓词
      （交付完备 = 每在册 agent `results_posted ≥1`）——封死 spawn→send 窗口
- [x] 全量 watcher 测试换 fact API（`spawn_running`/`finish_and_post` 改
      note_* 调用，语义不变即验收）；`run_child_loop` 的 peek 前瞻循环
      **整体删除**（见文首修正 2）

### Phase 4：状态语义透出（phi-kernel-tools）（✅ 2026-09-04）
- [x] `list_agents` 输出 `Queued`（"排队中，结果随批次到达"）与 `running_secs`
- [x] 描述文字与状态机对齐（"done = 结果已投递或即将随批次到达"）；
      close 输出测试桩的 "idle" 字符串改 "done"（旧词表消失）

### Phase 5：phimint UI 订阅快照（可选）（✅ 2026-09-05）
- [x] 事件桥透传 lifecycle 快照（`TuiEvent::Lifecycle`，`subscribe_lifecycle`
      watch → UI 事件流）；`SubAgentStatus` 映射：`queued`/`running`→Running、
      `done`→Done、快照消失（unregister=正常退出或 close）→Done 但**不移除**
      （3s reaper 保留 review 窗口）；未知 `done` 不插入（新注册 agent 在首个
      send_task 前读 done，插入即幽灵条目）；条目在 spawn 时刻即出现，
      不再等子 agent 首次工具调用
- [x] 根状态 `Waiting{running}` 改读快照——经面板条目事实化间接达成：
      条目存在性与状态完全由快照维护，`refresh_waiting_count` 每个快照重算
- 回归 +6（task_panel_tests）：spawn 即建条目、running↔done 双向翻转、
  未知 done 不插入、消失保条目、Waiting 计数同步

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
