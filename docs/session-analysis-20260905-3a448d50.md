# Session 评估：20260905_3a448d50（A/B/C 修复后首跑）

同任务、同模型（mimo-v2.5-pro）、同 4 子 agent 对比分析。**历代最佳：root 行为
教科书级，机制零故障，成本与 d1020cc6（干净基线）同档。**

## 结论速览

| 维度 | 841ed65b（修复前） | 本 session | d1020cc6（基线） |
|------|------|------|------|
| list_agents | 86 次 | **1 次** | 0 次 |
| nudge / close | 2 / 4 | **0 / 0** | 0 / 0 |
| root 自研（close 后自写报告） | 14 reads | **0** | 0 |
| 批次注入 | 5 报告（deepseek×2 重复） | **4/4 无重复** | 4/4 |
| root LLM calls | 99 | **6** | 8 |
| root token | 84k in / 11k out | **25.9k / 3.25k** | 21.7k / 3.1k |
| 最终答案 | 两份（自研版+综合版） | **一份 6,799 字综合** | 一份 3,734 字 |

## Root 时间线（全部 UTC）

- 19:34:27 repo_map → 19:34:34 spawn×4（task 全绝对路径，F1 生效）+ repo_map
- 19:34:37 **list_agents 唯一一次**：4 个全 running（tool_calls=2 每个）——
  spawn 后 3 秒的 spot-check，符合工具描述"at most once"；副产品是排除了
  efad759c 型幽灵 spawn
- 19:34:48 结束 turn，进度注 54 字符（"4 个子 agent 全部运行中，等待结果返回…"）
  ——完全按 SYSTEM_PROMPT 剧本（announce → spot-check → end turn）
- 19:34:49 → 19:37:18 **root 零活动 2.5 分钟**（metrics 证明无 LLM call）——
  被动等待，841ed65b 的分歧行为（"利用等待时间自研"）未出现
- 19:36:12/19:36:35/19:37:18 子 agent 依次完成（current→deepseek→pi→codex），
  Focus 摘要 3 成功 1 超时降级 plain 通知（设计行为）
- 19:37:18 批次点火 → Batch+idle → **立即 Inject**（.6s 后合成 run 启动），
  一次注入 4/4 报告共 58,206 字符
- 19:37:18→19:38:01 合成：16.8k in / 2.4k out，单份表格化对比报告

## 新修复的暴露情况（诚实声明）

- **新二进制确认在跑**：list_agents 输出是新 `{"agents":[…]}` 结构（旧版是
  顶层数组）
- **A（delivery_note/pending_results）未实弹命中**：唯一一次 list_agents 时
  4 个子 agent 全在 running → pending=0 / note=None，按设计省略。也就是说
  "全员 done 但报告未达"的临界形态还没被实战验证过——下次遇到慢子 agent
  才会看到
- **C（prompt）明确生效**：end-turn 行为与新增条款（done=held、禁"利用
  等待时间"）逐条吻合
- **B（close 警告）未触发**：没有 close 调用（这正是期望）

## 异常面

0 truncation strike、0 denied、0 error。2 条已知良性 WARN：
1. completion judge 10s 超时 fail-open（19:34:49，d1020cc6 已知遗留项①）
2. analyze-current Focus 摘要 30s 超时 → plain 通知 fallback（plain-first 设计）

## 观测层更正（取证方法教训）

初稿此处曾断言"turn_NNN.jsonl 混录子 agent 事件且无归属字段，建议加
agent_path 归属"——**错误，撤回**。turn 文件每条事件本就带 `agent_id`
（root 无此字段，子 agent 形如 `root/analyze-codex`）；初稿抽查的第一行恰好
是 root 自己的 repo_map（agent_id=None 被省略），以偏概全。按归属重算：

- 本 session：root 工具调用恰 7 次（spawn×4 + repo_map×2 + list_agents×1，
  与 metrics 完全吻合），**等待期自研读 = 0**；其余 74 次工具调用全部分属
  4 个子 agent（analyze-current 21 reads 等）
- 顺带重取证 841ed65b：root 真实工具面 = list_agents×86 + read_file×7 +
  list_files×7 + spawn×4 + close×4 + send×2 + repo_map×2——**自研读 14 次
  且全部发生在 close×4 之后**；该文档初版"22:20:46 开始自研"与
  "130+ reads"两处归因有误（那些是子 agent 的读），真实分歧点是轮询循环
  本身。已在其文档补勘误 v2

**取证铁律**：turn_NNN.jsonl 分析必须按 `.agent_id` 过滤（root 无此字段），
不能拿全文件计数当 root 行为。
