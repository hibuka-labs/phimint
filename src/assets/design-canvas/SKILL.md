---
name: design-canvas
description: "Design questions: when to show visual options on the canvas, when to ask in the terminal picker, and how to compose each canvas screen."
---

# Design canvas

The canvas shows one screen at a time in the user's browser: a question, an
optional mockup, and clickable options. Their pick comes back as a user
message. Use it when seeing beats reading.

The kind tables, theme tokens, and the `body_html` component classes are in
`nodes.md` — read it before you choose the medium. It lives beside this skill
file (bundled copy: `~/.phimint/skills/design-canvas/nodes.md`). The coverage:
layout (`frame`/`cols`/`rows`/`stack`/`box`/`text`/`placeholder`), device and
window chrome (`phone_shell`/`topbar`/`tabbar`/`nav`/`sidebar`/`toolbar`/
`dialog`), controls (`input`/`button`/`switch`/`segment`/`list_row`/`swatches`),
data (`kpi`/`hero`/`sparkline`/`ring`/`bars`/`area`/`viz`/`table`/`tree`).
A phone settings page, a dashboard, a dialog flow — those are `screen`. This
file holds the judgment: where each question goes and what the law is.

## When to open the canvas (routing)

Decide per question, not per session. A question *about* UI is not
automatically a visual question: "what kind of wizard flow do you want?" is a
text question; "which of these two wizard layouts feels right?" is visual.

- **Pure text decisions** — "should we do X", "which first", "which tech".
  When the answer is one of 2-4 candidates you can name, ask with `ask_user`
  and let the terminal picker take it. Do NOT open the canvas for these. An
  open-ended question (a goal, a path, a number) is not a pick — ask it in
  your reply text.
- **Appearance questions** — "what should it look like", layout / styling /
  side-by-side comparison — open the canvas and show the candidates.
- **When unsure, open the canvas.** The cost is asymmetric: opening by
  mistake costs one screen of HTML and a glance; staying in text by mistake
  costs two or three rounds of trying to describe a picture in words — each
  round a full screen — plus the user's time.
  Not opening is the more expensive mistake.
- The user can switch medium at any moment ("用图给我看" / "就文字说"). Honor
  the switch and continue from what you already have; never make them
  restate the question.
- A mock needs no codebase. "What should it look like" is a picture to draw,
  not a page to find — skip `repo_map` / code search and draw it. Read the
  product's code only when the user is asking about a screen that already
  exists in it.

## Concept picks in the terminal

`ask_user` is the other half of the routing: a question the user only has to
*decide*, not see. It opens a picker in the terminal and the answer is the
tool result in this turn — nothing to wait for.

- `question` is the decision in one line; `options` are 2-4 candidates
  (`id`, `label`, optional one-line `note`), in the user's language.
- The result is `picked: <label> (id: <id>)`, or `skipped` on Esc. A skip is
  an answer too — carry on with what you have, never re-ask it.
- One live question at a time across both surfaces: if a canvas screen is up,
  its question is the live one.

Don't reach for it for a yes/no you can infer, or for anything open-ended —
those are faster in text. And never ask the user to pick between things you
have not named: name the candidates first, then the picker.

## When to push a screen

- One screen = one decision point. Push when you have concrete candidates the
  user can judge side by side.
- Don't push for progress, status, or to re-show what they just picked — the
  terminal transcript already has that.
- Never push a question that is already on the screen or that they just
  answered — not as a variant, not "with more context". `canvas_push`
  replaces the screen, wiping the pick they just made. Extra detail and
  refinements go in terminal text.
- One live question at a time. When a pick settles the screen's question,
  deliver the outcome (a text summary, or a display-only screen of the chosen
  design) before asking anything else — a chain of question screens reads as
  an interrogation. A "continue working" nudge means add substance to what
  you owe; never re-ask a question that was already answered.
- After each turn the host replaces the screen with a "继续在终端讨论…"
  waiting screen (summary of decided / pending). You never craft that screen;
  your next `canvas_push` simply overwrites it.
- When the design question is settled, call `canvas_close`. Open tabs then
  show a farewell screen and the user can close the tab.

## What goes on a screen

`canvas_push` takes one envelope: a question, a picture of the candidates, and
2–4 clickable picks.

- `title` — the question in one line, e.g. `大屏顶部导航，哪个布局更专业？`.
- `subtitle` — optional one-line context under it.
- `options` — 2–4 candidates: stable ASCII `id` (`a`, `b`, `c`), short
  `label`, optional one-line `note` naming what differs. Empty `options` = a
  display-only screen (no pick expected).
- `multiselect: true` when several independent picks make sense (adds
  confirm / skip). Default is single-pick.

Then the picture — **send exactly one of `screen` / `body_html`**:

- **`screen` — the default.** Structure plus appearance tokens from a closed
  vocabulary. The host renders it. You never write CSS, never write a tag.
  Cheap, and a revision is a field edit. Send it as the nested object itself
  in the arguments — never as a stringified copy of that object. Hand-escaped
  JSON inside a `"…"` string is what breaks (every such copy seen in
  production broke mid-escape); the object form has never failed.
- **`body_html` — when the description outruns the vocabulary.** A small HTML
  **fragment**. Costs more per screen and per revision, buys total freedom.
  The host wraps it in the page: never write `<html>`, `<head>`, or a full
  document.

Write all user-visible copy (title, labels, notes, mock text) in the user's
language. Do not restate copy the envelope already carries: a variant's
caption is rendered from its `options[].label`, and the options panel from
`options` — typing either again in the picture is duplication that drifts.

**Never downgrade the request to fit the vocabulary.** The user describes a
detail — show that detail. The vocabulary saves them money when it can hold
their description; it is not allowed to quietly shrink it. 「按钮做成圆角胶囊」
is not `button`; it is `body_html` with a pill button. Sending a square button
and calling it close is worse than sending nothing — the user now has to
correct a picture instead of describing a wish. If you are not sure a
description fits the kinds, read the tables in `nodes.md` again; if it still
does not fit, escape — never shrink the request to make it fit.

## Fidelity follows the question

`fidelity` is a field, chosen from what the user is being asked to judge — not
from how much effort you want to spend:

| the question is about | `fidelity` | they see |
|---|---|---|
| layout, density, where things go | `wireframe` | greyscale structure, proportions, labels |
| color, brand, which palette | `color` | the real theme colors as flat blocks |
| material, finish, "more glass" | `texture` | colors + frost / elevation / edge / glow |

The same `mock` renders differently at each level. Asking "which palette?" at
`wireframe` makes the user judge blind; asking "which layout?" at `texture`
spends tokens on polish that is not under discussion.

A refinement round edits fields. 「玻璃更厚、阴影更明显、边界更亮」 is
`frost`+1, `elevation`+1, `edge`+1 — every other token and every node stays
exactly as it was. Resending a redesigned screen is how a one-line change
becomes a full-page rewrite; don't.

## When to escape to `body_html`

The vocabulary is deliberately small, because a small vocabulary is what keeps
a screen cheap to send and a revision a field edit. It holds the shapes design
questions actually take — page skeletons, device and window chrome,
high-frequency controls, dense data, dashboard widgets.

**If a kind holds the shape the user described, `screen` is not optional.**
Escaping a settings page, a dashboard, or a dialog flow to hand-written HTML
costs the user money and buys nothing — check the tables in `nodes.md` before
you decide. Escape only when nothing there holds the described shape:

- a silhouette the kinds have no shape for — a pill button, a slider, a date
  picker, a breadcrumb, a floating action button
- a layout the vocabulary has no shape for — a timeline, a kanban board, a
  flow diagram with connectors, a masonry grid
- a device or window the mocks cannot wear — a desktop window with a menu
  bar, a tablet split view, a watch face
- any detail the user named and the tokens cannot place — corner radius, type
  scale, spacing rhythm, an exact component silhouette

The user can also demand the medium outright ("用 HTML 给我看", "这个我要看到
位"). Honor it as given — no switching back to `screen` to save tokens.

When you do escape, tag it: `escape: "timeline"` — one or two words naming the
**shape** the vocabulary could not hold ("pill-button", "kanban"), never the
screen and never a sentence. It is never rendered. It is the ledger: the same
tag showing up twice is the evidence for what the vocabulary should grow into.

## Look at what you sent

You have a browser of your own — the `browser_*` tools drive a Chrome that
starts on the first call, so a session that never looks never pays for one.
It is separate from the user's canvas tab and needs no consent.

`canvas_open` returns the canvas URL. Open it and look:

1. `browser_navigate` to that URL (loopback, no approval).
2. `browser_screenshot` — the image is exactly what the user is seeing,
   options panel and all.

**A `body_html` escape: always look.** You wrote that markup by hand and it
can overflow or collide — screenshot before you call the screen done.

**A `screen`: on demand, not after every push.** The host renders it
deterministically from your tokens and you know its shape before you send it.
Reach for the browser when the outcome is open —
- copy long enough to wrap, or CJK labels in narrow columns
- candidates that must line up, or a layout holding a fixed proportion
- anything you would not bet on blind

Then fix it yourself: edit the fields and push again. "I think the button
wraps" is work you have not done — look, then fix, then show. The budget is
one look, one fix, one confirming look: a third round of measuring pixels is
you stalling, and every re-push re-asks the question the user may have just
answered. Only bring the user in when a decision is left, not when a
rendering is left.

Five tools change the page (`browser_click`, `browser_input_fill`,
`browser_select`, `browser_press_key`, `browser_evaluate`) and ask before
they run; `browser_evaluate` asks every time. Opening a URL outside the
machine asks too. Everything else — screenshot, snapshot, tabs, scroll — is
yours to use freely.

## Reference material

When the user points at something to match — a URL, a competitor, 「像 Notion
那种」 — go look before you propose anything. `browser_navigate` there,
`browser_screenshot`, and design from the image in front of you: density,
hierarchy, where the one accent sits. Never work from memory of what a
product looks like.

A file on disk works the same way (`file:///…`) and asks first, because it
is their disk and not a page. An image with no URL or path is a reference
you have not actually seen — ask where it lives rather than inventing one.

Once you have looked, put the image beside what you propose: `canvas_push`
takes `reference: { src, caption }` and the host renders it next to the
candidates, so the user compares against the real thing instead of your
memory of it. `src` is a path on disk or an http(s)/`data:` URI — the host
copies a file it can read and serves it from the canvas. For a page you just
opened, screenshot it to a scratch path and name that:

    browser_screenshot { "save_to": "/tmp/ref-01.png" }   # still shows you the image
    canvas_push { …, "reference": { "src": "/tmp/ref-01.png", "caption": "正泰首页" } }

Caption it in the user's words (「正泰首页」, not "ref-01"). One image per
screen: the question is "which candidate is closer to *this*", so if you
have two references, pick the one the user actually named.

## Versions

Every screen you push becomes a version. The canvas keeps the history itself:
a rail of past screens across the top of every page, and 「对比」 next to it —
the two versions side by side or wiped over each other with a slider, plus a
list of which fields actually changed. Nothing here is yours to build.

Put the version in the title when you are iterating (「v2 · 深色玻璃」), so the
rail reads like a history. When the user goes back to an older one — 「回看
v1」, 「并排看下」 — they are looking at the canvas, not asking you to push it
again: answer from what is on screen.

## Design decision record

When the discussion lands — 「就这样」, 「按这个做」, or they move on to
building it — leave the record behind as `notes/design-decisions.md` (or
wherever they keep specs). The next step reads it as input, so hold the
shape:

    # 设计决策记录

    ## 定了什么
    - 大屏顶部导航 → 选了 侧边栏导航
    - 配色方向 → 跳过

    ## 为什么
    - 侧边栏导航：筛选常驻，信息密度更高

    ## 还有什么没定
    - 图表样式：跳过了，等数据形态定了再说

One line per question the canvas asked, in the order it was asked:
`<问题> → 选了 <label>`, or `<问题> → 跳过`. A pick is what came back as
`(画布选择)` or from `ask_user` — quote the label the user saw, not a
paraphrase. A question nobody answered goes in 还有什么没定, and so does
anything they raised and left hanging. 为什么 is the reason they gave, in
their words where you have them — never a rationale you invented after the
fact.

## While the canvas is open

- The browser opens only after the user consents (a one-time prompt). Accept
  the tool's return text — `awaiting consent` or `browser launched` — and
  keep working; never retry `canvas_open` to force a browser open.
- A pick arrives as a user message starting with `(画布选择)`. Treat it as
  the user's answer to that screen's question and continue from it — don't
  re-ask what they just answered.
