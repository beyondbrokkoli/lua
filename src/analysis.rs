use crate::ast::{CtorKey, Expr, Stmt};
use std::collections::{BTreeMap, BTreeSet};

pub struct AnalysisContext<'a> {
    pub sites: BTreeMap<*const Expr, usize>,
    pub ast: &'a [Stmt],
    /// Statement pointer -> source line (the diagnostic anchor for the
    /// shape, checker, and lowerer passes).
    pub stmt_lines: BTreeMap<*const Stmt, usize>,
    /// Ctor site id -> source line (parallel to `sites`' id space).
    pub ctor_lines: Vec<usize>,
}

pub fn build_context<'a>(
    ast: &'a [Stmt],
    stmt_line_seq: &[usize],
    ctor_lines: Vec<usize>,
) -> AnalysisContext<'a> {
    let mut sites = BTreeMap::new();
    number_sites(ast, &mut sites);
    assert_eq!(
        sites.len(),
        ctor_lines.len(),
        "ctor line sequence out of step with the site walk"
    );
    let mut stmt_lines = BTreeMap::new();
    let mut i = 0;
    record_stmt_lines(ast, &mut stmt_lines, stmt_line_seq, &mut i);
    assert_eq!(
        i,
        stmt_line_seq.len(),
        "stmt line sequence out of step with the AST walk"
    );

    AnalysisContext {
        sites,
        ast,
        stmt_lines,
        ctor_lines,
    }
}

/// Turn the parser's pre-order line sequence into the pointer-keyed
/// map. The walk must visit statements in the same order the parser
/// recorded their first tokens (function bodies in place inside their
/// expressions) — the assert in build_context holds them together.
fn record_stmt_lines(
    stmts: &[Stmt],
    map: &mut BTreeMap<*const Stmt, usize>,
    seq: &[usize],
    i: &mut usize,
) {
    for s in stmts {
        if *i >= seq.len() {
            return;
        }
        map.insert(s as *const Stmt, seq[*i]);
        *i += 1;
        record_stmt_exprs(s, map, seq, i);
    }
}

fn record_stmt_exprs(
    stmt: &Stmt,
    map: &mut BTreeMap<*const Stmt, usize>,
    seq: &[usize],
    i: &mut usize,
) {
    let expr =
        |e: &Expr, map: &mut BTreeMap<*const Stmt, usize>, i: &mut usize| {
            record_expr_lines(e, map, seq, i)
        };
    match stmt {
        Stmt::LocalDecl { exprs, .. } | Stmt::Print { exprs } => {
            for e in exprs {
                expr(e, map, i);
            }
        }
        Stmt::Assignment { expr: ev, .. } | Stmt::Expr { expr: ev } => expr(ev, map, i),
        Stmt::IndexAssign { obj, key, value } => {
            expr(obj, map, i);
            expr(key, map, i);
            expr(value, map, i);
        }
        Stmt::While { condition, body } => {
            expr(condition, map, i);
            record_stmt_lines(body, map, seq, i);
        }
        Stmt::Do { body } => record_stmt_lines(body, map, seq, i),
        Stmt::If {
            condition,
            then_body,
            else_body,
        } => {
            expr(condition, map, i);
            record_stmt_lines(then_body, map, seq, i);
            record_stmt_lines(else_body, map, seq, i);
        }
        Stmt::Return { value } => {
            if let Some(e) = value {
                expr(e, map, i);
            }
        }
    }
}

fn record_expr_lines(
    e: &Expr,
    map: &mut BTreeMap<*const Stmt, usize>,
    seq: &[usize],
    i: &mut usize,
) {
    match e {
        Expr::Function { body, .. } => record_stmt_lines(body, map, seq, i),
        Expr::TableCtor(entries) => {
            for (k, v) in entries {
                if let CtorKey::Expr(ke) = k {
                    record_expr_lines(ke, map, seq, i);
                }
                record_expr_lines(v, map, seq, i);
            }
        }
        Expr::Index { obj, key } => {
            record_expr_lines(obj, map, seq, i);
            record_expr_lines(key, map, seq, i);
        }
        Expr::BinaryOp { left, right, .. } => {
            record_expr_lines(left, map, seq, i);
            record_expr_lines(right, map, seq, i);
        }
        Expr::UnaryOp { expr, .. } => record_expr_lines(expr, map, seq, i),
        Expr::Call { callee, args } => {
            record_expr_lines(callee, map, seq, i);
            for a in args {
                record_expr_lines(a, map, seq, i);
            }
        }
        _ => {}
    }
}

pub fn number_sites(stmts: &[Stmt], sites: &mut BTreeMap<*const Expr, usize>) {
    for s in stmts {
        number_stmt(s, sites);
    }
}

fn number_stmt(stmt: &Stmt, sites: &mut BTreeMap<*const Expr, usize>) {
    match stmt {
        Stmt::LocalDecl { exprs, .. } => {
            for e in exprs {
                number_expr(e, sites);
            }
        }
        Stmt::Assignment { expr, .. } => number_expr(expr, sites),
        Stmt::IndexAssign { obj, key, value } => {
            number_expr(obj, sites);
            number_expr(key, sites);
            number_expr(value, sites);
        }
        Stmt::While { condition, body } => {
            number_expr(condition, sites);
            number_sites(body, sites);
        }
        Stmt::Do { body } => {
            number_sites(body, sites);
        }
        Stmt::If {
            condition,
            then_body,
            else_body,
        } => {
            number_expr(condition, sites);
            number_sites(then_body, sites);
            number_sites(else_body, sites);
        }
        Stmt::Print { exprs } => {
            for e in exprs {
                number_expr(e, sites);
            }
        }
        Stmt::Expr { expr } => number_expr(expr, sites),
        Stmt::Return { value } => {
            if let Some(e) = value {
                number_expr(e, sites);
            }
        }
    }
}

fn number_expr(expr: &Expr, sites: &mut BTreeMap<*const Expr, usize>) {
    match expr {
        Expr::TableCtor(entries) => {
            let id = sites.len();
            sites.insert(expr as *const Expr, id);
            for (key, val) in entries {
                if let CtorKey::Expr(ke) = key {
                    number_expr(ke, sites);
                }
                number_expr(val, sites);
            }
        }
        Expr::SysAllocCount => {}
        Expr::Index { obj, key } => {
            number_expr(obj, sites);
            number_expr(key, sites);
        }
        Expr::BinaryOp { left, right, .. } => {
            number_expr(left, sites);
            number_expr(right, sites);
        }
        Expr::UnaryOp { expr, .. } => number_expr(expr, sites),
        Expr::Call { callee, args } => {
            number_expr(callee, sites);
            for a in args {
                number_expr(a, sites);
            }
        }
        Expr::Function { body, .. } => {
            // Function bodies sit in one AST but execute wherever they
            // are called — their constructors need sites like any
            // other, so the analyzer's def-site walk can find them.
            number_sites(body, sites);
        }
        _ => {}
    }
}

/// The outer names a function's inlined body can rebind: every
/// assignment target not local to the function itself (params, locals,
/// nested scopes), collected transitively through nested function
/// definitions — a call to `f` inside a loop or branch behaves like an
/// assignment to each of these, so the lowerer's phi machinery must
/// see them as mutated.
pub fn scan_fn_touched(stmts: &[Stmt]) -> BTreeMap<*const Expr, BTreeSet<String>> {
    let mut touched = BTreeMap::new();
    let mut scopes = vec![BTreeSet::new()];
    scan_stmts(stmts, &mut scopes, None, &mut touched);
    touched
}

/// The scan state for one function: the scope depth it entered at, and
/// the names it touches outside its own scopes.
struct FnScan {
    entry_depth: usize,
    touched: BTreeSet<String>,
}

fn scan_stmts(
    stmts: &[Stmt],
    scopes: &mut Vec<BTreeSet<String>>,
    mut current_fn: Option<&mut FnScan>,
    touched: &mut BTreeMap<*const Expr, BTreeSet<String>>,
) {
    for s in stmts {
        scan_stmt(s, scopes, current_fn.as_deref_mut(), touched);
    }
}

fn base_identifier(expr: &Expr) -> Option<&String> {
    match expr {
        Expr::Identifier(name) => Some(name),
        Expr::Index { obj, .. } => base_identifier(obj),
        _ => None,
    }
}

fn scan_stmt(
    stmt: &Stmt,
    scopes: &mut Vec<BTreeSet<String>>,
    mut current_fn: Option<&mut FnScan>,
    touched: &mut BTreeMap<*const Expr, BTreeSet<String>>,
) {
    // A name is function-local exactly when it is declared at or below
    // the depth the enclosing function entered at — names declared in
    // outer scopes are the ones an inlined body rebinds.
    fn record(
        current_fn: Option<&mut FnScan>,
        scopes: &[BTreeSet<String>],
        name: &str,
    ) {
        if let Some(scan) = current_fn
            && !scopes[scan.entry_depth..].iter().any(|s| s.contains(name))
        {
            scan.touched.insert(name.to_string());
        }
    }
    match stmt {
        Stmt::LocalDecl { names, exprs } => {
            for e in exprs {
                scan_expr(e, scopes, current_fn.as_deref_mut(), touched);
            }
            scopes.last_mut().unwrap().extend(names.iter().cloned());
        }
        Stmt::Assignment { name, expr } => {
            record(current_fn.as_deref_mut(), scopes, name);
            scan_expr(expr, scopes, current_fn.as_deref_mut(), touched);
        }
        Stmt::IndexAssign { obj, key, value } => {
            // A store into an outer table changes its contents — for
            // the lowerer's stability checks that is as good as a
            // reassignment of the base name.
            if let Some(base) = base_identifier(obj) {
                record(current_fn.as_deref_mut(), scopes, base);
            }
            scan_expr(obj, scopes, current_fn.as_deref_mut(), touched);
            scan_expr(key, scopes, current_fn.as_deref_mut(), touched);
            scan_expr(value, scopes, current_fn.as_deref_mut(), touched);
        }
        Stmt::While { condition, body } => {
            scan_expr(condition, scopes, current_fn.as_deref_mut(), touched);
            scopes.push(Default::default());
            scan_stmts(body, scopes, current_fn.as_deref_mut(), touched);
            scopes.pop();
        }
        Stmt::Do { body } => {
            scopes.push(Default::default());
            scan_stmts(body, scopes, current_fn.as_deref_mut(), touched);
            scopes.pop();
        }
        Stmt::If {
            condition,
            then_body,
            else_body,
        } => {
            scan_expr(condition, scopes, current_fn.as_deref_mut(), touched);
            scopes.push(Default::default());
            scan_stmts(then_body, scopes, current_fn.as_deref_mut(), touched);
            scopes.pop();
            scopes.push(Default::default());
            scan_stmts(else_body, scopes, current_fn.as_deref_mut(), touched);
            scopes.pop();
        }
        Stmt::Print { exprs } => {
            for e in exprs {
                scan_expr(e, scopes, current_fn.as_deref_mut(), touched);
            }
        }
        Stmt::Expr { expr } => scan_expr(expr, scopes, current_fn.as_deref_mut(), touched),
        Stmt::Return { value } => {
            if let Some(e) = value {
                scan_expr(e, scopes, current_fn, touched);
            }
        }
    }
}

fn scan_expr(
    expr: &Expr,
    scopes: &mut Vec<BTreeSet<String>>,
    mut current_fn: Option<&mut FnScan>,
    touched: &mut BTreeMap<*const Expr, BTreeSet<String>>,
) {
    match expr {
        Expr::Function { params, body } => {
            // A nested definition: its touched set is relative to
            // itself, and everything it touches is also touched from
            // the enclosing function's perspective.
            let entry_depth = scopes.len();
            scopes.push(params.iter().cloned().collect());
            let mut inner = FnScan {
                entry_depth,
                touched: BTreeSet::new(),
            };
            scan_stmts(body, scopes, Some(&mut inner), touched);
            scopes.pop();
            if let Some(outer) = current_fn.as_deref_mut() {
                outer.touched.extend(inner.touched.iter().cloned());
            }
            touched.insert(expr as *const Expr, inner.touched);
        }
        Expr::Call { callee, args } => {
            scan_expr(callee, scopes, current_fn.as_deref_mut(), touched);
            for a in args {
                scan_expr(a, scopes, current_fn.as_deref_mut(), touched);
            }
        }
        Expr::Index { obj, key } => {
            scan_expr(obj, scopes, current_fn.as_deref_mut(), touched);
            scan_expr(key, scopes, current_fn.as_deref_mut(), touched);
        }
        Expr::BinaryOp { left, right, .. } => {
            scan_expr(left, scopes, current_fn.as_deref_mut(), touched);
            scan_expr(right, scopes, current_fn.as_deref_mut(), touched);
        }
        Expr::UnaryOp { expr, .. } => scan_expr(expr, scopes, current_fn.as_deref_mut(), touched),
        Expr::TableCtor(entries) => {
            for (k, v) in entries {
                if let CtorKey::Expr(ke) = k {
                    scan_expr(ke, scopes, current_fn.as_deref_mut(), touched);
                }
                scan_expr(v, scopes, current_fn.as_deref_mut(), touched);
            }
        }
        _ => {}
    }
}
