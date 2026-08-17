# phiforge 启动 Banner 设计

日期：2026-08-17
状态：已与用户逐屏确认（布局 / 配色 / 内容构成 / 品牌大小写）

## 背景与问题

当前启动时打印 4 行纯文本（`src/ui/mod.rs:100-103` 与 `src/inline.rs:114-117` 各一份）：

```
phiforge — coding agent on phi-agent.
Workspace: /Users/kangzengchen/source/buka/buka-works/phiforge
Logs: /Users/kangzengchen/.phiforge/sessions/<id>/session.log
Session: /Users/kangzengchen/.phiforge/sessions/<id>
```

全部使用 `LineKind::System` → `Color::DarkGray`（`src/ui/render.rs:366`），在黑色背景终端上接近隐形；形态随意、无品牌感。这是工具每次启动的"门面"，需要重新设计。

## 目标

- **门面化**：启动即有清晰、有记忆点的品牌形象。
- **对比度**：黑底终端正文 ≥ 4.5:1、大字 ≥ 3:1；亮色终端同样可读（两套色板，自动/手动选择）。
- **双模式一致**：TUI（默认）与 `--inline` 打印同一套 banner。
- **可关可换**：`--banner off` 可关闭；`--color-scheme dark|light|auto` 可覆盖配色（默认 auto）。
- **零新依赖**：字形静态嵌入；依赖已有 ratatui/crossterm。

## 已确认的设计

### 布局（方案 B：ASCII 大字）

- **图案行**：`ANSI Shadow` 字形拼出的 `phiforge`，6 行、约 70 列。块状字形下 7/8 字母大小写轮廓相同，故统一全大写轮廓——它是"图案（logo）"，不是被朗读的句子。
- **tagline 行**：真实的品牌拼写放这里（文字行才有大小写语义）：

  ```
  PhiForge · product-first coding agent on phi-agent · 先验再交 · v0.1.0
  ```

  - 品牌名官方拼写 **`PhiForge`**（用户确认；仓库路径/标识符维持现状小写）。
  - `v0.1.0` 取值 `env!("CARGO_PKG_VERSION")`，与 `--version` 永远一致。
- **信息行（V2 精简）**：仅两行，去掉 `Session`（它与 `Logs` 指向同一 `~/.phiforge/sessions/<id>` 目录，冗余）：

  ```
  Workspace  ~/source/buka/buka-works/phiforge
  Logs       ~/.phiforge/sessions/<id>/session.log
  ```

  - label 列定宽对齐（`Workspace` 9 字符 + 2 空格）。
  - 路径 `$HOME` 前缀替换为 `~`（`shorten_home`）；workspace 不在 home 下时保留原样。

### 色彩（熔炉橙）

| token | 作用域 | 黑底 | 亮底 |
|---|---|---|---|
| `LogoA` | 字形（交替） | `#ff7847` | `#c43e10` |
| `LogoB` | 字形（交替） | `#ffa94d` | `#d96a1f` |
| `Brand` | tagline 品牌名（加粗） | `#ffb066` | `#9a4b12` |
| `Tagline` | tagline 其余文字 | `#d0d7de` | `#57606a` |
| `Version` | `· v0.1.0` | `#7d8590` | `#6e7681` |
| `Label` | Workspace/Logs 标签 | `#ffb066` | `#9a4b12` |
| `Value` | 路径值 | `#d0d7de` | `#57606a` |

（黑底为默认，也是用户明确反馈过的场景。）

## 架构

### 新模块 `src/banner.rs`（唯一内容源）

```rust
pub enum BannerStyle { LogoA, LogoB, Brand, Tagline, Version, Label, Value }
pub struct BannerRow { pub spans: Vec<(String, BannerStyle)> }   // 行内分段样式
pub enum ColorScheme { Dark, Light }

pub const ANSI_LOGO: &[&[&str]] = /* … */;   // 6 行 × 8 字母，每字母为一个字体片段（两色交替）
pub fn resolve_scheme(flag: SchemeChoice, tty_dark: Option<bool>) -> ColorScheme;
pub fn shorten_home(p: &Path) -> String;
pub fn build(workspace: &Path, session_dir: &Path, log_path: &Path, scheme: ColorScheme) -> Vec<BannerRow>;
```

- **字形**：`ANSI SHADOW` 字形的 `p h i f o r g e`，逐字符核对后静态嵌入；每字母作为独立片段以支持 `LogoA`/`LogoB` 交替。实现时不引入 figlet 相关依赖。
- `build()` 输出 6（图案）+ 1（tagline）+ 2（信息）= 9 行。信息行数量与 tagline 文案在 `build()` 内集中定义，两个 UI 不再各自维护。

### 配色方案选择

- `--color-scheme auto|dark|light`，默认 `auto`。
- auto 探测链（在进入 raw mode / alternate screen **之前**，对 stdout 做）：
  1. `$COLORFGBG`（iTerm2 等会设）：末位背景位为 `7` → Light，否则 Dark；
  2. 无则发 OSC 11 查询 `\x1b]11;?\x07`，100ms 超时读回复，解析 `rgb:` 亮度（>0.6 → Light）；
  3. 超时/无回复/解析失败 → Dark（兜底）。
- 探测逻辑做成可注入的纯函数便于单测。

### TUI 接入（`src/ui/app.rs` + `src/ui/render.rs`）

- `OutputLine` 增加可选字段 `spans: Option<Vec<SpanSpec>>`，`SpanSpec { start: usize, len: usize, style: BannerStyle }`，为 `text` 的字节区间覆盖样式。`text` 仍是纯文本拼接，复制/选中/录像/`last_reply_text` 均不感知样式、语义不变。
- `App::push_banner(rows: Vec<BannerRow>)`：逐行直接入 `output`，**不走 `push_system` 的 100 列 soft-wrap**（字形行禁止折行；终端窗口过窄时由 ratatui 右侧裁剪，保持字形完整）。配色的 `ColorScheme` 由 App 持有（启动探测结果），渲染时读取。
- `push_system` 等现有调用点因 `OutputLine` 加字段需适配（构造处补 `spans: None`，或加 `OutputLine::new` 构造器集中处理）。
- `render.rs::render_output`：行存在 `spans` 时按 `SpanSpec` 逐段构 `Span`，样式映射 `BannerStyle → ratatui::Style`（含亮/暗两套 `Color::Rgb`）；被鼠标选中时仍整行叠加 `bg(DarkGray)`（覆盖 span 底色之上的高亮逻辑要保留）。
- App 持有 `scheme: ColorScheme`（启动探测结果），渲染时使用。

### inline 接入（`src/inline.rs`）

- `banner` 提供渲染到 ANSI 的辅助：`fn render_ansi(rows: &[BannerRow]) -> Vec<String>`（24-bit `\x1b[38;2;r;g;bm`）。
- 入口处先用 `renderer.line(&ansi)` 逐行输出（`Renderer::line` 对含 ANSI 的字符串原样透传，无需改动其核心）。

### 入口（`src/main.rs`）

- 新增两个 clap 参数：`--color-scheme <dark|light|auto>`（默认 `auto`）、`--banner <on|off>`（默认 `on`）。
- 探测在 UI 启动前完成；`--banner off` 时 `build()` 不调用、空行数组，两端都不打印。

### 调用点替换

- `src/ui/mod.rs:100-103` / `src/inline.rs:114-117`：替换为 `banner::build(...)` + 对应渲染调用。
- `src/inline.rs:860-861` 测试里也有同样的 banner 文本，同步更新期望。

## 测试

单元测试（`src/banner.rs` 内建 `#[cfg(test)]`）：

1. `shorten_home`：home 前缀替换、非 home 路径原样、路径恰等于 home、尾部斜杠边界。
2. `resolve_scheme`：`light`/`dark` 覆盖；auto + `COLORFGBG=*;7` → Light；auto + OSC 回复亮 → Light；auto + 超时/None → Dark。
3. `ANSI_LOGO`：8 字母片段拼接后 6 行等宽；字符集仅含框线字符（`█╔══╝╗║══╚║╔║` 等）。
4. `build()`：行数 = 9；tagline 含 `PhiForge` 与 `v{env!("CARGO_PKG_VERSION")}`；路径已 `~` 缩短；纯文本 golden 对比（`concat` 后与手写期望一致）。

集成冒烟（手动）：`cargo test` 全绿；`cargo run --inline` 与默认 TUI 各启动一次，肉眼核对颜色与对齐；`--color-scheme light` 与 `--banner off` 生效。

## 明确不做（范围外）

- 不引入 figlet 库；字形静态嵌入（构建期确定、零依赖）。
- 不做终端底色双向协商以外的探测协议；OSC 11 单向查询即可。
- 不修改 `Session` 之外的字段集合（已确认精简为 Workspace/Logs 两行）。
- 不改动仓库内既有的小写 `phiforge` 标识符与目录名；品牌拼写 `PhiForge` 仅用于 banner tagline 文案。

## 开放问题

无（品牌拼写已确认为 `PhiForge`，可后续在 README 等文档处统一，但不属于本次范围）。