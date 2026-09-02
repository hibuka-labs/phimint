# spawn_agent / 工具调用参数截断问题分析

> 状态：根因已定位（字节级抓包 + 直连探针证实）。第一轮修复消除崩溃/硬终止/400；第二轮修复 `send_message` 漏网（session 停止包合法 wrapper + 守卫识别 wrapper）；第三轮修复 `spawn_agent {}` 空对象回仿（模型回仿 sanitizer 存的 `{}` → schema-aware 守卫拦截）。剩余为"委托空转封顶 + prompt 降触发面"（§7.3/§7.4，未实施）。本文档记录完整上下文、证据与修复。
> 日期：2026-09-02　作者：诊断会话
> 相关 crate：`agent-base`、`llm-providers`、`phi-kernel-tools`、`phimint`

---

## 1. 摘要（TL;DR）

在使用 `mimo-v2.5-pro` 执行"用 4 个子 agent 并行分析多个工程"的任务时，`spawn_agent` / `wait_agent` / `send_message` 等多 agent 工具反复失败，TUI 表现为 `spawn_agent failed 3 consecutive times`、`wait_agent argument parsing failed: EOF while parsing a value (args: {"agent_path": )`，以及"卡住"的观感。

根因是**两层叠加**：

1. **Provider 侧（mimo-v2.5-pro）**：在负载下会在 `tool_calls` 的 `arguments` 生成流**中途截断**，却**谎报** `finish_reason=tool_calls`（而非 `length`）。留下悬空 JSON，如 `{"agent_path": `、`... ", "agent_type": `。
2. **框架侧（agent-base）**：截断守卫只认 `finish_reason=length`，谎报的 `tool_calls` 绕过守卫 → 截断参数进入执行 → 报 `ToolArgsInvalid` → 计入 `ConsecutiveFailureRecovery`（阈值 3）→ 硬终止/死亡螺旋。
3. **空对象回仿（框架侧·已修复）**：截断参数存成 `{}` 后，模型回仿此形状，于同一轮吐出"真 spawn + 空 `{}` spawn"双桶。`{}` 合法 JSON，旧守卫不拦 → typed schema 缺必填字段 → `argument parsing failed: {}`。**第三轮修复（§6.3）加 schema-aware 判据：具名调用 + 空对象 + 该工具有 required 字段 → 当退化调用走良性重发。**

第一轮修复（§6.1）把"非法 JSON 的结构判据"加入守卫，消除了崩溃与 400 级联；但发现一条 `send_message` 漏网路径——session 把截断参数预包成合法 JSON 错误对象，既骗过判据又被模型回仿。第二轮修复（§6.2）改为把截断 args 存成 `"{}"` 并让守卫识别 wrapper，堵住此路。第三轮修复（§6.3）堵住 `{}` 回仿路径：模型把 sanitizer 存的 `spawn_agent {}` 当作合法形状模仿，在并行轮次中吐出"真 spawn + 空 `{}` spawn"双桶；旧守卫对合法 JSON 不拦，导致 typed schema 执行失败。新守卫加 schema-aware 判据——具名调用 + 空对象 + 该工具有 required 字段 → 走良性重发路径，不计失败。剩余可优化项（委托空转封顶、prompt 收敛大参数）见 §7。

---

## 2. 触发上下文

**用户请求**（多轮复现均同类）：
> 用 4 个子 agent 并行分析以下工程（codex / deepseek-harness / pi 与当前工程）vs 当前项目……每个工程的 message 要详细，说明要分析：1.项目定位 2.架构亮点 3.技术栈 4.代码质量 5.当前不足。

**模型/端点配置**（来自 `.env`，凭证不外泄，仅程序加载）：
```
LLM_MODEL=mimo-v2.5-pro
LLM_BASE_URL=https://token-plan-cn.xiaomimimo.com/v1   # 复现时临时指向本地录制代理 127.0.0.1:8899
LLM_PROTOCOL=openai
max_tokens=24576
```
模型画像（`llm-providers`）：`has_reasoning=true, reasoning_mode=None`，即 reasoning 模型但**不接收 `reasoning_effort`**。

**涉及会话**：
| 会话 | 时间 | 现象 | 说明 |
|------|------|------|------|
| `20260901_ac9741e1` | 09-01 | TUI 报 `spawn_agent failed 3 consecutive times` | 最初上报；其 `session.log` 未含该字符串（错误文案由 phimint UI 层从事件构造），仅作背景 |
| `20260902_48f4a4fc` | 09-02 06:05 | "看起来卡住" | 日志末行停在 `phase1: parsing tool args`，系用户**手动中断**；无截断/abort 字符串，属长调用被中断，非崩溃 |
| `20260902_abe17003` | 09-02 09:30–09:43 | 委托阶段反复截断，最终自恢复转直接分析并**成功完成** | **本文所有硬证据来源**，经本地录制代理抓包 |

> 说明：本文证据锚定在 `abe17003` + `/tmp/phimint-capture/*` 抓包（可复现、字节级）。更早两个会话日志中**未检索到**对应错误字符串，故只作触发背景，不作证据。

---

## 3. 现象时间线（`abe17003`，本地时间）

```
09:30:47  turn1 calling LLM
09:30:58  turn1 done → 执行 repo_map
09:31:03  turn2 done tool_calls=4 [list_files,repo_map,list_files,list_files]
09:31:03  ✗ list_files ×3  "output exceeds the 16000-char limit"        ← 问题 C
09:31:41  turn4 done text_len=3515 + tool_call → WARN 截断，弹回重发（turn=4, finish=Stop）  ← 守卫首次生效
09:31:45  turn5 执行 wait_agent … 直到 09:34:45 才 done（≈3 分钟）        ← 问题：wait_agent 长阻塞
09:35:12  turn7 done tool_calls=1 → WARN 截断，弹回                          ← 问题 A（spawn 参数截断）
09:35:23  turn8 done → WARN 截断，弹回
09:35:28–41  4× spawn_agent **参数完整**，正常执行（子 agent 起来了）        ← 证明委托非必然坏，是概率性截断
09:35:49–38:07  turn13–23 连续 WARN 截断，弹回（子 agent 侧也在截断）
09:38:54  ✗ Tool 'send_message' argument parsing failed（args_len=256）    ← 问题 B：守卫漏网
09:39:02  ✗ Tool 'send_message' argument parsing failed（args_len=191）    ← 问题 B：守卫漏网
09:39:56–40:03  turn28–29 WARN 截断，弹回
09:40:13–21  模型自判"持续性问题" → 2× close_agent（关闭子 agent）
09:40:21  之后**再无截断 WARN**，转直接分析
09:42:16  text_len=3005, tool_calls=0 纯文本分析产出
09:43:21  turn10 text_len=5027 → guard: task complete → agent turn completed ← 成功结束
```

关键统计（全会话）：
- 截断守卫 WARN（拦下）：**15 次**
- session 包裹截断 WARN：15 次（`push_assistant_tool_calls`）
- `Tool '…' argument parsing failed`（漏网）：**2 次**（均 `send_message`）
- `failed 3 consecutive times`：**0 次** ✓
- `missing a function name` / HTTP 400：**0 次** ✓（幽灵桶修复生效）
- 峰值 input tokens ≈ 85,640（重发空转推高，未及 256K 压缩阈值）

---

## 4. 根因分析

### 4.1 Provider 层：mimo 截断 + 谎报 finish（决定性证据）

`/tmp/phimint-capture/sse_093512.txt` 原始 SSE（一次 `spawn_agent`）：
```
frag_count = 6,  raw finish_reason = "tool_calls"
  first frag: (index 0, name='spawn_agent', args='')
  last  frag: (index 0, name=None,          args=', "agent_type": ')   ← 断在键名处，JSON 未闭合
```
即 provider 发出了 `{"task_name": "analyze-codex", "agent_type": ` 后**流终止**，仍上报 `finish_reason=tool_calls`。全部 6 个 delta 都带 `index`（排除客户端 `unwrap_or(0)` 误并桶）。同批 `sse_093523` 的 `spawn_agent` args 完整合法（187 字符，`analyze-codex`）→ 证明是**逐次概率性**截断，而非必然。

**为何用户"概率很大"、而合成小请求打不出**：截断与**负载**相关（并行多 spawn + 数百 KB 请求体 + reasoning 模型把 24576 输出预算大量耗在 reasoning/正文，`arguments` 被饿死）。小请求无此压力，故 0/48。

### 4.2 框架层缺陷一：守卫只认 `length`（已修）

`agent-base/src/engine/runtime/react/tools.rs`（修复前）：
```rust
if finish_reason.is_truncated() {           // 仅 Truncated{length/max_tokens} 为真
    …  // 不执行，push "re-issue" 提示，TurnFlow::Continue（干净路径，不计失败）
}
```
`FinishReason::is_truncated()`（`agent-base/crates/agent-types/src/execution.rs:96`）只对 `Truncated{..}` 为真。mimo 的 `tool_calls`/`Stop` → `Other`/`Stop` → 守卫不触发 → 落到 `tool_engine.rs:414` `from_str` → `ToolArgsInvalid` → 计入 `ConsecutiveFailureRecovery`（阈值 3）→ `failed 3 consecutive times`/卡死。

**修复**：守卫增加**结构判据** `has_incomplete_args` —— 具名 call 的 args 非空且 `from_str::<Value>` 失败即触发，**与 finish_reason 无关**（因不信谎报）。

### 4.3 框架层缺陷二：幽灵桶 → HTTP 400（已修）

`agent-base/src/engine/runtime/llm_engine.rs` 组装 `tool_calls` 时不过滤 `name=""` 的桶；provider 偶发发出 `{index, name:"", args:{}}`，若保留 → assistant 历史出现无函数名的 tool_call → 下一请求 `HTTP 400 "…tool_calls[0] is missing a function name"`。

**修复**：组装处 `.filter(|(_, (_id,name,_))| !name.is_empty())`。全幽灵轮次改走既有良性分支 `turn_dispatch.rs:255 handle_incomplete_tool_call`（nudge）。本会话 400 = 0，生效。

### 4.4 框架层缺陷三（已修·第二轮）：session 把截断参数包成合法 JSON

`agent-base/src/engine/session.rs` `push_assistant_tool_calls`（约 265–316）对非法 args **包裹**成合法对象写入 assistant 历史：
```json
{"error":"tool_call_arguments_truncated","message":"…retry with complete arguments.","original_args_preview":"{\"agent_path\": "}
```
这制造**两个坑**：
1. 包裹体是**合法 JSON** → §4.2 的结构判据（"非法才截断"）**被绕过**；
2. 模型会把该对象**原样回仿成 tool_call 参数**（见 §3 `args_len:256/191`）→ 在 typed-tool schema 校验（`send_message` 缺 `agent_path`）时报 `ToolArgsInvalid`。

`send_message` 的失败原始记录（`abe17003` 09:38:54）：
```
Tool 'send_message' argument parsing failed:
{"error":"tool_call_arguments_truncated","message":"…","original_args_preview":"{\"agent_path\": "}
```
→ 证实漏网走的是"合法 wrapper / 回仿"路径，正是 §4.2 判据的盲区。

---

## 4.5 max_tokens / 截断 探针实证（直连 mimo-v2.5-pro，脚本加载 .env 不打印凭证）

| 探针 | 请求 `max_tokens` | `finish_reason` | `completion_tokens` | 可见内容 |
|------|------|------|------|------|
| A | 128 | **`length`** | 128 | 0 字（全花在 reasoning） |
| B | 8000 | `stop` | **508** | 585 字 |
| C | 40000 | （请求被接受，未报错，主动中止） | — | — |

结论：

1. **`finish_reason=length` 对纯文本是诚实的**（A）。mimo 的谎报是 **tool_call 参数流专属**——参数没写完时把 finish 标成 `tool_calls`/`stop`。故框架原有 length 路径有效；本轮"结构判据"正是补 tool_call 这条被谎报的路，方向正确。
2. **reasoning 挤占输出预算实锤**：A 请求 128 token，128 全烧在思考，可见正文 0。这直接印证 §4.1 的"reasoning + 长正文 → 工具参数被饿死截断"机制。
3. **调大 `max_tokens` 基本无用**：B 请求 8000，模型只写 508 就**自愿早停**，不是被上限卡；C 请求 40000 端点直接接受（说明 24576 未被服务端偷偷压小，我们设的数作数）。→ 瓶颈是"单次 tool_call 参数体积 + reasoning 挤占 + 模型对长参数生成不稳/自截"，不是"上限给少了"。
4. 因此有效杠杆是**减小 tool_call 参数体积 + 缩短委托前正文 + re-issue 封顶 + 框架堵严 tool_call 谎报**，而非调 `max_tokens`。

---

## 5. 附：其他观察（非致命，但影响体验）

- **C. `list_files` 输出超 16000 上限** ×3（09:31:03）：模型一次性 list 超大目录，工具封顶。模型随后恢复。→ 可在 prompt/工具侧引导分批或改用 `repo_map`/`rg`。
- **`wait_agent` 单次阻塞 ≈3 分钟**（09:31:45→09:34:45）：子 agent 尚在跑时 root 阻塞等待。需确认 `wait_agent` 的超时/唤醒语义（是否应由子 agent 完成事件驱动唤醒，而非固定阻塞）。
- **prompt 侧放大截断面**：`phimint/src/prompt/mod.rs` MULTI_AGENT 片段鼓励"详细 message / 并行多 spawn"，直接增大 `arguments` 体积 → 更易被 mimo 截断。

---

## 6. 修复状态

### 6.1 已实施（第一轮，agent-base，TDD 全绿 365）
1. `react/tools.rs`：守卫加 `has_incomplete_args` 结构判据；按成因分措辞（length→"output token limit"；结构截断→"provider truncated the argument stream"+建议一次一个/缩短）。
2. `llm_engine.rs`：组装丢弃 `name=""` 幽灵桶。
- 回归测试：`react/tests.rs::truncation_guard_blocks_tool_calls_on_finish_tool_calls_mimo`（逐字复刻用户 EOF 错误）、`llm_engine.rs::process_stream_tool_call_empty_id_and_name_ignored`（断言改为丢弃）。
- 效果（`abe17003`）：0 硬终止、0 次 400、15 次截断优雅弹回、模型自恢复转直接分析并成功完成。

### 6.2 第二轮（已实施，agent-base，全 366 测试绿）
- **session.rs `push_assistant_tool_calls`**：非法/截断 args 不再包成 `{error,original_args_preview,message}` 对象，改为**存 `"{}"`**（合法、无 400、且无"毒形状"可被模型回仿）。截断说明只由 tool_result 承载。（→ 移除 §4.4 的模仿源。）
- **react/tools.rs 守卫**：新增 `is_truncation_wrapper(&Value)`——即便 args 是合法 JSON，只要含 `error:"tool_call_arguments_truncated"` 或 `original_args_preview`+`message` 形状，即判为截断弹回（纵深防御历史残留/回仿的旧对象）。
- 回归测试：`session::push_assistant_tool_calls_invalid_json_sanitized_to_empty`、`react::tests::truncation_guard_recognizes_wrapper_echo`（模型回仿 wrapper → 守卫拦截，不执行）。
- 下游 `phimint` check 通过；单一 build warning（`tool_engine.rs:596 event_rx`）为既有、与本次无关。

### 6.3 第三轮（已实施，agent-base，全 378 测试绿）
- **根因**：§6.2 把截断 args 存成 `{}` 后，模型回仿此形状，在并行轮次中吐出"真 spawn + 空 `{}` spawn"双桶。`{}` 是合法 JSON → 旧守卫（case 2/3）不拦 → 执行 → typed schema 缺必填 `task_name/message` → `argument parsing failed: {}`。**这是我的 sanitizer 引入的新 echo 面，不是 provider 乱吐第二个桶**（同 run 里 repo_map×4/list_files×5 等多桶轮次全部正常执行，`{}` 仅出现在 spawn_agent，且紧跟在 sanitizer 命中之后）。
- **`tool_engine.rs`**：新增 `ToolEngine::tool_requires_params(name)` —— read-lock 读 `registry.get(name).schema()["required"]`，判断该工具是否有必填字段。
- **`react/tools.rs` 守卫**：新增 case 4 —— 具名调用 + args 解析为合法空对象 `{}` + 该工具有 `required` 字段 → 当退化调用走良性重发（不计失败，不执行）。措辞："it carried an empty argument object `{}`, but this tool requires fields"。无 `required` 的工具（如 `list_agents`）`{}` 合法 → 照常执行。
- 回归测试：
  - `truncation_guard_blocks_empty_args_for_required_field_tool`：`SpawnLikeTool`（required `[task_name,message]`）+ 双桶 `[真,{}]` → 断言命中 case 4 重发、无 `argument parsing failed`。
  - `no_arg_tool_with_empty_object_args_executes_normally`：`NoArgTool`（无 required）+ `{}` → 断言正常执行、不被重发（守卫生命线：不误伤无参工具）。
- 下游 `phimint` check 通过；`cargo test -p agent-base` 378 passed /5 ignored /0 failed。

---

## 7. 后续修复方案（第三轮·未实施）

1. ~~session.rs 停止包合法 wrapper~~ —— **已完成（§6.2）**。
2. ~~react/tools.rs 守卫识别 wrapper~~ —— **已完成（§6.2）**。
3. ~~`spawn_agent {}` 空对象回仿~~ —— **已完成（§6.3）**：schema-aware 守卫拦截具名 + 空对象 + required-field 的退化调用。
4. **re-issue 计数封顶 + 升级引导**：同一意图连续 2 次截断后，不再重复"重发"，改注入兜底指令："此参数反复被截断 → 拆小任务 / 大幅缩短 message / 直接执行而非委托"。把 §3 的 turn 4→33 空转压成 2 turn 决策。（对应问题 A 的"浪费"。）
5. **prompt 侧**（phimint，低风险）：MULTI_AGENT 片段收敛"详细大 message"的鼓励，`spawn_agent.message` 只放路径+一行目标，细节交子 agent 自身 prompt。减小 `arguments` 面 → 降截断概率。
6. **选择性执行（可选进阶）**：当前 case 4 和前三条一样是整轮重发（真 spawn 也跟着被退回）。若要更外科手术式（只丢退化 `{}` 桶、真桶照常执行），需拆分 `run_tool_turn` 的"全有或全无"分支，是更大改动。视并行 spawn 的重发频率决定是否值得。

---

## 8. 验证方法（复现抓包）

用录制透传代理抓 mimo 的字节级请求/响应，无需读取 `.env` 明文凭证：
```
UPSTREAM_BASE=https://token-plan-cn.xiaomimimo.com  # 注意 host 根，不带 /v1（避免 /v1/v1）
python3 /tmp/record_proxy.py                         # 监听 127.0.0.1:8899 → 写 /tmp/phimint-capture/
# 将 LLM_BASE_URL 临时指向 http://127.0.0.1:8899/v1，在 phimint 里复现同类"多 spawn"请求
```
- 每个 LLM 轮次产出 `req_*.json`（请求体）+ `sse_*.txt`（原始 SSE 逐行）。
- 判据脚本 `/tmp/analyze_wait.py`：对每个具名桶累积 args，分类 `WIRE-TRUNCATED`（累积 args 非合法 JSON → provider 侧截断）/ `SINGLE-FRAGMENT` / `PHANTOM`（无 name 无 id）。
- **复现结束务必**：把 `.env` 的 `LLM_BASE_URL` 改回 `https://token-plan-cn.xiaomimimo.com/v1`，`pkill -f record_proxy.py`。

---

## 9. 关键代码位置索引

| 层 | 文件 | 关注点 |
|----|------|--------|
| 引擎-流聚合 | `agent-base/src/engine/runtime/llm_engine.rs` | 253–272 delta 累积；353–367 组装（已加 name 过滤） |
| 引擎-守卫 | `agent-base/src/engine/runtime/react/tools.rs` | 41–70 `run_tool_turn` 截断守卫（已加结构判据 + case 4 schema-aware） |
| 工具注册表 | `agent-base/src/engine/runtime/tool_engine.rs` | `tool_requires_params(name)` —— schema-aware required-field 查询 |
| 引擎-分流 | `agent-base/src/engine/runtime/react/turn_dispatch.rs` | 64 `from_raw`；242/255 分支；344 `handle_incomplete_tool_call` |
| 会话-历史 | `agent-base/src/engine/session.rs` | 265–316 `push_assistant_tool_calls`（截断 args 存 `"{}"`，已实施） |
| 工具执行 | `agent-base/src/engine/runtime/tool_engine.rs` | 407–484 phase1 `from_str`；433–440 `ToolArgsInvalid` |
| 类型 | `agent-base/crates/agent-types/src/execution.rs` | 11–30 `FinishReason`；96 `is_truncated` |
| 协议 | `llm-providers/.../protocol/openai/protocol.rs` | `convert_delta`；`finish=length` 才报截断 |
| 多agent工具 | `phi-kernel-tools/src/multi_agent/{spawn,wait,send,close}_agent.rs` | args schema |
| prompt | `phimint/src/prompt/mod.rs` | 195–203 MULTI_AGENT 片段（放大截断面） |
