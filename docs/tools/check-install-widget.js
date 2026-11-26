#!/usr/bin/env node
/* Exercise the install-widget logic from docs/index.html without a browser.
 *
 * A tiny DOM shim stands in for the parts the widget touches: class lists,
 * dataset, and a querySelectorAll that matches the few selectors used.
 * The script block is pulled straight out of the page, so this checks the
 * shipped code, not a copy of it.
 *
 * Usage: node docs/tools/check-install-widget.js [path/to/index.html]
 */
const fs = require("fs");
const path = require("path");

const pagePath = process.argv[2] || path.join(__dirname, "..", "index.html");
const html = fs.readFileSync(pagePath, "utf8");

// ---- DOM shim ------------------------------------------------------------
class El {
  constructor(tag, attrs = {}) {
    this.tagName = tag.toUpperCase();
    this.attrs = { ...attrs };
    this.dataset = { ...(attrs.dataset || {}) };
    this.children = [];
    this._classes = new Set((attrs.class || "").split(/\s+/).filter(Boolean));
    this.textContent = "";
    this.innerHTML = "";
    this.listeners = {};
  }
  get classList() {
    const set = this._classes;
    return {
      add: (...c) => c.forEach((x) => set.add(x)),
      remove: (...c) => c.forEach((x) => set.delete(x)),
      toggle: (c, force) => (force ? set.add(c) : set.delete(c)),
      contains: (c) => set.has(c),
    };
  }
  get className() { return [...this._classes].join(" "); }
  setAttribute(k, v) {
    this.attrs[k] = v;
    if (k.startsWith("data-")) {
      const key = k.slice(5).replace(/-([a-z])/g, (_, c) => c.toUpperCase());
      this.dataset[key] = v;
    }
  }
  getAttribute(k) { return this.attrs[k] ?? null; }
  addEventListener(ev, fn) { (this.listeners[ev] ||= []).push(fn); }
  click() { (this.listeners.click || []).forEach((fn) => fn({ target: this })); }
  querySelector(sel) { return this.querySelectorAll(sel)[0] || null; }
  querySelectorAll(sel) {
    const out = [];
    const groups = sel.split(",").map((x) => x.trim());
    for (const group of groups) {
      const steps = group.split(/\s+/);
      // Match step 0 anywhere in this subtree, then each following step
      // among the descendants of what matched.
      const all = [];
      const collect = (el) => { all.push(el); el.children.forEach(collect); };
      collect(this);
      const first = all.filter((el) => matchStep(el, steps[0]));
      if (steps.length === 1) { first.forEach((h) => out.push(h)); continue; }
      const descend = (el, stepIdx) => {
        const step = steps[stepIdx];
        const last = stepIdx === steps.length - 1;
        const hits = el.children.filter((c) => matchStep(c, step));
        if (last) { hits.forEach((h) => out.push(h)); return; }
        hits.forEach((h) => h.children.forEach((c) => descend(c, stepIdx + 1)));
      };
      first.forEach((h) => descend(h, 1));
    }
    return out;
  }
}

/* Does one element match one simple selector step? */
function matchStep(el, step) {
  if (step.startsWith(".")) return el._classes.has(step.slice(1));
  if (step.startsWith("[")) {
    const attr = step.slice(1, -1).split("=")[0];
    return attr in el.dataset;
  }
  return el.tagName === step.toUpperCase();
}

function makeButton(cmd) {
  return new El("button", { class: "", dataset: { cmd } });
}
function makeInstallBlock() {
  const block = new El("div", { class: "install" });
  const platformRow = new El("div", { class: "platform-row" });
  ["windows", "macos", "linux"].forEach((p) => {
    const b = new El("button", { dataset: { platform: p } });
    platformRow.children.push(b);
  });
  block.children.push(platformRow);
  const tabs = new El("div", { class: "install-tabs" });
  ["curl", "brew", "scoop", "npm", "cargo"].forEach((c) => {
    tabs.children.push(makeButton(c));
  });
  block.children.push(tabs);
  const code = new El("code");
  const hl = new El("span", { class: "hl" });
  code.children.push(hl);
  block.children.push(code);
  const copy = new El("button", { class: "copy" });
  block.children.push(copy);
  return block;
}

// ---- Pull the widget script out of the page ------------------------------
// The last <script> block also carries the Pretext library glue, which needs
// the minified bundle. Take only the IIFE that owns the install widget.
const scripts = [...html.matchAll(/<script>([\s\S]*?)<\/script>/g)].map((m) => m[1]);
const full = scripts.find((s) => s.includes("PLATFORM_TABS"));
if (!full) {
  console.error("FAIL: could not find the install-widget script in " + pagePath);
  process.exit(1);
}
const start = full.indexOf('(function () {\n  "use strict";');
const widgetSrc = start === -1 ? full : full.slice(start);

// ---- Minimal globals the script touches ----------------------------------
const blocks = [makeInstallBlock(), makeInstallBlock()];
const platformRows = blocks.map((b) => b.querySelector(".platform-row"));
global.document = {
  documentElement: {
    getAttribute: () => "en",
    setAttribute() {},
    removeAttribute() {},
  },
  querySelectorAll: (sel) => {
    if (sel === ".install") return blocks;
    if (sel === ".platform-row") return platformRows;
    if (sel === ".install-tabs button")
      return blocks.flatMap((b) => b.querySelectorAll(".install-tabs button"));
    if (sel === "[data-theme-opt]") return [];
    if (sel === "[data-lang]") return [];
    return [];
  },
  querySelector: () => null,
  addEventListener() {},
};
// Node 22 ships a read-only `navigator` global, so a plain assignment is
// ignored and the test would silently run against the host's real platform.
// Define it as a configurable property instead.
Object.defineProperty(globalThis, "navigator", {
  value: { platform: "MacIntel", userAgentData: null },
  writable: true,
  configurable: true,
});
global.localStorage = { getItem: () => null, setItem() {} };
global.matchMedia = () => ({ matches: false, addEventListener() {} });
global.window = {
  addEventListener() {},
  matchMedia: global.matchMedia,
  localStorage: global.localStorage,
};

const sandbox = { document: global.document, navigator: global.navigator, window: global.window,
                  console, localStorage: global.localStorage, matchMedia: global.matchMedia };
sandbox.globalThis = sandbox;
const vm = require("vm");
vm.createContext(sandbox);
vm.runInContext(widgetSrc, sandbox);

// ---- Assertions ----------------------------------------------------------
let failures = 0;
function check(label, actual, expected) {
  const ok = actual === expected;
  if (!ok) failures++;
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) console.log(`      expected: ${expected}\n      actual:   ${actual}`);
}

const block = blocks[0];
const tabs = block.querySelectorAll(".install-tabs button");
/* The visible tabs, in the order the page shows them. CSS `order` sets the
   visual order per platform; the shim has no layout, so the test sorts by
   the same rules the stylesheet encodes. */
const ORDER = {
  windows: { scoop: 1, curl: 2, npm: 8, cargo: 9 },
  macos: { brew: 1, curl: 2, npm: 8, cargo: 9 },
  linux: { curl: 1, npm: 8, cargo: 9 },
};
const visible = () => {
  const p = detected();
  const rules = ORDER[p] || ORDER.macos;
  return tabs
    .filter((t) => !t.classList.contains("hidden"))
    .sort((a, b) => (rules[a.dataset.cmd] ?? 9) - (rules[b.dataset.cmd] ?? 9))
    .map((t) => t.dataset.cmd);
};
const active = () => (tabs.find((t) => t.classList.contains("on")) || {}).dataset.cmd;
const hlText = () => block.querySelector("code").querySelector(".hl").textContent;
const platformButtons = platformRows[0].querySelectorAll("button");
const clickPlatform = (p) => platformButtons.find((b) => b.dataset.platform === p).click();

console.log("detected platform (navigator=MacIntel):");
const detected = () =>
  (platformButtons.find((b) => b.classList.contains("on")) || { dataset: {} }).dataset.platform;
check("macos detected", detected(), "macos");
check("macos tabs", visible().join(","), "brew,curl,npm,cargo");
check("macos active tab", active(), "brew");
check("macos brew command", hlText(), "brew install hibuka-labs/phimint/phimint");

console.log("\nswitch to Windows:");
clickPlatform("windows");
check("windows tabs", visible().join(","), "scoop,curl,npm,cargo");
check("windows active tab", active(), "scoop");
check("windows platform button marked", platformButtons.find((b) => b.dataset.platform === "windows").classList.contains("on"), true);

const winTabs = tabs;
winTabs.find((t) => t.dataset.cmd === "curl").click();
check("windows curl is the PowerShell one", hlText(),
  "irm https://github.com/hibuka-labs/phimint/releases/latest/download/install.ps1 | iex");

console.log("\nswitch to Linux:");
clickPlatform("linux");
check("linux tabs", visible().join(","), "curl,npm,cargo");
check("linux active tab", active(), "curl");
check("linux curl command", hlText(),
  "curl -fsSL https://github.com/hibuka-labs/phimint/releases/latest/download/install.sh | bash");

console.log("\nsecond install block stays in sync:");
const block2 = blocks[1];
const tabs2 = block2.querySelectorAll(".install-tabs button");
check("block2 linux tabs", tabs2.filter((t) => !t.classList.contains("hidden")).map((t) => t.dataset.cmd).join(","), "curl,npm,cargo");
check("block2 active tab", (tabs2.find((t) => t.classList.contains("on")) || {}).dataset.cmd, "curl");

console.log(failures ? `\n${failures} failure(s)` : "\nall checks passed");
process.exit(failures ? 1 : 0);