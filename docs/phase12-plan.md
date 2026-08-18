# Phase 12 — TUI 输出优化（向 Claude Code 看齐）

> 目标：缩小 phimint 与 Claude Code 在终端输出体验上的差距，提升信息密度、降低噪音、增强可读性。
> 对标：Claude Code v2.1.234 终端输出（见 `tmp/claude.txt`）

---

## 背景

对比 Claude Code 和 phimint 的终端输出（`tmp/claude.txt` vs `tmp/phimint.txt`），主要差距：

| 维度 | Claude Code | phimint 现状 |
|---|---|---|
| AI 正文格式 | Markdown 渲染（标题/表格/代码块/分割线） | 纯文本，markdown 语法原样输出 |
| Thought | `Thought for 5s (ctrl+o to expand)` 折叠 | 原始 thinking 文本直接显示 |
| 工具调用 | 简洁格式，参数不展示 JSON | 完整 JSON 参数 `{"path":"Cargo.toml"}` |
| 工具结果 | 内联，无截断 | 截断已修复（wrap_width-20），但格式仍粗糙 |
| 用户输入前缀 | `❯`（单箭头） | `❯ ❯`（双箭头，bug） |
| 完成状态 | `✻ Sautéed for 31s` | 无 |

---

## 分阶段

### 12a Markdown 渲染（✅ 已完成 → pulldown-cmark 自定义渲染）

**目标**：AI 输出中的 markdown 语法实时渲染为带样式的 ratatui Text。

**实际方案**：放弃了 tui-markdown，改用 pulldown-cmark 自定义渲染（参考 codex 项目）

- **依赖**：`pulldown-cmark = "0.13"`（CommonMark 解析器）
- **实现**：`src/ui/render.rs` 中的 `MarkdownWriter` 状态机
- **渲染特性**：
  - 标题：无 `#` 前缀，不同级别不同样式（H1 青色+下划线，H2/H3 加粗）
  - 表格：box-drawing 边框 `┌┬┐├┼┤└┴┘`，支持列对齐（Left/Center/Right），CJK 宽度计算
  - 代码块：带语言标签，`│` 前缀，灰色文字
  - 行内代码：青色字体（无 backticks），和 Claude Code 一致
  - 粗体/斜体/删除线：标准 markdown 样式
  - 列表：`•` 符号
  - 引用：`│` 前缀
  - 分割线：`─` 线条
  - 链接：青色下划线
  - 图片：`[Image: url]` 格式
- **视觉映射**：`visual_to_output` + `visual_lines_text` 支持 markdown 展开后的选择/复制
- **验证**：所有 markdown 元素均正确渲染，221 测试全部通过

### 12e Markdown 渲染修复（✅ 已完成）

**目标**：修复 markdown 渲染问题

- ✅ 标题 `###` 前缀问题
- ✅ 分割线 `---` 显示问题
- ✅ 表格渲染（pulldown-cmark 0.13 header 事件顺序修复）
- ✅ 行内代码样式优化

### 12b Thought 折叠

**目标**：AI 的内部推理（thinking）折叠为一行摘要，可展开查看。

- **现状**：`RuntimeEvent::ThoughtDelta` 的文本直接追加到 output，用户看到大段 `The user wants me to...` 内部推理
- **方案**：
  - thinking 开始时：插入一行 `💭 思考中…（Ctrl+O 展开）`（dimmed 样式）
  - thinking 进行中：文本存入独立 buffer，不渲染到 output
  - thinking 结束时：更新为 `💭 思考 5s（Ctrl+O 展开）`
  - Ctrl+O 切换：展开/折叠 thinking 内容（用 `app.show_thoughts` 状态控制）
- **存储**：`App` 新增 `thought_lines: Vec<String>` + `show_thoughts: bool`
- **渲染**：展开时在 `💭` 行下方插入 thought_lines（dimmed + italic），折叠时隐藏
- **验证**：thinking 期间只看到一行摘要，Ctrl+O 可展开/折叠

### 12c 工具调用/结果格式精简

**目标**：减少工具调用行的噪音，让输出更紧凑。

- **工具调用行**（`⏺` 行）：
  - 现状：`⏺ read_file {"path":"Cargo.toml"}` — 展示完整 JSON
  - 优化：只展示关键参数值，去掉 JSON 结构
  - 例：`⏺ read_file Cargo.toml`、`⏺ repo_map .`、`⏺ search_content "pattern"`
  - 实现：`one_line` 之前提取关键字段（`path`/`pattern`/`query`），拼成简洁字符串
- **工具结果行**（`  ✓` 行）：
  - 现状已修复截断宽度，格式保持不变
  - 考虑：工具名和结果之间加视觉分隔（如 `  ✓ repo_map · Repository layout (1 module, 21 files)`）
- **验证**：工具调用行不再出现 `{}`/`{""}` 等 JSON 噪音

### 12d 用户输入前缀修复 + 完成状态

**目标**：修复双箭头 bug，添加任务完成反馈。

- **双箭头 `❯ ❯`**：
  - 原因：用户消息回显时，output 区插入 `❯ {text}`，但 composer 区也有 `❯` 前缀，视觉上叠加
  - 修复：output 区的用户消息用 `❯ ` 前缀（单箭头），确认 composer 区的 `❯` 不参与 output 渲染
  - 需要排查 `app.rs` 中 `LineKind::User` 的插入逻辑
- **完成状态**：
  - AI 任务完成时，在 output 区末尾插入 `✻ 完成 · 耗时 XXs`（绿色）
  - 从 `AgentStatus::Running` 切换到 `Idle` 时计算耗时
  - 参考 Claude Code 的 `✻ Sautéed for 31s`
- **验证**：用户输入只显示一个 `❯`，任务完成后有耗时提示

---

## 明确不做

- **不做 Mermaid/图片渲染**：TUI 环境无法展示，性价比低
- **不做 tree-sitter 代码高亮**：syntect 足够，不重复引入
- **不做完整折叠/展开系统**：只做 thought 折叠，其他内容不折叠

## 依赖

| 依赖 | 版本 | 用途 |
|---|---|---|
| pulldown-cmark | 0.13 | CommonMark 解析器，自定义 markdown 渲染 |

## 风险

- **tui-markdown 的 ratatui-core 版本兼容性**：tui-markdown 用 ratatui-core 0.1，phimint 用 ratatui 0.30。需要验证 `Text`/`Line`/`Span` 类型是否互通。如果不通，考虑升级 tui-markdown 或在边界做转换。
- **syntect 编译时间**：syntect 带语法定义文件，首次编译会增加 30-60s。后续增量编译无影响。
- **流式样式跳变**：正在流式的行从纯文本变为 styled，用户可能注意到。可接受（Claude Code 也有类似行为）。
