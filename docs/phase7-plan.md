# Phase 7 实施计划（Skills 生态接入）

把 phiforge 从「有 skill 机制、但加载 0 个」接到「能复用 Claude Code 的 skills 生态 + 用户能手动触发 + 开箱自带一批」。

## 背景（现状盘点，2026-08-16 查证）

- **机制已就绪（prompt-injection 懒加载）**：`base_agent_builder_with_excludes` 启动时扫 `~/.claude/skills` + `./.claude/skills`，每个 `skill-name/SKILL.md`（YAML frontmatter + markdown）注册为 `PromptSkill`；`LazySkillPrompter` 只在 system prompt 里放「名字+描述+路径」的紧凑清单，LLM 要用时 `read_file` 读完整 `SKILL.md`。这正是 Claude Code / Codex 的「渐进式披露」做法。
- **格式已兼容**：`PromptSkill` 的 frontmatter 照 Agent Skills 开放标准（agentskills.io）实现，是 Claude Code 的超集（`name`/`description` + `allowed-tools`/`disallowed-tools`/`model`/`user-invocable`/`disable-model-invocation`/`arguments`/`context`/`paths`），未知字段忽略。
- **四个缺口**：
  1. **目录已对齐**——框架默认扫 `.claude/skills` + `~/.claude/skills`，Claude Code 生态直接复用。
  2. **高级字段是「死字段」**——`allowed-tools`/`context: fork`/`paths`/`arguments`/`user-invocable` 只被解析并存着，运行时**无消费**（只有 prompter 用 `name`+`description`）。
  3. **无 `/skill` 入口**——TUI/inline 输入栏把 `/xxx` 当普通文本提交，无斜杠分发。
  4. **无自带 skills**——机制在、内容空。
- **上下文压缩已独立就绪**：`SummarizingMiddleware` 默认开（30k token 触发 LLM 总结）；`ContextWindowManager`（硬截断）未开（`has_context_window:false`）。

## 分阶段

### 7a 目录对齐（已完成）

**目标**：phiforge 直接复用 Claude Code 目录，不建 `.phi/`。

**做法**：框架（phi-agent `builder.rs`）默认扫描 `.claude/skills` + `~/.claude/skills`，phiforge 无需额外配置。用户把 skill 放在 `.claude/skills/<name>/SKILL.md` 即可同时被 Claude Code 和 phiforge 发现。

**任务**：~~已通过修改 phi-agent 默认值完成。~~

### 7b `/skill` 斜杠入口（已完成）

**目标**：用户输入 `/skill-name args` 能手动触发 skill，把 `SKILL.md` body（resolve 掉 `$ARGUMENTS`/`$name` 参数）注入上下文。

**做法**：
- `SkillResolver`（`src/skills.rs`）扫描 `.claude/skills` + `~/.claude/skills`，提供 `resolve()` + 模糊匹配。
- agent_loop 在 `run_turn` 前把 `/skill-name args` 解析成 skill body 再提交。
- TUI `/` picker（`SlashPicker`）弹出已加载 skill 的名字 + 描述，方向键选择、Enter 确认、Esc 取消。
- 未命中：当普通文本提交（不打断现有 `/xxx` 文本的宽松性）。

**模糊匹配优先级**：exact → suffix（`code-review` 命中 `requesting-code-review`）→ contains → word-overlap；同级取最短名。

**验证**：`/commit` 触发 → 上下文出现该 skill body；`/不存在的` 当普通文本。

### 7c 执行语义（可选，重，框架层）

**目标**：把 `allowed-tools`/`disallowed-tools`（工具门控）、`context: fork`（子 agent 隔离执行）、`paths`（改文件匹配才激活）从「死字段」变成真语义。

**定位**：这是框架（agent-works）的新功能，非 phiforge 侧接线。价值中高（fork 隔离防污染、工具门控保安全），但成本高、需与 phi-agent 上游对齐。**后置**，先做 7a/7b/7d，真需时再上。

### 7d 自带一批 skills（可选，内容工作）

**目标**：开箱即用。首批 3 个小型、高频：
- `commit`——跑 verify + 生成 conventional commit message。
- `code-review`——review 当前改动（diff + 潜在 bug）。
- `explain`——解释某段代码/报错。

**任务**：写 `.claude/skills/{commit,code-review,explain}/SKILL.md`（走 Claude Code 同款格式，天然兼容）。

### 7e 上下文硬截断兜底（可选，顺手，独立）

**目标**：给 `SummarizingMiddleware` 加一层确定性硬兜底（`has_context_window:false → true`）。

**做法**：`agent.rs` 加 `.context_window(128_000)`（或按模型 `max_context_tokens` 取），`ContextWindowManager` 保留首尾删中间。与 skills 无关，是「上下文不超」的另一半，可随手做或后置。

## 优先级 & 顺序

```
7a 目录对齐 ──▶ 7b /skill 入口 ──▶ 7d 自带 skills ──▶ 7c 执行语义（重，后置）
                                    7e 硬截断（独立，顺手）
```

- **7a 已完成**：框架默认值改为 `.claude/`，无需暴露配置口。
- **7b 已完成**：`SkillResolver` + 模糊匹配 + `/` picker（名字 + 描述）。
- **7d 轻量**：内容，随时可插。
- **7c 重**：框架层新功能，单独评估、后置。

## 待定决策（实施时定）

1. **~~7a 方案~~** 已定：框架默认值直接改 `.claude/`，不加配置口。
2. **skill 注入方式**：`/skill` 触发后 body 作「额外 user 消息」还是「system 注入」。（倾向 user 消息，改动最小、可观测。）
3. **7c 归属**：框架（agent-works）还是 phi-agent 上游做？需与上游同步，避免 fork 分叉。

## 依赖 / 风险

- ~~phi-agent 未 re-export skill 类型~~ → 7a 已完成（改框架默认值），7b 已接入 `agent-works::skill::{PromptSkill, Skill}`（经 `agent-works` 的 `skill` feature）。
- `PromptSkill::from_markdown` 用 `Box::leak` 存 `&'static str`（一次性启动加载可接受，见其注释）——热重载需注意。
- Claude Code skill 的 `name` 要求 kebab-case（字母数字+连字符），`PromptSkill` 已校验，兼容。
- 7c 若做 fork，需复用现有多 agent（`spawn_agent`/`ChildPermissionMode`）路径，注意与「子 agent 只读」硬闸门的关系。

## 验证

- 7a：`~/.claude/skills/<skill>/SKILL.md` 放入后，启动 phiforge，`session.log` 出现 `auto-loaded skill`，system prompt 清单含其名。
- 7b：输入 `/commit`，上下文出现 commit skill body；`/不存在的` 当普通文本。
- 7d：三 skill 各跑一次真机 smoke。
- 7e：构造超长对话看 `has_context_window:true` + trim 生效（或单测覆盖 `ContextWindowManager::trim`）。
