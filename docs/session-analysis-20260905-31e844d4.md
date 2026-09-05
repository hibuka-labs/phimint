# Session 分析：20260905_31e844d4（10.1 UX 批次首战 + 历代最低 root 成本）

- 任务：4 子 agent 对比分析（codex / deepseek-harness / pi / **phimint 自身**——套娃：
  analyze-phimint 的报告引用了当天刚实装的 `is_writing_hint` 代码）
- 模型：mimo-v2.5-pro；21:22:32–21:28:52 local（UTC+8）
- 结论：**机制连续第三次零故障；root 成本历代最低；`✍ writing…` 提示首次实战命中**
- 二进制：含 Phase 5（lifecycle 面板）+ 10.1 批次（writing 提示 + 聚焦实时 tail）

## Root 行为（root-authoritative）

| 维度 | 值 | 评价 |
|---|---|---|
| 工具调用 | 7：repo_map×2, execute_command(pwd)×1, spawn×4 | 历代最少（前两次 9） |
| list_agents | **0**（历代首次，连 spot-check 都没调） | 841ed65b：86 次 |
| nudge / close / 审批 | 0 / 0 / 0 | |
| LLM turns | 4 即 end-turn（21:23:19 judge 10s fail-open 放行，已知良性） | |
| 等待期 | 21:23:19 → 21:26:43 = 3m42s 完全被动 | |
| 首跑 token | 8,826 in / 835 out | 历代最低（d1020cc6 21.7k / 3a448d50 25.9k / 913766db 23.4k） |

## Fan-in：1.7× 时间差，批次零延迟

| agent | 交付时刻 | 用时 | 报告长度 |
|---|---|---|---|
| analyze-deepseek-harness | 21:25:12 | 2m11s | 7,757 |
| analyze-pi | 21:26:00 | 2m59s | 9,972 |
| analyze-phimint | 21:26:21 | 3m20s | 14,392 |
| analyze-codex | 21:26:43 | 3m42s | 11,995 |

- 批次点火 = codex post 同一秒（21:26:43），4/4 一次注入 44,258 字符，
  无重复；合成 64s / 18.2k in / 3.2k out，9,636 字结构化报告
  （借鉴项 × 来源 × 工作量 × 影响 表格）。
- deepseek 的 Focus 摘要 30s 超时 → 降级 plain 通知（设计行为，第三次出现；
  mimo 摘要 21-30s 贴着阈值）。

## 新功能实战验证

- **`✍ writing…`（10.1 方案 A）**：codex / phimint / pi 三个在「末工具完结后
  静默写报告」时段正确切换提示（如 codex：~1m58s 起显示，3m42s 翻 Done），
  替代的正是以前误导性的过期 `✓ read_file`。deepseek 末工具→Done 连贯，
  未触发（正确）。在飞工具保持 `→ tool`、Done 冻结，均未被误报。
- **Phase 5 事实链**：等待计数 4→3→2→1 正确递减；面板条目 spawn 即现。
- 取证注意：本 session 的 turn 文件里 grep `✍` 命中 2,276 行全是
  analyze-phimint 读 `src/ui/app.rs` 读到的源码文本——**分析含本仓库自身的
  session 时，功能标记词会被"被分析对象"污染，必须用面板行格式
  （`● <name> ✍ writing… │ <timer>`）做锚定**。

## 新发现：session_metrics totals 漏计合成 run（bug，待查）

`total_input_tokens` = 8,826，恰好等于首轮 4 turn 之和（5,706+149+2,235+736）；
合成 turn（18.2k in）**未并入 totals**，但 turns 列表包含它。上一 session
913766合并的（23,444 = 5,033 + 18,411 精确吻合）——同一路径两次
结果不同，疑与批次注入后合成 run 的 metrics 会话 merge 条件有关。影响仅限
token 统计报表偏低；telemetry merge 路径待查（低优先）。

## WARN 全量（4 条，均良性）

1-2. 启动期 skill 目录命名跳过 ×2（_gstack-command / connect-chrome）
3. judge timeout 10s fail-open（21:23:19）
4. deepseek Focus timeout 30s → plain notice（21:25:43）
