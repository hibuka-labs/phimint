# Phase 9 实施计划（CLAUDE.md + auto-memory）

把 phimint 从「无记忆」接到「和 Claude Code 共享同一套记忆体系」——**L1 静态指令**（CLAUDE.md 启动常驻注入）+ **L2 动态记忆**（auto-memory，`~/.claude/projects/<slug>/memory/`，方案 A）。遵循 7a 决策：完全复用 `.claude/` 目录，零迁移成本，切换 Claude Code 无压力。

## 背景（现状盘点，2026-08-17 查证）

- **CLAUDE.md 注入完全不存在**：phi-agent 和 agent-works 全源码无 `CLAUDE` 字样，Phase 9 从未实现。
- **框架已有 memory 提示词但两处过时/断层**：
  - `agent-works::build_memory_system_prompt()`（`agent-works/src/builder.rs:629`）已教 agent 用 `read_file`/`write_file` 管理 `.phi/memory/`（MEMORY.md 索引 + frontmatter `.md` 格式）。**路径还是 `.phi/memory/`**，与 7a 决策矛盾，未改。
  - 该 prompt **只有 phi-agent 框架默认 prompt（`prompt.rs:43`）调用**；phimint 用自定义 `SYSTEM_PROMPT`（`src/agent.rs` const）完全覆盖框架 prompt → **phimint 现在收不到任何 memory 指令**。
- **无记忆机制层**：纯 prompt-injection，无 `memory` 工具；frontmatter 校验、MEMORY.md 索引同步、去重全靠 LLM 自觉（易写坏、易失联）。
- **Claude Code 的 auto-memory 格式**（方案 A 目标）：`~/.claude/projects/<slug>/memory/`，slug = 工作区绝对路径 `/` → `-`（`/Users/.../phimint` → `-Users-...-phimint`）；`MEMORY.md` 索引（`- [Title](file.md) — hook`）+ 每个记忆一个 frontmatter `.md`。启动时只注入 MEMORY.md 索引，相关记忆按需 recall。
- **已有经验**：phimint 自己的记忆文件就在 `/Users/kangzengchen/.claude/projects/-Users-kangzengchen-source-buka-buka-works-phimint/memory/`，可直接当格式真值参考。

## 设计决策

1. **L2 记忆路径＝方案 A**：`~/.claude/projects/<slug>/memory/`，完全对齐 Claude Code 真实格式 → 与 Claude Code **互通**（phimint 写的记忆 Claude Code 能读，反之亦然）。不做项目内 `.claude/memory/`（B：与 Claude Code 不互通）。
2. **机制归属＝框架层（agent-works）**：和 skills 同一位置，其他 phi 系产品都能用；phimint 只接线。这符合「高内聚低耦合」。
3. **L1 CLAUDE.md＝框架默认能力 + consumer 接缝**：框架提供「注入 project + user CLAUDE.md」的默认行为；phimint 用自己的 SYSTEM_PROMPT 时，通过接缝附加（见下）。

## 分阶段

### 9a CLAUDE.md 启动注入（L1 静态指令）

**目标**：启动时把 项目根 `CLAUDE.md` + 用户 `~/.claude/CLAUDE.md` 注入 system prompt，与 Claude Code 共享同一份指令文件。

- **读取**：`build_claude_md_prompt(workspace)` —— 读 `~/.claude/CLAUDE.md`（用户，低优先）+ `<workspace>/CLAUDE.md`（项目，高优先），拼接成一段注入。文件不存在 → 空。
- **框架默认接缝**：`build_system_prompt()`（phi-agent `prompt.rs`）现在无参。改为提供 `build_system_prompt_with(workspace: Option<&Path>)`（或加 `system_prompt_for_workspace`），框架默认路径自动附加 CLAUDE.md 段；无参版本保持原样（兼容现有调用）。
- **phimint 接线**：phimint 用自定义 `SYSTEM_PROMPT` 覆盖了框架 prompt → **问题：memory/CLAUDE.md 注入会丢**。修法：`src/agent.rs::build()` 里把 `SYSTEM_PROMPT` 后面 append `crate::memory::build_claude_md_prompt(&workspace_root)` + `agent_works::build_memory_system_prompt()`（或统一封装成 phimint 自己的 `build_phimint_prompt(workspace)`）。
- **验证**：启动 phimint，system prompt 含 project CLAUDE.md 内容；无 CLAUDE.md 时行为不变。

### 9b auto-memory 机制（L2 动态记忆，框架层）

**目标**：真正的记忆机制，不是纯 prompt 靠 LLM 自觉——`memory` 工具 + 启动注入索引。

- **slug 生成**（复刻 Claude Code）：`workspace 绝对路径` → 去尾 `/` → 每段前缀 `-` → `/` 全换 `-`。`/Users/kangzengchen/source/buka/buka-works/phimint` → `-Users-kangzengchen-source-buka-buka-works-phimint`。放 agent-works 新模块（如 `agent-works/src/memory.rs`），`pub fn project_slug(root: &Path) -> String` + 单测。
- **目录**：`dirs::home_dir().join(".claude/projects").join(slug).join("memory")`。`agent-works` 已有 `dirs_next` 依赖（skills 扫描在用），复用。
- **`memory` 工具**（framework 注册，agent 可调用）：
  - `memory_write { name, description, type, content }` —— 校验 frontmatter + 写 `<name>.md` + **自动更新 MEMORY.md 索引**（若 name 已存在 → 更新文件体，索引不动；新增 → append 索引行）。
  - `memory_list` —— 返回 MEMORY.md 索引全文（比让 agent read_file 更稳）。
  - `memory_read { name }` —— 返回单个记忆文件全文。
  - `memory_delete { name }` —— 删文件 + 删索引行。
  - 前端是 Tool trait，复用 agent-works 工具注册机制（参考 `prompt_skill` 那套）。
- **启动注入索引**：`build_memory_system_prompt()` 改为**接受 memory 路径**，启动时把「记忆目录在哪 + MEMORY.md 索引全文」注入 system prompt（索引小，全量注入；具体记忆内容按需 `memory_read`）。路径写死 `.phi/memory/` → 改方案 A 真实路径。
- **提示词同步改**：`build_memory_system_prompt()` 全文更新为「用 `memory_write`/`memory_read` 等专用工具管理记忆，而非 read_file/write_file」+ 更新目录路径 + 加「`description` 是 recall 关键、同类记忆 update 不重复建、错记忆 delete」等纪律（对齐 Claude Code 的 working-principles 教训）。

### 9c phimint 接线（consumer 侧）

- `src/agent.rs::build()` 组合最终 system prompt：`SYSTEM_PROMPT` + CLAUDE.md 段（9a）+ memory 指令（9b，含当前 workspace 的记忆索引）。
- `Cargo.toml`：`agent-works` 的 features 确认含 memory 工具注册所需（类似现有 `skill`/`prompt_skill` feature）。
- 验证：启动 → system prompt 能看到本项目的 MEMORY.md 索引 + 记忆工具可用；`memory_write` 真机调用 → `~/.claude/projects/-Users-...-phimint/memory/` 出现新文件 + MEMORY.md 同步。

## 明确不做

- **不做向量/语义检索**：recall 靠 MEMORY.md 索引 + LLM 判断（Claude Code 同款），不需要 embedding。
- **不做跨会话自动写入**：写入仍靠 LLM 自觉 + 用户显式「记住」，不自动 dump 会话。
- **不做记忆去重/合并的确定性算法**：交给 LLM（提示词纪律），工具只保证「同名即更新」。

## 依赖 / 风险

- **`build_system_prompt()` 签名变化**：加 workspace 参数的版本，旧无参版本保留 → 兼容现有调用方（`phi` bin、serve/init/bridge_serve）。
- **phimint 覆盖框架 prompt 的断层**：9a/9c 必须一起做，否则只改框架 phimint 仍收不到。
- **`dirs_next` vs `dirs`**：agent-works 已有 `dirs_next`，统一用，避免双依赖。
- **MEMORY.md 索引竞态**：多 turn 并行写记忆需串行化——工具内对索引的 read-modify-write 加锁（`Mutex`），v1 用 `tokio::sync::Mutex` 或 std `Mutex`（工具执行是 async，注意跨 await）。

## 验证

- **9a**：放一段唯一标记进 `<workspace>/CLAUDE.md`，启动 phimint，system prompt / 首 turn 上下文出现该标记；删掉后消失。
- **9b**：`memory_write` 写一条 → `~/.claude/projects/<slug>/memory/` 生成 `<name>.md` + MEMORY.md 增加索引行；`memory_read` 读回；同名 `memory_write` 更新不重复；`memory_delete` 清文件 + 索引行。单测覆盖 slug 生成、frontmatter 校验、索引同步、增改删。
- **9c（真机互通）**：phimint 里写一条记忆，切到 Claude Code 同工程启动，能读到该记忆（方向 A 的卖点）；反之亦然。
- **回归**：无 CLAUDE.md / 无记忆目录时行为与现在完全一致；单测全绿 + `cargo build` 干净。
