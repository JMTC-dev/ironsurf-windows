# IronSurf's boa fork — `thirdparty/boa`

## What this is

A vendored fork of **`boa_ast` 0.22.0** (taken verbatim from the crates.io
registry source, `cargo` checksum-clean), carrying **one bug fix**. It is wired
into the workspace through `[patch.crates-io]` in the root `Cargo.toml`, so
`boa_engine`/`boa_parser`/`boa_runtime` (still stock 0.22.0 from crates.io)
compile against the patched `boa_ast`. The fix lives in git and survives
`cargo update` / clean checkouts — no registry patching workflow.

Only the crate that actually contains the bug is forked. All other boa crates
are consumed from crates.io untouched.

## The bug (upstream, unfixed as of boa `main` 2026-08)

Upstream `boa_ast::scope::Scope::escape_this_in_enclosing_function_scope`
(`src/scope.rs`) walks the compile-time scope chain when a `this` reference is
seen inside an arrow function and marks `this_escaped` on the **first enclosing
function scope** after a function boundary — *even when that scope belongs to
another arrow function*, which has no `this` binding of its own.

Consequence chain (`boa_engine` 0.22 runtime):

1. `ScopeIndexVisitor::visit_function_like` (`src/scope_analyzer.rs`) computes
   `requires_function_scope = function_scope().escaped_this() || ...` only for
   non-arrow functions. The wrong mark (on the outer arrow) is never read, and
   the real `this`-binding function (e.g. a class method) is never marked.
2. The method compiles without `HasFunctionScope`, so at call time no
   `FunctionEnvironment` (the one environment object holding the initialized
   `this`) is pushed.
3. `Opcode::This` inside the nested arrows walks the live environment chain,
   skips the lexical arrow envs, and bottoms out at the `ModuleEnvironment`
   (`Some(undefined)`) in modules — or falls off the chain to `globalThis` in
   classic scripts.

Observed in the wild: `@react-spring/core`'s
`observerAdded(e){ e==1 && each(this.inputs, ins => each(ins, v => isFluid(v) && addObserver(v, this))) }`
registered `undefined` into a spring's observers `Set`, and the next rAF
frame-loop flush threw `TypeError: cannot convert 'null' or 'undefined' to
object` (`packs/chevron_right-*.js` on infosec.exchange / Mastodon Glitch;
IronSurf task #69). Any `this` reached through **2+ nested arrows** is broken;
one arrow level happens to work, which is what made this look page-specific.

Reproduction (works on stock boa 0.22, module *or* classic script):

```js
class C { m(){ [0].forEach(() => { [0].forEach(() => {
  console.log(this instanceof C); // false — should be true
}); }); } }
new C().m();
```

## The fix

Two hunks, both minimal and additive (diff vs stock 0.22.0 preserved in
[`boa.diff`](boa.diff); regenerate with the script at the bottom):

* `src/scope.rs` — `Inner` gains `arrow: Cell<bool>` (default `false`),
  `Scope::set_arrow()` / `Scope::is_arrow()`, and
  `escape_this_in_enclosing_function_scope` now keeps walking while the scope
  it just crossed belongs to an arrow function, marking the nearest enclosing
  **non-arrow** function scope (the spec's real `this` binder). Behaviour for
  single-arrow escapes, plain functions, class fields (function scopes that are
  not arrows), module/global termination and direct eval is unchanged; no
  public API is removed or altered.
* `src/scope_analyzer.rs` — `function_declaration_instantiation` (the single
  choke point that creates function scopes for every function-like node) calls
  `set_arrow()` when the node is an (async) arrow function.

## Verification

* `cargo test -p boa_ast --tests` — upstream unit tests plus the fork
  regression file `boa_ast/tests/escape_this_nested_arrows.rs`
  (`autotests = false` in the vendored manifest, so the new `[[test]]` entry
  was registered explicitly).
* `cargo test -p ironsurf-tab` — the unit
  `js::tests::this_survives_nested_arrows_in_modules_and_scripts` (module +
  classic, 1–3 arrow levels, receiver identity) and the localhost e2e
  `tests/react_spring_observer_this.rs` (fixture title `SPRING-OK`, no
  `console [error]` on the tab's stderr). The pre-fix pins
  (`boa_0_22_bug_this_is_undefined_through_nested_arrows_in_modules`,
  `boa_bug_repro_undefined_observer_throws_the_production_typeerror`) were
  inverted on fix day.
* A 15-case `this`-identity probe matrix (methods, 1–3 nested arrows, async
  arrows, class fields, static methods, detached `.call`, blocks): stock 0.22
  fails 8, patched passes 15.

## Upstream

Not yet upstreamed; boa `main` (`core/ast/src/scope.rs`) still contains the
identical walk as of this fork. TODO: file the bug with the repro above and
send the two-hunk fix upstream; on acceptance, retire this fork by dropping the
`[patch.crates-io]` entry.

## Regenerating the diff

```sh
cargo update -p boa_ast --precise 0.22.0 --dry-run # must be a no-op
diff -u <(cd ~/.cargo/registry/src/*/boa_ast-0.22.0 && tar cf - .) \
        <(cd thirdparty/boa/boa_ast && tar cf - .) # or: git diff against the import commit
```
