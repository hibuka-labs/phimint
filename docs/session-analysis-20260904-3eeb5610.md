# Session 20260904_3eeb5610 分析记录

> 分析完成（2026-09-04）。任务：4 个子 agent 分析 codex / deepseek-harness / pi /
> 当前工程并对比借鉴点。用户观感：① 主 agent TUI "一直在刷"；② "分析 deepseek"
> 的子 agent 显示的是当前工程文件；③ 出现 `analyze-deepseek-retry`；
> ④ 整体"乱套"。总时长 ~16 min（07:31:57–07:48），42 个 LLM turn，
> 85,301 in / 15,624 out tokens，5 次 spawn、10 次 list_agents、1 次 send_message。

## 时间线（精确重建）

| 时刻 | 事件 |
|------|------|
| 07:31:57 | 用户提问；父 agent repo_map |
| 07:32:28–44 | 依序 spawn codex / deepseek / pi / phimint 四个 child（phimint 的任务是**幻觉路径** `demo/phimint`，真实工程在 `buka-works/phimint`） |
| 07:32–07:36 | **4 路 child 思考流同时灌入主转录**（turn_001 共 ~700 流式事件） |
| 07:36:40 | **Batch #1 正确注入**（48,373 字符，4 份报告：codex ✓ / pi ✓ / phimint ✓ / deepseek ✗ "model produced only reasoning, no output"） |
| 07:36:46 | 父 agent spawn `analyze-deepseek-retry`（**任务文本正确**，绝对路径 deepseek-harness） |
| 07:36:46–07:43:40 | **父 agent 不结束回合**：28 次读 phimint 文件 + **`sleep 5/10/15/30×5` + 10 次 list_agents 手搓轮询等待** + 直接读 deepseek 文件（绝对路径 ×4）+ **写出一份完整终报**；同期 retry child 思考流灌转录 7 分钟（turn_002 共 ~4,900 流式事件） |
| （同期） | **retry child 跑偏**：读 `./Cargo.toml`（相对路径 → phimint 的），发现"项目名为 phimint 但目录名是 deepseek-harness"，接受荒谬解释继续分析 phimint 全套文件 |
| 07:43:46 | Batch #2 注入：retry round-1 报告 15,380 字符——**内容是 phimint 分析**（开头"现在我有足够的信息来撰写完整的分析报告了"） |
| 07:43:52 | 父 agent **违规 nudge**："请输出你的完整分析报告。"（误把开场白当作"还没写报告"） |
| 07:46:20 | Batch #3 注入：retry round-2 报告 35,568 字符 → **截断至 24,057**（截断标记生效✓）——仍是 phimint 分析 |
| 07:46–07:48 | run4：父 agent 写第二份终报（3,676 字符），**自己承认**"deepseek 子 agent 错误地分析了 phimint，但我已直接读取其关键文件获得完整信息" |

## 结论一：上一轮修复全部生效 ✅

| 修复 | 本 session 证据 |
|------|----------------|
| P1 出队 peek（防 phantom-idle） | 3 个批次全部正确：batch1 齐装 4 报告（含失败通知）、batch2/batch3 各 1 报告。**无碎裂、无提前触发、无掉队单发** |
| P0 注入限幅（24k 字符/报告） | 35,568 字符报告被截断，标记 `[⚠️ 报告过长已截断：35568/24000 字符…]` 进入上下文 |
| P0 安全阀 120k | 48KB batch1 轻松通过，无静默 pop |
| 错误上浮 | deepseek 失败原因（"model produced only reasoning, no output"）清晰到达父 agent |
| 失败恢复 | 父 agent 对失败的正确反应（spawn retry）出现了——这是想要的恢复路径 |

**机制层在本 session 零故障。** fan-in、限幅、投递、错误通道全部按设计工作。

## 结论二：三个新故障模式（全是行为层）

### 故障 ①：child 相对路径滑落 → 分析错项目（本 session 最大损失）

retry child 的思考轨迹（frames.txt 原文）：

> "用户要求分析一个名为 deepseek-harness 的项目" ✓
> → "让我从探索项目开始。这是一个 Rust 项目。让我查看 Cargo.toml" ← **相对路径，读到了 phimint 的**
> → "**这是一个 Rust 项目，名为 phimint（从 Cargo.toml 的 package name 可以看出），但目录名是 deepseek-harness**" ← 发现矛盾，选择荒谬解释
> → 一路分析 agent.rs / gate.rs / approval.rs / lsp.rs / skills.rs / docs —— 7 分钟全浪费

- child cwd = phimint workspace root；任务里的绝对路径开局即被丢弃
- 模型发现读到的内容与任务目标矛盾时，**合理化了矛盾而不是怀疑自己的路径**
- 后果：deepseek-harness 自始至终没有任何 child 成功分析；最终对比里的 deepseek
  部分来自父 agent 自己的 4 次阅读

**框架缺口**：`CHILD_SYSTEM_PROMPT`（spawn_agent.rs）不含 cwd 信息、无绝对路径纪律、
无"内容与任务不符即停"指令。前三个 child 碰巧用了绝对路径，retry 滑落后无任何护栏。

### 故障 ②：父 agent 发明 `sleep` 轮询等待（新形态的行为偏离）

run2 中父 agent 的操作序列：`sleep 5` → `sleep 10` → `sleep 15` → 交替
`sleep 30` + `list_agents`（5 轮，纯等待 ~3 分钟）→ 期间穿插 28 次 phimint
文件阅读。思考原文："**我需要等待deepseek-harness的分析完成。同时，我可以先开始
整理已有的三个分析报告**"——它知道要等，但把 shell sleep 当成了等待原语。

- prompt 说 "To wait, simply end your turn"，但另一条 "keep going until the
  query is completely resolved" 与之冲突：模型选择持住回合自己等
- 每轮 sleep+poll 之间还夹杂阅读——回合 7 分钟不结束，retry child 的流式事件
  同期持续灌屏（见结论三）
- 此前四轮修的是轮询 list_agents（52×→禁绝），这次模型换了个原语继续轮询

### 故障 ③：报告 churn + 违规 nudge

1. run2 结束时父 agent **已写出一份完整终报**（不等 deepseek）——"不等齐再合成"
2. batch2 的 retry 报告以"现在我有足够的信息来撰写完整的分析报告了"开头，
   父 agent 把开场白误读为"还没交报告"，发出 prompt 明令禁止的
   nudge（"请输出你的完整分析报告。"）→ 多一轮 round → 35k 错误报告
3. 最终 run4 又写一份终报。用户在 ~12 分钟里看到 **两份完整终报 + 三次批次注入 +
   两路流式刷屏** —— 即"乱套"的直接观感
4. 父 agent 自始至终没发现 retry 报告内容是错的项目（最终才承认）

## 结论三：UX 层——"一直在刷"的直接原因

`ui/handlers/runtime.rs:88-126`：child 的 TextDelta/ThoughtDelta 经
`stream.push_text/push_thought` **直通主转录**（task #23 可见性改造的副作用）。

- 07:32–07:36：4 路 child 思考（~700 事件）
- 07:36:46–07:43:46：retry child 独占灌屏 7 分钟（turn_002：3,404 text_delta +
  1,009 thought_delta ≈ 10 事件/秒），叠加父 agent 自己的工具流
- frames.txt 11.3MB / perf.log 105,855 行是这一现象的产物
- scroll_down "already at bottom" 1,071 条（07:32/36/38/39 突发）= 用户与刷屏
  对抗的痕迹，是症状不是原因

可见性是对的，**粒度错了**：长任务 child 的思考流应该进任务面板（已存在），
不是主转录。

## 根因分层

| 层 | 本 session 表现 | 状态 |
|----|----------------|------|
| 框架机制（agent-works fan-in/限幅/投递） | 零故障 | **已收敛** |
| 框架缺口（child 环境无防护） | 故障 ①：child 无 cwd/路径纪律 | **已修（F1）** |
| prompt 软约束 | 故障 ②③：sleep 轮询、nudge、双终报 | **部分可修**（sleep 禁令可加；"end turn vs keep going"张力是模型层的，只能缓解） |
| 模型行为（mimo） | 相对路径滑落后合理化、开场白误读 | 无法根治，只能靠框架护栏兜住 |
| UX 策略 | 故障 ③：child 流直通主转录 | **已修（F3，体感最大）** |

## 修复方案

### F1（P0，phi-kernel-tools）：child 环境防护 ✅ 已实现
不注入任何领域假设（不放"项目/目标目录"措辞），只注入**事实 + 通用纪律**：

- `CHILD_SYSTEM_PROMPT`（spawn_agent.rs）追加 path discipline 段：
  - 事实性规则：工具调用里的相对路径相对于你的 working directory 解析（具体值
    在下方给出）；任务给出绝对路径时，工具调用中**逐字使用**。
  - 纪律规则：当观察与任务描述矛盾时，先重新核实所处位置与路径，再下结论
    （针对"合理化矛盾"的滑落模式）。
- `SpawnAgentTool` 新增 `workspace_root` 字段，spawn 时拼接
  `Working directory: <cwd>` —— 0 LLM 成本，不重新引入 Focus 的超时面。
- 落点：`phi-kernel-tools/src/multi_agent/spawn_agent.rs`（prompt + 拼接 +
  守卫测试 `child_prompt_carries_path_discipline`，含领域中立断言：prompt
  不得含 "project"/"target"）；`mod.rs::create_all_tools` 传递 cwd；
  `phi-agent/src/agent/builder.rs` 捕获 `ma_cwd` 注入工厂。
- 设计讨论结论（用户确认）：cwd 的语义就是"进程工作目录，相对路径的解析基准"，
  框架层只陈述事实，不做"任务目标通常不在 cwd"这类个性化推断。

### F2（P0，phimint prompt）：堵 sleep 轮询 ✅ 已实现
- SYSTEM_PROMPT multi-agent 段新增（与 list_agents 禁令同级别、无条件）：
  "NEVER use shell commands to pass time while sub-agents run (`sleep`,
  `wait`, watch loops, repeated no-op calls). Ending your turn is the ONLY
  wait mechanism — if you catch yourself waiting, end the turn."
- 守卫测试 `live_prompt_bans_shell_wait_loops`（agent.rs prompt_guard_tests）。
- "end turn vs keep going" 的模型层张力不再试图用措辞根除（历史证明每轮换
  新原语），靠禁令收窄 + F3 降低其观感成本。

### F3（P0，phimint UI）：child 流降粒度 ✅ 已实现
child 的 TextDelta/ThoughtDelta 不再进入共享 stream（其 pending tail 就是
主视图的 live tail），改为**每 child 独立累加**（`App::child_streams`）：
- 主视图 live tail 只反映 root 流 —— 7 分钟灌屏（~10 事件/秒）从根源消除；
- child 内容仍进任务面板的 child 视图（`sub_agent_transcripts`）：
  child 的 tool call / RunFinished / RunCancelled 时 flush 落盘，
  时序保证文字在工具行/done 标记之前；
- 附带修复：多个 child 并发时互相切碎 pending 文本的隐性碎裂 bug；
- 清理路径全覆盖：cleanup_completed_agents / settle_after_turn /
  RunFinished(root) / RunCancelled(root) 同步清 `child_streams`；
- 测试：`child_deltas_bypass_main_stream_tail`、
  `child_stream_flushes_before_its_tool_line`、
  `root_tail_survives_child_interleaving`、
  `cleanup_completed_agents_drops_child_stream` +
  task_panel_tests 3 个旧契约测试更新到新落盘时机。全量 289 通过。

### F4（P1）：nudge 防线的框架化观察
本轮 nudge 由"开场白误读"触发。若 F1/F2 后仍出现同型 nudge，考虑在
`send_message(trigger=true)` 返回值中附加 child 当前状态（"child 已交付过
报告，nudge 将开启新一轮任务"），让模型看见后果。（与状态机文档的
"不加硬门"决策一致，先观察。）

### 不修
- 幻觉路径 `demo/phimint`：模型编造，child 靠 cwd 兜底碰巧对了。F1 的
  "名称不匹配即停"同时覆盖此类。
- list_agents 10 次轮询：是 sleep 循环的伴随动作，F2 落地后失去动机。

## 与前几轮的关系

2438d139（丢结果）→ 0cf95e79（死通道）→ 9255c25e（65×轮询）→ d8fc41dc（验证通过）
→ c6559510（批次碎裂+静默pop）→ **本 session（机制零故障，行为三新形态）**。
规律：每轮修掉的是"机制 bug + 当轮行为形态"；机制已收敛，行为偏差每轮换新原语
（read 洪流 → list_agents 轮询 → sleep 轮询 → nudge）。剩余优化都在
**护栏密度**（F1/F2 把新原语堵上）与**信息呈现**（F3 让用户不被刷屏淹没）。
