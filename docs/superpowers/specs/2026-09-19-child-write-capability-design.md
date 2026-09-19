# 子 agent 写能力（Child Write Capability）设计

日期：2026-09-19（v3，含 eng-review + 外部声音修订）
状态：已评审
范围：agent-base（1 字段）/ agent-works（能力解析、开关、写门）/ phi-kernel-tools（spawn schema）/ phimint（配置 + TUI 来源标识）

修订记录：
- v2（eng-review）：发现1 notes 排除 / 发现2 AllowAlways 表述+回归测试 / 发现3 动态 schema 记入限制 / 发现4 解析器结构体返回+规则单点 / TODO-1 preset 糖升级实施（D5）/ TODO-2 写门升级实施（D6）
- v3（外部声音 10 条，逐条核实）：发现1+5 → 实现载体改**排除集调整**（D2）；发现2+3 → `child_read_only` 默认**保持 true** + `read_only` **强制化**（D3/D3.1，§8 补语义收紧脚注）；发现4 → 父 prompt 任务分界指引入清单；发现6 → 写子 agent 工具面审计记录（§3）；发现7 → 子 session "always" 随 session 蒸发记入 D4；发现8 → §6.2 定时器修正（300s 审批超时默认 Deny）；发现9 → 能力回显返回通道入清单；发现10 → phimint 先 ask 后 auto（D1 节奏决策）

## 1. 问题与目标

### 1.1 现状

phimint 的子 agent 是四层锁死的只读体：

| 层 | 位置 | 机制 |
|---|---|---|
| 工具硬门 | `phimint/src/agent.rs:407-413` | `child_excluded_tools` 全局排除 `write_file`/`edit_file`/`execute_command`/`task_output`/`task_cancel` |
| 提示层 | `agent.rs:420` | `child_read_only: true` → 所有子 agent 注入 read-only nudge |
| spawn 工具 | `phi-kernel-tools/src/multi_agent/spawn_agent.rs:147` | `full_permission = false` 硬编码 |
| schema | 同上 | 无工具集/能力参数，LLM 无法表达"这个任务需要写" |

框架层（agent-works）已具备全部基础设施：per-child 白名单（`ChildConfig.tool_names` + "先排除再白名单"管线）、审批委托（子 agent 的 approval 请求路由到父 handler）、四个 preset（`coder`/`tester` 含写工具）、`write_tools` 部署配置（`ControlConfig`，默认 `write_file`/`edit_file`/`execute_command`）。

### 1.2 造成的损失

1. **写是父 agent 的串行瓶颈**：多文件独立修改任务中，子 agent 并行调研完后，diff 只能写在报告里由父 agent 串行重打，并行优势在最后一步归零。
2. **报告→重打的信息损耗**：子 agent 看到的精确行号/上下文经文字转述再重建，耗 token 且易错位。
3. **无法跑测试**：`tester` 类任务（写测试 + `cargo test` 验证）做不到。

### 1.3 目标

- **产品层（phimint）**：子 agent 可直接写代码、跑测试。
- **框架层（能力沉淀）**：写能力是部署无关的通用机制——运维 agent 等后续部署只改配置（自己的 `write_tools`/排除表），不改框架。
- **审批**：子 agent 的变更操作获得与主 agent 一致的用户确认体验（`ask` 弹确认并**标明来源子 agent**；`auto` 跟随主 agent 静默；`deny` 一律拒绝）。
- **并行安全**：子 agent 之间的文件级写入互斥，任务分界失守从静默交错变为响亮失败。

## 2. 核心设计决策

### D0：放开节奏（phimint）

phimint **先只在 ask 模式放写子 agent**（`allow_child_write = policy.is_some()` 的临时形态；auto 模式子 agent 暂保持只读），手动验收稳定后翻一行常量放开 auto。框架零改动，可逆。

### D1：能力（capability）与审批（approval）正交

**LLM 只选择工具面（能调什么），永远不选择审批语义（调用时谁批准）。**

- 新增的 spawn 参数只映射到工具面（per-spawn 排除集 / 白名单）。
- `full_permission` 在工具层保持硬编码 `false`，继续由现有链条解析（`ChildPermissionMode` × `AgentAutonomy`，`build.rs::spawn_permission`）。

因此"审批像主 agent 一样"不需要任何新机制，是现有配置矩阵的自然结果：

| phimint 审批模式 | policy | 子 agent 拿到 | 子 agent 写文件时 |
|---|---|---|---|
| `ask` | 有 | 父 policy + 父 approval handler（委托） | TUI 弹确认（走既有 QueuedApprovalHandler 队列），带来源标识（见 D4） |
| `auto` | 无 | `ChildPermissionMode::Full` → AllowAll | 静默（与主 agent auto 行为一致，**有意为之**；按 D0 验收后才放开） |
| `deny` | 有 | DenyAll handler | 一律拒绝 |

> 注：子 agent 不因持有写工具而获得任何审批豁免。能力放开 ≠ 提权。

### D2：能力词汇 + 排除集调整（实现载体）

spawn schema 暴露的是**语义能力**，不是工具名列表：

```
tools: "read_only"（默认） | "write" | <preset 名>（见 D5）
```

**实现载体（v3 修订，外部声音发现 1+5）**：解析器输出 **per-spawn 调整后的排除集**，`build.rs` 的注册循环**原样消费**（循环代码零改动——排除检查本来就是门，改的是门内的名单）：

| autonomy | capability | 解析器输出的排除集 | tool_names |
|---|---|---|---|
| `Manual` | 任意 | `child_excluded_tools ∪ write_tools` | None / preset 白名单 |
| `Auto` | `read_only` | `child_excluded_tools ∪ write_tools`（**强制真只读**，v3） | None |
| `Auto` | `write`，allow=true | `child_excluded_tools ∖ write_tools` | None |
| `Auto` | `write`，allow=false | `child_excluded_tools ∪ write_tools` | None + 降级原因 |
| `Auto` | preset（同上两行按 allow 与 preset 是否含写工具） | 同上 | preset 白名单 |

规则表述（与载体无关，保持不变）：**写能力 = `write_tools` 成员豁免自己在排除表中的条目；`read_only` = `write_tools` 强制加入排除（有牙的真只读，v3）**。

设计要点：

1. **`write` 能力不产生白名单**（`tool_names = None`）——子 agent 拿到全量业务工具减调整后排除集，不触发 §5.4 后验校验的 warn 噪音，无 ToolNotFound 耦合，不与注册循环的"排除在前"检查对抗（v2 补集白名单方案的管线陷阱，外部声音发现 1）。
2. **phimint 的排除表核心条目不用改**（另补 `notes.*` 两个变更工具，见 §3），默认 spawn 依旧只读。
3. **运维 agent 的接入方式**：部署时把自己的变更工具声明进 `write_tools`（如 `restart_service`/`update_config`）、在排除表中列出禁入工具，子 agent 请求 `tools: "write"` 即可——框架零改动。这就是能力沉淀点。
4. **`Manual` 优先级最高**：排除集恒并 `write_tools`，能力无法自我提权；Manual 合并（第一道闸）与能力调整（第二道闸）各自独立生效。

### D3：部署级总开关，默认关闭

`MultiAgentConfig` 新增：

```rust
/// 是否允许子 agent 通过 spawn 参数请求写能力。默认 false：
/// 框架升级对既有部署零行为变化；开启是显式的、逐部署的 opt-in。
pub allow_child_write: bool,   // 默认 false
```

- `false` 时，`tools: "write"`（或含写工具的 preset）请求**降级为只读**，解析器在返回值中携带降级原因，spawn 输出 message 中明说——延续框架 §5.4 "部署侧收紧 warn 不 error" 的先例，且输出回显避免了"假完成"陷阱（父 agent 看得到降级事实）。
- 不默认复用 `write_tools` 的存在性做隐式开关：`write_tools` 有非空默认值，隐式推导会让所有升级部署被动开门。

可配置项总览：

| 配置项 | 层 | 默认 | 语义 |
|---|---|---|---|
| `allow_child_write`（新） | `MultiAgentConfig` | `false` | 部署是否允许 LLM 为子 agent 请求写能力 |
| `write_tools`（既有） | `ControlConfig` | 三个写工具 | "写能力"映射到的具体工具名；运维部署替换为自己的 |
| `child_excluded_tools`（既有） | `MultiAgentConfig` | 空 | 禁入子 agent 的工具；写能力下仅 `write_tools` 成员可豁免 |
| `child_read_only`（既有，语义精化） | `MultiAgentConfig` | **`true` 保持不变**（v3） | 强制全员 nudge 的偏执开关（含写子 agent）；见 D3.1 |
| `child_permission_mode`（既有，不变） | `MultiAgentConfig` | `Full` | 审批语义，与能力正交 |
| `autonomy`（既有，不变） | `ControlConfig` | `Auto` | `Manual` 强制只读，压倒一切 |
| `child_write_gate`（新，见 D6） | `ControlConfig` | `true` | 子 agent 间文件级写互斥开关 |

### D3.1：read-only nudge 计算规则（v3 修订：默认值不翻）

`child_read_only` 默认**保持 `true`**。外部声音发现 2 证伪了 v2 的"翻默认严格等价"论证：框架默认部署（排除表为空）的子 agent 今天就持有写工具，nudge 是它们唯一的只读约束（`config.rs:218,220`），翻默认 = 升级即静默摘除 nudge，违反兼容承诺。

新规则：

```
nudge(child) = autonomy == Manual
            || child_read_only（强制全员 nudge 的偏执开关，默认开）
            || 解析后排除集仍含全部 write_tools（子 agent 实际无写工具）
```

- **默认部署**：`child_read_only=true` → 全员 nudge，与今日行为逐字相同（且 v3 的 `read_only` 强制化让它们的子 agent 真正拿不到写工具，机制与提示对齐）。
- **phimint**：显式设 `child_read_only: false`（替换现状的 `true`），nudge 交给计算规则——写子 agent 不再被灌输只读纪律。
- nudge 注入点在 `build_child_runtime_with_config` 中先于工具注册，按解析器输出的排除集做纯名字层面的判断即可（无需先构建 runtime）。

### D4：审批请求携带来源标识

`ApprovalRequest`（agent-base）新增可选字段：

```rust
/// 发起方标识（如子 agent 的 task_name / agent_path）。主 agent 发起时为 None。
#[serde(default)]
pub source: Option<String>,
```

- **agent-works**：子 agent 构建路径用一层薄包装 handler（`SourcedApprovalHandler { inner, source }`）委托父 handler 前填充 `source`（仅当为 None 时填充，不覆盖）。委托机制本身零改动。
- **phimint TUI**：审批弹窗渲染来源（如 `[sub:coder-1] Write file: src/x.rs`）；`source == None` 渲染与现状一致。serde default 保证序列化向后兼容。
- **AllowAlways 隔离**：approval 缓存键控在 **session_manager 的 `(session_id, action_key)`**（`tool_engine.rs:282,356-358`），子 agent 持独立 session_id，"always" 授权不泄漏到父/兄弟 session。测试钉死该不变式。
- **"always" 的生命周期（外部声音发现 7）**：授予子 agent 的 "always" 落在其用后即弃的 session 里，子 agent 关闭即蒸发——下一个子 agent 对同一操作会重新弹窗。这是隔离的代价与特性，文档化接受。

### D5：preset 语法糖（eng-review TODO-1 升级为本次实施）

`tools` 参数值空间扩展：`"read_only" | "write" | "researcher" | "coder" | "reviewer" | "tester"`。

展开语义：

1. preset 名 → `ChildPreset::by_name(name)`（未知名 → 工具调用错误，LLM 可自行换合法值重试）。
2. preset 的 `tool_names` 白名单**与 D2 的调整后排除集联用**（排除检查先跑，白名单通道现状不变）：写能力生效时 preset 的写工具条目穿过 `∖ write_tools` 的排除集存活；降级/只读时它们被 `∪ write_tools` 拦下（§5.4 warn + drop），子 agent 得到 preset 角色但只有读工具 + 降级回显。
3. preset 的角色 prompt 与既有脚手架（`CHILD_SYSTEM_PROMPT` 报告格式 + 路径纪律 + cwd）**拼接**（角色在前，脚手架在后），不替换。
4. preset 的 `max_turns` 照常生效；`model` 字段不启用（既有 TODO 不变）。

Tradeoff（文档化接受）：preset 白名单是精确的五件套（read/write/edit/list/execute，无 ripgrep/repomap/diagnostics），与 `write`（全量业务工具）形状不同。原则：**preset = 框架文档定义的精确形状**；若实战中 coder 子 agent 缺检索工具，调 preset 定义，不改展开规则。

### D6：写门（WorkspaceWriteGate）——子 agent 间文件级写互斥（eng-review TODO-2 升级为本次实施）

**问题**：多个写子 agent 并行改同一文件会静默互相覆盖，事后难归因。v1 依赖父 agent 分任务纪律，纪律失守无机制兜底。

**机制**（进程内，全部子 agent 同进程，无跨进程锁需求）：

```
WorkspaceWriteGate（agent-works，Mutex<HashMap<CanonicalPath, agent_path>>）
  子 agent 调 write_file / edit_file
    └─ GatedTool 包装器（build_child_runtime_with_config 时逐子包装）
         ├─ gate.try_claim(path, self_agent) → Ok → 委托 inner 工具执行
         ├─ 已被其他 agent 持有 → 立即 Err("file locked by <name>")（不阻塞、不等待）
         └─ 已被自己持有 → 幂等 Ok
  agent 关闭（正常 close / 超时 reaper / 错误路径）→ release_all(agent_path)
```

设计要点：

- **任务期持有**：声明在子 agent 生命周期内有效，不是单次调用期——这才是"分任务文件不相交"的机械化：第二个子 agent 一写就得到指名错误，向父报告，父重新分界。
- **try_claim 永不阻塞**：无等待即无死锁面。
- **声明即释放**：释放钩子挂在 MultiAgentRuntime 的关闭路径（close_agent + 超时 + 错误清理）。
- **父 agent 豁免**：gate 只包子 agent 的工具实例；父的协调权不受限（父改被锁文件是它自己的判断）。
- **诚实边界**：`execute_command` 的 shell 重定向在门外（包装器拦不住任意命令）——gate 覆盖 write_file/edit_file 主通道，shell 通道仍靠任务分界纪律 + 父 prompt 指引（§3 phimint 4）。
- **开关**：`ControlConfig.child_write_gate: bool`，默认 `true`；关掉即回到纯 prompt 纪律。
- **高内聚落点**：gate 与包装器在 agent-works multi_agent 模块内；phi-kernel-tools 工具层零改动；运维部署免费获得。

## 3. 分层职责与改动清单（高内聚低耦合）

依赖方向保持不变：phimint → phi-agent/phi-kernel-tools → agent-works → agent-base。每一层的改动只依赖下一层的公开接口。

### agent-base（数据层，~5 行）

- `ApprovalRequest` 加 `source: Option<String>`（serde default）。无行为。

### agent-works / multi_agent（能力、开关、写门的归属层）

1. **`ChildToolCapability` 枚举**：`ReadOnly | Write | Preset(&'static str)`（新，`multi_agent` 公开导出）。
2. **能力解析器（纯函数）**：
   - 返回 `CapabilityResolution { excluded_tools: BTreeSet<String>, tool_names: Option<BTreeSet<String>>, system_prompt_suffix: Option<String>, max_turns: Option<u32>, degraded_reason: Option<String> }`——**不用 Result 错误通道承载降级**（降级是成功解析附带说明）。
   - **排除规则单一编码点（关键接线，外部声音发现 1）**：`build.rs` 现有的排除集构造（`build.rs:124-135`，含 Manual 合并）**迁入解析器**；解析器输出的 `excluded_tools` 是 per-spawn 的，`build_child_runtime_with_config` 的注册循环**逐字消费该输出**——循环本身零改动，消费点在实施计划中单列并以集成测试锁定（"write 子 agent 能注册 write_file"是集成断言，不是单元断言）。
   - 语义矩阵见 D2 表格。
3. **`spawn_child_with_history`** 加 `capability` 参数，**并扩展返回外部声音发现 9：现签名 `Result<String,String>` 无处携带 registered/降级信息）——返回 `(agent_path, CapabilityResolution 摘要)` 或等价形态，供 spawn 输出回显。调用方仅 phi-kernel-tools 一处。
4. **nudge 计算**改 per-child（D3.1），依据解析器输出的排除集。
5. **`SourcedApprovalHandler`**（D4）。
6. **`WorkspaceWriteGate` + `GatedTool`**（D6）：gate 注册表、包装器、关闭路径释放钩子。
7. `MultiAgentConfig::allow_child_write` + `ControlConfig::child_write_gate`（`child_read_only` 默认值不动）。
8. 既有 preset / `spawn_with_config` 路径不受影响（preset fixture 测试全绿不动，充当回归哨兵）。

### phi-kernel-tools / multi_agent（schema 层，不认识具体工具名）

- `SpawnAgentArgs.tools: Option<String>`：`"read_only"`|`"write"`|四个 preset 名（schema enum 约束），解析为 `ChildToolCapability` 传给 runtime。
- spawn 输出 message 回显实际能力（含降级说明、preset 名）——数据来自第 3 条的返回通道。
- `DESCRIPTION` 补使用指引："read-only 调研任务省略 tools；需要改文件/跑命令的任务请求 write；标准人设可直接用 preset 名（审批模式下用户会逐次确认）"。
- schema 测试：参数存在性、默认值、非法值错误路径。

### phimint（产品层，消费配置 + 渲染）

1. `agent.rs`：`allow_child_write = policy.is_some()`（D0 先 ask 后 auto；验收后翻常量 `true`）；**显式设 `child_read_only: false`**（D3.1）；**排除表补 `notes.append_to_file`、`notes.write_file`**（eng-review 发现1：两个变更工具今天就在 `business_tools` 里且未排除——子 agent 本可污染父会话 notes，顺带修复；`history.*` 只读不动）。
2. **父 agent system prompt 补任务分界指引**（外部声音发现 4）：并行写子 agent 时按文件不相交分派任务；同文件冲突由写门兜底报错。
3. 审批弹窗（TUI popup）渲染 `ApprovalRequest.source`（`[sub:NAME]` 前缀或独立行）。
4. 子 agent 结果面板/`list_agents` 已含 `spawned_tools` 回显，无需改动（实现时核对）。
5. **写子 agent 工具面审计记录**（外部声音发现 6）：`write` 能力下子 agent 获得 business ∖ (excluded ∖ write_tools) = 含 SkillTool / `history.*`（只读）/ ripgrep / repomap / diagnostics。审计结论：SkillTool 仅返回技能文本、history 只读、notes 已排除——可接受，记录在案。

## 4. 数据流（ask 模式示例）

```
父 LLM ── spawn_agent{task, tools:"write"} ──▶ SpawnAgentTool
    └─ runtime.spawn_child_with_history(capability=Write)
         ├─ 解析器 ─▶ { excluded: phimint表 ∖ write_tools, tool_names: None, degraded: None }
         ├─ nudge 计算：排除集已减写工具 → 不注入 read-only nudge
         ├─ 子工具注册：write_file/edit_file 包 GatedTool(gate, agent_path)
         └─ child = builder(policy=父 policy, approval=SourcedApprovalHandler(父 handler, source))
子 LLM ── write_file(src/x.rs) ─▶ GatedTool.try_claim ─▶ Ok ─▶ policy.evaluate_approval
    ─▶ ApprovalRequest{source:"coder-1"} ─▶ QueuedApprovalHandler
    ─▶ TUI 弹窗 "[sub:coder-1] Write file: src/x.rs" ─▶ 用户 y/a/n（300s 不应答 → Deny）
另一子 agent ── edit_file(src/x.rs) ─▶ GatedTool.try_claim ─▶ Err("locked by coder-1")
    ─▶ 子 agent 向父报告冲突，父重新分界
agent 关闭 ─▶ gate.release_all("coder-1")
```

auto 模式同路径，policy 为 `None`、handler 为 AllowAll → 无弹窗（主 agent 镜像；按 D0 验收后放开）。

## 5. 测试策略

| 层 | 用例 |
|---|---|
| agent-works | 解析矩阵（Auto/Manual × allow true/false × read_only/write/preset × 排除表含/不含 write_tools 成员）——**含集成断言：write 子 agent 的注册集确实含 write_file**（外部声音发现 1 的回归锁）；降级 reason 文案；nudge 三条件（含默认部署 `child_read_only=true` 全员 nudge）；`SourcedApprovalHandler` 填充、不覆盖、Err 透传；**写门**：try_claim 幂等/指名错误/任务期持有/关闭释放/reaper 路径释放/父豁免；既有 preset fixture 回归（必须全绿不动） |
| agent-base | `ApprovalRequest` serde 向后兼容（旧 JSON 无 source 可反序列化） |
| phi-kernel-tools | schema 断言（enum 值域）、参数解析、preset 名解析错误路径、降级回显文案（数据来自返回通道） |
| phimint | 配置接线测试（含 ask-only 条件形态）；**CRITICAL 回归**：默认 spawn（无 tools 参数）注册集不含任何写工具（含 notes.*）——钉死 /review 只读不变式；notes 排除断言；TUI 弹窗有/无 source 双渲染；集成：ask 模式 spawn write 子 agent → 审批队列 item 带 source；**跨 session AllowAlways 隔离**（子 always 不影响父/兄弟，子关闭后 grants 蒸发） |
| 手工验收 | ask 模式弹窗显示来源；300s 内应答；两子 agent 写同文件第二个收到指名错误；read_only 默认 spawn 与升级前一致；验收后翻 auto 常量复验静默路径 |

遵循项目纪律：单元测试全绿后才交用户手动验收。

## 6. 已知限制与风险（v1 明确不做）

1. **写门不覆盖 shell 通道**：`execute_command` 的重定向/tee 写文件在 gate 之外。gate 覆盖主通道（write_file/edit_file），shell 通道靠任务分界纪律 + 父 prompt 指引。完整覆盖需要命令分析，明确不做。
2. **审批等待定时器（v3 修正，外部声音发现 8）**：审批等待受**逐次审批超时**约束——`APPROVAL_TIMEOUT_SECS` 默认 300 秒，超时**默认 Deny**（`tool_engine.rs:312-334`），先于 10 分钟 task_timeout 到期。用户晚答得到的是子 agent 的"缺权限"报告而非写入。此为既有主 agent 同款语义，v1 接受；需要更长决策窗口时调环境变量。
3. **auto 模式静默写**：多个写子 agent 并行无确认直接改主工作区。主 agent 镜像语义的直接推论；按 D0 在 ask 验收通过后放开。
4. **动态 spawn schema（eng-review 发现3处置）**：`allow_child_write=false` 的部署里 schema 仍宣传 write/preset 选项，LLM 可能反复请求、反复被降级（输出回显兜底）。动态 description/schema 留待真实多部署需求出现。
5. **spawn 参数面**：`tools` 参数为一个枚举字符串，不会显著加剧 efad759c 点名的 spawn 调用截断面。
6. **preset 白名单形状**：五件套不含 ripgrep 等检索工具（D5 tradeoff），文档化接受。

## 7. 备选方案（已否决）

- **v2 的补集白名单载体**：与注册循环"排除在前"的既有检查对抗，漏改消费顺序即静默空转（外部声音发现 1/5）——被排除集调整替代。
- **`child_read_only` 默认翻 false**：对默认部署构成升级即行为漂移（外部声音发现 2）——默认保持 true，phimint 显式关。
- **preset-only（不提供裸 `write` 枚举）**：LLM 无法表达"要写但不要人设"；`write` 与 preset 并存。
- **按审批模式全局开门（删排除表常驻写工具）**：只读研究员/评审也持写工具，误写面大。
- **写门做成阻塞等待**：死锁面 + 饥饿 + 空烧 token；立即失败 + 父重新分界更符合"响亮失败"原则。

## 8. 兼容性与迁移

- 所有新配置默认关闭/等价旧行为，**不 opt-in 的部署升级后零行为变化**，一处例外（v3 语义收紧，外部声音发现 3 的代价，文档化）：**默认部署（排除表为空）的子 agent 失去它们此前名义持有、但被 nudge 明令禁止使用的写工具**——`read_only` 强制化把"提示不写"变成"拿不到写工具"。这是框架自身文档立场（"children are read-only"）的机制对齐；若有部署确曾依赖子 agent 经默认工具写入（与 nudge 纪律相悖），显式请求 `tools:"write"` 即可恢复。
- `ApprovalRequest.source` serde default，旧日志/旧调用方不受影响。
- phimint 侧 `..MultiAgentConfig::default()` 结构更新语法天然兼容新字段。
- `/review` 等内部依赖只读子 agent 的场景：默认 spawn 不带 `tools` → read_only → 行为不变（CRITICAL 回归测试钉死）。

## 9. 实施阶段

- **阶段一（框架层）**：agent-base 字段 → agent-works（解析器单点 [含排除集迁入与消费接线] + nudge + SourcedApprovalHandler + allow_child_write + 写门 + preset 展开 + 返回通道）→ phi-kernel-tools（schema + 解析 + 回显）。独立可验证（fixture 回归 + 新测试矩阵 + write 注册集成断言）。
- **阶段二（产品层）**：phimint（ask-only 条件接线 + notes 排除 + child_read_only:false + 父 prompt 分界指引 + TUI source 渲染）+ 集成测试 + 手工验收；验收通过后翻 auto 常量（一行）。

## GSTACK REVIEW REPORT

| Review | Trigger | Why | Runs | Status | Findings |
|--------|---------|-----|------|--------|----------|
| CEO Review | `/plan-ceo-review` | Scope & strategy | 0 | — | — |
| Codex Review | `/plan-eng-review` 外部声音 | Independent 2nd opinion | 1 | issues_found | 10 条：2 致命/高危确认（管线陷阱、兼容论证为假），8 条采纳或机械修正，0 条驳回 |
| Eng Review | `/plan-eng-review` | Architecture & tests (required) | 1 | clean | 4 项架构/质量发现全部裁决落档；0 critical gaps；测试缺口 6 项补齐 |
| Design Review | `/plan-design-review` | UI/UX gaps | 0 | — | — |
| DX Review | `/plan-devex-review` | Developer experience gaps | 0 | — | — |

- **CROSS-MODEL:** 外部声音推翻 v2 两个核心实现声明（补集白名单载体、child_read_only 默认翻转），v3 按"排除集调整 + 默认保持 true + read_only 强制化"重建；三项跨模型分歧均经用户裁决。
- **VERDICT:** ENG CLEARED — 设计文档 v3 为实施基线，进入 writing-plans。

NO UNRESOLVED DECISIONS
