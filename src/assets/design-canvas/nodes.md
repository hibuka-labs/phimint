# design-canvas · vocabulary reference

Load this when you compose a picture — the field names are a closed set and a
stray field is rejected, so they are not guessable. The judgment (where each
question goes, when to escape) is in `SKILL.md`.

## Envelope shape

```jsonc
{
  "title": "配色方向，选一个？",
  "options": [{ "id": "deep-blue", "label": "深空蓝", "note": "冷、克制" }],
  "screen": {
    "fidelity": "color",
    "mock": { "kind": "cols", "children": [ /* shared structure */ ] },
    "notice": "字体层级后续再定",
    "variants": [{
      "id": "deep-blue",
      "theme": { "bg": "#0A1428", "accent": "#00D4FF" },
      "emph": ["hero"],
      "content": { "hero": "128,640" }
    }]
  }
}
```

`screen` travels as the nested object itself in the tool arguments — write the
braces and quotes as structure, never as a `"…"` string holding escaped JSON.
Every stringified copy seen in production broke mid-escape and cost a re-push.

A node is `{ "kind": …, "id": …, …fields }`. Every node may take an `id` —
name the ones `emph` / `content` will point at. A kind only takes the fields
it declares.

## Layout nodes — where things sit

| kind | fields | use for |
|---|---|---|
| `frame` | `title?`, `body` | browser/dashboard window chrome around a body |
| `cols` / `rows` | `ratio?[]`, `children[]` | bands; `ratio` are relative weights (numbers, decimals fine: `[1, 5, 0.7]`). Without `ratio` every band hugs its content — give weights when bands should share height |
| `stack` | `count`, `child` | N copies of one cell |
| `box` | `w?`, `h?`, `text?` | a sized region (`w`/`h` are percentages of the parent — number or string, `18` = `18%`) |
| `text` | `text` | any one line of copy, including region annotations |
| `placeholder` | `text?` | empty region ("内容区") |

## Device & window chrome — what a form factor wears

| kind | fields | use for |
|---|---|---|
| `phone_shell` | `body` | a phone: status bar and home indicator are drawn for you |
| `topbar` | `title?`, `back?`, `action?` | mobile screen header (back needs a title) |
| `tabbar` | `items[]`, `active?` | bottom tab bar; `active` is the selected index |
| `nav` / `sidebar` | `items[]` | labelled desktop navigation (1–8 items) |
| `toolbar` | `items[]` | action strip; `items` are **label strings** (each becomes a small button). A filter or input is its own node, composed beside the strip in `cols` — never inside `items` |
| `dialog` | `title`, `text?` \| `body?`, `actions[]` | modal card; must say something |

## High-frequency controls

| kind | fields | use for |
|---|---|---|
| `input` | `text?` | text field |
| `button` | `text?` | a plain button |
| `switch` | `label?`, `on?` | toggle |
| `segment` | `items[]`, `active?` | segmented control; `active` is the selected index |
| `list_row` | `title`, `note?`, `meta?`, `chevron?`, `avatar?` | list cell (settings row, feed item) |
| `swatches` | — | the theme's color chips |

## Data & content

| kind | fields | use for |
|---|---|---|
| `kpi` | `label?`, `value`, `delta?` | number card |
| `hero` | `text` | oversized centre number |
| `sparkline` | `shape?: up\|down\|flat` | mini trend line |
| `ring` | `pct` 0–100, `label?` | donut progress |
| `bars` | `items[{label, value?}]` | ranking bars |
| `area` | `label?` | trend area chart |
| `viz` | `form: map\|funnel\|graph\|area`, `label?` | centre-piece visual |
| `table` | `cols[]`, `rows[][]` | dense data table (1–8 cols; a cell may be blank) |
| `tree` | `items[{label, depth?}]` | indented tree (`depth` 0–4, items 1–12) |

Bounds that keep a mock a mock: labelled item lists 1–8 (`nav`, `sidebar`,
`tabbar`, `segment`, `toolbar`) — label strings only, a node map in `items` is
always wrong; `bars` 1–10, `table` ≤10 rows and every row
exactly one cell per column (cols 1–8), `tree` ≤12 items, dialog `actions`
≤3, `active` must index a real item. Mock data is decorative — a
`ring` at 86% and a `sparkline` shape are stand-ins. The canvas is for design
judgment, not data review.

The vocabulary is closed: no `style`, no `class`, no `radius`, no font knob.
Anything outside these tables is not a field you may send — it goes to
`body_html` instead.

## Theme tokens

On `variants[].theme`:

- colors: `bg`, `panel`, `border`, `accent`, `accent2`, `text`, `text_bright` —
  `#rgb` / `#rrggbb` / `#rrggbbaa`, or `rgb()` / `hsl()`, nothing else
- texture knobs, only meaningful at `fidelity: "texture"`:
  `surface: glass|matte|metal`, `frost` 1–3 (blur), `elevation` 1–3 (shadow),
  `edge` 1–3 (border brightness), `glow` 0–2 (outer bloom)

## Variants

One card each, aligned to `options[].id`:

- `id` must match an `options[].id`. The caption is rendered from that
  option's `label`; never type it into the picture.
- `mock` — optional tree override, for candidates that are *structurally*
  different (layout comparisons). Otherwise the shared `screen.mock` is
  reused: a color comparison is one tree plus N themes.
- `emph: [node_id]` — the nodes that take the accent in *this* candidate.
  "Where does the one highlight go?" is a one-element list difference between
  variants, not two whole pictures. Ids resolve against the variant's own
  tree when it overrides `mock`.
- `content: {node_id: text}` — per-variant override of that node's **primary
  copy slot**; secondary slots stay as authored. Primary slots:

  | kind | slot |
  |---|---|
  | `frame` | `title` |
  | `box`, `input`, `button`, `text`, `placeholder`, `hero` | `text` |
  | `kpi` | `value` |
  | `ring`, `area`, `viz` | `label` |
  | `topbar`, `list_row` | `title` |
  | `switch` | `label` |
  | `dialog` | `text` (or `title` when it says it with `body`) |

  Nodes without one (`cols`, `rows`, `stack`, `nav`, `sidebar`, `tabbar`,
  `toolbar`, `segment`, `table`, `tree`, `bars`, `sparkline`, `swatches`,
  `phone_shell`) take no `content` override — change the shared tree or that
  variant's `mock`.

## Escaping to `body_html`

When a kind cannot hold what the user described, send raw HTML instead of a
`screen` — and tag the gap: `escape: "pill-button"`. The component classes are
still the contract:

- `.cards` / `.card` / `.card-body` / `.card-image` — side-by-side candidates
- `.split` with `.mock-nav` / `.mock-sidebar` / `.mock-content` — page layouts
- `.mockup` / `.mockup-header` / `.mockup-body` — a wireframe frame; inside it
  `.mock-button` / `.mock-input` for controls
- `.placeholder` — image or region slot
- `.pros-cons` / `.pros` / `.cons` — tradeoff lists
- `.notice` / `.note` / `.label` / `.section` — callouts and grouping

Never hand-write the options panel (`.options` / `.option`) in either path —
the host generates it from `options`.
