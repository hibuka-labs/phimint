# Session 分析：20260905_913766db（状态机 + Phase 5 首战）

- 任务：4 子 agent 对比分析（codex / deepseek-harness / pi / 当前工程 phimint）
- 模型：mimo-v2.5-pro；总时长 ~9 分钟（15:09:47–15:18:30 local）
- 结论：**连续第二次干净跑**；首次在实战中验证 3× 完成时间差下的 fan-in 正确性
- 取证注意：turn_001.jsonl 在 root turn_end 即停止记录子 agent 事件——子 agent
  在 root 回合结束后的收尾阶段（含最终报告生成）**不在事件日志里**，必须用
  session.log（h2 帧/Focus 时间戳）+ frames.txt（面板计时器/等待计数）重建时间线

## Root 行为（root-authoritative）

| 维度 | 值 | 评价 |
|---|---|---|
| 工具调用 | 9：repo_map×1, list_files×1, spawn×4, list_agents×1, execute_command×2 | 与 3a448d50 同档（7 次）；+2 echo 旁白（见下） |
| list_agents | 1 次 spot-check（spawn 后 ~10s） | 连续第二个 session 无轮询 |
| nudge / close / 审批 | 0 / 0 / 0 | |
| 等待期 | 15:11:02 → 15:17:05 ≈ 6 分钟被动等待，零 root 活动 | 期间 3 个子 agent 已完成，root 未被 Progress 唤醒（Progress-never-gates-Batch） |
| Token | 23,444 in / 3,558 out，8 LLM turns | 与 d1020cc6（21.7k/3.1k）、3a448d50（25.9k/3.25k）同档 |
| 批次 | 4/4 一次注入，42,460 字符（current 9,994 / codex 9,440 / deepseek 10,240 / **pi 12,636**），无重复，24k/报告上限未触及 | |
| 合成 | 61.3s，18.4k in / 2.4k out，6,881 字最终报告 | |

WARN 仅 3 条，全部已知良性：judge 10s fail-open（15:10:49）、2 条 skill 目录命名跳过（启动期）。

## 核心验证：完成时间差 3× 下的 fan-in（841ed65b 反面场景）

| agent | 完成时刻 | 用时 |
|---|---|---|
| analyze-current-project | 15:12:34 | 2m16s |
| analyze-codex | 15:12:40 | 2m22s |
| analyze-deepseek-harness | 15:13:29 | 3m11s |
| analyze-pi | 15:17:05 | **6m47s** |

- 批次精确在 pi post_result 的瞬间点火（15:17:05-06），无提前、无碎裂——
  交付完备谓词（每在册 agent `results_posted ≥ 1`）在真实不对称下成立。
  841ed65b 同型场景（不对称→root 不信合同→轮询+强杀健康 done agent）未复现。
- Phase 5 等待计数全程正确递减，帧数与完成时刻吻合：
  4 个运行中 ×543 帧 → 3 ×15 → 2 ×64 → 1 ×278（≈pi 独行的 4.6 分钟）。
- 面板条目 spawn 后 9s 即存在（Phase 5 生效，不再等首个子工具事件），
  活动列（→/✓ + 工具名）与秒表逐秒走动（liveness tick 生效）。

## pi 专项：不卡，是真忙了 6m47s

用户观察：pi 最后一句 "Now let me examine the core source files in detail"
后长时间无输出，其他 2-3 分钟就结束了。

**定性：正常慢，非 hang。** 证据：

1. **h2 流持续有数据**：15:14–15:17 每分钟 ~360 数据帧（≈6 帧/秒），与所有
   agent 前期活跃期流速一致——token 在持续生成，无死区。
2. **零异常**：无 task_timeout（10min 远未到）、无截断 strike、无重试。
3. **收尾动作完整**：最后 4 个 read_file（pi/packages/agent/src/ 下
   reducer.ts / agent.ts / agent-loop.ts / agent-harness.ts）→ 生成
   **全场最长报告 12,636 字符**（其他 9.4k–10.2k），报告质量正常
   （Focus 摘要："工程成熟度高，核心极简可扩展"）。
4. **慢的来源**：21 次工具调用（11× list_files——pi 仓库是嵌套
   monorepo，逐层探索步骤多）+ 最长的最终生成（~3.5 分钟纯写报告）。

**"没动静"的体感解释**：F3 per-child 流降级——子 agent 报告文本只进子 agent
聚焦视图，主视图仅剩面板 ● + 秒表。等待计数显示 1 个运行中（事实正确），
但内容层无动感。备选 UX（需框架新增"LLM 生成中"事实，暂缓）：面板活动列
显示 writing report…

## 新观察：root 的 echo 旁白

turn 4 / turn 6 各一次 `execute_command`：
`echo "waiting for sub-agents to complete"` / `echo "agents still running,
ending turn to wait"`。

- 不是 sleep/wait（F2 完好，零耗时），是**话痨式旁白**——多花 2 个工具
  调用 + ~2 个 turn + ~500 token。
- 软约束级别，暂不处理；若再现可考虑 prompt 加一句禁 shell 旁白
  （与 F2 同段落），风险是过度约束。

## 取证方法记录

- 子 agent 完成时刻：frames.txt 面板冻结计时器（✓ + Xm Ys）× Focus
  调用时间戳（15:12:34/41、15:13:29、15:17:06）互证。
- 活跃度：session.log h2 `framed_read` 按秒计数（rtk 环境下先落临时文件
  再 awk 分桶）。
- 报告长度：session.log 批次 turn 的 `user_input` 反 JSON 转义后按
  `[子 agent X 已完成]` 切分。
- **教训**：turn_NNN.jsonl ≠ 全程记录；root 结束回合后的子 agent 阶段
  只能靠 session.log/frames.txt。此前两份分析（841ed65b/3a448d50）未受
  影响的根因是当时取证目标都发生在 root 回合内。
