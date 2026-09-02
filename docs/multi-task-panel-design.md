# 多任务面板设计文档

## 1. 概述

为 phimint 的多任务（decompose/merge）功能添加任务面板 UI，让用户直观看到并行子代理的执行状态。

### 1.1 设计目标

- 任务列表一目了然
- 选择任务时，主输出区域切换显示该任务详情
- 自动清理完成的任务
- 输入框位置固定

### 1.2 参考

- Claude Code 的交互模式（主输出区域切换显示）
- 当前 phimint 的 `sub_agents` 状态跟踪

---

## 2. UI 布局

### 2.0 布局决策

任务面板放在**输入框上方**（非 Claude Code 的下方布局）。

**理由**：
- 输入框是核心交互区，位置应固定
- 任务列表是监控信息，动态增减不应影响输入框位置
- 视觉层次更清晰：内容区 → 操作区 → 状态区

```
╭───────────────────────────────────────────────────────────────╮
│                                                               │
│  主输出区域（可滚动）                                          │
│  - 用户输入                                                   │
│  - 主代理输出                                                 │
│  - 子代理输出（带 [name] 前缀）                                │
│  - 选中子任务时，显示该子任务的详情                             │
│                                                               │
╰───────────────────────────────────────────────────────────────╯
╭─ Tasks (2) ──────────────────────────────────────────────────╮  ← 任务面板
│   cache     src/cache.rs                     │ running 2s    │
│ > auth      src/auth/mod.rs, src/auth/jwt.rs │ running 5s    │  ← 选中项
╰───────────────────────────────────────────────────────────────╯
╭───────────────────────────────────────────────────────────────╮
│ _                                                           │  ← 输入框（固定位置）
╰───────────────────────────────────────────────────────────────╯
╭───────────────────────────────────────────────────────────────╮
│ 状态栏                                                        │
╰───────────────────────────────────────────────────────────────╯
```

### 2.1 交互逻辑

1. `↑`/`↓` 在任务列表中选择任务
2. 选中任务后，**主输出区域**切换显示该子任务的详情（输出）
3. 按 `↓` 到输入框，主输出区域恢复显示主任务的输出

**优点**：
- 列表高度固定（不会膨胀）
- 输入框位置固定
- 利用已有主输出空间显示详情

### 2.2 任务列表视图

```
╭─ Tasks (N) ──────────────────────────────────────────────────╮
│ {marker}{status} {name}     {files}           │ {time}       │
│ {marker}{status} {name}     {files}           │ {time}       │
╰───────────────────────────────────────────────────────────────╯
```

- `marker`: `>` 选中 / ` ` 未选中
- `status`: `●` 运行中 / `✓` 完成（完成状态显示 2-3 秒后移除）
- `name`: 任务名称（来自 decompose slice 的 name）
- `files`: 关键文件（最多显示 2 个，多了用 `...`）
- `time`: 运行时间（秒）

### 2.3 主输出区域切换

选中子任务时，主输出区域显示该子任务的详情：

```
╭───────────────────────────────────────────────────────────────╮
│ [auth] ⏺ read_file src/auth/mod.rs                         │  ← 子任务详情
│ [auth]   ✓ read_file (89 lines)                             │
│ [auth] ⏺ read_file src/auth/jwt.rs                         │
│ [auth]   ✓ read_file (45 lines)                             │
│ [auth] ⏺ search_content "token"                            │
│ [auth]   ✓ search_content (8 matches)                       │
│ [auth] ⏺ read_file src/auth/middleware.rs                  │
│ [auth]   (执行中...)                                         │
╰───────────────────────────────────────────────────────────────╯
╭─ Tasks (2) ──────────────────────────────────────────────────╮
│   cache     src/cache.rs                     │ running 2s    │
│ > auth      src/auth/mod.rs, src/auth/jwt.rs │ running 5s    │
╰───────────────────────────────────────────────────────────────╯
╭───────────────────────────────────────────────────────────────╮
│ _                                                           │
╰───────────────────────────────────────────────────────────────╯
```

未选中（或选中输入框）时，显示主任务的输出。

---

## 3. 数据结构

### 3.1 扩展 SubAgentState

```rust
/// 子代理详细状态
pub struct SubAgentState {
    /// 任务名称（来自 decompose slice name）
    pub name: String,
    /// 状态
    pub status: SubAgentStatus,
    /// 关键文件列表（来自 decompose slice files）
    pub files: Vec<String>,
    /// 任务描述（来自 decompose slice task）
    pub task: String,
    /// 上下文信息（来自 decompose slice context）
    pub context: String,
    /// 启动时间
    pub started_at: Instant,
    /// 完成时间（用于计算显示多久后移除）
    pub completed_at: Option<Instant>,
    /// 子代理的工具调用事件（用于详情展示）
    pub events: Vec<ToolEvent>,
}

/// 工具调用事件（简化版，用于详情展示）
pub struct ToolEvent {
    pub tool_name: String,
    pub summary: String,
    pub is_finished: bool,
}
```

### 3.2 任务面板状态

```rust
/// 焦点位置
pub enum FocusTarget {
    /// 焦点在输入框
    Input,
    /// 焦点在任务列表，选中指定索引的任务
    TaskList(usize),
}

/// 任务面板 UI 状态
pub struct TaskPanel {
    /// 当前焦点位置
    pub focus: FocusTarget,
}
```

### 3.3 App 结构更新

```rust
pub struct App {
    // ... 现有字段 ...

    /// 子代理状态（替换原来的 BTreeMap<String, SubAgentStatus>）
    pub sub_agents: BTreeMap<String, SubAgentState>,

    /// 任务面板状态
    pub task_panel: TaskPanel,

    /// 子代理的 transcript 存储（用于主输出区域切换显示）
    /// key: agent_id, value: 该子代理的输出行
    pub sub_agent_transcripts: BTreeMap<String, Vec<OutputLine>>,
}
```

---

## 4. 状态生命周期

```
decompose 返回 slices
        ↓
解析 slices，创建 SubAgentState 列表
        ↓
子代理启动 → 添加到 sub_agents，status = Running
        ↓
子代理执行中 → 收集 ToolEvent，更新 events
        ↓
子代理完成 → status = Done，记录 completed_at
        ↓
2-3 秒后 → 从 sub_agents 移除
        ↓
所有子代理完成 → 面板消失
```

### 4.1 状态转换

| 事件 | 动作 |
|------|------|
| `decompose` 返回 | 初始化 sub_agents（从 slices 解析） |
| 子代理 `RunFinished` | 设置 status = Done，记录 completed_at |
| 子代理 `RunCancelled` | 设置 status = Done，记录 completed_at |
| 子代理 `ToolCallStarted` | 添加 ToolEvent（is_finished = false） |
| 子代理 `ToolCallFinished` | 更新最后一个 ToolEvent（is_finished = true） |
| 定时检查 | 移除 completed_at 超过 3 秒的任务 |

### 4.2 面板显示条件

```rust
fn should_show_task_panel(&self) -> bool {
    !self.sub_agents.is_empty()
}
```

---

## 5. 交互逻辑

### 5.1 键盘操作

| 按键 | 条件 | 动作 |
|------|------|------|
| `↑` | 焦点在任务列表 | 选择上一个任务 |
| `↓` | 焦点在任务列表 | 选择下一个任务 |
| `↓` | 选中最后一个任务 | 焦点移到输入框 |
| `↑` | 焦点在输入框（且输入框为空） | 焦点移到任务列表（选中最后一个） |

### 5.2 焦点状态

```
焦点位置：
- 任务列表：主输出区域显示选中子任务的详情
- 输入框：主输出区域显示主任务的输出
```

### 5.3 焦点切换流程

```
默认状态：焦点在输入框，显示主任务输出
    ↓
按 ↑（输入框为空）→ 焦点移到任务列表
    ↓
按 ↑/↓ → 在任务列表中选择，主输出区域切换显示
    ↓
按 ↓（选中最后一个）→ 焦点回到输入框，显示主任务输出
```

---

## 6. 渲染逻辑

### 6.1 布局计算

```rust
fn draw(f: &mut Frame, app: &mut App) {
    let has_task_panel = app.should_show_task_panel();

    let mut constraints = vec![
        Constraint::Min(1),     // 主输出区域
    ];

    if has_task_panel {
        let task_count = app.sub_agents.len();
        constraints.push(Constraint::Length(task_count as u16 + 2)); // 任务面板
    }

    constraints.push(Constraint::Length(3));  // 输入框
    constraints.push(Constraint::Length(1));  // 状态栏

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());

    let mut output_idx = 0;
    let mut next_idx = 1;

    render_output(f, app, chunks[output_idx]);

    if has_task_panel {
        render_task_panel(f, app, chunks[next_idx]);
        next_idx += 1;
    }

    render_input(f, app, chunks[next_idx]);
    render_status(f, app, chunks[next_idx + 1]);
}
```

### 6.2 主输出区域切换

```rust
fn render_output(f: &mut Frame, app: &App, area: Rect) {
    let content = match &app.task_panel.focus {
        FocusTarget::TaskList(index) => {
            // 获取选中的 agent_id
            let agent_id = app.sub_agents.keys().nth(*index);
            if let Some(id) = agent_id {
                // 显示选中子任务的详情
                app.sub_agent_transcripts.get(id).unwrap_or(&app.transcript)
            } else {
                &app.transcript
            }
        }
        FocusTarget::Input => {
            // 显示主任务的输出
            &app.transcript
        }
    };

    // 渲染 content 到 area
    // ...
}
```

### 6.3 任务面板高度计算

```rust
fn calculate_panel_height(app: &App) -> u16 {
    let task_count = app.sub_agents.len();
    // 固定高度：边框(2) + 任务行数
    (task_count as u16 + 2).min(10) // 最大高度限制
}
```

### 6.4 任务行渲染

```rust
fn render_task_row(
    state: &SubAgentState,
    is_selected: bool,
    area: Rect,
    buf: &mut Buffer,
) {
    let marker = if is_selected { ">" } else { " " };
    let status_icon = match state.status {
        SubAgentStatus::Running => "●",
        SubAgentStatus::Done => "✓",
    };
    let status_color = match state.status {
        SubAgentStatus::Running => Color::Cyan,
        SubAgentStatus::Done => Color::Green,
    };

    let files = format_files(&state.files, 20); // 截断
    let time = format_time(state.started_at.elapsed());

    let bg_color = if is_selected {
        Color::DarkGray
    } else {
        Color::Reset
    };

    // 渲染一行：marker status name files | time
    let spans = vec![
        Span::styled(format!("{marker} "), Style::default().bg(bg_color)),
        Span::styled(format!("{status_icon} "), Style::default().fg(status_color).bg(bg_color)),
        Span::styled(format!("{:<12}", state.name), Style::default().bg(bg_color)),
        Span::styled(format!("{:<20}", files), Style::default().fg(Color::DarkGray).bg(bg_color)),
        Span::styled(format!("│ {:>5}", time), Style::default().fg(Color::DarkGray).bg(bg_color)),
    ];

    Paragraph::new(Line::from(spans)).render(area, buf);
}
```

---

## 7. 数据来源

### 7.1 从 decompose 获取任务信息

**方案**：修改 `decompose` 工具，通过 `UserEvent` 发送结构化的 slices 数据。

```rust
// 在 decompose 工具的 call 方法中
ctx.emit_user_event(UserEvent::Structured {
    event_type: "decompose_result".to_string(),
    data: json!({
        "strategy": decomp.strategy,
        "slices": decomp.slices,
    }),
});
```

UI 层监听 `UserEvent::Structured`，解析并初始化 `sub_agents`。

### 7.2 子代理 transcript 收集

**关键改动**：子代理输出不再混入主 transcript，而是单独存储。

```rust
RuntimeEvent::TextDelta { text, agent_id, .. } => {
    match agent_id {
        Some(id) if !id.is_empty() => {
            // 子代理输出 → 存入 sub_agent_transcripts
            self.sub_agent_transcripts
                .entry(id.clone())
                .or_default()
                .push(OutputLine { text, kind: LineKind::Normal, ... });
        }
        _ => {
            // 主代理输出 → 存入主 transcript
            self.transcript.push(OutputLine { text, kind: LineKind::Normal, ... });
        }
    }
}
```

### 7.3 从 RuntimeEvent 收集工具调用

子代理的工具调用事件同时更新两个地方：
1. `SubAgentState.events`（用于任务面板显示统计）
2. `sub_agent_transcripts`（用于主输出区域显示详情）

```rust
RuntimeEvent::ToolCallStarted { tool_name, agent_id, .. } => {
    if let Some(agent_id) = agent_id.as_deref() {
        // 更新 SubAgentState.events
        if let Some(state) = self.sub_agents.get_mut(agent_id) {
            state.events.push(ToolEvent { tool_name, ... });
        }
        // 更新 sub_agent_transcripts
        self.sub_agent_transcripts
            .entry(agent_id.to_string())
            .or_default()
            .push(OutputLine { text: format!("⏺ {tool_name}"), kind: LineKind::Tool, ... });
    }
}
```

---

## 8. 实现步骤

### Phase 1: 基础框架（最小可用）

1. 定义 `SubAgentState`、`TaskPanel`、`FocusTarget` 结构
2. 修改 `App`，添加 `sub_agents`、`task_panel`、`sub_agent_transcripts` 字段
3. 修改 `render`，在输入框上方渲染任务面板（只显示列表，不支持选择）
4. 修改 `handle_runtime`，监听 `UserEvent::Structured` 初始化 sub_agents

### Phase 2: 子代理 transcript 分离

1. 修改 `handle_runtime` 中的 `TextDelta`/`ThoughtDelta` 处理逻辑
2. 子代理输出存入 `sub_agent_transcripts`，不混入主 transcript
3. 主代理输出继续存入主 transcript
4. 确保现有子代理输出（带 [name] 前缀）不再出现

### Phase 3: 主输出区域切换

1. 实现 `render_output` 的切换逻辑
2. 根据焦点状态选择显示主 transcript 还是子代理 transcript
3. 测试切换显示效果

### Phase 4: 键盘交互

1. 添加焦点状态管理
2. 实现 `↑`/`↓` 在任务列表中选择
3. 实现焦点切换：`↓` 到输入框，`↑` 回到列表

### Phase 5: 自动清理

1. 添加定时检查逻辑（在主循环中检查 completed_at）
2. 移除完成超过 3 秒的任务
3. 所有任务完成后隐藏面板
4. 清理对应的 sub_agent_transcripts

---

## 9. 设计决策（已确认）

| 决策项 | 方案 |
|--------|------|
| 任务面板位置 | 输入框上方 |
| 详情显示方式 | 主输出区域切换显示 |
| 完成任务移除时机 | 显示 2-3 秒后 |
| 键盘交互 | 完整方案（支持 ↑/↓ 选择，焦点切换） |
| 子代理输出 | 单独存储，选择时切换显示 |
| decompose 数据来源 | 通过 UserEvent::Structured 发送 |

---

## 10. 未来扩展

- 任务面板支持拖拽排序
- 点击任务跳转到对应的输出位置
- 显示子代理的 token 消耗统计
- 支持取消单个子代理
