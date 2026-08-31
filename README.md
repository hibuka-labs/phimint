# phimint

**读作** `/ˈfaɪmɪnt/`（phi-mint，同英文 "fie-mint"），不是「皮敏特」。

> 基于 [phi-agent](https://github.com/) 的 AI 编码 agent —— **永远不把编不过的代码交给你**。

phimint 在终端里做一款「产品优先」的编码工具：改完先验、验过才交。它同时是 `phi-agent` 框架的真实压测场——写代码是唯一把「理解需求 → 读码 → 多文件改 → 编译/测试 → 报错迭代」整条链路跑全的场景。

## 特性

- **流式 TUI（默认界面）**：ratatui + crossterm，Claude Code 风格固定底部输入栏，逐字流式输出、工具调用内联、执行命令逐行实时回显、审批弹窗（`y`/`a`/`n`）。
- **「先验再交」强制闸门**：动过代码文件却没跑 `verify`，就拦下「报 done」、逼先验（`VerifyEnforcementMiddleware`，consumer-side、零改框架）。
- **LSP 快速内环**：手写最小 LSP 客户端，改完不重编译秒级拿到 `file:line:col` 诊断（rust-analyzer / typescript-language-server / clangd）。
- **拉取式上下文架构**：`repo_map`（tree-sitter 符号/结构索引）+ `search_content`（ripgrep），agent 主动搜、主动读，而非把整个仓库塞进 context。
- **多 agent 大任务分解**：`decompose`（判串行/并行）→ 只读子 agent 扇出调查 → `merge`（冲突检测 + 合并验证）。
- **三态审批**：`auto`（全自动）/ `ask`（写操作逐条弹窗）/ `deny`（只读）。
- **多语言**：注册制 `LanguageAdapter`，Rust / Java / TypeScript / JavaScript / C / C++ 开箱即用，加一门语言只需补配置。
- **优雅架构**：高内聚低耦合的 UI 模块设计，统一泛型补全器、事件处理分离、模块化测试文件，易于扩展和维护。

## 快速开始

### 前置条件

- Rust 工具链（edition 2024）
- 一个 OpenAI 兼容的 LLM API（OpenAI / DeepSeek / Groq / Ollama / Copilot …）
- 本仓库的 sibling crates：`phi-agent` / `phi-kernel-tools` / `agent-base` / `log-core`（与 phimint 同级目录，`Cargo.toml` 用 `path` 引用）
- （可选）`cargo` / `npm` / `make` 等构建工具，以及对应语言的 LSP server 在 `PATH` 上

### 配置

```bash
cp .env.example .env
# 编辑 .env，填入：
#   LLM_API_KEY=sk-...
#   LLM_MODEL=deepseek-chat        # 或 gpt-4o / llama3 / copilot …
#   LLM_BASE_URL=https://api.deepseek.com/v1
```

### 运行

```bash
cargo run                          # 启动 TUI（默认界面）
cargo run -- -w ../some-project    # 指定工作区目录
```

## 使用

```
cargo run -- [OPTIONS]

  -w, --workspace <PATH>   工作区目录（默认：当前目录）
      --model <NAME>       模型名（覆盖 LLM_MODEL / OPENAI_MODEL）
      --base-url <URL>     API base URL（覆盖 env）
      --approval <MODE>    auto（默认）/ ask / deny
      --shell-timeout-ms   shell 命令超时（默认 120000）
      --session <ID>       会话 ID（默认 PHI_SESSION_ID，否则自动生成）
      --inline             改用行内聊天（TUI 是默认）
      --log-level <LEVEL>  session.log 级别（默认 info）
```

### 界面与模式

- **TUI（默认）**：底部固定输入栏，输出在上方滚动，支持鼠标滚轮、括号粘贴、CJK 双宽。`Ctrl+Y` 复制最后一条 AI 回复到剪贴板；复制任意文本＝左键拖选出行范围 → `Ctrl+C`，或右键（弹出「拷贝 / 取消」菜单后**鼠标点击**或方向键+`Enter`）；退出 `Ctrl+C`（或输入后按 `Esc`/`Ctrl+D`）。
  > macOS 提示：`Cmd+C` 只在终端把该组合键转发给程序时才生效（需把终端自己的「复制」快捷键改为别的组合）。macOS 自带 Terminal.app 不支持转发 `Cmd` 修饰键**，`Cmd+C` 无法进程序，请用 `Ctrl+C` 或右键菜单。iTerm2 可在 Preferences → Keys 将 Copy 重绑到 `⌘⇧C` 后让 `Cmd+C` 生效。
- **行内聊天（`--inline`）**：不进 alternate screen，`TextDelta` 直写 stdout，适合原生终端回滚 / 管道。
- **审批三态**：
  | 模式 | 行为 |
  |---|---|
  | `auto` | 全自动，什么都不问 |
  | `ask` | 写操作逐条弹窗：`y` 放行一次 / `a` 永久放行 / `n` 拒绝 |
  | `deny` | 只读，拒绝所有写 |

### 一次典型任务

```
用户: "帮我在 lib 里加一个带缓存的 get_user 函数"
1. repo_map 拿结构 → search_content 定位 → read_file 读相关代码
2. edit_file / write_file 改代码
3. diagnostics 秒级拿类型错误（LSP 快速内环）
4. verify 跑 cargo check 拿精简错误摘要 → 修 → 重跑（验证闭环）
5. 闸门放行（验过了）→ 汇报改动
```

## 工具面

| 工具 | 作用 |
|---|---|
| `repo_map` | tree-sitter 符号/结构索引（多语言，拉取式上下文） |
| `search_content` | ripgrep 内容搜索（regex） |
| `read_file` / `write_file` / `edit_file` / `list_files` | 文件读写与精准编辑（`edit_file` 原子写 + 4 级匹配） |
| `execute_command` | shell（跑构建/测试/命令，逐行流式回显 + 超时/取消杀进程组，输出自截断） |
| `verify` | 跑构建/测试命令，把报错解析成 `file:line:col code message` 精简摘要 |
| `diagnostics` | LSP 诊断（改完不重编译的快速内环；服务缺失时降级到 `verify`） |
| `update_plan` | 复杂任务先展示结构化清单（目标+步骤+状态），随进度更新 |
| `decompose` / `merge` | 多 agent 大任务分解 / 冲突检测 + 合并验证 |
| `spawn_agent` / `send_message` / `wait_agent` / `list_agents` / `close_agent` | 只读子 agent 原语（调查并汇报，主 agent 负责所有编辑） |

## 工作原理

phimint 是 `phi-agent` 框架的 consumer：复用框架的 agent 循环 / ReAct / 审批 / 会话事件流，注入编码专属工具与策略。

- **强制 verify 闸门**：`EditTracker` 包装 `write_file`/`edit_file` 置脏、`verify`/`merge` 清脏；agent 改了代码却想「报 done」时，`on_post_llm` 压掉本次结束并注入「先 verify」，`max_nudges`(3) 后降级为「⚠️ 未验证」标记交用户仲裁。框架只提供 `skip_push`/`follow_up_message` 通用能力，编码专属策略留在 consumer。
- **多 agent**：`decompose` 用一次嵌套 LLM 调用产出 `{strategy, slices}`；`parallel` 时每 slice 一个**只读**子 agent 调查，主 agent 汇总报告后自行编辑；`merge` 按文件聚合 diff、标冲突、再跑一次编译验证。
- **LSP**：进程级单例 server（按语言），`didOpen`/`didChange`/`didSave` 同步 + `publishDiagnostics` 缓存，`diagnostics` 工具拉取。
- **多语言边界**：`lang.rs` 注册表（扩展名 → 默认 verify 命令 → manifest → LSP server）+ 通用兜底（列文件 / ripgrep / 任意 verify 命令 / 原始 stderr 尾部），任何语言零成本覆盖。

## 会话与可观测性

每次运行落一份会话到 `~/.phimint/sessions/<id>/`：

| 文件 | 内容 |
|---|---|
| `session.log` | tracing 日志（`log-core` 写入） |
| `turn_NNN.jsonl` | 每轮结构化事件流（工具调用、文本增量、推理 …） |
| `frames.txt` | TUI 帧翻页书（去色留布局，供离线回看 UI） |
| `inline.raw` | 行内模式字节流镜像（`src/bin/replay.rs` 可离线回放） |

复用 `--session <id>` 会在同一目录续号追加 turn。

## 项目结构

```
phimint/
├── src/
│   ├── main.rs        # CLI 入口（clap），组装 agent + 选择 UI
│   ├── agent.rs       # 系统提示 + 工具注册 + 审批/多 agent/闸门接线
│   ├── approval.rs    # 审批两层：ApprovalPolicy 闸门 + 决策 handler（含 TUI 队列）
│   ├── gate.rs        # 强制 verify 闸门（VerifyEnforcementMiddleware）
│   ├── lang.rs        # 语言注册表（多语言边界）
│   ├── lsp.rs         # 手写 LSP 客户端（diagnostics）
│   ├── inline.rs      # 行内聊天（--inline）
│   ├── ui/            # ratatui TUI 模块
│   │   ├── app.rs           # App 核心状态机（事件驱动）
│   │   ├── app_tests.rs     # App 单元测试（独立文件）
│   │   ├── render.rs        # ratatui 帧渲染
│   │   ├── render_tests.rs  # 渲染单元测试（独立文件）
│   │   ├── completer.rs     # 统一补全系统（InlineCompleter<T> 泛型）
│   │   ├── handlers/        # 事件处理模块（高内聚低耦合）
│   │   │   ├── mod.rs       # handlers 模块声明
│   │   │   ├── runtime.rs   # 运行时事件处理（RuntimeEvent）
│   │   │   └── keyboard.rs  # 键盘事件处理（KeyCode/Mouse）
│   │   ├── markdown.rs      # Markdown 渲染（tui-markdown）
│   │   ├── input.rs         # 多行输入组件（Composer）
│   │   ├── stream.rs        # 流式状态（未提交的文本/思考）
│   │   ├── transcript.rs    # 输出缓冲（已提交的行）
│   │   ├── viewport.rs      # 滚动视口（offset + follow-bottom）
│   │   ├── selection.rs     # 鼠标选择 + 右键菜单状态
│   │   ├── wrap.rs          # 文本换行逻辑
│   │   ├── frame_log.rs     # 帧翻书日志
│   │   ├── picker.rs        # 通用 picker 抽象（提及/技能选择器）
│   │   ├── mention.rs       # @ 文件路径补全（Phase 10）
│   │   └── mod.rs           # ui 模块声明
│   ├── tools/         # 应用工具
│   │   ├── decompose.rs     # 大任务分解（串行/并行策略）
│   │   ├── diagnostics.rs   # LSP 诊断工具
│   │   └── workspace.rs     # 工作区操作
│   └── bin/replay.rs  # 离线回放 inline.raw
├── docs/              # 设计文档与分阶段计划/记录
└── Cargo.toml
```

## 开发

```bash
cargo build            # 编译
cargo test             # 运行所有测试（单元测试在 *_tests.rs 独立文件中）
cargo clippy           # lint
```

### 测试组织

项目采用 **模块分离测试** 模式（Rust 社区最佳实践）：

- **业务代码**：`app.rs`、`render.rs` 等只包含业务逻辑
- **测试代码**：`app_tests.rs`、`render_tests.rs` 等独立测试文件
- **访问权限**：测试仍能访问 `pub(crate)` 字段和方法（测试在 crate 内部）
- **编译隔离**：`#[cfg(test)]` 确保测试代码不会进入生产构建

这种结构让源文件更聚焦、测试更易维护，同时保持 Rust 的类型安全和访问控制。

依赖的 sibling crates 用 `path` 引用（`Cargo.toml`），任一 crate 里 `cargo test` 直接吃本地源码；发布时用 `version` 字段走 crates.io。

更多背景与决策记录见 `docs/design.md`（设计）与 `docs/phase*-plan.md`（各阶段计划/完成记录）。

## License

MIT（见 `Cargo.toml`）。
