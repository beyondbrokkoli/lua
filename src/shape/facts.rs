use super::core::LayoutVerdict;
use crate::ast::{Expr, StaticType, Stmt};
use glm_rt::{signal, trace};
use std::collections::{BTreeMap, BTreeSet};

/// One spared subtree at a free point, resolved to a register by the
/// lowerer: `Name` reads a live borrower binding's current register
/// (faithful — null exactly where the borrower itself dropped, which
/// degenerates the keep to a plain free); `Ghost` names a housed row
/// whose pointer a store captured into an SSA register at store time.
#[derive(Clone, PartialEq, Debug)]
pub enum Keep {
    Name(String),
    /// A housed row's store-time value register.
    #[allow(dead_code)]
    Ghost(usize),
}

#[derive(Clone)]
pub struct DoExitFree {
    #[allow(dead_code)]
    pub site: usize,
    pub carrier: String,
    /// The borrower keeps: outer bindings whose registers point into
    /// this free's release set ride glm_tbl_free_except — the base
    /// drops around them instead of refusing the drop.
    pub keeps: Vec<Keep>,
}

/// Which join a mixed binding's per-arm provenance came from: an
/// if/else join (the tag phi: true = the then arm ran) or an inline
/// call's return-edge join (the tag phi: true = a deferring return
/// edge ran). The tag is materialized by the lowerer at the join and
/// read at the free point.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TagSrc {
    If(*const Stmt),
    Call(*const Expr),
}

/// An arm-dependent free plan: a binding whose join register holds a
/// gated row on one arm and a plain table on the other frees per-arm —
/// the tag gates a branch at the free point, the deferred arm emitting
/// nothing (its row dies with its base), the freeing arm freeing the
/// carrier's faithful register (with its borrower keeps).
#[derive(Clone)]
pub struct ArmFreeEntry {
    pub carrier: String,
    pub tag: TagSrc,
    /// The tag value on which NOTHING frees (the deferred arm); the
    /// other value frees the carrier's faithful register.
    pub nothing_on: bool,
    pub keeps: Vec<Keep>,
}

/// An inline closure's definition, keyed by its `Expr::Function` node
/// pointer. Written by the checker (params + the original body + the
/// resolved return type); read by the lowerer at every call site it
/// inlines.
#[derive(Clone)]
pub struct FnDef {
    pub params: Vec<String>,
    /// The ORIGINAL body, as a raw slice pointer — every pointer-keyed
    /// fact (ctor sites, scope frees, move poisons) keys on the
    /// original AST nodes, so a cloned body would detach from them.
    /// Sound while the AST lives, which spans every phase.
    pub body: *const [Stmt],
    pub ret: StaticType,
    /// Calls are monomorphic: the first checked call stores its
    /// argument types here and every later call unifies against them.
    /// Without it, each call would instantiate fresh parameter
    /// unknowns and `f(1)` after `f("s")` would pass whenever the body
    /// doesn't conflict on its own.
    pub signature: Option<Vec<StaticType>>,
}

impl FnDef {
    /// # Safety
    /// The AST that produced this definition must still be alive.
    pub unsafe fn body(&self) -> &[Stmt] {
        unsafe { &*self.body }
    }
}

#[derive(Default)]
pub struct ShapeFacts {
    pub sites: BTreeMap<*const Expr, usize>,
    pub elems: Vec<StaticType>,
    /// The boundary `arg` table's element type as the checker resolved
    /// it from usage (Integer until something pins it: an arithmetic
    /// operand, a comparison, a condition, a store or constructor
    /// element, a print — Float/Bool/String otherwise). The lowerer
    /// seeds the `arg` binding's type from it and the host parses the
    /// CLI words against it, so both sides of the boundary speak the
    /// one cell type the script's own code demanded.
    pub boundary_elem: StaticType,
    #[allow(dead_code)]
    pub layouts: Vec<LayoutVerdict>,
    pub free_sites: BTreeSet<*const Stmt>,
    /// The keep lists of `x = nil` drops whose carrier frees around
    /// live borrowers: stmt -> the spared binding names. Absent = the
    /// drop frees plain (free_sites stays the gate for both).
    pub free_keeps: BTreeMap<*const Stmt, Vec<Keep>>,
    pub do_exit_frees: BTreeMap<(*const Stmt, u8), Vec<DoExitFree>>,
    /// The root chunk's scope-exit frees: top-level bindings still
    /// owning a heap site after the last statement. The lowerer emits
    /// them in the program's tail block — the fall-through path only
    /// (a boundary return jumps past the tail; its own path carries a
    /// ret_path_frees batch instead).
    pub root_exit_frees: Vec<DoExitFree>,
    /// Per statement: constructor sites born inside it whose value is
    /// only read through (no binding ever owns them, so no scope-exit
    /// pass can reach them). The lowerer frees each from its birth
    /// register at the end of the consuming statement.
    pub stmt_temp_frees: BTreeMap<*const Stmt, Vec<usize>>,
    /// Per if/while statement: the ctor sites born inside its
    /// condition, freed where the condition value is consumed — once
    /// before an if's branch, per evaluation in a while's header block
    /// (each iteration births a fresh header in the same register), or
    /// once in the pre-header when the reserved-loop fast path hoists
    /// the bound. `and`/`or` operands inside a condition are excluded
    /// (the arm-temp plan owns them: their births are path-dependent).
    pub cond_temp_frees: BTreeMap<*const Stmt, Vec<usize>>,
    /// Per expression node (an `and`/`or` operand): the ctor sites
    /// born evaluating that operand. The left operand always runs —
    /// its frees land right after its value feeds the branch; the
    /// right operand only runs on its own short-circuit path, so its
    /// frees land inside the evaluated arm, where the birth actually
    /// happened.
    pub arm_temp_frees: BTreeMap<*const Expr, Vec<usize>>,
    /// Per `return` statement: the exit frees of every scope the
    /// return crosses (block scopes, the inline body's own scope, and
    /// at the boundary the root chunk), minus the value's read-reach —
    /// the sites the returned table IS or lives inside ride the
    /// keep-free below instead. The lowerer emits the batch in the
    /// return's block, before the jump: without it a `return` would
    /// leap past every enclosing scope's natural-exit frees and leak
    /// them.
    pub ret_path_frees: BTreeMap<*const Stmt, Vec<DoExitFree>>,
    /// Per `return` statement: the crossed-scope frees whose site sits
    /// in the value's read-reach — emitted as glm_tbl_free_except
    /// through the carrier's current register with the return value's
    /// register as the keep. The runtime skips the kept subtree by
    /// pointer identity at every depth, so `return x[k]` releases the
    /// base shell and sibling rows while the handed-out row survives
    /// for its receiver, and `return t` degenerates to a no-op (the
    /// keep IS the table).
    pub ret_path_keep_frees: BTreeMap<*const Stmt, Vec<DoExitFree>>,
    /// Per `return` statement: the statement-born ctor sites in the
    /// value's index-base spine (a ctor temp or inlined call result
    /// indexed on the spot) — freed keep-style from their birth
    /// registers, no carrier involved.
    pub ret_path_keep_temps: BTreeMap<*const Stmt, Vec<usize>>,
    /// Per drop/rebind statement: the displaced names to free through
    /// their pre-update registers, each with its borrower keeps (the
    /// displaced value frees around live borrowers of its subtree).
    pub rebind_frees: BTreeMap<*const Stmt, BTreeMap<String, Vec<Keep>>>,
    // === mixed-join arm plans ===
    /// If statements needing a tag phi (one Bool per join: true = the
    /// then arm ran) and the then arm's class (true = that arm's plan
    /// frees nothing).
    pub join_tags: BTreeMap<*const Stmt, bool>,
    /// Call nodes needing a tag phi over their return edges (per-edge
    /// class in ReturnCtx order, fallthrough=false appended by the
    /// lowerer).
    pub call_tags: BTreeMap<*const Expr, Vec<bool>>,
    /// Arm-dependent frees at the four natural free points (drops,
    /// rebinds, scope exits, the root tail). The return path is
    /// exempt: its value register IS the keep, so glm_tbl_free_except
    /// is faithful on every arm without a tag.
    /// Per store statement: the ghosts housed by that store — the
    /// lowerer captures the store's value register for each (an SSA
    /// register holds the row pointer for the whole function), the
    /// register every origin-side Ghost keep resolves to.
    pub store_ghost_keeps: BTreeMap<*const Stmt, Vec<usize>>,
    /// Per constructor: (entry index, housed ghost) pairs — the
    /// entry's value register is the row's keep.
    pub ctor_ghost_keeps: BTreeMap<*const Expr, Vec<(usize, usize)>>,
    pub arm_free_sites: BTreeMap<*const Stmt, Vec<ArmFreeEntry>>,
    pub arm_rebind_frees: BTreeMap<*const Stmt, Vec<ArmFreeEntry>>,
    pub arm_do_exit_frees: BTreeMap<(*const Stmt, u8), Vec<ArmFreeEntry>>,
    pub arm_root_exit_frees: Vec<ArmFreeEntry>,
    pub move_poisons: BTreeMap<*const Stmt, BTreeSet<String>>,
    pub name_dense: BTreeMap<(String, usize), bool>,
    pub stored_ctor_parents: BTreeSet<usize>,
    pub substitutions: BTreeMap<usize, StaticType>,
    pub local_types: BTreeMap<(*const Stmt, String), StaticType>,
    pub row_reads: BTreeMap<usize, (String, usize)>,
    pub cell_children: BTreeMap<usize, BTreeSet<usize>>,
    pub dense_ctor_len: BTreeMap<usize, i64>,
    // === inline closures ===
    /// The body's scope-exit frees key: the lowerer replays these at
    /// every inline site (analyzer, def-site walk).
    pub fn_scope_keys: BTreeMap<*const Expr, (*const Stmt, u8)>,
    /// The outer names an inlined body rebinds, transitively through
    /// nested definitions (pure AST scan; feeds the lowerer's loop/if
    /// phi machinery).
    pub fn_touched: BTreeMap<*const Expr, BTreeSet<String>>,
    /// Params + cloned body + resolved return type (checker).
    pub fn_defs: BTreeMap<*const Expr, FnDef>,
    /// Call expr -> its function's expr, so the lowerer inlines the
    /// right body at the right site (checker).
    pub call_defs: BTreeMap<*const Expr, *const Expr>,
    /// Diagnostic anchors from the parser: statement pointer -> line,
    /// ctor site id -> line (parallel to `sites`' id space).
    pub stmt_lines: BTreeMap<*const Stmt, usize>,
    pub ctor_lines: Vec<usize>,
    pub diagnostics: Vec<String>,
}

impl ShapeFacts {
    pub fn elem_of(&self, ctor: &Expr) -> StaticType {
        let id = self.sites[&(ctor as *const Expr)];
        let elem = self.elems[id].clone();
        self.resolve_elem_type(elem)
    }

    fn resolve_elem_type(&self, ty: StaticType) -> StaticType {
        match &ty {
            StaticType::Unknown(id) => match self.substitutions.get(id) {
                Some(resolved) => self.resolve_elem_type(resolved.clone()),
                None => ty,
            },
            StaticType::Table(inner) => {
                let resolved = self.resolve_elem_type((**inner).clone());
                StaticType::Table(Box::new(resolved))
            }
            _ => ty,
        }
    }

    pub fn is_dense_at_depth(&self, name: &str, depth: usize) -> bool {
        self.name_dense
            .get(&(name.to_string(), depth))
            .copied()
            .unwrap_or(false)
    }

    pub fn check_row_reads(&mut self) {
        let reads: Vec<(usize, String, usize)> = self
            .row_reads
            .iter()
            .map(|(&base, (name, depth))| (base, name.clone(), *depth))
            .collect();
        let mut reported: BTreeSet<usize> = BTreeSet::new();
        for (base, name, depth) in reads {
            let mut level: BTreeSet<usize> =
                self.cell_children.get(&base).cloned().unwrap_or_default();
            for _pierce in 2..=depth {
                let mut next: BTreeSet<usize> = BTreeSet::new();
                for row in level {
                    if reported.insert(row)
                        && matches!(
                            self.resolve_elem_type(self.elems[row].clone()),
                            StaticType::Unknown(_)
                        )
                    {
                        signal!(trace::TRACE_ANALYZE_MISSING_VALUE);
                        let line = self.ctor_lines[row];
                        self.diagnostics.push(format!(
                            "line {line}: Type Error: a row of '{name}' is read before it is \
                             ever given a value (table site #{row})"
                        ));
                    }
                    if let Some(kids) = self.cell_children.get(&row) {
                        next.extend(kids.iter().copied());
                    }
                }
                level = next;
            }
        }
    }

    pub fn is_free(&self, stmt: &Stmt) -> bool {
        self.free_sites.contains(&(stmt as *const Stmt))
    }
}
