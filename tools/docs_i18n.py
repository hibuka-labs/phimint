#!/usr/bin/env python3
"""Generate docs/index.html (English, default) from docs/zh/index.html (Chinese).

The English page is the default one: it sits at the site root, carries the
x-default hreflang, and is what GitHub / README / social links point at. The
Chinese page lives at /zh/ and is the single source of truth for copy: it
carries `data-en` / `data-en-aria` / `data-en-alt` attributes with the English
strings. This script bakes the English variant into the root page so each URL
carries exactly one language (required for hreflang / bilingual SEO):

  docs/zh/index.html -> Chinese, canonical https://hibuka-labs.github.io/phimint/zh/
  docs/index.html    -> English, canonical https://hibuka-labs.github.io/phimint/   (GENERATED)

Usage:
  python3 tools/docs_i18n.py           # regenerate docs/index.html
  python3 tools/docs_i18n.py --check   # exit 1 if the generated page is stale

Run this after any copy change in docs/zh/index.html.
"""

import argparse
import difflib
import json
import re
import sys
from html.parser import HTMLParser
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SRC = REPO / "docs" / "zh" / "index.html"
OUT = REPO / "docs" / "index.html"

BASE = "https://hibuka-labs.github.io/phimint"

GITEE_CMD = (
    "curl -fsSL https://gitee.com/chenkangzeng_admin/phimint/releases/download/"
    "latest/install.sh | PHIMINT_MIRROR=gitee bash"
)
GITHUB_CMD = (
    "curl -fsSL https://github.com/hibuka-labs/phimint/releases/latest/download/"
    "install.sh | bash"
)

VOID = {
    "area", "base", "br", "col", "embed", "hr", "img", "input",
    "link", "meta", "param", "source", "track", "wbr",
}
RAWTEXT = {"script", "style"}
# Start tags that implicitly close a still-open same-group tag (browser nesting).
IMPLIED_CLOSE = {
    "p": {"p"},
    "li": {"li"},
    "dt": {"dt", "dd"},
    "dd": {"dt", "dd"},
    "tr": {"tr", "td", "th"},
    "td": {"td", "th"},
    "th": {"td", "th"},
    "option": {"option"},
}


class Element:
    def __init__(self, tag, attrs):
        self.tag = tag
        self.attrs = list(attrs)  # [(name, value|None)]
        self.children = []

    def get(self, name):
        for k, v in self.attrs:
            if k == name:
                return v
        return None

    def set(self, name, value):
        for i, (k, _) in enumerate(self.attrs):
            if k == name:
                self.attrs[i] = (k, value)
                return
        self.attrs.append((name, value))

    def remove(self, name):
        self.attrs = [(k, v) for k, v in self.attrs if k != name]

    def classes(self):
        return [c for c in (self.get("class") or "").split() if c]

    def set_classes(self, cs):
        if cs:
            self.set("class", " ".join(cs))
        else:
            self.remove("class")


class Comment:
    def __init__(self, data):
        self.data = data


class Decl:
    def __init__(self, data):
        self.data = data


class DomParser(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.root = Element("#root", [])
        self.stack = [self.root]

    def handle_starttag(self, tag, attrs):
        closer = IMPLIED_CLOSE.get(tag)
        if closer:
            while len(self.stack) > 1 and self.stack[-1].tag in closer:
                self.stack.pop()
        el = Element(tag, attrs)
        self.stack[-1].children.append(el)
        if tag not in VOID:
            self.stack.append(el)

    def handle_startendtag(self, tag, attrs):
        self.stack[-1].children.append(Element(tag, attrs))

    def handle_endtag(self, tag):
        if tag in VOID:
            return
        for i in range(len(self.stack) - 1, 0, -1):
            if self.stack[i].tag == tag:
                del self.stack[i:]
                return

    def handle_data(self, data):
        self.stack[-1].children.append(data)

    def handle_comment(self, data):
        self.stack[-1].children.append(Comment(data))

    def handle_decl(self, decl):
        self.root.children.append(Decl(decl))


def parse(html_str):
    p = DomParser()
    p.feed(html_str)
    p.close()
    return p.root.children


def esc_text(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def esc_attr(s):
    return s.replace("&", "&amp;").replace('"', "&quot;")


def serialize(nodes):
    out = []
    for n in nodes:
        if isinstance(n, str):
            out.append(esc_text(n))
        elif isinstance(n, Comment):
            out.append("<!--" + n.data + "-->")
        elif isinstance(n, Decl):
            out.append("<!" + n.data + ">")
        else:
            attrs = "".join(
                f" {k}" if v is None else f' {k}="{esc_attr(v)}"' for k, v in n.attrs
            )
            if n.tag in VOID:
                out.append(f"<{n.tag}{attrs}>")
            elif n.tag in RAWTEXT:
                inner = "".join(c for c in n.children if isinstance(c, str))
                out.append(f"<{n.tag}{attrs}>{inner}</{n.tag}>")
            else:
                out.append(f"<{n.tag}{attrs}>{serialize(n.children)}</{n.tag}>")
    return "".join(out)


def walk(el):
    yield el
    for c in el.children:
        if isinstance(c, Element):
            yield from walk(c)


def find_one(el, pred):
    for e in walk(el):
        if pred(e):
            return e
    return None


def en_seo_block(title_en, desc_en):
    """The English SEO head block that replaces the zh one inside SEO-HEAD markers."""
    d = json.dumps(desc_en, ensure_ascii=False)
    t = esc_attr(title_en)
    de = esc_attr(desc_en)
    return f'''<!-- ===== SEO: canonical / hreflang / social / structured data ===== -->
<link rel="canonical" href="{BASE}/">
<link rel="alternate" hreflang="zh-CN" href="{BASE}/zh/">
<link rel="alternate" hreflang="en" href="{BASE}/">
<link rel="alternate" hreflang="x-default" href="{BASE}/">
<meta property="og:type" content="website">
<meta property="og:site_name" content="phimint">
<meta property="og:title" content="{t}">
<meta property="og:description" content="{de}">
<meta property="og:url" content="{BASE}/">
<meta property="og:image" content="{BASE}/assets/og-image.png">
<meta property="og:image:width" content="1200">
<meta property="og:image:height" content="630">
<meta property="og:image:alt" content="phimint — open-source AI coding agent">
<meta property="og:locale" content="en_US">
<meta name="twitter:card" content="summary_large_image">
<meta name="twitter:image" content="{BASE}/assets/og-image.png">
<meta name="twitter:title" content="{t}">
<meta name="twitter:description" content="{de}">
<script type="application/ld+json">
{{
  "@context": "https://schema.org",
  "@type": "SoftwareApplication",
  "name": "phimint",
  "description": {d},
  "url": "{BASE}/",
  "applicationCategory": "DeveloperApplication",
  "applicationSubCategory": "AI coding agent",
  "operatingSystem": "macOS, Linux, Windows",
  "isAccessibleForFree": true,
  "license": "https://opensource.org/licenses/MIT",
  "codeRepository": "https://github.com/hibuka-labs/phimint",
  "downloadUrl": "https://github.com/hibuka-labs/phimint/releases/latest/download/install.sh",
  "offers": {{ "@type": "Offer", "price": "0", "priceCurrency": "USD" }}
}}
</script>'''


def replace_marker(children, new_nodes):
    """Replace the body of `<!-- SEO-HEAD --> ... <!-- /SEO-HEAD -->` with new_nodes."""
    out = []
    i = 0
    while i < len(children):
        c = children[i]
        if isinstance(c, Comment) and c.data.strip() == "SEO-HEAD":
            out.append(c)
            i += 1
            while i < len(children) and not (
                isinstance(children[i], Comment)
                and children[i].data.strip() == "/SEO-HEAD"
            ):
                i += 1
            out.extend(new_nodes)
            if i < len(children):
                out.append(children[i])
                i += 1
        else:
            if isinstance(c, Element):
                c.children = replace_marker(c.children, new_nodes)
            out.append(c)
            i += 1
    return out


def to_english(children):
    html_el = find_one(_as_root(children), lambda e: e.tag == "html")
    html_el.set("lang", "en")
    html_el.set("data-lang", "en")  # L(zh, en) helper picks English strings

    title_el = find_one(html_el, lambda e: e.tag == "title")
    desc_el = find_one(html_el, lambda e: e.tag == "meta" and e.get("name") == "description")
    if not title_el or not title_el.get("data-en"):
        sys.exit("error: <title data-en=...> not found in source")
    if not desc_el or not desc_el.get("data-en"):
        sys.exit("error: <meta name=\"description\" data-en=...> not found in source")
    title_en = title_el.get("data-en")
    desc_en = desc_el.get("data-en")

    # 1) bake data-en strings into content; drop zh-only subtrees; unwrap en-only
    def transform(nodes):
        out = []
        for c in nodes:
            if isinstance(c, Element):
                if "zh-only" in c.classes():
                    continue
                c.children = transform(c.children)
                cs = c.classes()
                if "en-only" in cs:
                    c.set_classes([x for x in cs if x != "en-only"])
                de, da, dalt = c.get("data-en"), c.get("data-en-aria"), c.get("data-en-alt")
                if da is not None:
                    c.set("aria-label", da)
                    c.remove("data-en-aria")
                if dalt is not None:
                    c.set("alt", dalt)
                    c.remove("data-en-alt")
                if de is not None:
                    if c.tag == "meta":
                        c.set("content", de)
                    elif c.tag == "title":
                        c.children = [de]
                    else:
                        c.children = parse(de)
                    c.remove("data-en")
                    c.remove("data-zh")
                nav = c.get("data-lang-nav")
                alt = c.get("data-lang-alt")
                if nav == "zh":
                    c.set("href", "zh/")
                    c.set_classes([x for x in c.classes() if x != "on"])
                elif nav == "en":
                    c.set("href", "./")
                    cs = c.classes()
                    if "on" not in cs:
                        cs.append("on")
                    c.set_classes(cs)
                if alt == "zh":
                    c.set("href", "zh/")
                elif alt == "other":
                    # "switch to the other language": on the Chinese page this
                    # points at the English root, on the English page at /zh/.
                    c.set("href", "zh/")
                    c.set("hreflang", "zh-CN")
                if nav is not None:
                    c.remove("data-lang-nav")
                if alt is not None:
                    c.remove("data-lang-alt")
                # The English page lives at the site root: drop the "../" the
                # Chinese page (under /zh/) needs for its relative assets.
                for attr in ("src", "href"):
                    v = c.get(attr)
                    if v and v.startswith("../") and not v.startswith("../../"):
                        c.set(attr, v[3:])
                out.append(c)
            else:
                out.append(c)
        return out

    children[:] = transform(children)

    # 2) swap the marked zh SEO head block for the English one
    children[:] = replace_marker(children, parse("\n" + en_seo_block(title_en, desc_en)))
    return title_en, desc_en


def _as_root(children):
    r = Element("#root", [])
    r.children = children
    return r


def self_check(text, title_en):
    problems = []
    if re.search(r'\bdata-en[a-z-]*="', text):
        problems.append("data-en attribute leaked into output")
    if re.search(r'class="[^"]*zh-only', text):
        problems.append("zh-only element leaked into output")
    if re.search(r'class="[^"]*en-only', text):
        problems.append("en-only element leaked into output")
    if 'lang="en"' not in text:
        problems.append('missing <html lang="en">')
    if GITHUB_CMD not in text:
        problems.append("English install command missing")
    if GITEE_CMD in text:
        problems.append("Chinese install command leaked into output")
    if re.search(r'(?:src|href)="\.\./', text):
        problems.append('"../" relative path left in output (English page is at the site root)')
    if 'src="demo.gif"' not in text:
        problems.append("demo.gif not rewritten for the root-relative English page")
    if esc_text(title_en) not in text:
        problems.append("English <title> missing")
    if '"og:locale" content="en_US"' not in text:
        problems.append("og:locale en_US missing")
    # hreflang trio, both directions
    for want in (
        f'<link rel="canonical" href="{BASE}/">',
        f'<link rel="alternate" hreflang="zh-CN" href="{BASE}/zh/">',
        f'<link rel="alternate" hreflang="en" href="{BASE}/">',
        f'<link rel="alternate" hreflang="x-default" href="{BASE}/">',
    ):
        if want not in text:
            problems.append(f"missing SEO tag: {want}")
    # the footer/header language links must point at the sibling page
    if 'href="zh/"' not in text:
        problems.append("language switch does not link to /zh/")
    if re.search(r'href="\./"[^>]*hreflang="en"', text) is None:
        problems.append("English self link missing")
    return problems


def root_warnings(children):
    """Sync guard: the Chinese page's hand-written SEO block must match its title/description."""
    root = _as_root(children)
    title_el = find_one(root, lambda e: e.tag == "title")
    desc_el = find_one(root, lambda e: e.tag == "meta" and e.get("name") == "description")
    og_title = find_one(root, lambda e: e.tag == "meta" and e.get("property") == "og:title")
    og_desc = find_one(root, lambda e: e.tag == "meta" and e.get("property") == "og:description")
    warns = []
    if title_el and og_title:
        t = "".join(c for c in title_el.children if isinstance(c, str))
        if og_title.get("content") != t:
            warns.append("docs/zh/index.html: og:title is out of sync with <title>")
    if desc_el and og_desc and og_desc.get("content") != desc_el.get("content"):
        warns.append("docs/zh/index.html: og:description is out of sync with meta description")
    return warns


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true", help="fail if generated page is stale")
    args = ap.parse_args()

    source = SRC.read_text(encoding="utf-8")
    children = parse(source)
    warns = root_warnings(children)
    for w in warns:
        print(f"warning: {w}", file=sys.stderr)

    title_en, desc_en = to_english(children)
    out = serialize(children)
    out = out.replace(GITEE_CMD, GITHUB_CMD)  # static install line (JS zh branch is dead code here)

    problems = self_check(out, title_en)
    if problems:
        for p in problems:
            print(f"error: {p}", file=sys.stderr)
        sys.exit(1)

    if args.check:
        current = OUT.read_text(encoding="utf-8") if OUT.exists() else ""
        if current != out:
            diff = difflib.unified_diff(
                current.splitlines(), out.splitlines(),
                "docs/index.html (on disk)", "docs/index.html (regenerated)",
                lineterm="", n=1,
            )
            print("\n".join(list(diff)[:40]), file=sys.stderr)
            print("error: docs/index.html is stale; run: python3 tools/docs_i18n.py", file=sys.stderr)
            sys.exit(1)
        print("ok: docs/index.html is up to date")
        return

    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(out, encoding="utf-8")
    print(f"wrote {OUT.relative_to(REPO)} ({len(out)} bytes, title: {title_en})")


if __name__ == "__main__":
    main()
