# Phase 10 实施计划（`@` 文件提及 / 路径选择器）

把 phiforge 从「手动拷贝路径」接到「输入框敲 `@` 快速选一个文件/目录路径，选中即插进输入」。本质是一个**文件浏览器选择器**，自动管理路径，省去用户手动拷贝。核心场景：参考工作区外的项目（`@../../demo/codex/`）。

## 背景（现状盘点，2026-08-16 查证）

- **输入链路**：`src/ui/input.rs` 的 `Composer` 是纯文本缓冲，无任何 `@` 感知；`run_tui`（`src/ui/mod.rs`）收到 Enter → `Action::Submit(text)` → `Cmd::Run(text)` → `agent.run_turn(session, &text, ..)`，整段当普通文本喂给 LLM。
- **读工具被锁在工作区内**：
  - phiforge 侧 `repo_map` / `search_content` 用 `validate_workspace_path`（`src/tools/mod.rs:26`），拒绝 `..` 越界 + 绝对路径。
  - 框架侧 `read_file` / `list_files`（`base_agent_builder` 注册，phi-kernel-tools 私有 `resolve_path`）行为一致，同样拒绝越界。
- **但读侧的锁不是安全边界**：agent 已有 `execute_command`（shell），`auto` 模式下本来就能 `cat` 任意文件。锁是「组织/聚焦」边界（让 `repo_map`/`list_files` 无参时知道「当前项目」是什么），不是能力边界。

## 设计决策：读写都放开、无沙箱（对齐 Claude Code）

- **读写都放开**：`read_file` / `write_file` / `edit_file` / `repo_map` / `search_content` / `list_files` 全部允许工作区外路径（绝对路径 + `..`）。
- **安全靠审批层，不靠路径边界**：`auto` 模式下 agent 可读写磁盘任意处（同 Claude Code 默认），`ask`/`deny` 审批是真正的控制。不加任何 `allow_external_*` flag。
- **默认范围不变**：`repo_map`/`search_content` 不带 `path` 时仍默认 `.`（工作区），保持聚焦；只有**显式给出的路径**（含 `@` 进来的）才落到工作区外。
- 理由：锁从来不是安全边界（`execute_command`/shell 在 `auto` 下本来就能读写任意文件），锁只是「组织/聚焦」边界。与其维护一个可被 shell 绕过、还拦住了合法「参考兄弟项目」场景的假沙箱，不如直接对齐 Claude Code，把安全明确交给审批层。

## 分阶段

### 10a `@` 路径选择器（TUI，纯 UI 层）

**目标**：敲 `@` 进入「路径选择」态，输路径 + 实时列目录 + 上下选择，选中把路径当普通文字插进输入。

- **触发**：`Composer` 里输入 `@` 进入 mention 态，`Esc` 取消并退回普通输入。
- **交互**：打 `@` 后继续输路径（支持 `..` 回退、`/` 下钻、绝对路径），旁边 popup 实时列「当前前缀对应目录」的文件 + 子目录，`↑↓` 移动、`Enter`/`Tab` 选中、打字过滤。
- **候选来源**：`std::fs` 递归遍历目录（复用 `target` / `node_modules` / `.git` 排除），或 `rg --files`。工作区外路径直接按绝对路径列。
- **插入**：选中后 `composer.insert_str` 插入——工作区内 → 相对路径（`src/gate.rs`）；工作区外 → 绝对路径（`/Users/.../demo/codex/`）。插入后是普通文字，走既有 `Cmd::Run` 提交，**不碰 turn / 不碰框架**。
- **回退边界**：`..` 可一路回退到文件系统根，工作区根**不是**地板（与「读放开」一致）。

### 10b 工具放开（跨框架，读写都放）

- **phiforge 侧**：`validate_workspace_path`（`src/tools/mod.rs`）直接放宽——不再拒绝 `..` 越界、放行绝对路径，只拒绝空路径。`repo_map` / `search_content` / `diagnostics` 调用点不变（它们把结果传给 `rg` 或 `root.join`，绝对/`..` 路径语义本就正确）。
- **框架侧**：phi-kernel-tools 的 `resolve_path`（`file/mod.rs`）直接放宽——绝对路径原样用、相对路径 join 到 workspace、不再拒绝越界。`read_file` / `write_file` / `edit_file` / `list_files` 四个工具的描述 + schema + metadata 同步更新，让 LLM 知道「工作区相对或绝对路径」都允许。
- **写工具同样放开**：`write_file` / `edit_file` 也可写工作区外（同 Claude Code）。

## 明确不做

- **不注入文件内容**：只插路径，agent 自己 `read_file` / `repo_map`（拉取式架构，不重复实现读）。
- **不做 fzf 模糊补全**：v1 是「输路径 + 列目录」的浏览器式导航；若体验不够再补模糊搜索。
- **inline 模式不做**：`--inline` 另行讨论删除（用户已表态想删），`@` 只在 TUI。

## 已定决策

1. **放开方式**：直接放宽框架（`resolve_path`）+ phiforge（`validate_workspace_path`）的路径校验，读写都放，**不加 flag**。不自注册覆盖。
2. **工作区外路径插入形态**：绝对路径（区内仍相对路径）。
3. **安全模型**：无工作区沙箱，安全交给审批层（`auto`/`ask`/`deny`），对齐 Claude Code。

## 依赖 / 风险

- **框架 `resolve_path` 的越界拒绝**：已直接放宽（`file/mod.rs`），不再有 flag 穿透链路。
- **`auto` 模式能力扩大**：读写工具放开后，`auto` 下 agent 可读写磁盘任意处（同 Claude Code 默认）；危险动作仍走审批（`ask`/`deny`）。用户已知悉并接受，文档同步标注。
- **大目录列出的性能**：工作区外点到一个超大目录时，`std::fs` 全量列会慢；需限制单次列出的条目数（`mention.rs` 的 `MAX_ENTRIES=200` 已做）。

## 验证

- **10a**：真机 TUI 敲 `@` → 输 `../../demo/codex/` → 选中 → 输入框出现路径 → agent 能 `repo_map`/`read_file` 读到 codex 项目。
- **10b**：`read_file("../../demo/codex/README.md")` 成功；`write_file("../outside.txt")` 也成功（读写都放开）；`search_content` 带 `path="../.."` 能搜外部。
- **回归**：工作区内相对路径行为不变；`repo_map`/`search_content` 无参默认仍工作区；单测全绿 + `cargo build` 干净。
