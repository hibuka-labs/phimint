# phimint — 使用手册

完整参考：命令、键位、审批、工具面、会话文件与内部结构。安装与起步见
[README_CN](../README_CN.md) — [English](USAGE.md)

## 命令行

```
phimint [OPTIONS] [COMMAND]

Commands:
  update             升级到最新版本（一键脚本安装的自替换二进制；
                     brew/npm/cargo/scoop 安装的给出对应升级命令）。
                     `phimint update --check` 仅检查不升级。

  uninstall          卸载 phimint 并清掉它留下的数据。二进制交给当初
                     安装它的渠道移除，然后删掉 ~/.phimint/（API key、
                     会话、历史、笔记），并摘掉安装脚本加进 PATH 的那条——
                     Windows 上是用户 PATH 条目，Unix 上是 shell 配置里
                     那段带标记的块。--keep-data 保留 ~/.phimint/。
                     --yes 跳过确认。

  init               写出配置骨架而不启动 TUI。安装脚本跑的是
                     `phimint init --if-missing`；已存在的配置绝不覆盖。

  config             不启动 TUI 地查看或修改配置文件。`config show` 打印
                     phimint 实际读到的配置（API key 打码）；
                     `config set <键> <值>` 写一个成员
                     （api_key、base_url、protocol、main）。api_key 可以传
                     `-`，让密钥从 stdin 进来，不落在命令行历史里。

  -w, --workspace <PATH>    工作区目录（默认：当前目录）
      --model <NAME>        仅本次运行的主模型名（覆盖 `model` 指针与 `main`，不写回文件）
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
      --session-retention-days <N> 会话保留天数（默认 30）
      --no-update-check     跳过启动时的更新检查
```

## 界面与交互

- **输入与输出**：底部固定输入栏（多行 Composer、括号粘贴、CJK 双宽），输出区逐字流式 + Markdown 渲染；`@` 触发文件路径补全，`/` 触发 skill 选择。
- **按键**：

  | 按键 | 行为 |
  |---|---|
  | `Enter` | 发送（同时跳到最新内容） |
  | `Ctrl+J` | 换行（全终端可用；上报 Kitty 键盘协议的终端亦支持 `Shift+Enter`） |
  | `Up` / `Down` | 翻历史输入（fish 式循环：当前草稿也在环里） |
  | `Alt+Up` / `Alt+Down` | 多行草稿内移动光标 |
  | `Shift+Tab` | 切换审批档位（auto ⇄ ask） |
  | `Ctrl+O` | 展开/折叠思考块与超长粘贴消息 |
  | `Ctrl+V` | 粘贴剪贴板里的图片（截图）——模型看不见图时会直接拒收 |
  | `Ctrl+Y` | 复制最后一条 AI 回复 |
  | `Ctrl+C` | 按状态分流：有选区→复制、运行中/审批中→取消（没跑出内容的草稿回到输入栏）、输入栏有草稿→清空、空闲→提示后再次按下退出 |
  | `Ctrl+D` | 退出 |
  | `Esc` | 清除选区；无选区则清空输入栏 |
  | `PageUp` / `PageDown` | 滚动输出区 |
  | `Ctrl+End` | 跳到最新内容 |

- **鼠标**：滚轮滚动、左键拖选出行范围、右键弹出拷贝菜单。
- **Terminal.app**：需开启 **View → Allow Mouse Reporting** 滚轮才能滚动输出区，未开启时用 `fn+Up` / `fn+Down` 翻页。该终端不上报 Kitty 键盘协议，`Cmd+C` 不会被识别为拷贝——先选中文本再按 `Ctrl+C`，或用右键菜单。
- **审批三态**：

  | 模式 | 行为 |
  |---|---|
  | `auto` | 全自动，什么都不问（带写能力的子 agent 也自由写） |
  | `ask` | 写操作逐条弹窗：`y` 放行一次 / `a` 永久放行 / `n` 拒绝 |
  | `deny` | 只读，拒绝所有写 |

  `Shift+Tab` 随时在 `auto` ⇄ `ask` 之间切换（状态栏显示当前档位）；`--approval` 指定启动档位。

## 工具面

| 工具 | 作用 |
|---|---|
| `code_cell` | 在常驻 Python 会话里跑代码单元——状态（变量、import、函数）跨调用存活；批量改、重命名、数据整理的快车道 |
| `read_file` / `write_file` / `edit_file` / `list_files` | 文件读写与精准编辑（`edit_file` 原子写 + 4 级匹配；phi-kernel-tools） |
| `read_image` | 让模型真正「看见」图片：Ctrl+V 粘贴截图，agent 直接看（也可指向文件；phi-kernel-tools） |
| `execute_command` | shell（跑构建/测试/命令，逐行流式回显 + 超时/取消杀进程组；超出工具预算的输出落盘成文件，agent 可回读） |
| `search_content` | ripgrep 内容搜索（regex） |
| `repo_map` | tree-sitter 符号/结构索引（多语言，拉取式上下文） |
| `diagnostics` | LSP 诊断（改完不重编译的快速内环；server 缺失降级到 shell） |
| `update_plan` | 复杂任务先展示结构化清单（display-only，Codex 风格；agent-base） |
| `current_model` | 当前会话正在服务的模型（agent 回答「你是什么模型」前会先调它） |
| `spawn_agent` / `send_message` / `wait_agent` / `list_agents` / `close_agent` | 子 agent 原语（agent-works；推荐用法见上：spawn 后结束回合等推送） |
| `add_tool` | agent 代写并注册工具（root-only；全貌确认卡点头后才落盘） |
| `canvas_open` / `canvas_push` / `canvas_close` | 设计画布——浏览器屏幕带可点选项；打开浏览器需一次性同意（[`phi-canvas`](https://github.com/hibuka-labs/phi-canvas)） |
| `ask_user` | 终端选择器提问概念选择（2–4 个候选，单发，Esc = 跳过；也可以不选、直接回一句话让 agent 先答） |
| `/connect` · `/tools` · `plugin add` / `plugin remove` | 外部工具面：MCP 服务、脚本工具、wasm 插件包（逐工具审批档位，fail-closed） |

## 工作原理

phimint 是 [`phi-agent`](https://docs.phiagent.dev/zh/) 框架的 consumer：agent 循环 / ReAct / 审批 / 会话事件流 / 守卫全部来自框架（经 facade 一个依赖引入），phimint 只做产品壳——系统提示、工具注册、审批接线、TUI。

- **推送式 fan-in**（agent-works）：子 agent 的报告在完成时被持有，主 agent 回合结束的瞬间批量注入为新回合；进度信息 display-only（不唤醒、不插话）；单任务超 10 分钟硬停并推 Error 结果——挂死的子 agent 一定能唤醒父级。子 agent 默认只读：`write_file` / `edit_file` / `execute_command` 不进工具面，除非该次 spawn 申请了写能力（`tools: "write"`，或 `coder`/`tester` preset）；带写能力的子 agent 按互斥文件集干活，撞上被占用的文件会得到 `file locked by <agent>`，`ask` 下每次子 agent 写入都弹窗确认。
- **TUI 架构**：`phi-agent-tui` 里的 shell 主循环，后台任务跑 agent 回合并把 `RuntimeEvent` 推过 mpsc 通道，主循环把事件灌进 `App` 状态机、轮询键盘、重绘——`src/shell/wire.rs` 把 shell 留的每个槽位都填上，文案与策略留在这边。输入与事件只经通道相遇，无共享可变状态，agent 循环零侵入。
- **任务面板**：`phi-agent-tui` 的 `task_panel.rs` 记账子 agent 生命周期（spawn 出现 / done 翻转 / 完成 3s 后回收，根 agent 忙碌或用户正在看面板时不回收），每个子 agent 独立流式缓冲，交叉输出互不切断；`child_results.rs` 把框架的 fan-in 路由事件映射成面板文案与聚焦子 agent 的实时尾随。
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

输入栏历史放在 `~/.phimint/input-history.jsonl`，与 `sessions/` 同在 `~/.phimint/`
下（仅本人可读，一个全局文件；`Up` / `Down` 按工作区过滤后回放）。

## 项目结构

```
phimint/
├── src/
│   ├── main.rs        # CLI 入口（clap）、装配、选配色
│   ├── lib.rs         # 模块根——二进制只是它上面的一层薄壳
│   ├── cli.rs         # CLI 面：所有 flag 与子命令
│   ├── command.rs     # shell 命令解析 + 风险分级
│   ├── agent.rs       # 系统提示 + 工具注册 + 多 agent/守卫/审批接线
│   ├── agent/         # agent 装配：build.rs、gate.rs、prompt.rs、spawn_resolve.rs
│   ├── approval.rs    # 审批两层：ApprovalPolicy 闸门 + 决策 handler（含 TUI 队列）
│   ├── banner.rs      # 启动 banner（明暗两套配色）
│   ├── model/         # 配置、档位、路由、持久化、首次运行闸门
│   ├── shell/         # TUI 产品壳：wire.rs 负责填 phi-agent-tui 留的槽位
│   ├── tools/         # 应用工具壳（核心在 code-intel / phi-kernel-tools）
│   │   ├── diagnostics.rs   # LSP 诊断工具
│   │   ├── repomap.rs       # repo_map 工具
│   │   ├── ripgrep.rs       # search_content 工具
│   ├── external/      # 外部工具插座：MCP 服务、脚本工具、wasm 插件
│   ├── session/       # 会话存储、恢复、保留期
│   ├── ops/           # 卸载、终端自愈、安装来源记账
│   └── update/        # 更新检查器 + release manifest
└── Cargo.toml
```

## 从源码运行
需要 Rust 工具链（edition 2024）。可选：各语言 LSP server 在 `PATH` 上以启用
LSP 快速内环（rust-analyzer / typescript-language-server / clangd）；被指向的项目
还需要 `cargo` / `npm` / `make` 等构建工具。其余依赖全部来自 crates.io——
`cargo build` 不需要同级检出任何兄弟仓库。
```bash
cargo run                                # 从源码树启动
cargo run -- -w ../some-project          # 指定工作区目录
```

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
