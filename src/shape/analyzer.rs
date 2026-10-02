use crate::analysis::AnalysisContext;
use crate::ast::{BinOp, CtorKey, Expr, Stmt, UnOp};
use glm_rt::{signal, trace};
use std::collections::{BTreeMap, BTreeSet};

use super::core::{
    BOUNDS_FAIL_THRESHOLD, BOUNDARY_ROOT, LayoutVerdict, MOVED_ROOT, NULL_ROOT, RowLineage,
    SPARSE_THRESHOLD, TableShape, is_ghost, is_root,
};
use super::facts::ShapeFacts;
use super::helpers::{const_key_value, extract_guard, merge_table_scopes};
use super::ty::{Ty, arith_ty, join_ty, scalar};
use Ty::{Bool, Conflict, Flt, Int, Pending, Str, Tbl};

struct ShapeError(String);

// The FFI boundary: `arg` names the table the host passes across
// @glm_exec's parameter. Seeded into the analyzer's root scope so
// `arg[i]` resolves, and PINNED via the BOUNDARY_ROOT sentinel: the
// host is the sole owner of the header, so every move of `arg` into
// another binding or carrier table is rejected at compile time — a
// script-side drop or the returned table's deep free would otherwise
// collide with the host's own glm_tbl_free. Cell reads (`arg[i]`) are
// copies and stay legal, and call arguments are not moves (inline
// params alias the caller's register), so read-only `f(arg)` bodies
// stay legal. The pin crosses call boundaries through the move
// machinery: a body that MOVES its parameter is recorded in
// fn_moved_params (transitively through callees), and a call site
// passing a pinned table to such a parameter is rejected; a body that
// returns a parameter BARE taints the call's result (fn_ret_params),
// so moving the result onward is rejected too. The taint reaches
// through whole return chains (`return g(h(x))`): each def walk maps
// its returns back to the parameters feeding them, so a pinned
// argument poisons every result a parameter flows into, however deep
// the call graph. With that, the pin no longer leans on the def-site
// contains_tables derivation at all — no route the compiler accepts
// lets the boundary header enter a script free graph.
fn boundary_root_scope() -> BTreeMap<String, TableShape> {
    let mut scope = BTreeMap::new();
    scope.insert(
        "arg".to_string(),
        TableShape {
            ty: Tbl(Box::new(Int)),
            layout: LayoutVerdict::default(),
            aliases: BTreeSet::from([BOUNDARY_ROOT]),
            lineage: BTreeMap::new(),
        },
    );
    scope
}

struct Ledger {
    diagnostics: Vec<String>,
    conflict_reported: BTreeSet<usize>,
}

#[derive(Clone, Copy)]
struct Recording {
    _priv: (),
}

type Gate = Option<Recording>;

trait GateOn {
    fn on(self) -> bool;
}

impl GateOn for Option<Recording> {
    fn on(self) -> bool {
        self.is_some()
    }
}

struct WalkState {
    scopes: Vec<BTreeMap<String, TableShape>>,
    // Inline-closure bindings, parallel to `scopes`: name -> the
    // `Expr::Function` node it holds, or None where a non-function
    // binding shadows one (a tombstone, so shadowing hides the fn).
    fn_scopes: Vec<BTreeMap<String, Option<*const Expr>>>,
    changed: bool,
    conv_fires: BTreeSet<u8>,
    // Mixed-join provenance, parallel to `scopes`: for a binding whose
    // aliases mix a row ghost with real sites, WHICH join produced the
    // mix — the tag the free points need to split their plans per arm.
    // Maintained every pass but trusted only under planning (the
    // recording pass); None = plan on the union shape (the leak
    // fallback). A name whose shape changes across a loop latch or a
    // second join drops to None: its tag cannot name the current
    // value's arm reliably.
    prov: Vec<BTreeMap<String, Option<Prov>>>,
}

/// Which join produced a mixed binding's shape. The tag phi semantics:
/// for an if join, true = the then arm ran; for a call's return-edge
/// join, true = a deferring return edge ran.
#[derive(Clone, Copy, PartialEq)]
enum Prov {
    If(*const Stmt),
    Call(*const Expr),
}

struct LatticeState {
    sites: BTreeMap<*const Expr, usize>,
    site_elem: Vec<Ty>,
    needed: Vec<bool>,
    child_sites: BTreeMap<usize, BTreeSet<usize>>,
    ghost_next: usize,
    // Row ghosts (see core.rs): one token per row-read AST node,
    // cached across fixed-point passes so every pass mints the same
    // id for the same node — the converged shapes stay comparable and
    // the fixpoint can settle. Counts UP from ROW_GHOST_FLOOR.
    row_ghosts: BTreeMap<*const Expr, usize>,
    row_ghost_next: usize,
    // The mint record of every row ghost: the base binding's non-root
    // aliases at the read (sites, or the parent row's ghost for
    // chained reads) plus the const key when literal — the row's
    // IDENTITY. Two reads naming the same base and the same slot are
    // the same header at runtime, so affine sole-holding refuses the
    // second one a separate owning binding (one row, one owner).
    // Re-recorded every pass (bases grow monotonically), so the
    // recording pass's entry is the converged one.
    ghost_mints: BTreeMap<usize, GhostMint>,
    // The slot map: site -> literal key -> the node living in that
    // cell (a ctor child site at construction, a row ghost once a row
    // read was stored there). Reads with literal keys resolve their
    // ghosts through it; stores update it.
    site_slots: BTreeMap<usize, BTreeMap<i64, usize>>,
    // The birth-site projection: ghost -> the ctor site whose header
    // the row IS. Resolved at mint (single real base site, literal
    // key, known slot); unresolved ghosts (dynamic keys, growing
    // bases) are not storable — the physical graph is site-keyed.
    ghost_site: BTreeMap<usize, usize>,
    // Rows stored into cells (their ghosts). A housed row's release
    // is claimed by its housing's death and spared on its origin's
    // frees (keep = the store's captured value register).
    housed_ghosts: BTreeSet<usize>,
    // Housed rows already released by a planned free: reading the row
    // again — through its origin OR its housing — would touch freed
    // memory, so later reads refuse.
    claimed_ghosts: BTreeSet<usize>,
}

struct GhostMint {
    bases: BTreeSet<usize>,
    key: Option<i64>,
}

struct OwnershipState {
    free_sites: BTreeSet<*const Stmt>,
    free_keeps: BTreeMap<*const Stmt, Vec<super::facts::Keep>>,
    do_exit_frees: BTreeMap<(*const Stmt, u8), Vec<super::facts::DoExitFree>>,
    root_exit_frees: Vec<super::facts::DoExitFree>,
    stmt_temp_frees: BTreeMap<*const Stmt, Vec<usize>>,
    cond_temp_frees: BTreeMap<*const Stmt, Vec<usize>>,
    arm_temp_frees: BTreeMap<*const Expr, Vec<usize>>,
    ret_path_frees: BTreeMap<*const Stmt, Vec<super::facts::DoExitFree>>,
    ret_path_keep_frees: BTreeMap<*const Stmt, Vec<super::facts::DoExitFree>>,
    ret_path_keep_temps: BTreeMap<*const Stmt, Vec<usize>>,
    rebind_grants: BTreeMap<*const Stmt, BTreeMap<String, Vec<super::facts::Keep>>>,
    stored_ctor_parents: BTreeSet<usize>,
    move_poisons: BTreeMap<*const Stmt, BTreeSet<String>>,
    store_ghost_keeps: BTreeMap<*const Stmt, Vec<usize>>,
    ctor_ghost_keeps: BTreeMap<*const Expr, Vec<(usize, usize)>>,
    arm_free_sites: BTreeMap<*const Stmt, Vec<super::facts::ArmFreeEntry>>,
    arm_rebind_frees: BTreeMap<*const Stmt, Vec<super::facts::ArmFreeEntry>>,
    arm_do_exit_frees: BTreeMap<(*const Stmt, u8), Vec<super::facts::ArmFreeEntry>>,
    arm_root_exit_frees: Vec<super::facts::ArmFreeEntry>,
}

struct HoldsState {
    hold_edges: BTreeMap<usize, BTreeSet<usize>>,
    cyclic_stores: BTreeMap<String, String>,
    num_assigns: BTreeMap<String, Vec<NumExpr>>,
}

struct ReadSideState {
    tbl_value_stores: BTreeSet<usize>,
    nested_store_bases: BTreeMap<usize, String>,
    row_reads: BTreeMap<usize, (String, usize)>,
    row_links: BTreeMap<usize, BTreeSet<usize>>,
    user_store_sites: BTreeSet<usize>,
}

struct LayoutFactsState {
    name_dense: BTreeMap<(String, usize), bool>,
    verdicts: Vec<LayoutVerdict>,
    dense_ctor_len: BTreeMap<usize, i64>,
}

/// Every table-constructor site inside a function body (nested
/// definitions included) — their element types resolve at call sites,
/// not in the def-site walk.
fn collect_body_sites(
    stmts: &[Stmt],
    sites: &BTreeMap<*const Expr, usize>,
    out: &mut BTreeSet<usize>,
) {
    fn walk_stmt(s: &Stmt, sites: &BTreeMap<*const Expr, usize>, out: &mut BTreeSet<usize>) {
        match s {
            Stmt::LocalDecl { exprs, .. } | Stmt::Print { exprs } => {
                exprs.iter().for_each(|e| walk_expr(e, sites, out))
            }
            Stmt::Assignment { expr, .. } | Stmt::Expr { expr } => walk_expr(expr, sites, out),
            Stmt::IndexAssign { obj, key, value } => {
                walk_expr(obj, sites, out);
                walk_expr(key, sites, out);
                walk_expr(value, sites, out);
            }
            Stmt::While { condition, body } => {
                walk_expr(condition, sites, out);
                body.iter().for_each(|s| walk_stmt(s, sites, out));
            }
            Stmt::Do { body } => body.iter().for_each(|s| walk_stmt(s, sites, out)),
            Stmt::If {
                condition,
                then_body,
                else_body,
            } => {
                walk_expr(condition, sites, out);
                then_body.iter().for_each(|s| walk_stmt(s, sites, out));
                else_body.iter().for_each(|s| walk_stmt(s, sites, out));
            }
            Stmt::Return { value } => {
                if let Some(e) = value {
                    walk_expr(e, sites, out);
                }
            }
        }
    }
    fn walk_expr(e: &Expr, sites: &BTreeMap<*const Expr, usize>, out: &mut BTreeSet<usize>) {
        if let Some(&id) = sites.get(&(e as *const Expr)) {
            out.insert(id);
        }
        match e {
            Expr::TableCtor(entries) => {
                for (k, v) in entries {
                    if let CtorKey::Expr(ke) = k {
                        walk_expr(ke, sites, out);
                    }
                    walk_expr(v, sites, out);
                }
            }
            Expr::Index { obj, key } => {
                walk_expr(obj, sites, out);
                walk_expr(key, sites, out);
            }
            Expr::BinaryOp { left, right, .. } => {
                walk_expr(left, sites, out);
                walk_expr(right, sites, out);
            }
            Expr::UnaryOp { expr, .. } => walk_expr(expr, sites, out),
            Expr::Call { callee, args } => {
                walk_expr(callee, sites, out);
                args.iter().for_each(|a| walk_expr(a, sites, out));
            }
            Expr::Function { body, .. } => body.iter().for_each(|s| walk_stmt(s, sites, out)),
            _ => {}
        }
    }
    stmts.iter().for_each(|s| walk_stmt(s, sites, out));
}

/// The base names every `t[...] = ...` in the subtree stores into
/// (nested scopes and function bodies included).
fn collect_stored_bases(stmts: &[Stmt]) -> BTreeSet<String> {
    fn walk_stmt(s: &Stmt, out: &mut BTreeSet<String>) {
        match s {
            Stmt::IndexAssign { obj, .. } => {
                if let Some(base) = stmt_base(obj) {
                    out.insert(base);
                }
                walk_obj_value(s, out);
            }
            Stmt::LocalDecl { exprs, .. } | Stmt::Print { exprs } => {
                exprs.iter().for_each(|e| walk_expr(e, out))
            }
            Stmt::Assignment { expr, .. } | Stmt::Expr { expr } => walk_expr(expr, out),
            Stmt::While { condition, body } => {
                walk_expr(condition, out);
                body.iter().for_each(|s| walk_stmt(s, out));
            }
            Stmt::Do { body } => body.iter().for_each(|s| walk_stmt(s, out)),
            Stmt::If {
                condition,
                then_body,
                else_body,
            } => {
                walk_expr(condition, out);
                then_body.iter().for_each(|s| walk_stmt(s, out));
                else_body.iter().for_each(|s| walk_stmt(s, out));
            }
            Stmt::Return { value } => {
                if let Some(e) = value {
                    walk_expr(e, out);
                }
            }
        }
    }
    fn walk_obj_value(s: &Stmt, out: &mut BTreeSet<String>) {
        if let Stmt::IndexAssign { obj, key, value } = s {
            walk_expr(obj, out);
            walk_expr(key, out);
            walk_expr(value, out);
        }
    }
    fn walk_expr(e: &Expr, out: &mut BTreeSet<String>) {
        match e {
            Expr::TableCtor(entries) => {
                for (k, v) in entries {
                    if let CtorKey::Expr(ke) = k {
                        walk_expr(ke, out);
                    }
                    walk_expr(v, out);
                }
            }
            Expr::Index { obj, key } => {
                walk_expr(obj, out);
                walk_expr(key, out);
            }
            Expr::BinaryOp { left, right, .. } => {
                walk_expr(left, out);
                walk_expr(right, out);
            }
            Expr::UnaryOp { expr, .. } => walk_expr(expr, out),
            Expr::Call { callee, args } => {
                walk_expr(callee, out);
                args.iter().for_each(|a| walk_expr(a, out));
            }
            Expr::Function { body, .. } => body.iter().for_each(|s| walk_stmt(s, out)),
            _ => {}
        }
    }
    fn stmt_base(e: &Expr) -> Option<String> {
        match e {
            Expr::Identifier(n) => Some(n.clone()),
            Expr::Index { obj, .. } => stmt_base(obj),
            _ => None,
        }
    }
    let mut out = BTreeSet::new();
    stmts.iter().for_each(|s| walk_stmt(s, &mut out));
    out
}

struct Analyzer {
    walk: WalkState,
    ledger: Ledger,
    lattice: LatticeState,
    own: OwnershipState,
    holds: HoldsState,
    reads: ReadSideState,
    layout: LayoutFactsState,
    // === inline closures ===
    // One join slot per function body being walked at its definition
    // site: every `return` in the body merges its shape into the top.
    fn_rets: Vec<Option<TableShape>>,
    // Per return-edge shapes of the body being walked (ReturnCtx
    // order) — popped into fn_ret_edge_shapes at the def's end.
    fn_edge_frames: Vec<Vec<TableShape>>,
    fn_ret_shapes: BTreeMap<*const Expr, TableShape>,
    fn_ret_edge_shapes: BTreeMap<*const Expr, Vec<TableShape>>,
    // Mixed-join provenance records (see WalkState.prov): the per-arm
    // shapes of a tagged if-join, and the tag classes the lowerer
    // materializes (if stmt -> then arm defers; call -> per-edge
    // defers).
    join_arm_shapes: BTreeMap<(*const Stmt, String), (TableShape, TableShape)>,
    join_tags: BTreeMap<*const Stmt, bool>,
    call_tags: BTreeMap<*const Expr, Vec<bool>>,
    fn_scope_keys: BTreeMap<*const Expr, (*const Stmt, u8)>,
    // Constructor sites living inside function bodies: their element
    // types resolve at call sites (checker substitutions), not in the
    // def-site walk where params are pending — the read-before-value
    // gate must wait for that resolution.
    fn_body_sites: BTreeSet<usize>,
    // Same story for a caller's constructors that are stored into only
    // through a parameter: the def-site walk sees a pending param, the
    // call site connects the real ctor sites.
    fn_params: BTreeMap<*const Expr, Vec<String>>,
    fn_store_params: BTreeMap<*const Expr, BTreeSet<String>>,
    fn_param_sites: BTreeSet<usize>,
    // === pinned boundary, cross-phase ===
    // Parameters a body MOVES (a move source: assignment/index-assign
    // RHS or ctor entry — recorded in poison_moved), transitively
    // through callees that move the corresponding parameter. Passing
    // the pinned boundary table to such a parameter would let the
    // host-owned header end up inside a carrier the script frees.
    fn_moved_params: BTreeMap<*const Expr, BTreeSet<String>>,
    // Parameters a body returns BARE (`return x`): at a call site whose
    // argument in that position is pinned, the call's RESULT is pinned
    // too — the header came straight back out.
    fn_ret_params: BTreeMap<*const Expr, BTreeSet<usize>>,
    // The READ-reach twin of fn_ret_shapes: which heap sites a body's
    // returned value may point INTO at runtime — outer names it hands
    // out (borrows), row bases whose cells it returns — recorded
    // BEFORE the ownership strip. Only the return-path free planner
    // reads it: the host owns what it receives across the boundary, so
    // a returned table's reach must never be freed on the path that
    // hands it out, borrowed or not.
    fn_ret_reach: BTreeMap<*const Expr, BTreeSet<usize>>,
    // One accumulation frame per walked body (parallel to fn_rets):
    // the union of its returns' read-reach.
    fn_ret_reaches: Vec<BTreeSet<usize>>,
    // The function currently being walked at its def site (innermost
    // last): its pointer and params, for moved/returned-param recording.
    fn_walk: Vec<(*const Expr, Vec<String>)>,
    // The scope depth each walked body entered at (parallel to
    // fn_walk): the return join strips aliases owned by bindings
    // OUTSIDE that depth — a body returning an outer name lends the
    // table out, it does not transfer it.
    fn_scope_entries: Vec<usize>,
}

/// How a sub-expression's table value leaves the statement: consumed
/// by the enclosing evaluation (dies at the statement's end) or handed
/// to an owner that outlives it (a binding, a cell, the boundary, a
/// stored-through parameter).
#[derive(Clone, Copy, PartialEq)]
enum Fate {
    Dies,
    Escapes,
}

#[derive(Clone, Debug, PartialEq)]
enum NumExpr {
    Lit(i64),
    Var(String),
    Add(Box<NumExpr>, Box<NumExpr>),
    Mul(Box<NumExpr>, Box<NumExpr>),
    Other,
}

fn num_of(expr: &Expr) -> NumExpr {
    match expr {
        Expr::Integer(v) => NumExpr::Lit(*v),
        Expr::Identifier(n) => NumExpr::Var(n.clone()),
        Expr::BinaryOp {
            op: BinOp::Add,
            left,
            right,
        } => NumExpr::Add(Box::new(num_of(left)), Box::new(num_of(right))),
        Expr::BinaryOp {
            op: BinOp::Mul,
            left,
            right,
        } => NumExpr::Mul(Box::new(num_of(left)), Box::new(num_of(right))),
        _ => NumExpr::Other,
    }
}

pub fn analyze(ctx: &AnalysisContext<'_>) -> ShapeFacts {
    let n = ctx.sites.len();
    let mut a = Analyzer {
        walk: WalkState {
            scopes: vec![boundary_root_scope()],
            fn_scopes: vec![BTreeMap::new()],
            changed: false,
            conv_fires: BTreeSet::new(),
            prov: vec![BTreeMap::new()],
        },
        ledger: Ledger {
            diagnostics: Vec::new(),
            conflict_reported: BTreeSet::new(),
        },
        lattice: LatticeState {
            sites: ctx.sites.clone(),
            site_elem: vec![Pending; n],
            needed: vec![false; n],
            child_sites: BTreeMap::new(),
            ghost_next: usize::MAX / 2,
            row_ghosts: BTreeMap::new(),
            row_ghost_next: super::core::ROW_GHOST_FLOOR + 1,
            ghost_mints: BTreeMap::new(),
            site_slots: BTreeMap::new(),
            ghost_site: BTreeMap::new(),
            housed_ghosts: BTreeSet::new(),
            claimed_ghosts: BTreeSet::new(),
        },
        own: OwnershipState {
            free_sites: BTreeSet::new(),
            free_keeps: BTreeMap::new(),
            do_exit_frees: BTreeMap::new(),
            root_exit_frees: Vec::new(),
            stmt_temp_frees: BTreeMap::new(),
            cond_temp_frees: BTreeMap::new(),
            arm_temp_frees: BTreeMap::new(),
            ret_path_frees: BTreeMap::new(),
            ret_path_keep_frees: BTreeMap::new(),
            ret_path_keep_temps: BTreeMap::new(),
            rebind_grants: BTreeMap::new(),
            stored_ctor_parents: BTreeSet::new(),
            move_poisons: BTreeMap::new(),
            store_ghost_keeps: BTreeMap::new(),
            ctor_ghost_keeps: BTreeMap::new(),
            arm_free_sites: BTreeMap::new(),
            arm_rebind_frees: BTreeMap::new(),
            arm_do_exit_frees: BTreeMap::new(),
            arm_root_exit_frees: Vec::new(),
        },
        holds: HoldsState {
            hold_edges: BTreeMap::new(),
            cyclic_stores: BTreeMap::new(),
            num_assigns: BTreeMap::new(),
        },
        reads: ReadSideState {
            tbl_value_stores: BTreeSet::new(),
            nested_store_bases: BTreeMap::new(),
            row_reads: BTreeMap::new(),
            row_links: BTreeMap::new(),
            user_store_sites: BTreeSet::new(),
        },
        layout: LayoutFactsState {
            name_dense: BTreeMap::new(),
            verdicts: vec![LayoutVerdict::Growing; n],
            dense_ctor_len: BTreeMap::new(),
        },
        fn_rets: Vec::new(),
        fn_edge_frames: Vec::new(),
        fn_ret_shapes: BTreeMap::new(),
        fn_ret_edge_shapes: BTreeMap::new(),
        join_arm_shapes: BTreeMap::new(),
        join_tags: BTreeMap::new(),
        call_tags: BTreeMap::new(),
        fn_scope_keys: BTreeMap::new(),
        fn_body_sites: BTreeSet::new(),
        fn_params: BTreeMap::new(),
        fn_store_params: BTreeMap::new(),
        fn_param_sites: BTreeSet::new(),
        fn_moved_params: BTreeMap::new(),
        fn_ret_params: BTreeMap::new(),
        fn_ret_reach: BTreeMap::new(),
        fn_ret_reaches: Vec::new(),
        fn_walk: Vec::new(),
        fn_scope_entries: Vec::new(),
    };

    let mut prev_end: Vec<BTreeMap<String, TableShape>> = Vec::new();
    loop {
        a.walk.changed = false;
        a.walk.scopes = vec![boundary_root_scope()];
        a.walk.fn_scopes = vec![BTreeMap::new()];
        a.walk.prov = vec![BTreeMap::new()];
        a.walk_stmts(None, ctx.ast);
        let end = a.walk.scopes.clone();
        let stable = end == prev_end && !a.walk.changed;
        prev_end = end;
        if stable {
            break;
        }
    }

    for slot in std::mem::take(&mut a.walk.conv_fires) {
        trace::compiler_trace_signal(slot);
    }

    let rec: Gate = Some(Recording { _priv: () });
    a.walk.scopes = vec![boundary_root_scope()];
    a.walk.fn_scopes = vec![BTreeMap::new()];
    a.walk.prov = vec![BTreeMap::new()];
    a.ledger.diagnostics.clear();
    a.walk_stmts(rec, ctx.ast);

    // The ownership exits, recorded once over the converged lattice:
    // the root chunk's scope-exit frees, then the statement-temporary
    // plan (a pure AST classification — no lattice mutation).
    if let Err(err) = a.decide_root_exit(rec) {
        a.ledger.diagnostics.push(err.0);
    }
    a.plan_stmt_temp_frees(ctx.ast);

    let mut facts = a.into_recorded().into_facts(n);
    // The pure-AST scan: which outer names each inlined body rebinds.
    facts.fn_touched = crate::analysis::scan_fn_touched(ctx.ast);
    facts
}

struct Recorded {
    recording: bool,
    ledger: Ledger,
    lattice: LatticeState,
    own: OwnershipState,
    holds: HoldsState,
    reads: ReadSideState,
    layout: LayoutFactsState,
    fn_scope_keys: BTreeMap<*const Expr, (*const Stmt, u8)>,
    fn_body_sites: BTreeSet<usize>,
    fn_param_sites: BTreeSet<usize>,
    join_tags: BTreeMap<*const Stmt, bool>,
    call_tags: BTreeMap<*const Expr, Vec<bool>>,
}

impl Analyzer {
    fn into_recorded(self) -> Recorded {
        Recorded {
            recording: true,
            ledger: self.ledger,
            lattice: self.lattice,
            own: self.own,
            holds: self.holds,
            reads: self.reads,
            layout: self.layout,
            fn_scope_keys: self.fn_scope_keys,
            fn_body_sites: self.fn_body_sites,
            fn_param_sites: self.fn_param_sites,
            join_tags: self.join_tags,
            call_tags: self.call_tags,
        }
    }
}

impl Recorded {
    fn into_facts(mut self, n: usize) -> ShapeFacts {
        let mut elems = vec![crate::ast::StaticType::Integer; n];

        let site_ids: Vec<usize> = self.lattice.sites.values().copied().collect();
        for id in site_ids {
            if self.lattice.needed[id] && self.lattice.site_elem[id] == Pending {
                // Constructor sites inside function bodies — and caller
                // ctors stored into through a parameter — resolve at
                // their call sites (checker substitutions bind the
                // element unknowns); the def-site walk sees pending
                // params and must not gate them here.
                if self.fn_body_sites.contains(&id) || self.fn_param_sites.contains(&id) {
                    signal!(self.recording, trace::TRACE_ANALYZE_PENDING);
                    elems[id] = crate::ast::StaticType::Unknown(id);
                    continue;
                }
                signal!(self.recording, trace::TRACE_ANALYZE_MISSING_VALUE);
                self.ledger.diagnostics.push(format!(
                    "Type Error: a table is read before it is ever given a value \
             (table site #{id})"
                ));
                elems[id] = crate::ast::StaticType::Unknown(id);
            } else if self.lattice.site_elem[id] == Pending {
                signal!(self.recording, trace::TRACE_ANALYZE_PENDING);
                elems[id] = crate::ast::StaticType::Unknown(id);
            } else {
                signal!(self.recording, trace::TRACE_ANALYZE_RESOLVED);
                elems[id] = self.elem_type_of_tbl(id);
            }
        }

        for (&site, name) in &self.reads.nested_store_bases {
            if !self.reads.tbl_value_stores.contains(&site) {
                signal!(self.recording, trace::TRACE_STMT_IDX_NULL_ROW);
                self.ledger.diagnostics.push(format!(
                    "Type Error: cannot index a row of '{name}' — no table is ever \
             stored into '{name}', and stores do not create rows (table site #{site})"
                ));
            }
        }

        for (base, value) in &self.holds.cyclic_stores {
            signal!(self.recording, trace::TRACE_FAIL_CYCLIC_STORE);
            self.ledger.diagnostics.push(format!(
                "Type Error: cyclic table store — '{value}' stored into '{base}' \
         would create a table cycle (glm's element types are finite)"
            ));
        }

        for id in 0..n {
            propagate_elem_down(&mut elems, &self.lattice.child_sites, id);
        }

        let mut cell_children = self.lattice.child_sites.clone();
        for (parent, kids) in std::mem::take(&mut self.reads.row_links) {
            cell_children.entry(parent).or_default().extend(kids);
        }

        ShapeFacts {
            sites: self.lattice.sites,
            elems,
            layouts: self.layout.verdicts,
            free_sites: self.own.free_sites,
            free_keeps: self.own.free_keeps,
            do_exit_frees: self.own.do_exit_frees,
            root_exit_frees: self.own.root_exit_frees,
            stmt_temp_frees: self.own.stmt_temp_frees,
            cond_temp_frees: self.own.cond_temp_frees,
            arm_temp_frees: self.own.arm_temp_frees,
            ret_path_frees: self.own.ret_path_frees,
            ret_path_keep_frees: self.own.ret_path_keep_frees,
            ret_path_keep_temps: self.own.ret_path_keep_temps,
            rebind_frees: self.own.rebind_grants,
            move_poisons: self.own.move_poisons,
            name_dense: self.layout.name_dense,
            stored_ctor_parents: self.own.stored_ctor_parents,
            substitutions: BTreeMap::new(),
            local_types: BTreeMap::new(),
            row_reads: std::mem::take(&mut self.reads.row_reads),
            cell_children,
            dense_ctor_len: std::mem::take(&mut self.layout.dense_ctor_len),
            fn_scope_keys: self.fn_scope_keys,
            join_tags: self.join_tags,
            call_tags: self.call_tags,
            store_ghost_keeps: self.own.store_ghost_keeps,
            ctor_ghost_keeps: self.own.ctor_ghost_keeps,
            arm_free_sites: self.own.arm_free_sites,
            arm_rebind_frees: self.own.arm_rebind_frees,
            arm_do_exit_frees: self.own.arm_do_exit_frees,
            arm_root_exit_frees: self.own.arm_root_exit_frees,
            fn_touched: BTreeMap::new(),
            fn_defs: BTreeMap::new(),
            call_defs: BTreeMap::new(),
            diagnostics: self.ledger.diagnostics,
        }
    }

    fn report_conflict(&mut self, id: usize) -> crate::ast::StaticType {
        if self.ledger.conflict_reported.insert(id) {
            self.ledger.diagnostics.push(format!(
                "Type Error: heterogeneous tables are not supported (table site #{id})"
            ));
        }
        crate::ast::StaticType::Unknown(id)
    }

    fn elem_type_of_tbl(&mut self, id: usize) -> crate::ast::StaticType {
        if self.lattice.site_elem[id] == Conflict {
            signal!(self.recording, trace::TRACE_SHAPE_CONFLICT_GUARD);
            return self.report_conflict(id);
        }

        if let Some(children) = self.lattice.child_sites.get(&id).cloned() {
            signal!(self.recording, trace::TRACE_CHILD_FAST_PATH);
            let mut uniform_type: Option<crate::ast::StaticType> = None;

            for &child_id in &children {
                if self.lattice.site_elem[child_id] == Conflict {
                    signal!(self.recording, trace::TRACE_ELEM_CHILD_CONFLICT);
                    return self.report_conflict(child_id);
                }

                let child_ty = self.elem_type_of_tbl(child_id);

                let child_pending = matches!(child_ty, crate::ast::StaticType::Unknown(_));
                match &uniform_type {
                    None => {
                        signal!(self.recording, trace::TRACE_ELEM_CHILD_FIRST);
                        uniform_type = Some(child_ty.clone());
                    }
                    Some(expected) => {
                        let expected_pending =
                            matches!(expected, crate::ast::StaticType::Unknown(_));
                        if !child_pending && !expected_pending && *expected != child_ty {
                            signal!(self.recording, trace::TRACE_ELEM_CHILD_MISMATCH);
                            return self.report_conflict(child_id);
                        }
                        if expected_pending && !child_pending {
                            uniform_type = Some(child_ty.clone());
                        }
                        signal!(self.recording, trace::TRACE_ELEM_CHILD_MATCH);
                    }
                }
            }

            if let Some(ty) = uniform_type {
                signal!(self.recording, trace::TRACE_ELEM_CHILD_UNIFORM);
                return crate::ast::StaticType::Table(Box::new(ty));
            }
        }

        signal!(self.recording, trace::TRACE_FALLBACK_RESOLVE);
        match self.lattice.site_elem[id].clone() {
            Tbl(inner) => {
                signal!(self.recording, trace::TRACE_ELEM_FALLBACK_TBL);
                crate::ast::StaticType::Table(Box::new(self.ty_to_static(&inner, id)))
            }
            Int => {
                signal!(self.recording, trace::TRACE_ELEM_FALLBACK_INT);
                crate::ast::StaticType::Integer
            }
            Pending => {
                signal!(self.recording, trace::TRACE_ANALYZE_PENDING);
                crate::ast::StaticType::Unknown(id)
            }
            Flt => {
                signal!(self.recording, trace::TRACE_ELEM_FALLBACK_FLT);
                crate::ast::StaticType::Float
            }
            Bool => {
                signal!(self.recording, trace::TRACE_ELEM_FALLBACK_BOOL);
                crate::ast::StaticType::Boolean
            }
            Str => {
                signal!(self.recording, trace::TRACE_ELEM_FALLBACK_STR);
                crate::ast::StaticType::String
            }
            Conflict => {
                signal!(self.recording, trace::TRACE_ELEM_FALLBACK_CONFLICT);
                self.report_conflict(id)
            }
        }
    }

    fn ty_to_static(&mut self, ty: &Ty, site: usize) -> crate::ast::StaticType {
        match ty {
            Int => {
                signal!(self.recording, trace::TRACE_TY_STATIC_INT);
                crate::ast::StaticType::Integer
            }
            Pending => {
                signal!(self.recording, trace::TRACE_TY_STATIC_PENDING);
                let ghost = self.lattice.ghost_next;
                self.lattice.ghost_next -= 1;
                crate::ast::StaticType::Unknown(ghost)
            }
            Flt => {
                signal!(self.recording, trace::TRACE_TY_STATIC_FLT);
                crate::ast::StaticType::Float
            }
            Bool => {
                signal!(self.recording, trace::TRACE_TY_STATIC_BOOL);
                crate::ast::StaticType::Boolean
            }
            Str => {
                signal!(self.recording, trace::TRACE_TY_STATIC_STR);
                crate::ast::StaticType::String
            }
            Tbl(inner) => {
                signal!(self.recording, trace::TRACE_TY_STATIC_TBL);
                crate::ast::StaticType::Table(Box::new(self.ty_to_static(inner, site)))
            }
            Conflict => {
                signal!(self.recording, trace::TRACE_TY_STATIC_CONFLICT);
                self.report_conflict(site)
            }
        }
    }
}

fn get_base_identifier(expr: &Expr) -> Option<&String> {
    match expr {
        Expr::Identifier(name) => Some(name),
        Expr::Index { obj, .. } => get_base_identifier(obj),
        _ => None,
    }
}

fn propagate_elem_down(
    elems: &mut [crate::ast::StaticType],
    child_sites: &BTreeMap<usize, BTreeSet<usize>>,
    site: usize,
) {
    let crate::ast::StaticType::Table(elem) = elems[site].clone() else {
        return;
    };
    if matches!(*elem, crate::ast::StaticType::Unknown(_)) {
        return;
    }
    if let Some(children) = child_sites.get(&site) {
        for &child in children {
            if matches!(elems[child], crate::ast::StaticType::Unknown(_)) {
                elems[child] = (*elem).clone();
                propagate_elem_down(elems, child_sites, child);
            }
        }
    }
}

fn provably_nonneg(
    num_assigns: &BTreeMap<String, Vec<NumExpr>>,
    expr: &NumExpr,
    self_name: &str,
    path: &mut BTreeSet<String>,
) -> bool {
    match expr {
        NumExpr::Lit(v) => *v >= 0,
        NumExpr::Var(n) => {
            if n == self_name {
                true
            } else if path.contains(n) {
                false
            } else {
                let Some(shapes) = num_assigns.get(n) else {
                    return false;
                };
                path.insert(n.clone());
                let ok = shapes
                    .iter()
                    .all(|sh| provably_nonneg(num_assigns, sh, n, path));
                path.remove(n);
                ok
            }
        }
        NumExpr::Add(a, b) | NumExpr::Mul(a, b) => {
            provably_nonneg(num_assigns, a, self_name, path)
                && provably_nonneg(num_assigns, b, self_name, path)
        }
        NumExpr::Other => false,
    }
}

fn guard_provably_nonneg_expr(num_assigns: &BTreeMap<String, Vec<NumExpr>>, expr: &Expr) -> bool {
    let mut path = BTreeSet::new();
    provably_nonneg(num_assigns, &num_of(expr), "", &mut path)
}

fn guard_provably_nonneg(num_assigns: &BTreeMap<String, Vec<NumExpr>>, guard: &str) -> bool {
    num_assigns.get(guard).is_some_and(|shapes| {
        let mut path = BTreeSet::new();
        shapes
            .iter()
            .all(|sh| provably_nonneg(num_assigns, sh, guard, &mut path))
    })
}

fn guard_only_ascends(
    num_assigns: &BTreeMap<String, Vec<NumExpr>>,
    stmts: &[Stmt],
    guard: &str,
) -> bool {
    for s in stmts {
        match s {
            Stmt::Assignment { name, expr } if name == guard => {
                let Expr::BinaryOp {
                    op: BinOp::Add,
                    left,
                    right,
                } = expr
                else {
                    return false;
                };
                if !matches!(left.as_ref(), Expr::Identifier(n) if n == guard) {
                    return false;
                }
                if !guard_provably_nonneg_expr(num_assigns, right) {
                    return false;
                }
            }
            Stmt::While { body, .. } | Stmt::Do { body } => {
                if !guard_only_ascends(num_assigns, body, guard) {
                    return false;
                }
            }
            Stmt::If {
                then_body,
                else_body,
                ..
            } if !guard_only_ascends(num_assigns, then_body, guard)
                || !guard_only_ascends(num_assigns, else_body, guard) =>
            {
                return false;
            }
            _ => {}
        }
    }
    true
}

fn deepfree_children(
    lattice: &LatticeState,
    own: &OwnershipState,
    reads: &ReadSideState,
    site: usize,
) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    let mut level = BTreeSet::from([site]);
    let mut seen = BTreeSet::new();
    while let Some(&s) = level.iter().next() {
        level.remove(&s);
        if !seen.insert(s) {
            continue;
        }
        let flagged = lattice.child_sites.contains_key(&s) || own.stored_ctor_parents.contains(&s);
        if !flagged {
            continue;
        }
        for src in [&lattice.child_sites, &reads.row_links] {
            if let Some(kids) = src.get(&s) {
                for &k in kids {
                    if !is_root(&k) && out.insert(k) {
                        level.insert(k);
                    }
                }
            }
        }
    }
    out
}

/// Is `origin` (a real base site, or another binding's ghost) still
/// housed somewhere that outlives the free being planned — a live
/// binding that owns the site, or whose deep-free subtree contains it?
/// A ghost-owning binding's free defers to its base's own death: the
/// base's deep free composts the row (the runtime walker recurses into
/// cells), so an earlier row free would leave the base walking freed
/// memory. Same-scope siblings count as live (a mid-statement drop
/// runs while they are still readable).
fn origin_housed(
    scopes: &[BTreeMap<String, TableShape>],
    lattice: &LatticeState,
    own: &OwnershipState,
    reads: &ReadSideState,
    origin: usize,
    skip_depth: usize,
    skip_name: &str,
) -> bool {
    for (d, scope) in scopes.iter().enumerate() {
        for (n, ts) in scope {
            if d == skip_depth && n == skip_name {
                continue;
            }
            if ts.aliases.contains(&origin) {
                return true;
            }
            for &s in ts.aliases.iter().filter(|s| !is_root(s)) {
                if deepfree_children(lattice, own, reads, s).contains(&origin) {
                    return true;
                }
            }
        }
    }
    false
}

/// The full release set of a free through `node`: the node itself plus
/// every header its deep free walks into (cells, stored rows, their
/// subtrees). A ghost's own subtree is invisible here until rows carry
/// a birth-site projection (stage: row housing) — but borrowers NAME
/// the ghost in their lineage directly, so the harm test below still
/// sees through it at every chain depth.
fn release_set(
    lattice: &LatticeState,
    own: &OwnershipState,
    reads: &ReadSideState,
    node: usize,
) -> BTreeSet<usize> {
    let mut s = deepfree_children(lattice, own, reads, node);
    s.insert(node);
    s
}

/// The live bindings a free of `release` would harm: every binding
/// whose lineage names a node inside the release. A lineage key is an
/// ancestor of the binding's row (each read deepens the chain and
/// seeds its base's tokens), so naming any released node means the
/// binding's register points into the tree being composted. Scalar
/// bindings are excluded — their register copied a cell value, not a
/// row pointer. `upto` bounds the scan: scope-exit batches spare only
/// OUTER borrowers (batch-mates die in the same instant, their ghosts
/// deferring through batch coverage); drops and rebinds run
/// mid-statement, where every live binding counts.
fn borrowers_of(
    scopes: &[BTreeMap<String, TableShape>],
    release: &BTreeSet<usize>,
    upto: usize,
    skip_depth: usize,
    skip_name: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    for (d, scope) in scopes[..upto].iter().enumerate() {
        for (n, ts) in scope {
            if d == skip_depth && n == skip_name {
                continue;
            }
            if scalar(&ts.ty) {
                continue;
            }
            if ts.lineage.keys().any(|o| release.contains(o)) {
                out.push(n.clone());
            }
        }
    }
    out
}

/// Is a ghost origin (a parent row's token) released by this batch:
/// directly, or through any of its mint bases — the row lives under
/// one of them, and a batch entry freeing that base releases it.
fn ghost_origin_covered(
    lattice: &LatticeState,
    covered: &BTreeSet<usize>,
    origin: usize,
) -> bool {
    covered.contains(&origin)
        || lattice
            .ghost_mints
            .get(&origin)
            .is_some_and(|m| m.bases.iter().any(|b| covered.contains(b)))
}

fn holds_reaches(holds: &HoldsState, from: usize, to: usize) -> bool {
    if from == to {
        return true;
    }
    let mut seen: BTreeSet<usize> = BTreeSet::new();
    let mut stack = vec![from];
    while let Some(x) = stack.pop() {
        if !seen.insert(x) {
            continue;
        }
        if let Some(next) = holds.hold_edges.get(&x) {
            for &y in next {
                if y == to {
                    return true;
                }
                stack.push(y);
            }
        }
    }
    false
}

/// The mixed-join free plan: how a binding whose aliases mix a row
/// ghost with real sites leaves its free point.
enum MixedPlan {
    /// Not mixed — the caller's normal channels own it.
    NotMixed,
    /// Every arm defers (or the union is housed with no usable tag):
    /// emit nothing; the row arm dies with its base, the ctor arm's
    /// header outlives unnamed (the leak fallback).
    Suppress,
    /// Every arm frees: one unconditional free through the faithful
    /// join register (it holds whichever arm ran).
    Fire { keeps: Vec<String> },
    /// The arms disagree: a tag-gated branch at the free point — the
    /// deferring arm emits nothing, the freeing arm frees the carrier
    /// register (with the union of the freeing arms' borrower keeps).
    ArmSplit {
        tag: super::facts::TagSrc,
        nothing_on: bool,
        keeps: Vec<String>,
    },
}

impl Analyzer {
    fn probe(&mut self, rec: Gate, slot: u8) {
        if rec.on() {
            trace::compiler_trace_signal(slot);
        } else {
            self.walk.conv_fires.insert(slot);
        }
    }

    fn walk_stmts(&mut self, rec: Gate, stmts: &[Stmt]) {
        for s in stmts {
            if let Err(err) = self.walk_stmt(rec, s) {
                signal!(rec.on(), trace::TRACE_GHOST_BAIL);
                self.ledger.diagnostics.push(err.0);
                return;
            }
        }
    }

    fn walk_stmt(&mut self, rec: Gate, stmt: &Stmt) -> Result<(), ShapeError> {
        match stmt {
            Stmt::LocalDecl { names, exprs } => {
                let mut sets = Vec::with_capacity(names.len());
                let mut moved_srcs: Vec<(usize, String)> = Vec::new();
                // Bind function names before walking their bodies so a
                // recursive call resolves here (and the checker reports
                // the recursion with its own message). Non-function
                // bindings tombstone the name so shadowing hides any
                // outer function.
                for (name, expr) in names.iter().zip(exprs.iter()) {
                    let ptr = match expr {
                        Expr::Function { .. } => Some(expr as *const Expr),
                        _ => None,
                    };
                    self.declare_fn_binding(name, ptr);
                }
                for name in &names[exprs.len()..] {
                    self.declare_fn_binding(name, None);
                }
                let note_move = |an: &mut Self,
                                 src: &str,
                                 target: &str,
                                 moved: &mut Vec<(usize, String)>|
                 -> Result<(), ShapeError> {
                    if src == target {
                        return Ok(());
                    }
                    let (d, bind) = an.resolve(rec, src)?;
                    an.record_moved_param(src);
                    an.check_boundary_pin(&bind, src)?;
                    if matches!(bind.ty, Tbl(_)) {
                        if moved.iter().any(|(_, m)| m == src) {
                            return Err(ShapeError(format!(
                                "Lifetime Error: '{src}' is moved more than once — one table \
                                 value has exactly one receiving binding"
                            )));
                        }
                        moved.push((d, src.to_string()));
                    }
                    Ok(())
                };
                for (name, expr) in names.iter().zip(exprs.iter()) {
                    if let Expr::Identifier(src) = expr {
                        note_move(self, src, name, &mut moved_srcs)?;
                    }
                }
                for (idx, expr) in exprs.iter().enumerate() {
                    if matches!(expr, Expr::Nil) {
                        signal!(rec.on(), trace::TRACE_BIND_SCALAR);
                        sets.push(TableShape {
                            ty: Pending,
                            layout: LayoutVerdict::default(),
                            aliases: BTreeSet::from([NULL_ROOT]),
                            lineage: BTreeMap::new(),
                        });
                    } else if let Expr::Function { params, body } = expr {
                        // The def-site walk: ownership facts and return
                        // shape, frees keyed (stmt, 2 + idx) so several
                        // functions may share one `local` line.
                        self.walk_fn_def(
                            rec,
                            expr as *const Expr,
                            params,
                            body,
                            (stmt as *const Stmt, 2 + idx as u8),
                        )?;
                        sets.push(TableShape {
                            ty: Pending,
                            layout: LayoutVerdict::default(),
                            aliases: BTreeSet::new(),
                            lineage: BTreeMap::new(),
                        });
                    } else {
                        let bind = self.infer_bind(rec, expr)?;
                        self.check_uses(rec, expr)?;
                        self.collect_entry_moves(rec, expr, &mut moved_srcs)?;
                        if scalar(&bind.ty) {
                            signal!(rec.on(), trace::TRACE_BIND_SCALAR);
                        } else if matches!(expr, Expr::TableCtor(_)) {
                            signal!(rec.on(), trace::TRACE_BIND_HEAP);
                        }
                        sets.push(bind);
                    }
                }
                while sets.len() < names.len() {
                    signal!(rec.on(), trace::TRACE_BIND_SCALAR);
                    sets.push(TableShape {
                        ty: Pending,
                        layout: LayoutVerdict::default(),
                        aliases: BTreeSet::from([NULL_ROOT]),
                        lineage: BTreeMap::new(),
                    });
                }

                for (name, expr) in names.iter().zip(exprs.iter()) {
                    self.holds
                        .num_assigns
                        .insert(name.clone(), vec![num_of(expr)]);
                }

                let depth = self.walk.scopes.len() - 1;
                for (idx, (name, set)) in names.iter().zip(sets).enumerate() {
                    if let Some(old) = self.walk.scopes.last().and_then(|s| s.get(name)).cloned() {
                        self.rebind_death(rec, name, depth, stmt, &old, &set.aliases)?;
                    }
                    self.check_row_ownership(name, depth, &set, &moved_srcs)?;
                    let prov = self.prov_for_bind(exprs.get(idx), &set);
                    let scope = self.walk.scopes.last_mut().unwrap();
                    scope.insert(name.clone(), set);
                    self.walk.prov[depth].insert(name.clone(), prov);
                }
                for (d, src) in &moved_srcs {
                    self.poison_moved(*d, src);
                }
                if rec.on() {
                    let entry = self
                        .own
                        .move_poisons
                        .entry(stmt as *const Stmt)
                        .or_default();
                    for (_, src) in &moved_srcs {
                        entry.insert(src.clone());
                    }
                }
            }

            Stmt::Assignment { name, expr } => {
                let entry = self.holds.num_assigns.entry(name.clone()).or_default();
                let shape = num_of(expr);
                if !entry.contains(&shape) {
                    entry.push(shape);
                }
                if let Expr::Function { params, body } = expr {
                    self.assign_fn_binding(name, Some(expr as *const Expr));
                    self.walk_fn_def(
                        rec,
                        expr as *const Expr,
                        params,
                        body,
                        (stmt as *const Stmt, 2),
                    )?;
                } else {
                    self.assign_fn_binding(name, None);
                }
                if matches!(expr, Expr::Nil) {
                    signal!(rec.on(), trace::TRACE_STMT_ASSIGN_NIL);
                    self.drop_reference(rec, name, stmt)?;
                    return Ok(());
                }
                let bind = self.infer_bind(rec, expr)?;
                self.check_uses(rec, expr)?;
                let mut moved_srcs: Vec<(usize, String)> = Vec::new();
                if let Expr::Identifier(src) = expr {
                    if src != name {
                        self.record_moved_param(src);
                        self.check_boundary_pin(&bind, src)?;
                        if matches!(bind.ty, Tbl(_)) {
                            let (d, _) = self.resolve(rec, src)?;
                            moved_srcs.push((d, src.clone()));
                        }
                    }
                } else {
                    self.collect_entry_moves(rec, expr, &mut moved_srcs)?;
                }
                if scalar(&bind.ty) {
                    signal!(rec.on(), trace::TRACE_BIND_SCALAR);
                } else if matches!(expr, Expr::TableCtor(_)) {
                    signal!(rec.on(), trace::TRACE_BIND_HEAP);
                }

                if matches!(bind.ty, Tbl(_)) {
                    signal!(rec.on(), trace::TRACE_STMT_ASSIGN_TBL);
                    let mut incoming = Pending;
                    for s in &bind.aliases {
                        if !is_root(s) && !is_ghost(s) {
                            signal!(rec.on(), trace::TRACE_STMT_ASSIGN_TBL_VALID);
                            incoming = join_ty(&incoming, &self.lattice.site_elem[*s]);
                        }
                    }
                    if scalar(&incoming) {
                        signal!(rec.on(), trace::TRACE_STMT_ASSIGN_TBL_SCALAR);
                        let old = self.resolve_aliases(rec, name)?;
                        for s in old {
                            if !is_root(&s) && !is_ghost(&s) {
                                signal!(rec.on(), trace::TRACE_STMT_ASSIGN_TBL_SCALAR_VALID);
                                self.decide(rec, s, &incoming);
                            }
                        }
                    }
                }

                let target = self.walk.scopes.iter().rposition(|s| s.contains_key(name));
                if let Some(depth) = target {
                    signal!(rec.on(), trace::TRACE_STMT_ASSIGN_RESOLVED);
                    let old = self.walk.scopes[depth]
                        .get(name)
                        .cloned()
                        .expect("just checked");
                    self.rebind_death(rec, name, depth, stmt, &old, &bind.aliases)?;
                    self.check_row_ownership(name, depth, &bind, &moved_srcs)?;
                    let prov = self.prov_for_bind(Some(expr), &bind);
                    self.walk.scopes[depth].insert(name.clone(), bind);
                    self.walk.prov[depth].insert(name.to_string(), prov);
                    for (d, src) in &moved_srcs {
                        self.poison_moved(*d, src);
                    }
                    if rec.on() {
                        let entry = self
                            .own
                            .move_poisons
                            .entry(stmt as *const Stmt)
                            .or_default();
                        for (_, src) in &moved_srcs {
                            entry.insert(src.clone());
                        }
                    }
                    return Ok(());
                }
                return Err(ShapeError(format!(
                    "Scope Error: assignment to undeclared variable '{}'",
                    name
                )));
            }

            Stmt::IndexAssign { obj, key, value } => {
                let (t, obj_sites, _) = self.infer_expr(rec, obj)?;
                self.infer_expr(rec, key)?;
                self.check_table_use(rec, obj)?;
                self.check_uses(rec, obj)?;
                self.check_uses(rec, key)?;

                let (vt, vsites, vlineage) = self.infer_expr(rec, value)?;
                self.check_uses(rec, value)?;

                // The borrow-escape flip: a row read CAN be stored — it
                // moves into the cell, its identity (the birth-site
                // projection) recorded so the origin's frees spare the
                // row and the housing's death claims it. The refusals
                // that remain: a row the lattice cannot resolve
                // (dynamic or unproven key), a row stored into a
                // target the lattice cannot see (the pinned boundary,
                // a pending parameter), a row already owned by a live
                // binding or already housed, a cyclic store (the row
                // already inside its destination), and a slot already
                // housing a row (the displaced row would leak unnamed
                // inside its origin). A PURE borrow (all roots with
                // lineage — the boundary's rows) still refuses.
                let value_ghosts: Vec<usize> = vsites.iter().copied().filter(is_ghost).collect();
                let mut stored_ghosts: Vec<usize> = Vec::new();
                if matches!(vt, Tbl(_)) {
                    if !value_ghosts.is_empty() {
                        signal!(rec.on(), trace::TRACE_ROW_HOUSING);
                        let targets: Vec<usize> = obj_sites
                            .iter()
                            .copied()
                            .filter_map(|s| self.proj_node(s))
                            .collect();
                        if targets.is_empty() {
                            signal!(rec.on(), trace::TRACE_FAIL_ROW_STORE);
                            return Err(ShapeError(
                                "Lifetime Error: a row read cannot be stored into an \
                                 unresolved or host-owned table — cells can only house \
                                 rows whose base the lattice can see"
                                    .to_string(),
                            ));
                        }
                        for &g in &value_ghosts {
                            let Some(&birth) = self.lattice.ghost_site.get(&g) else {
                                signal!(rec.on(), trace::TRACE_FAIL_ROW_STORE);
                                return Err(ShapeError(
                                    "Lifetime Error: a row read with a dynamic or unproven \
                                     key cannot be stored — its birth site is not visible \
                                     to the lattice; store a row read through a literal \
                                     key instead"
                                        .to_string(),
                                ));
                            };
                            for &s in &targets {
                                if s == birth || self.birth_closure(s).contains(&birth) {
                                    signal!(rec.on(), trace::TRACE_FAIL_ROW_STORE);
                                    return Err(ShapeError(
                                        "Lifetime Error: cyclic row store — the row read \
                                         would be stored into a table that already contains \
                                         it (glm's element types are finite)"
                                            .to_string(),
                                    ));
                                }
                            }
                            self.check_row_housing(g, value)?;
                            stored_ghosts.push(g);
                        }
                        if let Some(k) = const_key_value(key) {
                            for &s in &targets {
                                if let Some(&existing) = self
                                    .lattice
                                    .site_slots
                                    .get(&s)
                                    .and_then(|m| m.get(&k))
                                    && is_ghost(&existing)
                                    // A previous pass recorded THIS store's
                                    // own ghost in the slot — re-housing the
                                    // same row is the store, not a second one.
                                    && !value_ghosts.iter().any(|&vg| self.same_row(vg, existing))
                                {
                                    signal!(rec.on(), trace::TRACE_FAIL_ROW_STORE);
                                    return Err(ShapeError(
                                        "Lifetime Error: the destination cell already houses \
                                         a row — free it through its housing before storing \
                                         another (the displaced row would leak unnamed)"
                                            .to_string(),
                                    ));
                                }
                            }
                        }
                    } else if vsites.iter().all(is_root) && !vlineage.is_empty() {
                        signal!(rec.on(), trace::TRACE_FAIL_BORROW_ESCAPE);
                        return Err(ShapeError(
                            "Lifetime Error: a row read cannot be stored — borrows hold no \
                             ownership, and cells own exactly the headers they are given"
                                .to_string(),
                        ));
                    }
                }

                for &s in &obj_sites {
                    if !is_root(&s) && !is_ghost(&s) {
                        self.reads.user_store_sites.insert(s);
                    }
                }

                let mut base_obj = obj;
                let mut expected_ty = vt.clone();
                let mut nested = false;

                while let Expr::Index {
                    obj: parent_obj, ..
                } = base_obj
                {
                    signal!(rec.on(), trace::TRACE_STMT_IDX_NESTED);
                    expected_ty = Tbl(Box::new(expected_ty));
                    base_obj = parent_obj.as_ref();
                    nested = true;
                }

                let (_, base_sites, _) = self.infer_expr(rec, base_obj)?;

                if nested {
                    for &s in &base_sites {
                        if !is_root(&s) && !is_ghost(&s) {
                            self.reads.user_store_sites.insert(s);
                        }
                    }
                }

                if nested && let Some(name) = get_base_identifier(base_obj) {
                    for &s in &base_sites {
                        if !is_root(&s) && !is_ghost(&s) {
                            self.reads.nested_store_bases.insert(s, name.clone());
                        }
                    }
                }

                let mut value_sites: BTreeSet<usize> = BTreeSet::new();
                if matches!(vt, Tbl(_)) {
                    if rec.on() {
                        signal!(rec.on(), trace::TRACE_STMT_IDX_TBL_VALUE);
                    }
                    value_sites.extend(vsites.iter().copied().filter(|c| !is_root(c) && !is_ghost(c)));
                    // A stored row's projection IS its header: the
                    // birth site rides the physical graph (row_links)
                    // exactly like a stored constructor's site.
                    for &g in &value_ghosts {
                        if let Some(b) = self.lattice.ghost_site.get(&g) {
                            value_sites.insert(*b);
                        }
                    }
                    // A ROW value is the leaf: its lineage chains name
                    // its ANCESTORS (the base and its other rows were
                    // NOT stored — only this row moved).
                    if value_ghosts.is_empty() {
                        for (&origin, (_, d)) in &vlineage {
                            value_sites.extend(self.reachable_rows(origin, *d));
                        }
                    }
                    let targets: Vec<usize> = obj_sites
                        .iter()
                        .copied()
                        .filter_map(|s| self.proj_node(s))
                        .collect();
                    for &s in &targets {
                        self.reads.tbl_value_stores.insert(s);
                        for &c in &value_sites {
                            self.reads.row_links.entry(s).or_default().insert(c);
                        }
                    }
                }

                if matches!(vt, Tbl(_)) {
                    let proj_bases: Vec<usize> = base_sites
                        .iter()
                        .copied()
                        .filter_map(|x| self.proj_node(x))
                        .collect();
                    for s in proj_bases {
                        self.own.stored_ctor_parents.insert(s);
                        signal!(rec.on(), trace::TRACE_STMT_IDX_STORED_CTOR);
                    }
                }

                let mut cyclic_sites: BTreeSet<usize> = BTreeSet::new();
                let mut cyc_sources: BTreeSet<usize> = value_sites.clone();
                if matches!(value, Expr::TableCtor(_)) {
                    self.expr_named_sites(rec, value, &mut cyc_sources);
                }
                if matches!(vt, Tbl(_)) {
                    for (&origin, (_, d)) in &vlineage {
                        if *d >= 2 {
                            cyc_sources.extend(self.reachable_rows(origin, *d));
                        }
                    }
                }
                let lineage_feedback = |s: &usize| {
                    vlineage
                        .iter()
                        .any(|(&origin, (_, d))| origin == *s && (nested || *d >= 2))
                };
                if matches!(vt, Tbl(_)) && !matches!(value, Expr::TableCtor(_)) {
                    let value_name = match value {
                        Expr::Identifier(n) => n.clone(),
                        _ => "<row>".to_string(),
                    };
                    for &s in &base_sites {
                        if is_root(&s) {
                            continue;
                        }
                        let cyclic = lineage_feedback(&s)
                            || cyc_sources
                                .iter()
                                .any(|&v| holds_reaches(&self.holds, v, s));
                        if cyclic {
                            let base_name = get_base_identifier(base_obj)
                                .cloned()
                                .unwrap_or_else(|| "<row>".to_string());
                            cyclic_sites.insert(s);
                            self.holds
                                .cyclic_stores
                                .entry(base_name)
                                .or_insert(value_name.clone());
                        } else {
                            for &v in &value_sites {
                                self.holds.hold_edges.entry(s).or_default().insert(v);
                            }
                        }
                    }
                } else if matches!(vt, Tbl(_)) && cyc_sources.iter().any(|v| base_sites.contains(v))
                {
                    let base_name = get_base_identifier(base_obj)
                        .cloned()
                        .unwrap_or_else(|| "<row>".to_string());
                    for &s in &base_sites {
                        if !is_root(&s) {
                            cyclic_sites.insert(s);
                            self.holds
                                .cyclic_stores
                                .entry(base_name.clone())
                                .or_insert("<constructor>".to_string());
                        }
                    }
                }

                let mut moved_srcs: Vec<(usize, String)> = Vec::new();
                if let Expr::Identifier(src) = value {
                    self.record_moved_param(src);
                    let (d, bind) = self.resolve(rec, src)?;
                    self.check_boundary_pin(&bind, src)?;
                    if matches!(vt, Tbl(_)) {
                        moved_srcs.push((d, src.clone()));
                    }
                } else if let Expr::TableCtor(_) = value {
                    self.collect_entry_moves(rec, value, &mut moved_srcs)?;
                }
                for (d, src) in &moved_srcs {
                    self.poison_moved(*d, src);
                }
                if rec.on() {
                    let entry = self
                        .own
                        .move_poisons
                        .entry(stmt as *const Stmt)
                        .or_default();
                    for (_, src) in &moved_srcs {
                        entry.insert(src.clone());
                    }
                }

                if matches!(t, Tbl(_) | Pending) {
                    signal!(rec.on(), trace::TRACE_STMT_IDX_BASE_TBL);
                    let proj_bases: Vec<usize> = base_sites
                        .iter()
                        .copied()
                        .filter_map(|x| self.proj_node(x))
                        .collect();
                    for s in proj_bases.iter().copied().filter(|x| !cyclic_sites.contains(x)) {
                        signal!(rec.on(), trace::TRACE_STMT_IDX_VALID_ALIAS);
                        self.decide(rec, s, &expected_ty);
                    }
                }

                if rec.on() {
                    signal!(rec.on(), trace::TRACE_STMT_IDX_CHECK_THRESH);
                    let real_base_sites: BTreeSet<usize> = base_sites
                        .iter()
                        .copied()
                        .filter_map(|s| self.proj_node(s))
                        .collect();
                    self.check_key_threshold(rec, key, &real_base_sites)?;
                }

                // The housing records: the stored row's birth site
                // rides the target's physical graph, its ghost is
                // spoken for (one housing per row), its slot map entry
                // names it, and its store-time register rides the
                // lowerer's keep capture.
                if !stored_ghosts.is_empty() {
                    let slot = const_key_value(key);
                    for &g in &stored_ghosts {
                        let birth = self.lattice.ghost_site[&g];
                        let tgt_sites: Vec<usize> = obj_sites
                            .iter()
                            .copied()
                            .filter_map(|x| self.proj_node(x))
                            .collect();
                        for tgt in tgt_sites {
                            self.reads.row_links.entry(tgt).or_default().insert(birth);
                            self.own.stored_ctor_parents.insert(tgt);
                            if let Some(k) = slot {
                                self.lattice
                                    .site_slots
                                    .entry(tgt)
                                    .or_default()
                                    .insert(k, g);
                            }
                        }
                        self.lattice.housed_ghosts.insert(g);
                        if rec.on() {
                            self.own
                                .store_ghost_keeps
                                .entry(stmt as *const Stmt)
                                .or_default()
                                .push(g);
                            signal!(rec.on(), trace::TRACE_ROW_HOUSED);
                        }
                    }
                }
            }

            Stmt::While { condition, body } => {
                self.infer_expr(rec, condition)?;
                self.check_uses(rec, condition)?;

                if rec.on()
                    && let Some(guard) = extract_guard(condition)
                {
                    signal!(rec.on(), trace::TRACE_STMT_WHILE_FILL);
                    self.detect_fill_loop(rec, guard, body);
                }

                let entry = self.walk.scopes.clone();
                let mut head = entry.clone();
                let scope_key = (stmt as *const Stmt, 0);
                loop {
                    self.walk.scopes = head.clone();
                    self.check_uses(rec, condition)?;
                    self.with_scope(|an| {
                        an.walk_stmts(rec, body);
                        an.decide_scope_exit(rec, scope_key)
                    })?;
                    let latch = self.walk.scopes.clone();
                    let mut merged = entry.clone();
                    merge_table_scopes(&mut merged, &head, &latch);
                    if merged == head {
                        signal!(rec.on(), trace::TRACE_STMT_WHILE_STABLE);
                        break;
                    }
                    head = merged;
                }
                self.walk.scopes = head;
                // A name the loop's fixpoint changed may hold the
                // latch's value at the free point — the pre-loop tag
                // cannot name it. Drop to the union fallback.
                for depth in 0..self.walk.scopes.len() {
                    let names: Vec<String> =
                        self.walk.scopes[depth].keys().cloned().collect();
                    for name in names {
                        let changed = entry
                            .get(depth)
                            .and_then(|s| s.get(&name))
                            .is_some_and(|e| e.aliases != self.walk.scopes[depth][&name].aliases);
                        if changed {
                            self.walk.prov[depth].insert(name, None);
                        }
                    }
                }
            }

            Stmt::If {
                condition,
                then_body,
                else_body,
            } => {
                signal!(rec.on(), trace::TRACE_STMT_IF);
                self.infer_expr(rec, condition)?;
                self.check_uses(rec, condition)?;
                let snapshot = self.walk.scopes.clone();
                let prov_snapshot = self.walk.prov.clone();
                self.with_scope(|an| {
                    an.walk_stmts(rec, then_body);
                    an.decide_scope_exit(rec, (stmt as *const Stmt, 0))
                })?;
                let then_exit = self.walk.scopes.clone();
                self.walk.scopes = snapshot;
                self.walk.prov = prov_snapshot.clone();
                self.with_scope(|an| {
                    an.walk_stmts(rec, else_body);
                    an.decide_scope_exit(rec, (stmt as *const Stmt, 1))
                })?;
                let else_exit = self.walk.scopes.clone();
                merge_table_scopes(&mut self.walk.scopes, &then_exit, &else_exit);
                // Provenance: a name the arms bind DIFFERENTLY whose
                // join is mixed carries the join's tag (the free points
                // split their plans per arm); a second join over an
                // already-tagged name drops to the union fallback — its
                // old tag cannot name the new value's arm.
                for depth in 0..self.walk.scopes.len() {
                    let names: Vec<String> =
                        self.walk.scopes[depth].keys().cloned().collect();
                    for name in names {
                        let (Some(tt), Some(et)) = (
                            then_exit.get(depth).and_then(|s| s.get(&name)),
                            else_exit.get(depth).and_then(|s| s.get(&name)),
                        ) else {
                            continue;
                        };
                        if tt.aliases == et.aliases {
                            continue;
                        }
                        let joined = &self.walk.scopes[depth][&name];
                        let mixed = joined.aliases.iter().any(is_ghost)
                            && joined.aliases.iter().any(|x| !is_root(x) && !is_ghost(x));
                        let cur = self.walk.prov[depth]
                            .get(&name)
                            .copied()
                            .flatten();
                        let new_prov = if mixed && cur.is_none() {
                            self.join_arm_shapes.insert(
                                (stmt as *const Stmt, name.clone()),
                                (tt.clone(), et.clone()),
                            );
                            Some(Prov::If(stmt as *const Stmt))
                        } else {
                            None
                        };
                        self.walk.prov[depth].insert(name, new_prov);
                    }
                }
            }

            Stmt::Do { body } => {
                signal!(rec.on(), trace::TRACE_STMT_DO);
                self.with_scope(|an| {
                    an.walk_stmts(rec, body);
                    an.decide_scope_exit(rec, (stmt as *const Stmt, 0))
                })?
            }

            Stmt::Print { exprs } => {
                signal!(rec.on(), trace::TRACE_STMT_PRINT);
                for e in exprs {
                    self.infer_expr(rec, e)?;
                    self.check_uses(rec, e)?;
                }
            }

            Stmt::Expr { expr } => {
                // A call on its own line: the value is discarded, the
                // effects are not.
                self.infer_expr(rec, expr)?;
                self.check_uses(rec, expr)?;
            }

            Stmt::Return { value } => {
                // The boundary: the returned value crosses into the
                // host's ownership, so it is checked as a use — nothing
                // is poisoned or freed; execution never continues past
                // the hand-off. Inside a function body (inlined at its
                // call sites), the value's shape also joins the body's
                // return shape.
                if let Some(v) = value {
                    let (ty, mut aliases, lineage) = self.infer_expr(rec, v)?;
                    self.check_uses(rec, v)?;
                    // A return hands its function's parameter straight
                    // back out when the returned expression IS a
                    // parameter or reaches one through a call chain
                    // (`return g(h(x))`): at a call site whose argument
                    // in that position is the pinned boundary table,
                    // the call's RESULT is pinned too. Recorded on
                    // every pass; the sets only grow.
                    let frame = self.fn_walk.last().map(|(fp, ps)| (*fp, ps.clone()));
                    if let Some((fn_ptr, params)) = frame {
                        let reaches = self.return_reaches_param(v, &params);
                        if !reaches.is_empty() {
                            self.fn_ret_params
                                .entry(fn_ptr)
                                .or_default()
                                .extend(reaches);
                        }
                    }
                    // The value's READ-reach — the heap sites the
                    // returned table IS or lives inside — for the two
                    // consumers that must not free it: the body's
                    // def-site reach frame (a caller's return-path
                    // planner consults it through fn_ret_reach) and
                    // this return's own path batch below. A scalar
                    // value references nothing: its table-typed
                    // sub-evaluations all die at the statement.
                    let mut reach: BTreeSet<usize> = BTreeSet::new();
                    if matches!(ty, Tbl(_) | Pending) {
                        self.ret_reach_sites(v, &mut reach);
                    }
                    if let Some(slot) = self.fn_ret_reaches.last_mut() {
                        slot.extend(reach.iter().copied());
                    }
                    if rec.on() {
                        self.plan_return_exits(Some((v, &ty)), &reach, stmt as *const Stmt);
                    }
                    if let Some(slot) = self.fn_rets.last_mut() {
                        // A body returning an outer name LENDS the
                        // table out — a borrow, not a transfer: strip
                        // every alias a binding outside the body still
                        // owns, so the caller's ownership flow can
                        // never free (or double-free) the original
                        // owner's header. Constructor-born sites and
                        // roots (the pinned-boundary taint) survive.
                        if let Some(&entry) = self.fn_scope_entries.last() {
                            let outer_owned: BTreeSet<usize> = self.walk.scopes[..entry]
                                .iter()
                                .flat_map(|scope| scope.values())
                                .flat_map(|ts| ts.aliases.iter().copied())
                                .filter(|s| !is_root(s))
                                .collect();
                            aliases.retain(|s| is_root(s) || !outer_owned.contains(s));
                        }
                        let shape = TableShape {
                            ty,
                            layout: LayoutVerdict::default(),
                            aliases,
                            lineage,
                        };
                        match slot {
                            None => *slot = Some(shape.clone()),
                            Some(joined) => {
                                joined.join(&shape);
                            }
                        }
                        // The per-edge twin: a caller whose result mixes
                        // a row edge with a ctor edge plans its frees
                        // per return edge (the tag phi over the exit's
                        // edges, in this same statement order).
                        if let Some(frame) = self.fn_edge_frames.last_mut() {
                            frame.push(shape);
                        }
                    }
                } else if rec.on() {
                    // A bare return: nothing crosses out, so nothing is
                    // excluded — every crossed scope frees in full.
                    let no_reach = BTreeSet::new();
                    self.plan_return_exits(None, &no_reach, stmt as *const Stmt);
                }
            }
        }
        Ok(())
    }

    fn infer_bind(&mut self, rec: Gate, expr: &Expr) -> Result<TableShape, ShapeError> {
        let (ty, aliases, lineage) = self.infer_expr(rec, expr)?;
        Ok(TableShape {
            ty,
            layout: LayoutVerdict::default(),
            aliases,
            lineage,
        })
    }

    fn enter_scope(&mut self) {
        self.walk.scopes.push(BTreeMap::new());
        self.walk.fn_scopes.push(BTreeMap::new());
        self.walk.prov.push(BTreeMap::new());
    }

    fn exit_scope(&mut self) {
        if let Some(dying) = self.walk.scopes.last() {
            for name in dying.keys() {
                self.holds.num_assigns.remove(name);
            }
        }
        self.walk.fn_scopes.pop();
        self.walk.prov.pop();
        self.walk.scopes.pop();
    }

    fn with_scope<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.enter_scope();
        let r = f(self);
        self.exit_scope();
        r
    }

    fn resolve_fn(&self, name: &str) -> Option<*const Expr> {
        for scope in self.walk.fn_scopes.iter().rev() {
            if let Some(entry) = scope.get(name) {
                return *entry;
            }
        }
        None
    }

    /// Declare a name's inline-closure binding in the CURRENT scope
    /// (a `local`, shadowing anything outer).
    fn declare_fn_binding(&mut self, name: &str, ptr: Option<*const Expr>) {
        self.walk
            .fn_scopes
            .last_mut()
            .unwrap()
            .insert(name.to_string(), ptr);
    }

    /// Bind (or clear) a name's inline-closure binding at its innermost
    /// visible scope (an assignment to an existing binding).
    fn assign_fn_binding(&mut self, name: &str, ptr: Option<*const Expr>) {
        let depth = self
            .walk
            .fn_scopes
            .iter()
            .rposition(|s| s.contains_key(name))
            .unwrap_or(self.walk.fn_scopes.len() - 1);
        self.walk.fn_scopes[depth].insert(name.to_string(), ptr);
    }

    /// Walk a function body at its definition site: params bound as
    /// pending values in a fresh scope, every `return` joining its
    /// shape, and the body's scope-exit frees recorded under `key` for
    /// the lowerer to replay at every inline site.
    fn walk_fn_def(
        &mut self,
        rec: Gate,
        fn_ptr: *const Expr,
        params: &[String],
        body: &[Stmt],
        key: (*const Stmt, u8),
    ) -> Result<(), ShapeError> {
        self.fn_scope_keys.insert(fn_ptr, key);
        collect_body_sites(body, &self.lattice.sites, &mut self.fn_body_sites);
        self.fn_params.insert(fn_ptr, params.to_vec());
        // Which parameters the body stores into — a caller's ctor
        // passed as that argument is stored through, and its element
        // resolves at the call site, not here.
        let stored: BTreeSet<String> = collect_stored_bases(body)
            .into_iter()
            .filter(|n| params.contains(n))
            .collect();
        self.fn_store_params.insert(fn_ptr, stored);
        self.fn_rets.push(None);
        self.fn_edge_frames.push(Vec::new());
        self.fn_ret_reaches.push(BTreeSet::new());
        self.fn_walk.push((fn_ptr, params.to_vec()));
        // The body's own scope will sit at this index once with_scope
        // pushes it; the return join borrows the boundary to strip
        // outer-owned aliases (borrows, not transfers).
        self.fn_scope_entries.push(self.walk.scopes.len());
        let mut result = Ok(());
        self.with_scope(|an| {
            let scope = an.walk.scopes.last_mut().unwrap();
            for p in params {
                // Params carry whatever the caller passes: pending
                // type, no nil/moved root — reading and storing through
                // them is fine, they own nothing at the def site.
                scope.insert(
                    p.clone(),
                    TableShape {
                        ty: Pending,
                        layout: LayoutVerdict::default(),
                        aliases: BTreeSet::new(),
                        lineage: BTreeMap::new(),
                    },
                );
            }
            an.walk_stmts(rec, body);
            if let Err(err) = an.decide_scope_exit(rec, key) {
                result = Err(err);
            }
        });
        self.fn_scope_entries.pop();
        self.fn_walk.pop();
        let joined = self.fn_rets.pop().unwrap_or(None);
        let edges = self.fn_edge_frames.pop().unwrap_or_default();
        let joined_reach = self.fn_ret_reaches.pop().unwrap_or_default();
        // Recorded on every pass, not just the recording one: during
        // the fixed-point loop `Expr::Call` reads this map, so gating
        // the insert on the recording pass would leave every call
        // Pending while the lattice converges — the loop would settle
        // on a state where call results carry no aliases. Each pass
        // re-joins from scratch and the recording pass's shape (over
        // the converged lattice) lands last, so the final entry stays
        // the authoritative one. The reach twin follows the same
        // discipline: a caller's return-path planner reads it at call
        // sites walked after this def.
        if joined.is_some() {
            self.fn_ret_reach.insert(fn_ptr, joined_reach);
        }
        if !edges.is_empty() {
            self.fn_ret_edge_shapes.insert(fn_ptr, edges);
        }
        if let Some(shape) = joined {
            self.fn_ret_shapes.insert(fn_ptr, shape);
        }
        result
    }

    fn resolve(&self, rec: Gate, name: &str) -> Result<(usize, TableShape), ShapeError> {
        for (depth, scope) in self.walk.scopes.iter().enumerate().rev() {
            if let Some(bind) = scope.get(name) {
                signal!(rec.on(), trace::TRACE_RESOLVE_FOUND);
                return Ok((depth, bind.clone()));
            }
        }
        Err(ShapeError(format!(
            "Scope Error: reference to undeclared variable '{}'",
            name
        )))
    }

    fn resolve_aliases(&self, rec: Gate, name: &str) -> Result<BTreeSet<usize>, ShapeError> {
        Ok(self.resolve(rec, name)?.1.aliases)
    }

    fn poison_moved(&mut self, depth: usize, name: &str) {
        self.walk.scopes[depth].insert(
            name.to_string(),
            TableShape {
                ty: Pending,
                layout: LayoutVerdict::default(),
                aliases: BTreeSet::from([MOVED_ROOT]),
                lineage: BTreeMap::new(),
            },
        );
        if let Some(scope) = self.walk.prov.get_mut(depth) {
            scope.remove(name);
        }
    }

    /// Which of `params` a returned expression hands straight back
    /// out: the parameter itself, or a call whose callee returns one
    /// of ITS parameters bare at an argument position fed by a
    /// parameter — recursively, so `return g(h(x))` reaches through
    /// the whole chain. Callees' sets are complete because their
    /// definition walks precede this call site.
    fn return_reaches_param(&self, expr: &Expr, params: &[String]) -> BTreeSet<usize> {
        match expr {
            Expr::Identifier(n) => params.iter().position(|p| p == n).into_iter().collect(),
            Expr::Call { callee, args } => match callee.as_ref() {
                Expr::Identifier(name) => {
                    let Some(fn_ptr) = self.resolve_fn(name) else {
                        return BTreeSet::new();
                    };
                    let Some(ret) = self.fn_ret_params.get(&fn_ptr) else {
                        return BTreeSet::new();
                    };
                    let mut out = BTreeSet::new();
                    for &j in ret {
                        if let Some(arg) = args.get(j) {
                            out.extend(self.return_reaches_param(arg, params));
                        }
                    }
                    out
                }
                _ => BTreeSet::new(),
            },
            _ => BTreeSet::new(),
        }
    }

    /// A parameter used as a move-source RHS (assignment, index-assign,
    /// or ctor entry) is recorded whether or not the analyzer can type
    /// the value: params are Pending at the def-site walk, so the Tbl
    /// gates never see them — but the pin must, since the lowerer moves
    /// the register all the same.
    fn record_moved_param(&mut self, name: &str) {
        if let Some((fn_ptr, params)) = self.fn_walk.last()
            && params.iter().any(|p| p == name)
        {
            self.fn_moved_params
                .entry(*fn_ptr)
                .or_default()
                .insert(name.to_string());
        }
    }

    /// The boundary table is pinned: the host owns its header, so it
    /// may never be moved into another binding or carrier table. Cell
    /// reads (`arg[i]`) are copies and stay legal — a move would let a
    /// script-side drop or the returned table's deep free collide with
    /// the host's own glm_tbl_free.
    fn check_boundary_pin(&self, bind: &TableShape, src: &str) -> Result<(), ShapeError> {
        if bind.aliases.contains(&BOUNDARY_ROOT) {
            return Err(ShapeError(format!(
                "Lifetime Error: the boundary table '{src}' is pinned — the host owns \
                 its header; read its cells ('{src}[i]') instead of moving the table"
            )));
        }
        Ok(())
    }

    fn collect_entry_moves(
        &mut self,
        rec: Gate,
        expr: &Expr,
        out: &mut Vec<(usize, String)>,
    ) -> Result<(), ShapeError> {
        if let Expr::TableCtor(entries) = expr {
            for (_, e) in entries {
                if let Expr::Identifier(n) = e {
                    let (d, bind) = self.resolve(rec, n)?;
                    self.record_moved_param(n);
                    self.check_boundary_pin(&bind, n)?;
                    if matches!(bind.ty, Tbl(_)) && !out.iter().any(|(_, m)| m == n) {
                        out.push((d, n.clone()));
                    }
                } else {
                    self.collect_entry_moves(rec, e, out)?;
                }
            }
        }
        Ok(())
    }


    /// Project a node onto the site-keyed physical graph: a ghost
    /// maps to its row's birth site when resolved; real sites map to
    /// themselves; roots and unresolved ghosts map to nothing.
    fn proj_node(&self, n: usize) -> Option<usize> {
        if is_ghost(&n) {
            self.lattice.ghost_site.get(&n).copied()
        } else if !is_root(&n) {
            Some(n)
        } else {
            None
        }
    }

    /// The transitive mint bases of a ghost — its origin chain (each
    /// base may itself be a parent row's ghost). A free harming a
    /// housed row harms the borrowers reading it out of its origins
    /// too, whose lineage names the origin rather than the housing.
    fn ghost_base_closure(&self, g: usize, out: &mut BTreeSet<usize>) {
        if let Some(m) = self.lattice.ghost_mints.get(&g) {
            for &b in &m.bases {
                if out.insert(b) && is_ghost(&b) {
                    self.ghost_base_closure(b, out);
                }
            }
        }
    }

    /// The birth closure of a site: its constructor-nested subtree
    /// (child_sites only — no store edges), stable across the
    /// fixed-point passes. A row whose birth site lies inside the
    /// destination's birth closure already has a cell there; storing
    /// it again would give one header two cells in one free tree.
    fn birth_closure(&self, site: usize) -> BTreeSet<usize> {
        let mut out = BTreeSet::new();
        let mut level = BTreeSet::from([site]);
        let mut seen = BTreeSet::new();
        while let Some(&s) = level.iter().next() {
            level.remove(&s);
            if !seen.insert(s) {
                continue;
            }
            if let Some(kids) = self.lattice.child_sites.get(&s) {
                for &k in kids {
                    if !is_root(&k) && out.insert(k) {
                        level.insert(k);
                    }
                }
            }
        }
        out
    }

    /// The pure housing reach of a site: everything reachable through
    /// stored-value edges alone (row_links) — the birth-side edges
    /// (ctor child_sites) excluded. A housed row inside `site`'s
    /// housing reach is CLAIMED by site's free; one reached only by
    /// birth is SPARED (the origin keeps it for its housing).
    fn row_links_closure(&self, site: usize) -> BTreeSet<usize> {
        let mut out = BTreeSet::new();
        let mut level = BTreeSet::from([site]);
        let mut seen = BTreeSet::new();
        while let Some(&s) = level.iter().next() {
            level.remove(&s);
            if !seen.insert(s) {
                continue;
            }
            if let Some(kids) = self.reads.row_links.get(&s) {
                for &k in kids {
                    if !is_root(&k) && out.insert(k) {
                        level.insert(k);
                    }
                }
            }
        }
        out
    }

    /// The full release of a free through `node`, ghost-aware: a
    /// ghost's row IS its birth site's header and everything the
    /// housing graph stores inside it — the projection makes the
    /// ghost's runtime deep free visible to the site-keyed graph.
    fn node_release(&self, node: usize) -> BTreeSet<usize> {
        let mut s = release_set(&self.lattice, &self.own, &self.reads, node);
        if is_ghost(&node)
            && let Some(&b) = self.lattice.ghost_site.get(&node)
        {
            s.extend(release_set(&self.lattice, &self.own, &self.reads, b));
            s.insert(b);
        }
        s
    }

    /// The harm set of a would-be free through `node`: its full
    /// release, plus the origin chains of every housed row inside it
    /// — borrowers reading a housed row out of its origin name the
    /// origin in their lineage, so the housing's free must spare them
    /// through the origin, not the row.
    fn harm_set(&self, node: usize) -> BTreeSet<usize> {
        let mut s = release_set(&self.lattice, &self.own, &self.reads, node);
        let mut origins: BTreeSet<usize> = BTreeSet::new();
        for &g in &self.lattice.housed_ghosts {
            if let Some(&b) = self.lattice.ghost_site.get(&g)
                && s.contains(&b)
            {
                self.ghost_base_closure(g, &mut origins);
            }
        }
        s.extend(origins);
        s
    }

    /// Post-plan bookkeeping: every housed row a firing entry CLAIMS
    /// (its housing reach) that no keep spares is spent — later reads
    /// of the row (through its origin or its housing) refuse, because
    /// the free already released the header. Ghost keeps spare their
    /// own row; a Name keep spares its borrower's row (the register
    /// the runtime skip compares against).
    fn mark_claimed(&mut self, node: usize, keeps: &[super::facts::Keep]) {
        let (_, claim) = self.housed_in_release(node);
        if claim.is_empty() {
            return;
        }
        let mut spared: BTreeSet<usize> = BTreeSet::new();
        for k in keeps {
            match k {
                super::facts::Keep::Ghost(g) => {
                    if let Some(&b) = self.lattice.ghost_site.get(g) {
                        spared.insert(b);
                    }
                }
                super::facts::Keep::Name(n) => {
                    if let Ok((_, ts)) = self.resolve(None, n)
                        && let Some(&g) = ts.aliases.iter().find(|s| is_ghost(s))
                        && let Some(&b) = self.lattice.ghost_site.get(&g)
                    {
                        spared.insert(b);
                    }
                }
            }
        }
        for g in claim {
            if let Some(&b) = self.lattice.ghost_site.get(&g)
                && !spared.contains(&b)
            {
                self.lattice.claimed_ghosts.insert(g);
            }
        }
    }

    /// The housed rows a free through `node` must SPARE (reached only
    /// by birth — the origin side of a stored row) versus those it
    /// CLAIMS (inside its housing reach). One release per row: the
    /// housing's death frees it, the origin's frees skip it.
    fn housed_in_release(&self, node: usize) -> (Vec<usize>, Vec<usize>) {
        let release = release_set(&self.lattice, &self.own, &self.reads, node);
        let housing = self.row_links_closure(node);
        let mut spare = Vec::new();
        let mut claim = Vec::new();
        for &g in &self.lattice.housed_ghosts {
            let Some(&b) = self.lattice.ghost_site.get(&g) else {
                continue;
            };
            if housing.contains(&b) {
                claim.push(g);
            } else if release.contains(&b) {
                spare.push(g);
            }
        }
        (spare, claim)
    }

    /// The element type of the row a ghost names: one elem-step
    /// inside the mint base's own element (`base` holds rows, the row
    /// holds cells — `ghost_row_elem(ghost of base[k]) == elem of that
    /// row`). Chained mints resolve through their parent ghost's own
    /// bases; unknown bases stay Pending.
    fn ghost_row_elem(&self, ghost: usize) -> Ty {
        let Some(mint) = self.lattice.ghost_mints.get(&ghost) else {
            return Pending;
        };
        let mut r = Pending;
        for &b in &mint.bases {
            if is_ghost(&b) {
                r = join_ty(&r, &self.ghost_row_elem(b));
            } else if !is_root(&b)
                && let Tbl(inner) = &self.lattice.site_elem[b] {
                    r = join_ty(&r, inner.as_ref());
            }
        }
        r
    }

    /// Do two row ghosts name the same header? Identical tokens do;
    /// otherwise their mint records must agree on the slot key (both
    /// literal and equal — a dynamic key on either side conservatively
    /// collides, because the runtime values may agree) and share at
    /// least one base, since bases only grow across the fixpoint.
    fn same_row(&self, a: usize, b: usize) -> bool {
        if a == b {
            return true;
        }
        match (
            self.lattice.ghost_mints.get(&a),
            self.lattice.ghost_mints.get(&b),
        ) {
            (Some(ma), Some(mb)) => {
                ma.key == mb.key && !ma.bases.is_disjoint(&mb.bases)
            }
            _ => false,
        }
    }

    /// Affine sole-holding on rows: one row header has exactly one
    /// owning binding. A ghost-carrying value binding to a name
    /// refuses when another LIVE binding already owns the same row —
    /// both would plan independent fires through faithful registers,
    /// and whichever ran second would free the header twice. Move
    /// sources displaced by this same statement are exempt (their
    /// ownership transfers); rebinding the same name at the same
    /// depth replaces the old ghost rather than doubling it.
    fn check_row_ownership(
        &self,
        name: &str,
        depth: usize,
        shape: &TableShape,
        moved_srcs: &[(usize, String)],
    ) -> Result<(), ShapeError> {
        let new_ghosts: Vec<usize> = shape
            .aliases
            .iter()
            .copied()
            .filter(is_ghost)
            .collect();
        if new_ghosts.is_empty() {
            return Ok(());
        }
        for (d, scope) in self.walk.scopes.iter().enumerate() {
            for (n, ts) in scope {
                if d == depth && n == name {
                    continue;
                }
                if moved_srcs.iter().any(|(md, m)| *md == d && m == n) {
                    continue;
                }
                for &old in &new_ghosts {
                    if ts
                        .aliases
                        .iter()
                        .any(|&g| is_ghost(&g) && self.same_row(old, g))
                    {
                        return Err(ShapeError(format!(
                            "Lifetime Error: the row read into '{name}' is already owned by \
                             '{n}' — one row has exactly one owning binding; read it through \
                             '{n}' instead"
                        )));
                    }
                }
            }
        }
        Ok(())
    }


    /// The provenance of a freshly bound value: a MIXED shape (a row
    /// ghost alongside real sites) records which join produced it — a
    /// call's return-edge join, or a move inherits its source's tag.
    /// Everything else plans on the union shape (None).
    fn prov_for_bind(&self, expr: Option<&Expr>, shape: &TableShape) -> Option<Prov> {
        let has_ghost = shape.aliases.iter().any(is_ghost);
        let has_site = shape.aliases.iter().any(|x| !is_root(x) && !is_ghost(x));
        if !(has_ghost && has_site) {
            return None;
        }
        match expr {
            Some(e @ Expr::Call { .. }) => Some(Prov::Call(e as *const Expr)),
            Some(Expr::Identifier(src)) => self
                .resolve(None, src)
                .ok()
                .and_then(|(d, _)| self.walk.prov.get(d)?.get(src).copied().flatten()),
            _ => None,
        }
    }

    /// One arm's gate: does the arm defer? A ghost-carrying arm whose
    /// lineage origins are still housed emits nothing — the origin's
    /// own death composts the row. An arm without ghosts never defers.
    fn arm_defers(&self, name: &str, depth: usize, arm: &TableShape) -> bool {
        arm.aliases.iter().any(is_ghost)
            && arm.lineage.keys().any(|o| {
                origin_housed(
                    &self.walk.scopes,
                    &self.lattice,
                    &self.own,
                    &self.reads,
                    *o,
                    depth,
                    name,
                )
            })
    }

    fn arm_keeps(&self, name: &str, depth: usize, arm: &TableShape) -> Vec<String> {
        let mut release: BTreeSet<usize> = BTreeSet::new();
        for &r in arm.aliases.iter().filter(|s| !is_root(s)) {
            release.extend(release_set(&self.lattice, &self.own, &self.reads, r));
        }
        borrowers_of(
            &self.walk.scopes,
            &release,
            self.walk.scopes.len(),
            depth,
            name,
        )
    }

    /// The free plan for one arm of a provenance join — true defers
    /// (emits nothing), false frees the carrier's faithful register.
    fn plan_arm(&self, name: &str, depth: usize, arm: &TableShape) -> bool {
        self.arm_defers(name, depth, arm)
    }

    /// Affine sole-housing: a row stored into a cell refuses when
    /// another LIVE binding already owns the same header (both would
    /// plan independent fires) or another cell already houses it (both
    /// housings would claim it). The value's own Identifier source is
    /// exempt — its ownership moves into the cell.
    fn check_row_housing(&self, g: usize, value: &Expr) -> Result<(), ShapeError> {
        let skip = match value {
            Expr::Identifier(n) => Some(n.as_str()),
            _ => None,
        };
        for scope in &self.walk.scopes {
            for (n, ts) in scope {
                if Some(n.as_str()) == skip {
                    continue;
                }
                for &og in ts.aliases.iter().filter(|s| is_ghost(s)) {
                    if self.same_row(g, og) {
                        return Err(ShapeError(format!(
                            "Lifetime Error: the row read is already owned by '{n}' — one \
                             row has exactly one housing; store it through '{n}' instead"
                        )));
                    }
                }
            }
        }
        for &h in &self.lattice.housed_ghosts {
            if h != g && self.same_row(g, h) {
                return Err(ShapeError(
                    "Lifetime Error: the row read is already housed in another cell — \
                     one row has exactly one housing"
                        .to_string(),
                ));
            }
        }
        Ok(())
    }

    fn reachable_rows(&self, origin: usize, depth: usize) -> BTreeSet<usize> {
        let mut level = BTreeSet::from([origin]);
        for _ in 0..depth {
            let mut next = BTreeSet::new();
            for s in &level {
                if let Some(kids) = self.lattice.child_sites.get(s) {
                    next.extend(kids.iter().copied());
                }
                if let Some(kids) = self.reads.row_links.get(s) {
                    next.extend(kids.iter().copied());
                }
            }
            level = next;
        }
        level
    }

    fn decide(&mut self, rec: Gate, site: usize, vt: &Ty) {
        signal!(rec.on(), trace::TRACE_DECIDE_VISIT);
        if self.lattice.site_elem[site] == Conflict {
            signal!(rec.on(), trace::TRACE_DECIDE_CONFLICT);
            return;
        }

        let joined = join_ty(&self.lattice.site_elem[site], vt);
        if self.lattice.site_elem[site] != joined {
            self.probe(rec, trace::TRACE_JOIN_RETYPED);
            self.lattice.site_elem[site] = joined.clone();
            self.walk.changed = true;
        }

        let expected_var_ty = Tbl(Box::new(self.lattice.site_elem[site].clone()));

        let Self {
            walk:
                WalkState {
                    scopes,
                    conv_fires,
                    changed,
                    ..
                },
            ..
        } = self;
        for scope in scopes.iter_mut() {
            for ts in scope.values_mut() {
                if ts.aliases.contains(&site)
                    && !is_root(&site)
                    && (ts.ty == Pending || ts.ty != expected_var_ty)
                {
                    if rec.on() {
                        trace::compiler_trace_signal(trace::TRACE_DECIDE_UPDATE);
                    } else {
                        conv_fires.insert(trace::TRACE_DECIDE_UPDATE);
                    }
                    ts.ty = expected_var_ty.clone();
                    if ts.ty != Pending {
                        if rec.on() {
                            trace::compiler_trace_signal(trace::TRACE_DECIDE_UPDATE_CHANGED);
                        } else {
                            conv_fires.insert(trace::TRACE_DECIDE_UPDATE_CHANGED);
                        }
                        *changed = true;
                    }
                }
            }
        }
    }

    fn infer_expr(
        &mut self,
        rec: Gate,
        expr: &Expr,
    ) -> Result<(Ty, BTreeSet<usize>, RowLineage), ShapeError> {
        match expr {
            Expr::Integer(_) => {
                signal!(rec.on(), trace::TRACE_INFER_INT);
                Ok((Int, BTreeSet::new(), BTreeMap::new()))
            }
            Expr::Float(_) => {
                signal!(rec.on(), trace::TRACE_INFER_FLT);
                Ok((Flt, BTreeSet::new(), BTreeMap::new()))
            }
            Expr::Boolean(_) => {
                signal!(rec.on(), trace::TRACE_INFER_BOOL);
                Ok((Bool, BTreeSet::new(), BTreeMap::new()))
            }
            Expr::String(_) => {
                signal!(rec.on(), trace::TRACE_INFER_STR);
                Ok((Str, BTreeSet::new(), BTreeMap::new()))
            }
            Expr::Nil => {
                signal!(rec.on(), trace::TRACE_INFER_NIL);
                Ok((Conflict, BTreeSet::new(), BTreeMap::new()))
            }
            Expr::TableCtor(entries) => {
                let id = self.lattice.sites[&(expr as *const Expr)];
                if entries
                    .iter()
                    .enumerate()
                    .all(|(i, (k, _))| matches!(k, CtorKey::Const(c) if *c == i as i64))
                {
                    self.layout.dense_ctor_len.insert(id, entries.len() as i64);
                }
                let mut first_elem_ty: Option<Ty> = None;
                for (ei, (k, e)) in entries.iter().enumerate() {
                    // The slot map: a literal-key entry holding a
                    // constructor names the row born at that slot —
                    // later literal reads of the slot resolve their
                    // ghosts onto it.
                    if let CtorKey::Const(c) = k
                        && let Some(&child) =
                            self.lattice.sites.get(&(e as *const Expr))
                        && matches!(e, Expr::TableCtor(_))
                    {
                        self.lattice
                            .site_slots
                            .entry(id)
                            .or_default()
                            .insert(*c, child);
                    }
                    if let CtorKey::Expr(ke) = k {
                        let (kt, _, _) = self.infer_expr(rec, ke)?;
                        if matches!(kt, Flt | Bool | Str) {
                            signal!(rec.on(), trace::TRACE_INFER_IDX_BAD_KEY);
                            continue;
                        }
                        if rec.on() {
                            self.check_key_threshold(rec, ke, &BTreeSet::from([id]))?;
                        }
                    } else if let CtorKey::Const(c) = k
                        && rec.on()
                    {
                        self.check_key_threshold(rec, &Expr::Integer(*c), &BTreeSet::from([id]))?;
                    }
                    let (t, esites, elineage) = self.infer_expr(rec, e)?;
                    if matches!(t, Tbl(_)) {
                        self.reads.tbl_value_stores.insert(id);
                        if let Some(&child_site) = self.lattice.sites.get(&(e as *const Expr)) {
                            signal!(rec.on(), trace::TRACE_INFER_TBL_CHILD);
                            self.lattice
                                .child_sites
                                .entry(id)
                                .or_default()
                                .insert(child_site);
                        }
                        if !matches!(e, Expr::TableCtor(_)) {
                            // The borrow-escape flip: a RESOLVED row
                            // read stores (it moves into the entry —
                            // the housing records its birth site on
                            // the ctor, its slot, and its keep
                            // register). Unresolved rows and pure
                            // borrows (all roots with lineage) still
                            // refuse, and the sole-housing, cycle, and
                            // claimed-row gates apply verbatim.
                            let entry_ghosts: Vec<usize> = esites.iter().copied().filter(is_ghost).collect();
                            if !entry_ghosts.is_empty() {
                                signal!(rec.on(), trace::TRACE_ROW_HOUSING);
                                for &g in &entry_ghosts {
                                    let Some(&birth) = self.lattice.ghost_site.get(&g) else {
                                        signal!(rec.on(), trace::TRACE_FAIL_ROW_STORE);
                                        return Err(ShapeError(
                                            "Lifetime Error: a row read with a dynamic or \
                                             unproven key cannot be stored — its birth \
                                             site is not visible to the lattice; store a \
                                             row read through a literal key instead"
                                                .to_string(),
                                        ));
                                    };
                                    if id == birth
                                        || self.birth_closure(id).contains(&birth)
                                    {
                                        signal!(rec.on(), trace::TRACE_FAIL_ROW_STORE);
                                        return Err(ShapeError(
                                            "Lifetime Error: cyclic row store — the row \
                                             read would be stored into a table that \
                                             already contains it (glm's element types \
                                             are finite)"
                                                .to_string(),
                                        ));
                                    }
                                    self.check_row_housing(g, e)?;
                                }
                                for &g in &entry_ghosts {
                                    let birth = self.lattice.ghost_site[&g];
                                    self.reads.row_links.entry(id).or_default().insert(birth);
                                    self.lattice.housed_ghosts.insert(g);
                                    if let CtorKey::Const(c) = k {
                                        self.lattice
                                            .site_slots
                                            .entry(id)
                                            .or_default()
                                            .insert(*c, g);
                                    }
                                    if rec.on() {
                                        self.own
                                            .ctor_ghost_keeps
                                            .entry(expr as *const Expr)
                                            .or_default()
                                            .push((ei, g));
                                        signal!(rec.on(), trace::TRACE_ROW_HOUSED);
                                    }
                                }
                            } else if esites.iter().all(is_root) && !elineage.is_empty() {
                                signal!(rec.on(), trace::TRACE_FAIL_BORROW_ESCAPE);
                                return Err(ShapeError(
                                    "Lifetime Error: a row read cannot be stored — borrows \
                                     hold no ownership, and cells own exactly the headers \
                                     they are given"
                                        .to_string(),
                                ));
                            }
                            self.own.stored_ctor_parents.insert(id);
                            let proj_values: Vec<usize> = esites
                                .iter()
                                .copied()
                                .filter_map(|c| self.proj_node(c))
                                .collect();
                            for c in proj_values {
                                self.reads.row_links.entry(id).or_default().insert(c);
                            }
                        }
                    }
                    match first_elem_ty {
                        None => {
                            signal!(rec.on(), trace::TRACE_INFER_TBL_FIRST);
                            first_elem_ty = Some(t.clone());
                        }
                        Some(ref expected) => {
                            let joined = join_ty(expected, &t);
                            if joined == Conflict {
                                signal!(rec.on(), trace::TRACE_INFER_TBL_MISMATCH);
                                if self.lattice.site_elem[id] != Conflict {
                                    self.probe(rec, trace::TRACE_INFER_TBL_CONFLICT);
                                    self.lattice.site_elem[id] = Conflict;
                                    self.walk.changed = true;
                                }
                            } else {
                                signal!(rec.on(), trace::TRACE_INFER_TBL_MATCH);
                                if joined != *expected {
                                    first_elem_ty = Some(joined);
                                }
                            }
                        }
                    }
                    self.decide(rec, id, &t);
                }
                let resolved = if self.lattice.site_elem[id] != Pending {
                    signal!(rec.on(), trace::TRACE_INFER_TBL_RESOLVED);
                    self.lattice.site_elem[id].clone()
                } else {
                    signal!(rec.on(), trace::TRACE_INFER_TBL_PENDING);
                    first_elem_ty.unwrap_or(Pending)
                };
                Ok((
                    Tbl(Box::new(resolved)),
                    BTreeSet::from([id]),
                    BTreeMap::new(),
                ))
            }
            Expr::Index { obj, key } => {
                let (kt, _, _) = self.infer_expr(rec, key)?;
                if matches!(kt, Flt | Bool | Str) {
                    signal!(rec.on(), trace::TRACE_INFER_IDX_BAD_KEY);
                    self.infer_expr(rec, obj)?;
                    return Ok((Conflict, BTreeSet::new(), BTreeMap::new()));
                }
                let (t, sites, mut lineage) = self.infer_expr(rec, obj)?;
                if !matches!(t, Tbl(_)) {
                    signal!(rec.on(), trace::TRACE_INFER_IDX_BAD_OBJ);
                    // A row read through a PENDING value — a parameter,
                    // whose type resolves at the checker's call sites —
                    // defers instead of poisoning: Conflict here would
                    // mark every carrier the result is stored into as
                    // heterogeneous. Genuine scalars stay Conflict (the
                    // checker rejects them with a precise message).
                    let r = if matches!(t, Pending) { Pending } else { Conflict };
                    for (&origin, (name, d)) in &lineage {
                        if is_ghost(&origin) {
                            continue;
                        }
                        let depth = d + 1;
                        let e = self
                            .reads
                            .row_reads
                            .entry(origin)
                            .or_insert((name.clone(), depth));
                        if depth > e.1 {
                            e.1 = depth;
                        }
                    }
                    return Ok((r, BTreeSet::new(), BTreeMap::new()));
                }
                // Claimed rows: a planned free already released the
                // header, and row identity is the BIRTH SITE (a re-read
                // mints a fresh ghost onto the same row) — reading a
                // claimed row (through its origin's slot, its own
                // binding, or a dynamic key over a base with a claimed
                // slot) would touch freed memory.
                let claimed_rows: BTreeSet<usize> = self
                    .lattice
                    .claimed_ghosts
                    .iter()
                    .filter_map(|g| self.lattice.ghost_site.get(g).copied())
                    .collect();
                if !claimed_rows.is_empty() {
                    let err = || {
                        ShapeError(
                            "Lifetime Error: this row was already released with its \
                             housing — read it through the housing's cell before it \
                             drops"
                                .to_string(),
                        )
                    };
                    for &s in &sites {
                        let row = if is_ghost(&s) {
                            self.lattice.ghost_site.get(&s).copied()
                        } else {
                            Some(s)
                        };
                        if row.is_some_and(|r| claimed_rows.contains(&r)) {
                            return Err(err());
                        }
                    }
                    let slot_claimed = |base: usize, k: Option<i64>| -> bool {
                        self.lattice.site_slots.get(&base).is_some_and(|m| {
                            m.iter().any(|(kk, node)| {
                                k.is_none_or(|x| *kk == x)
                                    && self
                                        .proj_node(*node)
                                        .is_some_and(|p| claimed_rows.contains(&p))
                            })
                        })
                    };
                    let k = const_key_value(key);
                    for &s in &sites {
                        if !is_root(&s) && slot_claimed(s, k) {
                            return Err(err());
                        }
                    }
                }
                let mut r = Pending;
                for s in &sites {
                    if !is_root(s) && !is_ghost(s) {
                        signal!(rec.on(), trace::TRACE_INFER_IDX_VALID);
                        self.lattice.needed[*s] = true;
                        r = join_ty(&r, &self.lattice.site_elem[*s]);
                    } else if is_ghost(s) {
                        // Reading through a parent row's token: the
                        // row's element type is one step inside the
                        // mint base's own element (a ghost carries no
                        // per-site vector entry, so the chain resolves
                        // through its recorded bases).
                        r = join_ty(&r, &self.ghost_row_elem(*s));
                    }
                }
                for (&origin, (name, d)) in &lineage {
                    if is_ghost(&origin) {
                        continue;
                    }
                    let depth = d + 1;
                    let e = self
                        .reads
                        .row_reads
                        .entry(origin)
                        .or_insert((name.clone(), depth));
                    if depth > e.1 {
                        e.1 = depth;
                    }
                }
                for (_, d) in lineage.values_mut() {
                    *d += 1;
                }
                if let Expr::Identifier(name) = obj.as_ref() {
                    for &s in &sites {
                        if !is_root(&s) {
                            lineage.entry(s).or_insert((name.clone(), 1));
                        }
                    }
                }
                // The row's ownership token: a ghost minted per read
                // node, handed to whoever binds the value. Only a read
                // with lineage mints — a row read out of the pinned
                // boundary (`arg[i]`, all-root base sites) owns nothing
                // so the host's own free stays the row's only death,
                // and a pending base still defers above — and only a
                // TABLE-valued read mints: a scalar cell read copies a
                // value out of the row, owns no header, and a token
                // here would fire a TableFree through a non-pointer
                // register once its base died. Ghosts may enter lineage
                // as origin keys (a chained read `c = b[0]` tracks b's
                // ghost), keeping a parent row's death vetoed while
                // its rows are borrowed — but they never reach
                // row_reads or the per-site vectors. The mint record
                // (base aliases + const key) is the row's identity for
                // affine sole-holding: two reads of the same slot are
                // the same header, so the second owning bind is
                // refused.
                let aliases = if lineage.is_empty() || scalar(&r) || matches!(r, Conflict) {
                    BTreeSet::new()
                } else {
                    signal!(rec.on(), trace::TRACE_ROW_GHOST_MINT);
                    let node = expr as *const Expr;
                    let ghost = match self.lattice.row_ghosts.get(&node) {
                        Some(&g) => g,
                        None => {
                            let g = self.lattice.row_ghost_next;
                            self.lattice.row_ghost_next += 1;
                            self.lattice.row_ghosts.insert(node, g);
                            g
                        }
                    };
                    let bases: BTreeSet<usize> =
                        sites.iter().copied().filter(|s| !is_root(s)).collect();
                    self.lattice.ghost_mints.insert(
                        ghost,
                        GhostMint {
                            bases,
                            key: const_key_value(key),
                        },
                    );
                    // Resolve the birth site: a single real base with
                    // a literal key over a known slot names the row
                    // born there (a slot holding a ghost resolves
                    // through that ghost's own birth). Resolution is
                    // what makes the row storable — the physical graph
                    // (row_links, deepfree) is site-keyed.
                    if let Some(k) = const_key_value(key) {
                        let real: Vec<usize> = sites
                            .iter()
                            .copied()
                            .filter(|s| !is_root(s) && !is_ghost(s))
                            .collect();
                        if real.len() == 1
                            && let Some(&slot) = self
                                .lattice
                                .site_slots
                                .get(&real[0])
                                .and_then(|m| m.get(&k))
                        {
                            let birth = if is_ghost(&slot) {
                                self.lattice.ghost_site.get(&slot).copied()
                            } else {
                                Some(slot)
                            };
                            if let Some(b) = birth {
                                self.lattice.ghost_site.insert(ghost, b);
                            }
                        }
                    }
                    BTreeSet::from([ghost])
                };
                Ok((r, aliases, lineage))
            }
            Expr::Identifier(name) => {
                signal!(rec.on(), trace::TRACE_INFER_IDENT);
                let (depth, bind) = self.resolve(rec, name)?;
                let _ = depth;
                Ok((bind.ty, bind.aliases.clone(), bind.lineage.clone()))
            }
            Expr::BinaryOp { op, left, right } => {
                let (lt, _, _) = self.infer_expr(rec, left)?;
                let (rt, _, _) = self.infer_expr(rec, right)?;
                match op {
                    BinOp::Div if matches!(lt, Int | Flt) && matches!(rt, Int | Flt) => {
                        signal!(rec.on(), trace::TRACE_INFER_BINOP_ARITH);
                        Ok((Flt, BTreeSet::new(), BTreeMap::new()))
                    }
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::IntDiv | BinOp::Mod => {
                        signal!(rec.on(), trace::TRACE_INFER_BINOP_ARITH);
                        Ok((arith_ty(lt, rt), BTreeSet::new(), BTreeMap::new()))
                    }
                    _ => {
                        signal!(rec.on(), trace::TRACE_INFER_BINOP_OTHER);
                        Ok((Bool, BTreeSet::new(), BTreeMap::new()))
                    }
                }
            }
            Expr::UnaryOp { op, expr } => {
                let (t, _, _) = self.infer_expr(rec, expr)?;
                match op {
                    UnOp::Neg => {
                        signal!(rec.on(), trace::TRACE_INFER_UNOP_NEG);
                        Ok((t, BTreeSet::new(), BTreeMap::new()))
                    }
                    UnOp::Not => {
                        signal!(rec.on(), trace::TRACE_INFER_UNOP_NOT);
                        Ok((Bool, BTreeSet::new(), BTreeMap::new()))
                    }
                    UnOp::Len => {
                        signal!(rec.on(), trace::TRACE_INFER_UNOP_LEN);
                        Ok((Int, BTreeSet::new(), BTreeMap::new()))
                    }
                }
            }
            Expr::SysAllocCount => {
                signal!(rec.on(), trace::TRACE_INFER_SYSALLOC);
                Ok((Int, BTreeSet::new(), BTreeMap::new()))
            }
            Expr::Function { .. } => {
                // The function value itself owns nothing; its body was
                // walked at its definition site.
                signal!(rec.on(), trace::TRACE_INFER_IDENT);
                Ok((Pending, BTreeSet::new(), BTreeMap::new()))
            }
            Expr::Call { callee, args } => {
                let name = match callee.as_ref() {
                    Expr::Identifier(n) => n.as_str(),
                    _ => {
                        return Err(ShapeError(
                            "Scope Error: calls go through a function variable — only \
                             anonymous functions assigned directly to variables are \
                             callable"
                                .into(),
                        ));
                    }
                };
                let fn_ptr = self.resolve_fn(name).ok_or_else(|| {
                    ShapeError(format!(
                        "Scope Error: '{name}' is not a function — only anonymous \
                         functions assigned directly to variables are callable"
                    ))
                })?;
                let params = self.fn_params.get(&fn_ptr).cloned().unwrap_or_default();
                let stored_params = self.fn_store_params.get(&fn_ptr).cloned();
                let moved_params = self.fn_moved_params.get(&fn_ptr).cloned();
                let ret_params = self.fn_ret_params.get(&fn_ptr).cloned();
                // Set when a pinned argument reaches a parameter the
                // body hands straight back out.
                let mut ret_pinned = false;
                let mut ret_arg_sites: BTreeSet<usize> = BTreeSet::new();
                let mut ret_lineage: RowLineage = BTreeMap::new();
                for (i, a) in args.iter().enumerate() {
                    // The pin crosses a call boundary only through a
                    // MOVE: a body that moves its parameter (directly,
                    // or transitively via a callee) must never receive
                    // the pinned boundary table — the host-owned header
                    // would end up inside a carrier the script frees.
                    // Read-only bodies may take it: call arguments
                    // alias the caller's register, they do not move.
                    if let Expr::Identifier(n) = a {
                        let moves_it = moved_params
                            .as_ref()
                            .is_some_and(|m| params.get(i).is_some_and(|p| m.contains(p)));
                        if moves_it {
                            // Transitive: the caller's own param
                            // inherits the move through this call, so
                            // a later pinned call to IT rejects too.
                            let cur = self
                                .fn_walk
                                .last()
                                .map(|(fp, ps)| (*fp, ps.iter().any(|p| p == n)));
                            if let Some((cur_fn, is_param)) = cur
                                && is_param
                            {
                                self.fn_moved_params
                                    .entry(cur_fn)
                                    .or_default()
                                    .insert(n.clone());
                            }
                            if let Ok((_, bind)) = self.resolve(rec, n)
                                && bind.aliases.contains(&BOUNDARY_ROOT)
                            {
                                let p = params.get(i).cloned().unwrap_or_default();
                                return Err(ShapeError(format!(
                                    "Lifetime Error: '{name}' moves its parameter '{p}' \
                                     into another table or binding — the pinned table \
                                     '{n}' may not be passed to it (the host owns the \
                                     boundary header)"
                                )));
                            }
                        }
                        if ret_params.as_ref().is_some_and(|r| r.contains(&i))
                            && let Ok((_, bind)) = self.resolve(rec, n)
                            && bind.aliases.contains(&BOUNDARY_ROOT)
                        {
                            ret_pinned = true;
                        }
                    }
                    let (_, sites, arg_lineage) = self.infer_expr(rec, a)?;
                    // A ctor passed as a stored-into parameter is
                    // stored through inside the inlined body — its
                    // element resolves at the checker's call site.
                    if let Some(p) = params.get(i)
                        && let Some(stored) = &stored_params
                        && stored.contains(p)
                    {
                        self.fn_param_sites
                            .extend(sites.iter().copied().filter(|s| !is_root(s) && !is_ghost(s)));
                    }
                    // A parameter the body hands straight back carries
                    // the argument's ctor sites into the call result:
                    // the value passes through untouched, so the
                    // caller's ownership flow (rebind free, scope exit,
                    // temp plan) keeps holding it. Identifier arguments
                    // are EXCLUDED: their sites stay owned by the named
                    // binding — a pass-through of a name is a borrow
                    // (the caller-side twin of the def-site return-join
                    // strip), and co-owning them would break affine sole
                    // holding (a rebind or scope exit would free a live
                    // binding's header).
                    if ret_params.as_ref().is_some_and(|r| r.contains(&i))
                        && !matches!(a, Expr::Identifier(_))
                    {
                        // A bare-returned argument flows out through the
                        // call result untouched — and that includes its
                        // ROW IDENTITY: a row-read argument's ghost and
                        // lineage ride along, so the result binding owns
                        // the row (its fire gated on the argument's own
                        // base, exactly as if the read had bound
                        // directly). Without the lineage the borrow is
                        // invisible and the base's free would compost
                        // the row under the live result; without the
                        // ghost the row outlives every free plan unnamed.
                        ret_arg_sites.extend(sites.iter().copied().filter(|s| !is_root(s)));
                        for (origin, val) in &arg_lineage {
                            match ret_lineage.entry(*origin) {
                                std::collections::btree_map::Entry::Vacant(v) => {
                                    v.insert(val.clone());
                                }
                                std::collections::btree_map::Entry::Occupied(mut o) => {
                                    if val.1 > o.get().1 {
                                        o.get_mut().1 = val.1;
                                    }
                                }
                            }
                        }
                    }
                }
                // The call result flows the def-site-joined return
                // shape, so a returned table carries its ctor site into
                // the caller's ownership flow; no returns -> pending. A
                // pinned argument returned bare pins the result: moving
                // it onward would nest the boundary header.
                if let Some(shape) = self.fn_ret_shapes.get(&fn_ptr) {
                    let mut aliases = shape.aliases.clone();
                    aliases.extend(ret_arg_sites);
                    if ret_pinned {
                        aliases.insert(BOUNDARY_ROOT);
                    }
                    // The def-site return join's lineage plus the
                    // bare-returned arguments' lineages (a pass-through
                    // row's base chain rides the result so its free
                    // stays gated and the base's free keeps the
                    // result's row).
                    let mut lineage = shape.lineage.clone();
                    for (origin, (name, d)) in ret_lineage {
                        match lineage.entry(origin) {
                            std::collections::btree_map::Entry::Vacant(v) => {
                                v.insert((name, d));
                            }
                            std::collections::btree_map::Entry::Occupied(mut o) => {
                                if d > o.get().1 {
                                    o.get_mut().1 = d;
                                }
                            }
                        }
                    }
                    Ok((shape.ty.clone(), aliases, lineage))
                } else {
                    signal!(rec.on(), trace::TRACE_INFER_IDENT);
                    Ok((Pending, BTreeSet::new(), BTreeMap::new()))
                }
            }
        }
    }

    /// The scope-exit frees for the scope at `depth`, signals fired:
    /// the natural-exit decisions (block scopes, inline bodies, the
    /// root chunk) all land here.
    fn scope_exit_frees(
        &mut self,
        rec: Gate,
        depth: usize,
        key: (*const Stmt, u8),
    ) -> Result<(Vec<super::facts::DoExitFree>, Vec<super::facts::ArmFreeEntry>), ShapeError> {
        let (frees, arms) = match self.compute_scope_frees(depth, true) {
            Ok(v) => v,
            Err(err) => {
                signal!(rec.on(), trace::TRACE_DO_EXIT_ALIAS_SURVIVES);
                return Err(err);
            }
        };
        for _ in &frees {
            signal!(rec.on(), trace::TRACE_DO_EXIT_FREE_SITE);
        }
        let _ = key;
        Ok((frees, arms))
    }

    /// The pure half of the scope-exit composition: every binding in
    /// the scope at `depth` still owning a heap site (a non-root
    /// alias) owes one free at the scope's end — through its CURRENT
    /// register at the emit site, which is null exactly where a move
    /// or an inner free already disposed of it. The old compile-time
    /// refusal (a deep-free subtree still read by an outer lineage
    /// borrow) is now a keep-classification: the outer borrowers'
    /// registers ride glm_tbl_free_except, so the base drops around
    /// the borrowed rows instead of refusing the drop. The
    /// return-path planner reuses this mid-walk (a `return` exits
    /// every enclosing scope at once, so their frees are owed right
    /// there); the natural-exit decisions and it share one
    /// computation.
    ///
    /// Plan a mixed binding's free from its provenance: per-arm gates
    /// over the join's arm shapes (an if join) or the call's per-return
    /// edge shapes. Provenance that fails validation (the recorded
    /// arms no longer union to the binding's aliases — the shape moved
    /// since the join) falls back to the union plan.
    fn plan_mixed(&mut self, name: &str, depth: usize, shape: &TableShape) -> MixedPlan {
        use super::facts::TagSrc;
        let ghosts: Vec<usize> = shape.aliases.iter().copied().filter(is_ghost).collect();
        let sites: Vec<usize> = shape
            .aliases
            .iter()
            .copied()
            .filter(|s| !is_root(s) && !is_ghost(s))
            .collect();
        if ghosts.is_empty() || sites.is_empty() {
            return MixedPlan::NotMixed;
        }
        let union_housed = shape.lineage.keys().any(|o| {
            origin_housed(
                &self.walk.scopes,
                &self.lattice,
                &self.own,
                &self.reads,
                *o,
                depth,
                name,
            )
        });
        let fallback = |an: &Analyzer| -> MixedPlan {
            if union_housed {
                MixedPlan::Suppress
            } else {
                MixedPlan::Fire {
                    keeps: an.arm_keeps_union(name, depth, shape),
                }
            }
        };
        let Some(prov) = self
            .walk
            .prov
            .get(depth)
            .and_then(|s| s.get(name))
            .copied()
            .flatten()
        else {
            return fallback(self);
        };
        let arms: Vec<TableShape> = match prov {
            Prov::If(stmt) => self
                .join_arm_shapes
                .get(&(stmt, name.to_string()))
                .map(|(t, e)| vec![t.clone(), e.clone()]),
            Prov::Call(expr) => self.fn_ret_edge_shapes.get(&expr).cloned(),
        }
        .unwrap_or_default();
        let union: BTreeSet<usize> = arms
            .iter()
            .flat_map(|a| a.aliases.iter().copied())
            .filter(|s| !is_root(s))
            .collect();
        let mine: BTreeSet<usize> = shape.aliases.iter().copied().filter(|s| !is_root(s)).collect();
        if arms.is_empty() || union != mine {
            return fallback(self);
        }
        let defer: Vec<bool> = arms
            .iter()
            .map(|a| self.plan_arm(name, depth, a))
            .collect();
        if defer.iter().all(|&d| d) {
            return MixedPlan::Suppress;
        }
        let keeps: Vec<String> = arms
            .iter()
            .zip(defer.iter())
            .filter(|(_, d)| !**d)
            .flat_map(|(a, _)| self.arm_keeps(name, depth, a))
            .collect();
        if defer.iter().all(|&d| !d) {
            return MixedPlan::Fire { keeps };
        }
        match prov {
            Prov::If(stmt) => {
                self.join_tags.insert(stmt, defer[0]);
                MixedPlan::ArmSplit {
                    tag: TagSrc::If(stmt),
                    nothing_on: defer[0],
                    keeps,
                }
            }
            Prov::Call(expr) => {
                // Tag semantics: true = a deferring return edge ran
                // (the lowerer appends the fallthrough edge as false —
                // a null register frees nothing either way).
                self.call_tags.insert(expr, defer.clone());
                MixedPlan::ArmSplit {
                    tag: TagSrc::Call(expr),
                    nothing_on: true,
                    keeps,
                }
            }
        }
    }

    fn arm_keeps_union(&self, name: &str, depth: usize, shape: &TableShape) -> Vec<String> {
        let mut release: BTreeSet<usize> = BTreeSet::new();
        for &r in shape.aliases.iter().filter(|s| !is_root(s)) {
            release.extend(release_set(&self.lattice, &self.own, &self.reads, r));
        }
        borrowers_of(
            &self.walk.scopes,
            &release,
            self.walk.scopes.len(),
            depth,
            name,
        )
    }

    fn compute_scope_frees(
        &mut self,
        depth: usize,
        split: bool,
    ) -> Result<
        (
            Vec<super::facts::DoExitFree>,
            Vec<super::facts::ArmFreeEntry>,
        ),
        ShapeError,
    > {
        let dying = self.walk.scopes[depth].clone();
        let mut arm_frees: Vec<super::facts::ArmFreeEntry> = Vec::new();

        // Pass 1: classify every dying binding. Gate A (a housed
        // lineage origin) suppresses a row-owning binding entirely —
        // the origin's own death composts the row, and an earlier free
        // would leave the base walking freed memory; a MIXED binding
        // (ghost + real site) suppresses under the same gate because
        // the join register holds either arm's value, so no single
        // free is sound for both arms.
        struct Batch {
            name: String,
            sites: Vec<usize>,
            ghosts: Vec<usize>,
            housed: bool,
        }
        let mut batch: Vec<Batch> = Vec::new();
        // Fire-planned mixed bindings' union keeps, replayed onto their
        // entries below (per-register dedup at emission keeps one
        // free).
        let mut fired_keeps: Vec<(String, Vec<String>)> = Vec::new();
        for (name, shape) in &dying {
            let roots: Vec<usize> = shape
                .aliases
                .iter()
                .copied()
                .filter(|site| !is_root(site))
                .collect();
            if roots.is_empty() {
                continue;
            }
            let ghosts: Vec<usize> = roots.iter().copied().filter(is_ghost).collect();
            let sites: Vec<usize> = roots.iter().copied().filter(|s| !is_ghost(s)).collect();
            // A mixed binding under a usable join tag plans per arm:
            // the tag-gated branch frees the ctor arm's header through
            // the faithful register while the row arm defers under its
            // live base (the else-arm leak fix). Without provenance the
            // union fallback below stands (suppress under a housed
            // origin).
            if split && !ghosts.is_empty() && !sites.is_empty() {
                match self.plan_mixed(name, depth, shape) {
                    MixedPlan::NotMixed => {}
                    MixedPlan::Suppress => {
                        signal!(true, trace::TRACE_ROW_GHOST_DEFER);
                        batch.push(Batch {
                            name: name.clone(),
                            sites,
                            ghosts,
                            housed: true,
                        });
                        continue;
                    }
                    MixedPlan::Fire { keeps } => {
                        signal!(true, trace::TRACE_ROW_GHOST_FIRE);
                        batch.push(Batch {
                            name: name.clone(),
                            sites,
                            ghosts,
                            housed: false,
                        });
                        fired_keeps.push((name.clone(), keeps));
                        continue;
                    }
                    MixedPlan::ArmSplit {
                        tag,
                        nothing_on,
                        keeps,
                    } => {
                        signal!(true, trace::TRACE_JOIN_ARM_SPLIT);
                        arm_frees.push(super::facts::ArmFreeEntry {
                            carrier: name.clone(),
                            tag,
                            nothing_on,
                            keeps: keeps.into_iter().map(super::facts::Keep::Name).collect(),
                        });
                        // The binding's sites release on their arm;
                        // batch coverage below counts them all the same
                        // (conservative coverage defers, never
                        // double-frees).
                        batch.push(Batch {
                            name: name.clone(),
                            sites,
                            ghosts,
                            housed: true,
                        });
                        continue;
                    }
                }
            }
            let housed = !ghosts.is_empty()
                && shape.lineage.keys().any(|o| {
                    origin_housed(
                        &self.walk.scopes[..depth],
                        &self.lattice,
                        &self.own,
                        &self.reads,
                        *o,
                        depth,
                        name,
                    )
                });
            if housed {
                signal!(true, trace::TRACE_ROW_GHOST_DEFER);
            }
            batch.push(Batch {
                name: name.clone(),
                sites,
                ghosts,
                housed,
            });
        }

        // Pass 2: batch coverage. Site entries release their whole
        // subtrees (keep-style entries still release their shells and
        // every non-kept row), so a ghost whose origin is covered by
        // one defers — and its binding suppresses entirely, because
        // the shared join register would free the row arm the covering
        // entry already releases. Surviving the batch, a ghost fires
        // — borrow-aware: live bindings whose rows sit inside its
        // release ride its keep list, so a parent row drops around
        // its borrowed children instead of composting them (the
        // chained-borrow fix: a ghost's release is invisible to
        // deepfree, but every borrower NAMES the ghost in its
        // lineage).
        let covered: BTreeSet<usize> = batch
            .iter()
            .filter(|b| !b.housed)
            .flat_map(|b| b.sites.iter().copied())
            .flat_map(|site| release_set(&self.lattice, &self.own, &self.reads, site))
            .collect();
        let covered_flags: Vec<bool> = batch
            .iter()
            .map(|b| {
                b.housed
                    || b.ghosts.iter().any(|_g| {
                        let shape = &dying[&b.name];
                        shape.lineage.keys().any(|o| {
                            !shape.aliases.contains(o)
                                && ghost_origin_covered(&self.lattice, &covered, *o)
                        })
                    })
            })
            .collect();

        let mut frees: Vec<super::facts::DoExitFree> = Vec::new();
        for (i, b) in batch.iter().enumerate() {
            if covered_flags[i] {
                if !b.housed {
                    signal!(true, trace::TRACE_ROW_GHOST_DEFER);
                }
                continue;
            }
            let forced: Option<Vec<String>> = fired_keeps
                .iter()
                .find(|(n, _)| n == &b.name)
                .map(|(_, k)| k.clone());
            for site in &b.sites {
                // The origin-side obligation: housed rows reached only
                // by birth ride the keep list as their ghosts (the
                // store's captured register), and borrowers resolve
                // through the harm set (origin chains included).
                let (spare, _) = self.housed_in_release(*site);
                let harm = self.harm_set(*site);
                let mut keeps: Vec<super::facts::Keep> = match &forced {
                    Some(k) => k.iter().map(|n| super::facts::Keep::Name(n.clone())).collect(),
                    None => borrowers_of(&self.walk.scopes, &harm, depth, depth, &b.name)
                        .into_iter()
                        .map(super::facts::Keep::Name)
                        .collect(),
                };
                if !spare.is_empty() {
                    signal!(true, trace::TRACE_ROW_GHOST_KEEP);
                    keeps.extend(spare.iter().map(|&g| super::facts::Keep::Ghost(g)));
                }
                if keeps.iter().any(|k| !matches!(k, super::facts::Keep::Ghost(_))) {
                    signal!(true, trace::TRACE_ROW_KEEP_FREE);
                }
                self.mark_claimed(*site, &keeps);
                frees.push(super::facts::DoExitFree {
                    site: *site,
                    carrier: b.name.clone(),
                    keeps,
                });
            }
            for ghost in &b.ghosts {
                let harm = self.harm_set(*ghost);
                let keeps: Vec<super::facts::Keep> = match &forced {
                    Some(k) => k.iter().map(|n| super::facts::Keep::Name(n.clone())).collect(),
                    None => borrowers_of(
                        &self.walk.scopes,
                        &harm,
                        self.walk.scopes.len(),
                        depth,
                        &b.name,
                    )
                    .into_iter()
                    .map(super::facts::Keep::Name)
                    .collect(),
                };
                if !keeps.is_empty() {
                    signal!(true, trace::TRACE_ROW_KEEP_FREE);
                } else {
                    signal!(true, trace::TRACE_ROW_GHOST_FIRE);
                }
                frees.push(super::facts::DoExitFree {
                    site: *ghost,
                    carrier: b.name.clone(),
                    keeps,
                });
            }
        }
        Ok((frees, arm_frees))
    }

    fn decide_scope_exit(&mut self, rec: Gate, key: (*const Stmt, u8)) -> Result<(), ShapeError> {
        if !rec.on() {
            return Ok(());
        }

        let depth = self.walk.scopes.len() - 1;
        let (frees, arms) = self.scope_exit_frees(rec, depth, key)?;
        self.own.do_exit_frees.insert(key, frees);
        if !arms.is_empty() {
            self.own.arm_do_exit_frees.insert(key, arms);
        }
        Ok(())
    }

    /// The heap sites a returned table value may point INTO at
    /// runtime: the named binding it is (its aliases) or whose rows it
    /// reads (its lineage origins — `local b = a[0]; return b` hands
    /// out a row of `a`), the base chain of a row it returns a cell
    /// of, or a call result's reach (the body's def-site reach, plus
    /// bare-return argument positions flowing the arg's own reach out
    /// through the chain). Every site here rides the KEEP-free at the
    /// return: the deep free minus the value's subtree, so shells and
    /// sibling rows release while the handed-out table survives. Pure
    /// lookups only — the value's inference already ran.
    fn ret_reach_sites(&self, e: &Expr, out: &mut BTreeSet<usize>) {
        match e {
            Expr::Identifier(n) => {
                if let Ok((_, bind)) = self.resolve(None, n) {
                    out.extend(bind.aliases.iter().copied().filter(|s| !is_root(s)));
                    // Lineage origins: tables whose rows this binding's
                    // current value was read out of. Without them, a row
                    // bound to a name (`local b = a[0]`) would return
                    // with an empty reach — the plain batch would
                    // deep-free `a` underneath the row being handed to
                    // the host.
                    out.extend(bind.lineage.keys().copied().filter(|s| !is_root(s)));
                }
            }
            Expr::Index { obj, .. } => self.ret_reach_sites(obj, out),
            Expr::Call { callee, args } => {
                if let Expr::Identifier(name) = callee.as_ref()
                    && let Some(fn_ptr) = self.resolve_fn(name)
                {
                    if let Some(reach) = self.fn_ret_reach.get(&fn_ptr) {
                        out.extend(reach.iter().copied());
                    }
                    if let Some(rets) = self.fn_ret_params.get(&fn_ptr) {
                        for &i in rets {
                            if let Some(a) = args.get(i) {
                                self.ret_reach_sites(a, out);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// The statement-born ctor sites in a returned table value's
    /// index-base spine — a constructor temp or inlined call result
    /// indexed on the spot (`return ({..})[k]`, `return f()[k]`).
    /// They own no carrier, so they cannot ride the crossed-scope
    /// keep list; they free keep-style from their birth registers at
    /// the return, the handed-out row surviving inside them. Key
    /// positions are excluded: a key dies plainly with the statement.
    fn ret_base_temp_sites(&self, e: &Expr, out: &mut Vec<usize>) {
        if let Expr::Index { obj, .. } = e {
            match obj.as_ref() {
                Expr::TableCtor(_) => {
                    if let Some(&site) = self.lattice.sites.get(&(obj.as_ref() as *const Expr)) {
                        out.push(site);
                    }
                }
                Expr::Call { callee, .. } => {
                    if let Expr::Identifier(name) = callee.as_ref()
                        && let Some(fn_ptr) = self.resolve_fn(name)
                        && let Some(shape) = self.fn_ret_shapes.get(&fn_ptr)
                    {
                        out.extend(
                            shape
                                .aliases
                                .iter()
                                .copied()
                                .filter(|s| !is_root(s) && !is_ghost(s)),
                        );
                    }
                }
                _ => {}
            }
            self.ret_base_temp_sites(obj, out);
        }
    }

    /// The return-path free plan for one `return` statement: a return
    /// exits every enclosing scope in one jump, so each crossed scope
    /// owes its exit frees right there — the scopes between this
    /// return and its context (block scopes, the inline body's own
    /// scope, and at the boundary the root chunk, whose frees the
    /// natural tail emission would otherwise place on a path the jump
    /// never reaches). Entries whose site sits in the value's
    /// read-reach ride the KEEP list instead: freed through the same
    /// carrier with the value's own subtree skipped (the runtime
    /// compares pointers at every depth), so `return x[k]` releases
    /// the base shell and sibling rows while the handed-out row
    /// survives for its receiver — and `return t` degenerates to a
    /// runtime no-op (the keep IS the table). A scalar-valued return
    /// references nothing, so its table-typed sub-evaluations (an
    /// index base, a read-only call argument) plan as dying statement
    /// temps; a bare return keeps nothing.
    fn plan_return_exits(&mut self, value: Option<(&Expr, &Ty)>, reach: &BTreeSet<usize>, stmt: *const Stmt) {
        let entry = self.fn_scope_entries.last().copied().unwrap_or(0);
        // At the boundary return nothing outlives the program: every
        // borrower dies with the unwind, so sparing rows for them
        // would leak — keeps strip here and the crossed batches free
        // plain (the ghosts' own batch gates still defer what another
        // entry releases). Inside an inline body the caller's
        // borrowers live on past the return, so keeps stay.
        let boundary = self.fn_scope_entries.is_empty();
        let mut plain: Vec<super::facts::DoExitFree> = Vec::new();
        let mut keeps: Vec<super::facts::DoExitFree> = Vec::new();
        for depth in (entry..self.walk.scopes.len()).rev() {
            // A refusal here is the natural-exit decision's to report:
            // that decision still runs at the scope's syntactic end
            // and fails the build on the same conflict.
            if let Ok((frees, _)) = self.compute_scope_frees(depth, false) {
                for mut f in frees {
                    if boundary {
                        f.keeps.clear();
                    }
                    if reach.contains(&f.site) {
                        keeps.push(f);
                    } else {
                        plain.push(f);
                    }
                }
            }
        }
        plain.sort_by_key(|f| (f.site, f.carrier.clone()));
        plain.dedup_by_key(|f| (f.site, f.carrier.clone()));
        if !plain.is_empty() {
            for _ in &plain {
                signal!(true, trace::TRACE_RET_PATH_FREE_SITE);
            }
            self.own.ret_path_frees.insert(stmt, plain);
        }
        keeps.sort_by_key(|f| (f.site, f.carrier.clone()));
        keeps.dedup_by_key(|f| (f.site, f.carrier.clone()));
        if !keeps.is_empty() {
            for _ in &keeps {
                signal!(true, trace::TRACE_RET_KEEP_FREE_SITE);
            }
            self.own.ret_path_keep_frees.insert(stmt, keeps);
        }
        // The value's spine-born bases — ctor temps and call results
        // indexed on the spot — free keep-style from their birth
        // registers (no carrier exists for them).
        if let Some((v, ty)) = value
            && matches!(ty, Tbl(_) | Pending)
        {
            let mut keep_temps = Vec::new();
            self.ret_base_temp_sites(v, &mut keep_temps);
            keep_temps.sort_unstable();
            keep_temps.dedup();
            self.prune_deepfree_children(&mut keep_temps);
            if !keep_temps.is_empty() {
                signal!(true, trace::TRACE_RET_KEEP_FREE_SITE);
                self.own.ret_path_keep_temps.insert(stmt, keep_temps);
            }
        }
        // A scalar return value: every table consumed on the way to it
        // dies at the statement (an int copied out of a ctor is not the
        // ctor). A table value's own spine is skipped — it may be the
        // very table being handed out.
        if let Some((v, ty)) = value
            && !matches!(ty, Tbl(_) | Pending)
        {
            let mut sites = Vec::new();
            self.collect_temp_sites(v, Fate::Escapes, &mut sites);
            self.prune_deepfree_children(&mut sites);
            if !sites.is_empty() {
                signal!(true, trace::TRACE_STMT_TEMP_FREE_SITE);
                self.own.stmt_temp_frees.insert(stmt, sites);
            }
        }
    }

    /// The root chunk's exit pass: the same composition as every block
    /// scope, over the outermost scope, recorded once after the
    /// recording walk. No outer scopes exist, so the lineage refusal
    /// has nothing to check against — top-level frees are always
    /// granted.
    fn decide_root_exit(&mut self, rec: Gate) -> Result<(), ShapeError> {
        if !rec.on() {
            return Ok(());
        }

        let (frees, arms) = self.scope_exit_frees(rec, 0, (std::ptr::null_mut(), 0))?;
        for _ in &frees {
            signal!(rec.on(), trace::TRACE_ROOT_EXIT_FREE_SITE);
        }
        self.own.root_exit_frees = frees;
        self.own.arm_root_exit_frees = arms;
        Ok(())
    }

    // === statement temporaries ===

    /// Drop every site another site in the batch owns: a dying parent
    /// deep-frees its subtree at runtime, so freeing a child too would
    /// release its rows twice.
    fn prune_deepfree_children(&self, sites: &mut Vec<usize>) {
        let mut children: BTreeSet<usize> = BTreeSet::new();
        for &site in sites.iter() {
            children.extend(deepfree_children(&self.lattice, &self.own, &self.reads, site));
        }
        sites.retain(|s| !children.contains(s));
    }

    /// The statement-temporary free plan: for every statement, the
    /// constructor sites born inside it (or handed out of an inlined
    /// call) whose value is only read through — an index base, an
    /// operand, a print row, a read-only call argument. No binding
    /// ever owns such a site, so no scope-exit pass can reach it; the
    /// lowerer frees each from its birth register at the end of the
    /// consuming statement. Every ownership-transferring position is
    /// excluded — binding initializers, index-assign values, return
    /// values, ctor entries of an escaping ctor, and call arguments
    /// whose parameter moves the table or hands it back into a
    /// surviving call result — those sites ride the receiving owner's
    /// machinery instead. Conditions of while/if skip the plan (a
    /// loop's condition re-evaluates per iteration; the branch join
    /// has no single birth block), and so do `and`/`or` subtrees
    /// (short-circuit: the free would run where the birth did not).
    fn plan_stmt_temp_frees(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            let mut sites = Vec::new();
            match s {
                Stmt::Print { exprs } => {
                    for e in exprs {
                        self.collect_temp_sites(e, Fate::Dies, &mut sites);
                    }
                }
                Stmt::Expr { expr } => {
                    self.collect_temp_sites(expr, Fate::Dies, &mut sites);
                }
                Stmt::LocalDecl { exprs, .. } => {
                    for e in exprs {
                        self.collect_temp_sites(e, Fate::Escapes, &mut sites);
                    }
                }
                Stmt::Assignment { expr, .. } => {
                    self.collect_temp_sites(expr, Fate::Escapes, &mut sites);
                }
                Stmt::IndexAssign { obj, key, value } => {
                    self.collect_temp_sites(obj, Fate::Dies, &mut sites);
                    self.collect_temp_sites(key, Fate::Dies, &mut sites);
                    self.collect_temp_sites(value, Fate::Escapes, &mut sites);
                }
                // The returned value escapes — to the host at the
                // boundary, to the caller's flow at an inline site.
                Stmt::Return { .. } => {}
                Stmt::While { condition, body } => {
                    self.plan_cond_temp_frees(condition, s);
                    self.plan_stmt_temp_frees(body);
                }
                Stmt::Do { body } => {
                    self.plan_stmt_temp_frees(body)
                }
                Stmt::If {
                    condition,
                    then_body,
                    else_body,
                } => {
                    self.plan_cond_temp_frees(condition, s);
                    self.plan_stmt_temp_frees(then_body);
                    self.plan_stmt_temp_frees(else_body);
                }
            }
            if !sites.is_empty() {
                // One birth per site, and a dying parent deep-frees its
                // subtree at runtime — drop every site another dying
                // site owns, or the batch would free rows twice.
                self.prune_deepfree_children(&mut sites);
                if !sites.is_empty() {
                    signal!(true, trace::TRACE_STMT_TEMP_FREE_SITE);
                    self.own
                        .stmt_temp_frees
                        .insert(s as *const Stmt, sites);
                }
            }
            // Function bodies plan their own statements — they lower at
            // their inline sites, where the same plan applies verbatim.
            // Short-circuit arms plan wherever an `and`/`or` lowers.
            self.plan_fn_bodies_in_stmt(s);
            self.plan_arm_temp_frees_in_stmt(s);
        }
    }

    /// The condition-temporary plan for one if/while: the ctor sites
    /// born evaluating the condition, freed where the condition value
    /// is consumed (the lowerer's emission point differs per shape —
    /// once for an if, per iteration for a while's header, once in
    /// the pre-header for the reserved-loop bound). `and`/`or`
    /// subtrees self-exclude from the collection: their operand
    /// births are path-dependent, so the arm plan owns them instead.
    fn plan_cond_temp_frees(&mut self, cond: &Expr, stmt: &Stmt) {
        let mut sites = Vec::new();
        self.collect_temp_sites(cond, Fate::Dies, &mut sites);
        self.prune_deepfree_children(&mut sites);
        if !sites.is_empty() {
            signal!(true, trace::TRACE_COND_TEMP_FREE_SITE);
            self.own
                .cond_temp_frees
                .insert(stmt as *const Stmt, sites);
        }
    }

    /// Walk a statement's expressions planning the short-circuit arm
    /// entries: every `and`/`or` node contributes one entry per
    /// operand (keyed by the operand node), holding the ctor sites
    /// born evaluating it. The left operand always evaluates — its
    /// frees follow its value; the right arm births only on its own
    /// path, so its frees land inside the evaluated arm block. An
    /// operand that is itself an `and`/`or` collects nothing (its own
    /// operands carry their own entries), so every site lands in
    /// exactly one entry.
    fn plan_arm_temp_frees_in_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::LocalDecl { exprs, .. } | Stmt::Print { exprs } => {
                for e in exprs {
                    self.plan_arm_temp_frees_in_expr(e);
                }
            }
            Stmt::Assignment { expr, .. } | Stmt::Expr { expr } => {
                self.plan_arm_temp_frees_in_expr(expr)
            }
            Stmt::IndexAssign { obj, key, value } => {
                self.plan_arm_temp_frees_in_expr(obj);
                self.plan_arm_temp_frees_in_expr(key);
                self.plan_arm_temp_frees_in_expr(value);
            }
            Stmt::While { condition, body } => {
                self.plan_arm_temp_frees_in_expr(condition);
                for s in body {
                    self.plan_arm_temp_frees_in_stmt(s);
                }
            }
            Stmt::Do { body } => {
                for s in body {
                    self.plan_arm_temp_frees_in_stmt(s);
                }
            }
            Stmt::If {
                condition,
                then_body,
                else_body,
            } => {
                self.plan_arm_temp_frees_in_expr(condition);
                for s in then_body.iter().chain(else_body) {
                    self.plan_arm_temp_frees_in_stmt(s);
                }
            }
            Stmt::Return { value } => {
                if let Some(e) = value {
                    self.plan_arm_temp_frees_in_expr(e);
                }
            }
        }
    }

    fn plan_arm_temp_frees_in_expr(&mut self, e: &Expr) {
        match e {
            Expr::BinaryOp {
                op: BinOp::And | BinOp::Or,
                left,
                right,
            } => {
                for operand in [left.as_ref(), right.as_ref()] {
                    let mut sites = Vec::new();
                    self.collect_temp_sites(operand, Fate::Dies, &mut sites);
                    self.prune_deepfree_children(&mut sites);
                    if !sites.is_empty() {
                        signal!(true, trace::TRACE_ARM_TEMP_FREE_SITE);
                        self.own
                            .arm_temp_frees
                            .insert(operand as *const Expr, sites);
                    }
                }
                self.plan_arm_temp_frees_in_expr(left);
                self.plan_arm_temp_frees_in_expr(right);
            }
            Expr::TableCtor(entries) => {
                for (k, v) in entries {
                    if let CtorKey::Expr(ke) = k {
                        self.plan_arm_temp_frees_in_expr(ke);
                    }
                    self.plan_arm_temp_frees_in_expr(v);
                }
            }
            Expr::Index { obj, key } => {
                self.plan_arm_temp_frees_in_expr(obj);
                self.plan_arm_temp_frees_in_expr(key);
            }
            Expr::BinaryOp { left, right, .. } => {
                self.plan_arm_temp_frees_in_expr(left);
                self.plan_arm_temp_frees_in_expr(right);
            }
            Expr::UnaryOp { expr, .. } => self.plan_arm_temp_frees_in_expr(expr),
            Expr::Call { callee, args } => {
                self.plan_arm_temp_frees_in_expr(callee);
                for a in args {
                    self.plan_arm_temp_frees_in_expr(a);
                }
            }
            _ => {}
        }
    }

    /// The ctor sites (and inlined call-result sites) a sub-expression
    /// leaves dying at `fate`. Identifier contributions are excluded
    /// everywhere: a name's table is owned by the name's scope
    /// machinery, never by the statement reading it.
    fn collect_temp_sites(&self, e: &Expr, fate: Fate, out: &mut Vec<usize>) {
        match e {
            Expr::TableCtor(entries) => {
                if fate == Fate::Dies
                    && let Some(&site) = self.lattice.sites.get(&(e as *const Expr))
                {
                    out.push(site);
                }
                for (_, v) in entries {
                    self.collect_temp_sites(v, fate, out);
                }
            }
            Expr::Call { callee, args } => {
                if let Expr::Identifier(name) = callee.as_ref()
                    && let Some(fn_ptr) = self.resolve_fn(name)
                {
                    if fate == Fate::Dies
                        && let Some(shape) = self.fn_ret_shapes.get(&fn_ptr)
                    {
                        // Ghosts stay out of the temp channels: a temp
                        // frees from a ctor site's birth register, and
                        // a row ghost has none — its row stays housed
                        // by the base the inline return's keep-free
                        // already honored.
                        out.extend(
                            shape
                                .aliases
                                .iter()
                                .copied()
                                .filter(|s| !is_root(s) && !is_ghost(s)),
                        );
                    }
                    let empty = BTreeSet::new();
                    let empty_idx = BTreeSet::new();
                    let moved = self.fn_moved_params.get(&fn_ptr).unwrap_or(&empty);
                    let rets = self.fn_ret_params.get(&fn_ptr).unwrap_or(&empty_idx);
                    let params = self.fn_params.get(&fn_ptr).cloned().unwrap_or_default();
                    for (i, a) in args.iter().enumerate() {
                        // How the argument leaves the statement, by
                        // what the body does with its parameter:
                        // a MOVED param transfers the table to its
                        // receiver (the receiver's owner deep-frees it
                        // — the statement never touches it again); a
                        // bare-RETURNED param flows the value out
                        // through the call result, so it dies here
                        // exactly when the call's own value dies and
                        // the argument isn't a borrowed name; a
                        // STORED-THROUGH param is mutated in place —
                        // no ownership leaves, the table dies with the
                        // statement; everything else is a read, and
                        // reads die.
                        let is_moved = params
                            .get(i)
                            .is_some_and(|p| moved.contains(p));
                        let is_ret = rets.contains(&i);
                        let arg_fate = if is_moved {
                            Fate::Escapes
                        } else if is_ret {
                            if fate == Fate::Dies && !matches!(a, Expr::Identifier(_)) {
                                Fate::Dies
                            } else {
                                Fate::Escapes
                            }
                        } else {
                            Fate::Dies
                        };
                        self.collect_temp_sites(a, arg_fate, out);
                    }
                }
            }
            // The index's own result is a row read or a scalar — the
            // base and key are sub-evaluations, consumed here.
            Expr::Index { obj, key } => {
                self.collect_temp_sites(obj, Fate::Dies, out);
                self.collect_temp_sites(key, Fate::Dies, out);
            }
            // Short-circuit: the right operand's block may never run,
            // so a statement-end free would fire where no birth did.
            Expr::BinaryOp { op: BinOp::And | BinOp::Or, .. } => {}
            Expr::BinaryOp { left, right, .. } => {
                self.collect_temp_sites(left, Fate::Dies, out);
                self.collect_temp_sites(right, Fate::Dies, out);
            }
            Expr::UnaryOp { expr, .. } => self.collect_temp_sites(expr, Fate::Dies, out),
            // A function value owns nothing; its body plans separately.
            Expr::Function { .. } => {}
            Expr::Integer(_) | Expr::Float(_) | Expr::Boolean(_) | Expr::String(_)
            | Expr::Nil | Expr::Identifier(_) | Expr::SysAllocCount => {}
        }
    }

    /// Descend a statement's expressions, planning the statements of
    /// every function body found (their constructors need the plan
    /// wherever the body inlines). Statement lists are NOT recursed
    /// here — plan_stmt_temp_frees already recurses them and calls
    /// this per statement, so only the statement's own expressions
    /// (conditions included) carry function values to find.
    fn plan_fn_bodies_in_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::LocalDecl { exprs, .. } | Stmt::Print { exprs } => {
                for e in exprs {
                    self.plan_fn_bodies_in_expr(e);
                }
            }
            Stmt::Assignment { expr, .. } | Stmt::Expr { expr } => {
                self.plan_fn_bodies_in_expr(expr)
            }
            Stmt::IndexAssign { obj, key, value } => {
                self.plan_fn_bodies_in_expr(obj);
                self.plan_fn_bodies_in_expr(key);
                self.plan_fn_bodies_in_expr(value);
            }
            Stmt::While { condition, .. } => self.plan_fn_bodies_in_expr(condition),
            Stmt::If { condition, .. } => self.plan_fn_bodies_in_expr(condition),
            Stmt::Do { .. } => {}
            Stmt::Return { value } => {
                if let Some(e) = value {
                    self.plan_fn_bodies_in_expr(e);
                }
            }
        }
    }

    fn plan_fn_bodies_in_expr(&mut self, e: &Expr) {
        match e {
            Expr::Function { body, .. } => self.plan_stmt_temp_frees(body),
            Expr::TableCtor(entries) => {
                for (k, v) in entries {
                    if let CtorKey::Expr(ke) = k {
                        self.plan_fn_bodies_in_expr(ke);
                    }
                    self.plan_fn_bodies_in_expr(v);
                }
            }
            Expr::Index { obj, key } => {
                self.plan_fn_bodies_in_expr(obj);
                self.plan_fn_bodies_in_expr(key);
            }
            Expr::BinaryOp { left, right, .. } => {
                self.plan_fn_bodies_in_expr(left);
                self.plan_fn_bodies_in_expr(right);
            }
            Expr::UnaryOp { expr, .. } => self.plan_fn_bodies_in_expr(expr),
            Expr::Call { callee, args } => {
                self.plan_fn_bodies_in_expr(callee);
                for a in args {
                    self.plan_fn_bodies_in_expr(a);
                }
            }
            _ => {}
        }
    }

    fn expr_named_sites(&self, rec: Gate, expr: &Expr, out: &mut BTreeSet<usize>) {
        match expr {
            Expr::Identifier(n) => {
                if let Ok((_, b)) = self.resolve(rec, n) {
                    out.extend(b.aliases.iter().copied().filter(|&s| !is_root(&s)));
                }
            }
            Expr::TableCtor(entries) => {
                for (_, e) in entries {
                    self.expr_named_sites(rec, e, out);
                }
            }
            _ => {}
        }
    }

    fn drop_reference(&mut self, rec: Gate, name: &str, stmt: &Stmt) -> Result<(), ShapeError> {
        let (depth, bind) = self.resolve(rec, name)?;
        let roots: Vec<usize> = bind
            .aliases
            .iter()
            .copied()
            .filter(|r| !is_root(r))
            .collect();
        if !roots.is_empty() && rec.on() {
            // A mixed binding plans per arm first: under a usable join
            // tag the drop branches — the row arm defers, the ctor arm
            // frees the faithful register.
            let mixed_ghost = roots.iter().any(is_ghost);
            let mixed_site = roots.iter().any(|s| !is_ghost(s));
            if mixed_ghost && mixed_site {
                match self.plan_mixed(name, depth, &bind) {
                    MixedPlan::NotMixed => {}
                    MixedPlan::Suppress => {
                        signal!(rec.on(), trace::TRACE_ROW_GHOST_DEFER);
                    }
                    MixedPlan::Fire { keeps } => {
                        signal!(rec.on(), trace::TRACE_ROW_GHOST_FIRE);
                        self.own.free_sites.insert(stmt as *const Stmt);
                        if !keeps.is_empty() {
                            signal!(rec.on(), trace::TRACE_ROW_KEEP_FREE);
                            self.own.free_keeps.insert(
                                stmt as *const Stmt,
                                keeps.into_iter().map(super::facts::Keep::Name).collect(),
                            );
                        }
                        signal!(rec.on(), trace::TRACE_SHAPE_DROP);
                    }
                    MixedPlan::ArmSplit {
                        tag,
                        nothing_on,
                        keeps,
                    } => {
                        signal!(rec.on(), trace::TRACE_JOIN_ARM_SPLIT);
                        self.own.arm_free_sites.entry(stmt as *const Stmt).or_default().push(
                            super::facts::ArmFreeEntry {
                                carrier: name.to_string(),
                                tag,
                                nothing_on,
                                keeps: keeps.into_iter().map(super::facts::Keep::Name).collect(),
                            },
                        );
                        signal!(rec.on(), trace::TRACE_SHAPE_DROP);
                    }
                }
                self.walk.scopes[depth].insert(
                    name.to_string(),
                    TableShape {
                        ty: Pending,
                        layout: LayoutVerdict::default(),
                        aliases: BTreeSet::from([NULL_ROOT]),
                        lineage: BTreeMap::new(),
                    },
                );
                if let Some(scope) = self.walk.prov.get_mut(depth) {
                    scope.remove(name);
                }
                return Ok(());
            }
            let has_ghost = mixed_ghost;
            // Gate A — the row gate: a binding holding a row (a ghost
            // in its aliases) frees it only once no live binding houses
            // a lineage origin — the base's deep free is the row's
            // natural death, and freeing earlier would leave the base
            // walking freed memory. With the base gone (an inlined
            // body's keep-free already released it), the row is
            // ownerless and THIS free is its only death.
            let origins_housed = bind.lineage.keys().any(|o| {
                origin_housed(
                    &self.walk.scopes,
                    &self.lattice,
                    &self.own,
                    &self.reads,
                    *o,
                    depth,
                    name,
                )
            });
            if has_ghost {
                if origins_housed {
                    signal!(rec.on(), trace::TRACE_ROW_GHOST_DEFER);
                } else {
                    signal!(rec.on(), trace::TRACE_ROW_GHOST_FIRE);
                }
            }
            // The old borrow-conflict veto is a keep-classification
            // now: instead of refusing the drop while another binding
            // reads a row out of this subtree, the free spares every
            // borrower's register — the base drops around the
            // borrowed rows, and each borrower's own gate frees its
            // row later through its faithful register.
            if !origins_housed {
                // Ghost-aware release and harm (a stored row's birth
                // side spares it; borrowers resolve through origin
                // chains), with the origin-side Ghost keeps.
                let mut release: BTreeSet<usize> = BTreeSet::new();
                let mut keeps: Vec<super::facts::Keep> = Vec::new();
                for &r in &roots {
                    release.extend(self.node_release(r));
                }
                let mut harm = release.clone();
                for &r in &roots {
                    let h = self.harm_set(r);
                    harm.extend(h);
                }
                keeps.extend(
                    borrowers_of(
                        &self.walk.scopes,
                        &harm,
                        self.walk.scopes.len(),
                        depth,
                        name,
                    )
                    .into_iter()
                    .map(super::facts::Keep::Name),
                );
                for &r in &roots {
                    let (spare, _) = self.housed_in_release(r);
                    if !spare.is_empty() {
                        signal!(rec.on(), trace::TRACE_ROW_GHOST_KEEP);
                        keeps.extend(spare.iter().map(|&g| super::facts::Keep::Ghost(g)));
                    }
                }
                if keeps
                    .iter()
                    .any(|k| !matches!(k, super::facts::Keep::Ghost(_)))
                {
                    signal!(rec.on(), trace::TRACE_ROW_KEEP_FREE);
                }
                self.own.free_sites.insert(stmt as *const Stmt);
                if !keeps.is_empty() {
                    self.own.free_keeps.insert(stmt as *const Stmt, keeps.clone());
                }
                for &r in &roots {
                    self.mark_claimed(r, &keeps);
                }
                signal!(rec.on(), trace::TRACE_SHAPE_DROP);
            }
        }
        self.walk.scopes[depth].insert(
            name.to_string(),
            TableShape {
                ty: Pending,
                layout: LayoutVerdict::default(),
                aliases: BTreeSet::from([NULL_ROOT]),
                lineage: BTreeMap::new(),
            },
        );
        if let Some(scope) = self.walk.prov.get_mut(depth) {
            scope.remove(name);
        }
        Ok(())
    }

    fn rebind_death(
        &mut self,
        rec: Gate,
        name: &str,
        _depth: usize,
        stmt: &Stmt,
        old: &TableShape,
        incoming: &BTreeSet<usize>,
    ) -> Result<(), ShapeError> {
        let died: Vec<usize> = old
            .aliases
            .iter()
            .copied()
            .filter(|&s| !is_root(&s) && !incoming.contains(&s))
            .collect();
        if died.is_empty() {
            return Ok(());
        }
        if !rec.on() {
            return Ok(());
        }
        // A mixed displaced value plans per arm first: under a usable
        // join tag the displaced free branches — the row arm defers,
        // the ctor arm frees the pre-update register.
        let m_ghost = died.iter().any(is_ghost);
        let m_site = died.iter().any(|s| !is_ghost(s));
        if m_ghost && m_site {
            match self.plan_mixed(name, _depth, old) {
                MixedPlan::NotMixed => {}
                MixedPlan::Suppress => {
                    signal!(rec.on(), trace::TRACE_ROW_GHOST_DEFER);
                    return Ok(());
                }
                MixedPlan::Fire { keeps } => {
                    signal!(rec.on(), trace::TRACE_ROW_GHOST_FIRE);
                    self.own
                        .rebind_grants
                        .entry(stmt as *const Stmt)
                        .or_default()
                        .insert(
                            name.to_string(),
                            keeps.into_iter().map(super::facts::Keep::Name).collect(),
                        );
                    signal!(rec.on(), trace::TRACE_SHAPE_REBIND_DROP);
                    return Ok(());
                }
                MixedPlan::ArmSplit {
                    tag,
                    nothing_on,
                    keeps,
                } => {
                    signal!(rec.on(), trace::TRACE_JOIN_ARM_SPLIT);
                    self.own
                        .arm_rebind_frees
                        .entry(stmt as *const Stmt)
                        .or_default()
                        .push(super::facts::ArmFreeEntry {
                            carrier: name.to_string(),
                            tag,
                            nothing_on,
                            keeps: keeps.into_iter().map(super::facts::Keep::Name).collect(),
                        });
                    signal!(rec.on(), trace::TRACE_SHAPE_REBIND_DROP);
                    return Ok(());
                }
            }
        }
        // The displaced twin of the drop gate: a displaced row frees
        // only when no live binding houses a lineage origin (Gate A),
        // and otherwise classifies keep-style around its borrowers —
        // the register is faithful, so the grant is sound for whatever
        // the register holds.
        let has_ghost = m_ghost;
        let housed = old.lineage.keys().any(|o| {
            origin_housed(
                &self.walk.scopes,
                &self.lattice,
                &self.own,
                &self.reads,
                *o,
                _depth,
                name,
            )
        });
        if has_ghost {
            if housed {
                signal!(rec.on(), trace::TRACE_ROW_GHOST_DEFER);
                return Ok(());
            }
            signal!(rec.on(), trace::TRACE_ROW_GHOST_FIRE);
        }
        let mut harm: BTreeSet<usize> = BTreeSet::new();
        let mut keeps: Vec<super::facts::Keep> = Vec::new();
        for &d in &died {
            harm.extend(self.node_release(d));
            harm.extend(self.harm_set(d));
        }
        keeps.extend(
            borrowers_of(&self.walk.scopes, &harm, self.walk.scopes.len(), _depth, name)
                .into_iter()
                .map(super::facts::Keep::Name),
        );
        for &d in &died {
            let (spare, _) = self.housed_in_release(d);
            if !spare.is_empty() {
                signal!(rec.on(), trace::TRACE_ROW_GHOST_KEEP);
                keeps.extend(spare.iter().map(|&g| super::facts::Keep::Ghost(g)));
            }
        }
        if keeps
            .iter()
            .any(|k| !matches!(k, super::facts::Keep::Ghost(_)))
        {
            signal!(rec.on(), trace::TRACE_ROW_KEEP_FREE);
        }
        signal!(rec.on(), trace::TRACE_SHAPE_REBIND_DROP);
        for &d in &died {
            self.mark_claimed(d, &keeps);
        }
        self.own
            .rebind_grants
            .entry(stmt as *const Stmt)
            .or_default()
            .insert(name.to_string(), keeps);
        Ok(())
    }

    fn check_table_use(&mut self, rec: Gate, obj: &Expr) -> Result<(), ShapeError> {
        if let Expr::Identifier(name) = obj {
            signal!(rec.on(), trace::TRACE_CHK_TBL_IDENT);
            let (_, bind) = self.resolve(rec, name)?;
            if bind.aliases.contains(&NULL_ROOT) && rec.on() {
                signal!(rec.on(), trace::TRACE_CHK_TBL_NIL);
                return Err(ShapeError(format!(
                    "Lifetime Error: '{name}' may be nil here — table reads and \
                     stores through a possibly-nil name are rejected at compile time"
                )));
            }
            if bind.aliases.contains(&MOVED_ROOT) && rec.on() {
                signal!(rec.on(), trace::TRACE_CHK_TBL_NIL);
                return Err(ShapeError(format!(
                    "Lifetime Error: '{name}' may be moved here — table reads and \
                     stores through a possibly-moved name are rejected at compile time"
                )));
            }
        }
        Ok(())
    }

    fn check_len_operand(&mut self, rec: Gate, expr: &Expr) -> Result<(), ShapeError> {
        let (sites, name, row_read) = match expr {
            Expr::TableCtor(_) => (
                self.lattice
                    .sites
                    .get(&(expr as *const Expr))
                    .into_iter()
                    .copied()
                    .collect::<Vec<_>>(),
                "constructor".to_string(),
                false,
            ),
            Expr::Identifier(name) => {
                let (_, bind) = self.resolve(rec, name)?;
                (
                    bind.aliases
                        .iter()
                        .copied()
                        .filter(|&s| !is_root(&s))
                        .collect(),
                    name.clone(),
                    !bind.lineage.is_empty(),
                )
            }
            _ => {
                return Err(ShapeError(
                    "Type Error: '#' requires a named table or a table constructor".to_string(),
                ));
            }
        };
        if row_read || sites.is_empty() {
            return Err(ShapeError(format!(
                "Type Error: '#' requires a provably dense table — '{name}' has \
                 no compile-time border (a row read out of a table carries none)"
            )));
        }
        for &s in &sites {
            if !self.layout.dense_ctor_len.contains_key(&s)
                || self.reads.user_store_sites.contains(&s)
            {
                return Err(ShapeError(format!(
                    "Type Error: '#' requires a provably dense table — '{name}' has \
                     no compile-time border (stores and sparse constructors make \
                     borders unprovable)"
                )));
            }
        }
        let borders: BTreeSet<i64> = sites
            .iter()
            .filter_map(|s| self.layout.dense_ctor_len.get(s).copied())
            .collect();
        if borders.len() > 1 {
            return Err(ShapeError(format!(
                "Type Error: '#' requires one border — '{name}' may hold tables of \
                 differing lengths"
            )));
        }
        Ok(())
    }

    fn check_uses(&mut self, rec: Gate, expr: &Expr) -> Result<(), ShapeError> {
        match expr {
            Expr::Identifier(name) => {
                if let Ok((_, bind)) = self.resolve(rec, name)
                    && bind.aliases.contains(&NULL_ROOT)
                {
                    signal!(rec.on(), trace::TRACE_CHK_TBL_NIL);
                    return Err(ShapeError(format!(
                        "Lifetime Error: '{name}' may be nil here — reads through a \
                         possibly-nil name are rejected at compile time"
                    )));
                }
                if let Ok((_, bind)) = self.resolve(rec, name)
                    && bind.aliases.contains(&MOVED_ROOT)
                {
                    signal!(rec.on(), trace::TRACE_CHK_TBL_NIL);
                    return Err(ShapeError(format!(
                        "Lifetime Error: '{name}' may be moved here — reads through a \
                         possibly-moved name are rejected at compile time"
                    )));
                }
            }
            Expr::TableCtor(entries) => {
                signal!(rec.on(), trace::TRACE_CHK_USE_TBL);
                for (key, val) in entries {
                    if let CtorKey::Expr(ke) = key {
                        self.check_uses(rec, ke)?;
                    }
                    self.check_uses(rec, val)?;
                }
            }
            Expr::Index { obj, key } => {
                signal!(rec.on(), trace::TRACE_CHK_USE_IDX);
                self.check_table_use(rec, obj)?;
                self.check_uses(rec, obj)?;
                self.check_uses(rec, key)?;
            }
            Expr::UnaryOp { op, expr } => {
                if matches!(op, UnOp::Len) {
                    self.check_table_use(rec, expr)?;
                    self.check_len_operand(rec, expr)?;
                }
                self.check_uses(rec, expr)?;
            }
            Expr::BinaryOp { left, right, .. } => {
                signal!(rec.on(), trace::TRACE_CHK_USE_BINOP);
                self.check_uses(rec, left)?;
                self.check_uses(rec, right)?;
            }
            Expr::Call { args, .. } => {
                // The callee names a function, not a value — reads
                // through it were already ruled out at its binding.
                for a in args {
                    self.check_uses(rec, a)?;
                }
            }
            Expr::Function { .. } => {
                // The body's uses were checked at the def-site walk.
            }
            _ => {}
        }
        Ok(())
    }

    fn check_key_threshold(
        &mut self,
        rec: Gate,
        key: &Expr,
        sites: &BTreeSet<usize>,
    ) -> Result<(), ShapeError> {
        if let Some(i) = const_key_value(key) {
            signal!(rec.on(), trace::TRACE_THRESH_INT);
            if i >= BOUNDS_FAIL_THRESHOLD {
                signal!(rec.on(), trace::TRACE_THRESH_BOUNDS);
                return Err(ShapeError(
                    "Table Bounds Error: table index overflow".to_string(),
                ));
            }
            if i > SPARSE_THRESHOLD {
                signal!(rec.on(), trace::TRACE_THRESH_SPARSE);
                for &site in sites {
                    if !is_root(&site) && !is_ghost(&site) {
                        signal!(rec.on(), trace::TRACE_THRESH_SPARSE_ALIAS);
                        self.layout.verdicts[site] =
                            self.layout.verdicts[site].join(LayoutVerdict::Sparse);
                    }
                }
            }
        }
        Ok(())
    }

    fn detect_fill_loop(&mut self, rec: Gate, guard: &str, body: &[Stmt]) {
        if !guard_provably_nonneg(&self.holds.num_assigns, guard) {
            return;
        }
        if !guard_only_ascends(&self.holds.num_assigns, body, guard) {
            return;
        }

        let mutated = self.collect_mutated_names(rec, body);
        let mut fills = BTreeSet::new();

        fn collect_fills(
            stmts: &[Stmt],
            guard: &str,
            mutated: &BTreeSet<String>,
            fills: &mut BTreeSet<String>,
        ) {
            for stmt in stmts {
                if let Stmt::IndexAssign {
                    obj,
                    key: Expr::Identifier(key_ident),
                    ..
                } = stmt
                    && key_ident == guard
                    && let Some(table_name) = get_base_identifier(obj)
                    && !mutated.contains(table_name)
                {
                    fills.insert(table_name.clone());
                }
                match stmt {
                    Stmt::While { body, .. } | Stmt::Do { body } => {
                        collect_fills(body, guard, mutated, fills)
                    }
                    Stmt::If {
                        then_body,
                        else_body,
                        ..
                    } => {
                        collect_fills(then_body, guard, mutated, fills);
                        collect_fills(else_body, guard, mutated, fills);
                    }
                    _ => {}
                }
            }
        }

        collect_fills(body, guard, &mutated, &mut fills);

        let depth = self.walk.scopes.len();
        for table_name in fills {
            let site_ids: BTreeSet<usize> = self
                .walk
                .scopes
                .iter()
                .rev()
                .filter_map(|s| s.get(&table_name))
                .flat_map(|ts| ts.aliases.iter().copied())
                .collect();

            for &site in &site_ids {
                if !is_root(&site) && !is_ghost(&site) {
                    signal!(rec.on(), trace::TRACE_FILL_LOOP_DENSE);
                    if self.layout.verdicts[site] == LayoutVerdict::Growing {
                        self.layout.verdicts[site] = LayoutVerdict::Dense;
                    }
                }
            }

            self.layout.name_dense.insert((table_name, depth), true);
        }
    }

    fn collect_mutated_names(&self, rec: Gate, stmts: &[Stmt]) -> BTreeSet<String> {
        let mut mutated = BTreeSet::new();
        for s in stmts {
            match s {
                Stmt::Assignment { name, .. } => {
                    signal!(rec.on(), trace::TRACE_MUT_ASSIGN);
                    mutated.insert(name.clone());
                }
                Stmt::While { .. } => {}
                Stmt::Do { .. } => {}
                Stmt::If {
                    then_body,
                    else_body,
                    ..
                } => {
                    signal!(rec.on(), trace::TRACE_MUT_IF);
                    for ss in then_body {
                        if let Stmt::Assignment { name, .. } = ss {
                            signal!(rec.on(), trace::TRACE_MUT_THEN_ASSIGN);
                            mutated.insert(name.clone());
                        }
                    }
                    for ss in else_body {
                        if let Stmt::Assignment { name, .. } = ss {
                            signal!(rec.on(), trace::TRACE_MUT_ELSE_ASSIGN);
                            mutated.insert(name.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        mutated
    }
}
