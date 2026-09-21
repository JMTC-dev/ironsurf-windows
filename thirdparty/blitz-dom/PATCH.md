# blitz-dom 0.3.0-beta.2 — IronSurf patch

One line, one invariant.

## The bug: `:defined` never matched, so every element was `:not(:defined)`

`src/stylo.rs`, `StyloElement::match_non_ts_pseudo_class` answered
`NonTSPseudoClass::Defined => false` — the `:defined` selector **never**
matched any element. Any stylesheet using the standard web-components
FOUC-prevention idiom therefore hid the whole page forever:

```css
/* shipped by reddit (styles-css-*.css rule 13) and lit-based sites: */
:not(:defined):not(faceplate-auto-height-animator,…) { visibility: hidden }
```

In a real browser `:defined` matches every non-custom element and every
custom element whose `customElements` definition ran, so the rule only
hides not-yet-upgraded custom elements for the first milliseconds. With
`Defined => false`, `:not(:defined)` matched EVERY element, so the rule
painted `visibility:hidden` onto the entire tree: https://www.reddit.com/
loaded (DOM ~550 KB, 25 `shreddit-post`s, layout boxes correct) and
painted exactly one command — the `html` background — with zero glyphs,
zero fills, zero strokes. Bisected (`--dump-paint` over a rule-by-rule
split of the sheet) to exactly that rule.

## The fix (semantic, not cosmetic)

IronSurf's JS bridge has **no custom-element upgrades**: no shadow
roots, no lifecycle callbacks, no element is ever "awaiting definition"
(the `customElements` registry that later joined the bridge is a
lookup/`whenDefined` surface only — `define()` records a constructor,
never upgrades a node). The honest answer to `:defined` in such an engine is
**`true` for every element** — the state a browser reaches once all
definitions have run (the steady state pages are written for; sites
that *require* the pre-upgrade hidden state cannot have it, which is
precisely the gap that leaves Reddit blank without the patch).

```diff
-            NonTSPseudoClass::Defined => false,
+            NonTSPseudoClass::Defined => true,
```

Wired through `[patch.crates-io]` in the workspace `Cargo.toml`
(`blitz-dom = { path = "thirdparty/blitz-dom" }`), same pattern as the
boa_ast fork in `thirdparty/boa/PATCH.md`.

Task: reddit.com/r/LocalLLaMA load (2026-09-21 session, TASKLOG entry).
