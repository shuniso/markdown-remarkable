---
title: markdown-remarkable sample
tags: [sample, markdown]
---
# markdown-remarkable sample

A sample file exercising every Markdown feature `markdown-remarkable` supports. Open it
with `markdown-remarkable examples/sample.md` (or render it with `--export`) and check
that everything below looks right. The YAML frontmatter at the very top of
this file (`title:` / `tags:`) is intentionally *not* rendered — if you can
see it on the page, that's a bug.

## Headings

### A level-3 heading

#### A level-4 heading

## Emphasis

*Italic*, **bold**, ***bold italic***, and ~~strikethrough~~ text, plus a
mix: **bold with *nested italic* inside**.

## Lists

Unordered:

- First item
- Second item
  - Nested item
  - Another nested item
- Third item

Ordered:

1. First step
2. Second step
3. Third step

Task list:

- [x] Write the spec
- [x] Scaffold the crate
- [ ] Ship it

## Tables

| Feature | Supported | Notes |
|---|:---:|---|
| Tables | Yes | GFM-style, with alignment |
| Footnotes | Yes | See below[^1] |
| Syntax highlighting | No | Out of scope (YAGNI) |

## Code

Inline code: `let answer = 42;`.

A fenced code block:

```rust
fn greet(name: &str) -> String {
    format!("Hello, {name}!")
}
```

## Blockquotes

> Simple is better than complex.
>
> — attributed to someone, probably.

## Links

[The pulldown-cmark crate](https://github.com/pulldown-cmark/pulldown-cmark)
is what turns this file into HTML.

## Footnotes

Here's a sentence with a footnote reference.[^1] And another one.[^2]

[^1]: This is the first footnote's text.
[^2]: This is the second footnote's text, with **formatting** inside it.

## Horizontal rule

---

That's everything.
