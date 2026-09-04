# Session 分析：20260904_841ed65b

> **勘误 v2（2026-09-05，经 agent_id 归属重取证；推翻本文档初版两处归因）**。
> turn_NNN.jsonl 每条事件本就带 `agent_id`（root 无此字段，子 agent 形如
> `root/analyze-pi`）。按此重算 root 的真实工具面：**list_agents×86、
> read_file×7、list_files×7、spawn×4、close×4、send_message×2、repo_map×2**。
> ①初版"22:20:46 root 开始自己 read_file/list_files 利用等待时间"**归因
> 错误**——那些读是子 agent 的；分歧点的真实形态是 **86 次 list_agents
> 轮询**（root 在 22:24:40 close 之前没有任何自研读）。②"130+ reads"
> 同样是把子 agent 的读算到了 root 头上——root 自研读只有 **14 次，且全部
> 发生在 close×4 之后**（绝望阶段自写对比报告，0cf95e79 剧本成立但这部分
> 规模远小于初版所述）。三个问题的答案与核心结论不变；文档正文保留初版
> 表述，以本勘误为准。

状态机 Phase 1-4 新二进制首个实战 session。**机制层零故障**（与 d1020cc6 同级），
行为层复发 0cf95e79/c6559510 同族故障并走到最重形态。同模型（mimo-v2.5-pro）、
同任务、同 prompt——唯一变量是新二进制。

## 时间线（全部时间戳已对齐）

| 时刻 | 事件 |
|------|------|
| 22:20:31 | root repo_map（先看自己工作区，正常） |
| 22:20:43 | spawn ×4（analyze-codex/pi/current/deepseek），task 短而全路径，合规 |
| 22:20:46 | **分歧点**：root 不结束回合，开始自己 read_file/list_files "利用等待时间" |
| 22:21:35→ | root 转入 list_agents 轮询，每 ~2.5s 一次，共 **86 次** |
| 22:22:28→22:23:43 | 子 agent 依次交付：Focus Progress ×4（22:22:28 / 22:22:52 / 22:23:04 / 22:23:43） |
| ~22:23 | codex/current/pi 状态翻 `done`；deepseek 原任务仍 running（真在工作） |
| 22:23:39 | root nudge #1（send_message→deepseek，此时 deepseek 距交付还有 4s） |
| 22:23:43 | deepseek 交付 R4 → **此时全部 4 个都已交付**，但 quiescence 因 nudge#1 已入队不成立 |
| 22:23:53 | deepseek 出队 nudge#1 任务，重新 Running（root 眼中的"还在跑"是自己制造的） |
| 22:24:21 | root nudge #2 → 入队，deepseek 排队 |
| 22:24:40 | root **close_agent ×4**（强杀全部健康子 agent） |
| 22:24:58 | deepseek 在关闭标志下交付 nudge#1 的回复 R5 后退出；watcher 批次凑齐（R1-R5），quiescence 成立，**批次扣发**（root 正在回合中） |
| 22:24:52→22:25:16 | root 自己读 4 个工程（0cf95e79 剧本），写出自己的对比报告 |
| ~22:26:55 | turn 1 run_finished → **扣发批次 flush** → turn 2 以 user 消息注入全部 5 份报告（deepseek ×2 重复） |
| 22:26:55→ | turn 2：root 用真实报告正确综合（"Now I have all four sub-agent reports"） |

## 三个问题的答案

### 1. list_agents 为什么一直被调（86 次）

root 的推理流反复出现"我知道不该轮询"：*"I should stop polling"*（至少 6 次）、
*"I've been polling excessively"*——每次说完下一次继续调。起点是 spawn 后 3 秒
它选择"利用等待时间自己调研"，此后 list_agents 变成它的焦虑仪表盘：盯着
`last_activity_secs` 爬升（"codex 84s inactive but still running"）把**写报告期**
（无工具调用、纯 LLM 输出，activity 自然冻结）误读为卡死。纯行为故障，同
c6559510（52 次）且更重。

### 2. 子 agent 完成了，主 agent 为什么"没发现"

**这是本 session 最重要的新证据**。root 原话：

> "their results should have been pushed to me. However, **I don't actually see
> their reports in my context** - the results were supposed to be pushed
> automatically but **it seems the system didn't deliver them**."

机制上完全按设计走：Progress 永不唤醒父 agent（不变式 3）；批次在 root 回合中
扣发（Batch+running→turn 末 flush）。**回合内视角**下，"done + 报告未出现"
对模型不可与"系统没投递"区分——诚实状态 + 静默扣发 = 模型有理由不信合同。
d1020cc6 的模型 spawn 后立刻结束回合，从未进入这个两难；本 session 的模型
选择先自己干活，掉进去了。

### 3. 其他问题 + 为什么"改出了问题"

**机制层逐项核对，全部按设计工作，零回归证据**：

- 状态诚实：done 翻转时刻与结果交付时刻一致；running_secs 正确跟踪 deepseek 被
  nudge 打断后的重跑（164s→13s→40s 重置清晰可见）
- quiescence 正确：批次直到 R5（22:24:58）才凑齐——**"没发批次"不是 bug，
  是 deepseek 真没交齐**（它 22:23:43 才交第一份，之后被 root 的 nudge 重新拉起）
- 批次投递正确：turn 末 flush，5 份报告完整注入，turn 2 综合基于真实报告
- close 语义正确：3 个 redundant Closed 去重丢弃；deepseek 在关闭标志下
  交付完在飞任务才退出；nudge#2 的排队任务随 close 作废（无泄漏）
- guard 零误判；无 WARN/ERROR（除 skill 目录噪音）

**为什么感觉"改出了问题"**：这是新二进制首跑 + 行为层历代最重（86 轮询 vs
历史 52/65），观感上是退步。但同模型同任务下 d1020cc6 干净、本 session 崩，
分歧点在 spawn 后 3 秒的"要不要利用等待时间"——发生在任何状态被读取之前，
是纯模型方差（prompt 是概率软约束，见 c6559510 教训①）。**没有证据指向
状态机输出格式诱发**（模型推理引用的都是旧字段 last_activity_secs）。

## 真正暴露的设计缺口

状态机已把"结果已交付、批次已凑齐、正在扣发"变成 registry 里的**事实**
（results_posted、quiescent()），但这个事实对回合内的父 agent 不可见。
合同要求模型盲信"done = 在路上"，而它没有任何办法验证。这不是回归，
是 Phase 5（UI 订阅快照）一直缺的消费者补全。

## 修复候选

> **2026-09-05 更新：A/B/C 已全部落地（工作区未提交）**——A：registry 新事实
> `results_handed_over`/`pending_results` + watcher 批次点火时打点 +
> list_agents 输出重构为 `{agents, delivery_note}`（全员交付即"End your turn
> NOW"）；B：close_agent 关闭前读事实，有未投递报告/排队任务时返回警告 +
> 过期描述重写（删 wait_agent 残留、改 task-boundary 语义）；C：SYSTEM_PROMPT
> 写死 `done`=已扣报告 + 禁"利用等待时间自研"，guard 测试 +2。
> 测试：agent-works 404+21 / phi-kernel-tools 139 / phimint 297 全绿。

- **A（P0，读侧透出）**：list_agents / 生命周期快照透出投递事实——
  per-agent `results_posted` 或全局 `batch_held: N`；全员交付且批次扣发时
  list_agents 直接说"all reports ready — end your turn to receive them"。
  把盲信变成可验证事实，直接杀死"system didn't deliver"误信。
  纯读侧，riding on 状态机已有事实。
- **B（P0，close 警告）**：close_agent 对 done/有结果在批次的 agent 返回警告
  （"N reports pending in batch; closing does not revoke them"）。
  设计文档待定问题 3 自己写了升级条款（"若再实战回归一次同型故障"）——
  本 session 即该复发。
- **C（P1，prompt）**：SYSTEM_PROMPT 把 done 语义写到不可误读：
  "a done child's report is held by the runtime and will be injected the
  instant your turn ends; NEVER re-do its work; ending the turn is the ONLY
  way to receive it." + 禁"利用等待时间自研"。
- **D（记录不修）**：deepseek ×2 重复报告（nudge 响应与原报告同时入批）——
  幂等设计预期内，浪费 ~10k token，根因是 nudge 本身，A/B 落地后自然消失。

## 代价

root 侧 99 次 LLM 调用、229 次工具调用、84k in / 11k out token；22:20:46 →
22:25:16 的全部自研活动（130+ reads + 86 polls + 2 nudges + 4 closes）为纯浪费
——子 agent 报告本身就完整回答了任务。用户看到两份最终报告（turn 1 自研版 +
turn 2 真实综合版）。
