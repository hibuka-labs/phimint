# phimint

**读作** `/ˈfaɪmɪnt/`（phi-mint，同英文 "fie-mint"），不是「皮敏特」。

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-edition%202024-orange.svg)](https://www.rust-lang.org)
[![English](https://img.shields.io/badge/README-English-blue.svg)](README.md)

![phimint demo](docs/assets/readme-demo.gif)

> 基于 [phi-agent](https://github.com/hibuka-labs/phi-agent) 的终端 AI 编码 agent —— **「产品优先」**，同时是 phi-agent 框架的真实压测场。

phimint 跑「理解需求 → 读码 → 多文件改 → 编译/测试 → 报错迭代」的完整编码链路。设计目标是**「先验再交」**：改完先验、验过才交付——目前这一纪律由 system prompt 承载（交前编译/测试必须过），强制闸门已实现但暂缓接线（见「路线图」）。

## 特性

- **流式 TUI**：ratatui + crossterm，底部固定输入栏、逐字流式输出、工具调用内联、shell 命令逐行实时回显、审批弹窗、启动 banner（自动识别终端明暗配色）。
- **斜杠命令**：兼容 `.claude/skills` 的 skill 体系，`/skill-name args` 把 skill 正文注入本轮；`/resume` 交互式选择并恢复历史会话。
- **拉取式上下文**：`repo_map`（tree-sitter 符号/结构索引）+ `search_content`（ripgrep），agent 主动搜、主动读，而非把整个仓库塞进 context。
- **LSP 快速内环**：多 server 懒启动（rust-analyzer / typescript-language-server / clangd），改完不重编译秒级拿到 `file:line:col` 诊断；server 起不来降级到 shell。
- **多 agent 扇出 + 推送式 fan-in**：`spawn_agent` 派只读子 agent 调查，主 agent **结束回合即收全量报告**（无需轮询）；TUI 任务面板逐子 agent 实时跟踪，单任务 10 分钟超时硬停。
- **审批三态**：`auto`（全自动）/ `ask`（写操作逐条弹窗）/ `deny`（只读）；子 agent 权限跟随主 agent 模式（且永远只读）。
- **防护栏**：reasoning-only 纠偏（连续空转 → 自动关思考）、max-turns 预警（最后 3 轮逼收尾）、provider 截断守卫（谎报 `finish_reason` 的容错，agent-base 提供）、工具输出 16k 上限。
- **多语言**：`code-intel` 注册表（扩展名 → LSP server → verify 兜底），Rust / TypeScript / JavaScript / C / C++ 开箱即用。
- **模块化底座**：UI 组件与语言智能已沉淀为独立 crate —— [`phi-tui`](https://github.com/hibuka-labs/phi-tui)（聊天 TUI 组件）与 [`code-intel`](https://github.com/hibuka-labs/code-intel)（LSP/repomap/ripgrep 核心），phimint 只保留产品壳。

## 快速开始

### 前置条件

- Rust 工具链（edition 2024）
- 任一受支持的模型 provider 的 API key（OpenAI / Anthropic / DeepSeek / Aliyun / Moonshot / Gemini / Ollama …）
- （可选）各语言的 LSP server 在 `PATH` 上；`cargo` / `npm` / `make` 等构建工具

其余依赖全部来自 crates.io——`cargo build` 不需要同级检出任何兄弟仓库。

### 配置

创建 `~/.phimint/config.json`（JSON5，允许注释与尾逗号）：

```json
{
  "base_url": "https://api.openai.com/v1",
  "api_key": "sk-xxx",
  "main": "gpt-5.4-mini",
  "lite": "gpt-4o-mini",
  "advanced": "o1-preview"
}
```

不同 provider 分层配置：

```json
{
  "main": { "model": "gpt-5.4-mini", "base_url": "https://api.openai.com/v1", "api_key": "sk-openai" },
  "lite": { "model": "deepseek-chat", "base_url": "https://api.deepseek.com/v1", "api_key": "sk-deepseek" },
  "advanced": { "model": "claude-opus-4", "base_url": "https://api.anthropic.com", "api_key": "sk-ant" }
}
```

或使用 CLI 参数（完整字段见 `config.json.example`）：

```bash
cargo run -- --model gpt-5.4-mini --base-url https://api.openai.com/v1 --api-key sk-xxx
```

### 安装

每个版本都提供预编译二进制（macOS / Linux / Windows，arm64 + x64）。
一键安装（装到 `~/.local/bin`；国内用户直接用 Gitee 那行）：

```bash
curl -fsSL https://github.com/hibuka-labs/phimint/releases/latest/download/install.sh | bash   # GitHub
curl -fsSL https://gitee.com/chenkangzeng_admin/phimint/releases/download/latest/install.sh | PHIMINT_MIRROR=gitee bash  # Gitee（国内）
```

Windows（PowerShell）：

```powershell
irm https://github.com/hibuka-labs/phimint/releases/latest/download/install.ps1 | iex   # GitHub
$env:PHIMINT_MIRROR='gitee'; irm https://gitee.com/chenkangzeng_admin/phimint/releases/download/latest/install.ps1 | iex  # Gitee（国内）
```

或使用包管理器：

```bash
brew install phimint                     # macOS / Linux（Homebrew）
npm install -g phimint                   # 也支持 pnpm add -g / yarn global add
cargo install phimint                    # 从 crates.io
```

升级：一键脚本安装的用 `phimint update`；其余按安装方式选
`brew upgrade phimint` / `npm install -g phimint@latest` / `cargo install phimint --force`。
国内下载走 [Gitee Releases](https://gitee.com/chenkangzeng_admin/phimint/releases)，
更新检查器会自动兜底到 Gitee 镜像。

或直接从源码树运行：

```bash
cargo run                                # 从源码树启动
cargo run -- -w ../some-project          # 指定工作区目录
```

## 使用

```
phimint [OPTIONS] [COMMAND]

Commands:
  update             升级到最新版本（一键脚本安装的自替换二进制；
                     brew/npm/cargo 安装的给出对应升级命令）。
                     `phimint update --check` 仅检查不升级。

  -w, --workspace <PATH>    工作区目录（默认：当前目录）
      --model <NAME>        主模型名（覆盖 config.json）
      --lite-model <NAME>   lite 模型名（覆盖 config.json）
      --advanced-model <NAME>  advanced 模型名（覆盖 config.json）
      --base-url <URL>      API base URL（覆盖 config.json）
      --api-key <KEY>       API key（覆盖 config.json）
      --protocol <PROTO>    API 协议（默认按模型名推断）
      --config <PATH>       配置文件路径（默认 ~/.phimint/config.json）
      --approval <MODE>     auto（默认）/ ask / deny
      --shell-timeout-ms    shell 命令超时（默认 120000）
      --session <ID>        会话 ID（默认自动生成；复用同 ID 续写同一会话）
      --resume              交互式选择历史会话并恢复
      --log-level <LEVEL>   session.log 级别（默认 info）
      --color-scheme <S>    banner 配色：auto（默认，探测终端底色）/ dark / light
      --banner <on|off>     启动 banner（默认 on）
      --thinking-budget <N> 思考 token 预算（默认 8192）
      --reasoning-effort <E> 推理深度：none/low/medium/high/xhigh（默认 medium）
      --token-budget <N>    上下文窗口 token 预算（默认 210000）
      --session-retention-days <N> 会话保留天数（默认 7）
      --no-update-check     跳过启动时的更新检查
```

### 界面与交互

- **输入与输出**：底部固定输入栏（多行 Composer、括号粘贴、CJK 双宽），输出区逐字流式 + Markdown 渲染；`@` 触发文件路径补全，`/` 触发 skill 选择。
- **按键**：

  | 按键 | 行为 |
  |---|---|
  | `Enter` | 发送 |
  | `Shift+Enter` | 换行 |
  | `Ctrl+O` | 展开/折叠思考块 |
  | `Ctrl+Y` | 复制最后一条 AI 回复 |
  | `Ctrl+C` | 按状态分流：有选区→复制、运行中/审批中→取消、空闲→提示后再次按下退出 |
  | `Ctrl+D` | 退出 |
  | `Esc` | 清除选区；无选区则清空输入栏 |
  | `PageUp` / `PageDown` | 滚动输出区 |

- **鼠标**：滚轮滚动、左键拖选出行范围、右键弹出拷贝菜单。
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
3. diagnostics 秒级拿类型错误（LSP 快速内环，不重编译）
4. execute_command 跑 cargo check / 测试 → 修 → 重跑，直到全绿
5. 简述改动、汇报收工（system prompt 纪律：交前编译/测试必须过）
```

### 一次典型多 agent 协作

```
用户: "调查 X / Y / Z 三个问题"
1. spawn_agent × 3 派只读子 agent（各带窄切片任务），主 agent 立即结束回合
2. 子 agent 运行期间主 agent 什么都不做——报告是推送的，不是拉的
3. 全部完成后报告合为一帧批量到达（TUI 任务面板可见每个子 agent 状态与实时输出）
4. 主 agent 汇总报告，自行完成所有编辑（子 agent 永远只读）
```

## 工具面

| 工具 | 作用 |
|---|---|
| `read_file` / `write_file` / `edit_file` / `list_files` | 文件读写与精准编辑（`edit_file` 原子写 + 4 级匹配；phi-kernel-tools） |
| `execute_command` | shell（跑构建/测试/命令，逐行流式回显 + 超时/取消杀进程组） |
| `search_content` | ripgrep 内容搜索（regex） |
| `repo_map` | tree-sitter 符号/结构索引（多语言，拉取式上下文） |
| `diagnostics` | LSP 诊断（改完不重编译的快速内环；server 缺失降级到 shell） |
| `update_plan` | 复杂任务先展示结构化清单（display-only，Codex 风格；agent-base） |
| `spawn_agent` / `send_message` / `wait_agent` / `list_agents` / `close_agent` | 子 agent 原语（agent-works；推荐用法见上：spawn 后结束回合等推送） |

## 工作原理

phimint 是 `phi-agent` 框架的 consumer：agent 循环 / ReAct / 审批 / 会话事件流 / 守卫全部来自框架（经 facade 一个依赖引入），phimint 只做产品壳——系统提示、工具注册、审批接线、TUI。

- **推送式 fan-in**（agent-works）：子 agent 的报告在完成时被持有，主 agent 回合结束的瞬间批量注入为新回合；进度信息 display-only（不唤醒、不插话）；单任务超 10 分钟硬停并推 Error 结果——挂死的子 agent 一定能唤醒父级。子 agent 硬闸只读：`write_file` / `edit_file` / `execute_command` 不进子 agent 工具面。
- **TUI 架构**：`ui/run.rs` 主循环里，后台任务跑 agent 回合并把 `RuntimeEvent` 推过 mpsc 通道，主循环把事件灌进 `App` 状态机、轮询键盘、重绘——输入与事件只经通道相遇，无共享可变状态，agent 循环零侵入。
- **任务面板**：`task_panel.rs` 记账子 agent 生命周期（spawn 出现 / done 翻转 / 完成 3s 后回收，根 agent 忙碌或用户正在看面板时不回收），每个子 agent 独立流式缓冲，交叉输出互不切断；`child_results.rs` 把框架的 fan-in 路由事件映射成面板文案与聚焦子 agent 的实时尾随。
- **LSP**：`code-intel` 按语言懒启动进程级单例 server，`didOpen`/`didChange`/`didSave` 同步 + `publishDiagnostics` 缓存，`diagnostics` 工具拉取；注册表决定 server 选择，code-intel 不感知「phimint」。
- **守卫**（框架提供、产品调参）：reasoning-only 连续 2 次空转 → 注入「立即行动」nudge 并关闭思考；回合末裁决器 fail-open（裁决不可用不阻塞正常收尾）；`MaxTurnsNudgeMiddleware` 在 256 轮预算的最后 3 轮逼出最终答复。

## 会话与可观测性

每次运行落一份会话到 `~/.phimint/sessions/<id>/`：

| 文件 | 内容 |
|---|---|
| `session.log` | tracing 日志（log-core 写入） |
| `turn_NNN.jsonl` | 每轮结构化事件流（工具调用、文本增量、推理 …） |
| `frames.txt` | TUI 帧翻页书（去色留布局，供离线回看 UI） |
| `perf.log` | 每帧渲染耗时 CSV（性能回归排查） |
| `session_metrics.json` | 每轮 token 用量与会话汇总（phi-telemetry） |

复用 `--session <id>` 会在同一目录续号追加 turn；`--resume` 交互式挑选历史会话恢复。

## 项目结构

```
phimint/
├── src/
│   ├── main.rs        # CLI 入口（clap）、装配、选配色
│   ├── agent.rs       # 系统提示 + 工具注册 + 多 agent/守卫/审批接线
│   ├── approval.rs    # 审批两层：ApprovalPolicy 闸门 + 决策 handler（含 TUI 队列）
│   ├── banner.rs      # 启动 banner（明暗两套配色）
│   ├── gate.rs        # 强制 verify 闸门（VerifyEnforcementMiddleware，暂缓接线）
│   ├── skills.rs      # Skill 薄壳：re-export agent-works 的 skill 子系统 + phimint 默认目录策略
│   ├── tools/         # 应用工具壳（核心在 code-intel）
│   │   ├── diagnostics.rs   # LSP 诊断工具
│   │   ├── repomap.rs       # repo_map 工具
│   │   └── ripgrep.rs       # search_content 工具
│   └── ui/            # TUI 产品壳（通用组件已沉淀到 phi-tui）
│       ├── run.rs             # 主循环：终端装配、事件/命令通道、回合 runner
│       ├── app.rs (+tests)    # App 状态机（事件驱动）
│       ├── render.rs (+tests) # 帧渲染
│       ├── task_panel.rs (+tests)   # 子 agent 任务面板（生命周期 + 每子流式缓冲）
│       ├── child_results.rs (+tests)# fan-in 结果呈现（聚焦尾随、文案路由）
│       ├── frame_log.rs       # frames.txt + perf.log
│       └── handlers/          # keyboard.rs / mouse.rs / runtime.rs
└── Cargo.toml
```

## 路线图

- **先验再交（PARKED）**：`gate.rs` 的 `VerifyEnforcementMiddleware` 已实现——动过代码却没跑验证就拦下「报 done」、注入「先 verify」nudge；接线暂注释（`agent.rs`），当前纪律由 system prompt 承载。重新接线即恢复硬保证。
- **子 agent 受控写**：从只读调查演进到受限写委派（agent-works 的 `ChildPermissionMode` 基建已就位）。

## 开发

```bash
cargo build            # 编译
cargo test             # 运行所有测试（单元测试在 *_tests.rs 独立文件中）
cargo clippy           # lint
```

### 测试组织

**模块分离测试**模式：业务代码（`app.rs`、`render.rs` 等）只含业务逻辑，测试放独立文件（`app_tests.rs` 等），`#[cfg(test)]` 隔离出生产构建；测试仍在 crate 内部，可访问 `pub(crate)` 成员。

依赖全部为 crates.io 纯版本引用。若要同时改动兄弟 crate（phi-agent、phi-tui 等），在 `Cargo.toml` 加一段不提交的 `[patch.crates-io]` path 覆盖（见 [CONTRIBUTING.md](CONTRIBUTING.md)）。设计笔记在 `notes/`（本地目录，不进版本库）；`docs/` 放对外文档与素材。

README 演示 GIF 由录帧重建：`cargo test frame_reel -- --ignored` 把 ANSI 帧写到 `target/reel/`，再用 `python3 scripts/readme_reel.py` 渲染成 `docs/assets/readme-demo.gif`。

## 许可证

MIT（见 [LICENSE](LICENSE)）。

## 联系

GitHub Issues — [hibuka-labs/phimint](https://github.com/hibuka-labs/phimint/issues)

[English](README.md)
