//! IronSurf fork regression tests for the vendored `boa_ast` 0.22.0 fix in
//! `Scope::escape_this_in_enclosing_function_scope` (see `../PATCH.md`).
//!
//! Upstream stopped the `this` escape at the first enclosing function scope —
//! even when that scope belongs to an arrow function, which cannot bind
//! `this`. These tests exercise the scope API the way the scope analyzer uses
//! it (function scopes for functions, lexical body scopes, `set_arrow()` on
//! arrow function scopes) and pin which scope must end up `this_escaped`.

use boa_ast::scope::Scope;

fn module() -> Scope {
    Scope::new_global()
}

/// `method -> arrow -> arrow -> this`: the escape must reach the method.
#[test]
fn this_escapes_through_nested_arrows_to_the_real_function() {
    let m = module();
    let method = Scope::new(m.clone(), true); // binds `this`
    let outer_arrow = Scope::new(method.clone(), true);
    outer_arrow.set_arrow();
    let outer_body = Scope::new(outer_arrow.clone(), false);
    let inner_arrow = Scope::new(outer_body.clone(), true);
    inner_arrow.set_arrow();
    let inner_body = Scope::new(inner_arrow.clone(), false);

    inner_body.escape_this_in_enclosing_function_scope();

    assert!(
        method.escaped_this(),
        "`this` seen through nested arrows must escape to the nearest \
         non-arrow function scope (the `this` binder)"
    );
    assert!(
        !inner_arrow.escaped_this() && !outer_arrow.escaped_this(),
        "arrow-function scopes have no `this` binding and must not be marked"
    );
}

/// One arrow level keeps working exactly as before (this was upstream's only
/// correct case).
#[test]
fn single_arrow_escape_still_marks_the_enclosing_function() {
    let m = module();
    let method = Scope::new(m.clone(), true);
    let arrow = Scope::new(method.clone(), true);
    arrow.set_arrow();
    let body = Scope::new(arrow.clone(), false);

    body.escape_this_in_enclosing_function_scope();

    assert!(method.escaped_this());
    assert!(!arrow.escaped_this());
}

/// A `this`-binding function expression nested between arrows stops the walk:
/// it owns the binding arrows inside it refer to.
#[test]
fn walk_stops_at_the_first_non_arrow_function() {
    let m = module();
    let method = Scope::new(m.clone(), true);
    let arrow1 = Scope::new(method.clone(), true);
    arrow1.set_arrow();
    let fe = Scope::new(arrow1.clone(), true); // plain function: binds `this`
    let arrow2 = Scope::new(fe.clone(), true);
    arrow2.set_arrow();
    let body = Scope::new(arrow2.clone(), false);

    body.escape_this_in_enclosing_function_scope();

    assert!(fe.escaped_this(), "nearest binder wins");
    assert!(
        !method.escaped_this(),
        "the walk must not run past a real `this` binder"
    );
}

/// Top-level arrows in a module: nothing to mark except (harmlessly) the
/// module/global scope — same termination as upstream for that shape.
#[test]
fn module_top_level_arrows_terminate_at_the_module_scope() {
    let m = module();
    let outer_arrow = Scope::new(m.clone(), true);
    outer_arrow.set_arrow();
    let outer_body = Scope::new(outer_arrow.clone(), false);
    let inner_arrow = Scope::new(outer_body, true);
    inner_arrow.set_arrow();
    let body = Scope::new(inner_arrow.clone(), false);

    body.escape_this_in_enclosing_function_scope();

    assert!(
        !inner_arrow.escaped_this() && !outer_arrow.escaped_this(),
        "arrows are never the marked scope"
    );
}
