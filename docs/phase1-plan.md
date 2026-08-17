# phimint Phase 1 任务清单

> 目标：跑通「需求 → 写 → 跑测试 → 迭代修」的核心闭环 + 最小 TUI。
> 原则：每步可运行、可验收，不憋大招。核心循环先用 phi-agent 现成的 rustyline + TerminalRenderer，TUI 后置替换。

---

## Track A — 核心循环（先证明 agent 能写对代码）

### T1 骨架 + hello-agent

- **做什么**：建 crate；`Cargo.toml` 依赖 `phi-agent`（本地 path，含 `[patch.crates-io]`）；研究 `phi-agent/src/bin/phi/` 是怎么驱动 agent 的（session + 事件流）；用 `base_agent_builder()` 搭一个最小 agent 接真实 LLM。
- **验收**：`cargo run` 启动，问一句、答一句。

### T2 接入 file + shell 工具

- **做什么**：把 `phi-kernel-tools` 的 `read_file`/`write_file`/`edit_file`/`list_files` + `local_shell` 注册进 agent（feature `file` + `shell`）。
- **验收**：让 agent 读一个真实文件、写一个文件、跑一个 shell 命令，都成功。

### T3 RipgrepTool

- **做什么**：封装 `ripgrep` crate，实现 `Tool` trait（参数：pattern、path、忽略大小写等；返回命中列表带行号）。
- **验收**：agent 能按内容搜索代码，定位到具体文件和行。

### T4 LanguageAdapter + RustAdapter

- **做什么**：定义 `LanguageAdapter` trait（§5.3）+ `RustAdapter`（detect Cargo.toml；build/test 命令 = `cargo build`/`cargo test`）。
- **验收**：对一个 Rust 仓库，detect 出 Rust，拿到 build/test 命令。

### T5 RepoMap（tree-sitter）

- **做什么**：`tree-sitter` + `tree-sitter-rust`，遍历文件提取顶层符号（fn/struct/impl/mod/trait/enum），生成「文件 → 符号列表」索引，作为 system prompt / 初始上下文。
- **验收**：对一个真实 Rust 仓库（比如 phi-agent 自己）生成 RepoMap，能列出各文件的关键符号。

### T6 验证闭环（写 → 测 → 修 → 重跑）★ Phase 1 核心验收

- **做什么**：让 agent 接一个「写代码 + 跑 `cargo test` + 读报错 + 修」的完整任务，跑通迭代循环。v1 的「失败喂回」= shell 返回测试输出（LLM 自己读），不写专门中间件。
- **验收**：给一个「写个带测试的小函数」的任务，agent 能自己写完 → 测试不过 → 读懂报错 → 修到过。**这条过了，Phase 1 才算成立。**

---

## Track B — TUI + 审批（体验层，后置）

### T7 最小 ratatui TUI

- **做什么**：`ratatui` + `crossterm`，搭状态栏（§9.6 状态机）+ 输出区 + 底部输入框；用 `mpsc` 接 `RuntimeEvent` 流。
- **验收**：REPL 里能看到流式输出、工具调用、状态栏随事件切换。

### T8 审批模式

- **做什么**：`TuiApprovalHandler`（ask 模式，y/a/n → `AllowOnce`/`AllowAlways`/`Deny`）+ auto/deny 复用 `AutoApprovalHandler`。
- **验收**：三种模式可切换；ask 模式下写操作弹窗，y/a/n 生效。

---

## 依赖关系

```
T1 → T2 → T3 → T6（核心闭环，T3/T4/T5 可并行）
                T4 → （T5 独立）
T1 → T2 → … → T7 → T8（TUI 线，依赖 T2 之后即可并行推进）
```

- **T4、T5 可并行**（LanguageAdapter 和 RepoMap 互不依赖）。
- **Track B 可以在 T2 之后并行开始**，但核心验收看 T6。

## 备注（实现时注意）

- `deny` = 「只读」的隐含前提：确认 read 类工具被标为 `Safe`（不触发审批），否则 `DenyAll` 会把读也拦掉。
- 驱动 agent 的 REPL 循环：优先复用 `phi-agent/src/bin/phi/` 的模式（session + 事件流），不重造。
