use super::*;
#[derive(Clone, Debug, PartialEq)]
pub(super) enum NumExpr {
    Lit(i64),
    Var(String),
    Add(Box<NumExpr>, Box<NumExpr>),
    Mul(Box<NumExpr>, Box<NumExpr>),
    Other,
}

pub(super) fn num_of(expr: &Expr) -> NumExpr {
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

pub(super) fn provably_nonneg(
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

pub(super) fn guard_provably_nonneg_expr(
    num_assigns: &BTreeMap<String, Vec<NumExpr>>,
    expr: &Expr,
) -> bool {
    let mut path = BTreeSet::new();
    provably_nonneg(num_assigns, &num_of(expr), "", &mut path)
}

pub(super) fn guard_provably_nonneg(
    num_assigns: &BTreeMap<String, Vec<NumExpr>>,
    guard: &str,
) -> bool {
    num_assigns.get(guard).is_some_and(|shapes| {
        let mut path = BTreeSet::new();
        shapes
            .iter()
            .all(|sh| provably_nonneg(num_assigns, sh, guard, &mut path))
    })
}

pub(super) fn guard_only_ascends(
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

impl Analyzer {
    pub(super) fn infer_bind(&mut self, rec: Gate, expr: &Expr) -> Result<TableShape, ShapeError> {
        let (ty, aliases, lineage) = self.infer_expr(rec, expr)?;
        Ok(TableShape {
            ty,
            layout: LayoutVerdict::default(),
            aliases,
            lineage,
        })
    }

    pub(super) fn collect_entry_moves(
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

    pub(super) fn infer_expr(
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
                        && let Some(&child) = self.lattice.sites.get(&(e as *const Expr))
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
                    // A Pending entry value at the root scope can only
                    // be a boundary read (`arg[i]`) or a chained read
                    // of a site that errors on its own — flag the
                    // ctor site boundary-fed so a needed+Pending tail
                    // defers it to the checker's inference instead of
                    // rejecting (the fn-param twin).
                    if matches!(t, Pending)
                        && esites.iter().all(|s| is_root(s) || is_ghost(s))
                        && self.walk.scopes.len() == 1
                    {
                        self.reads.boundary_fed.insert(id);
                    }
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
                            let entry_ghosts: Vec<usize> =
                                esites.iter().copied().filter(is_ghost).collect();
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
                                    if id == birth || self.birth_closure(id).contains(&birth) {
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
                    let r = if matches!(t, Pending) {
                        Pending
                    } else {
                        Conflict
                    };
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
                        self.fn_param_sites.extend(
                            sites
                                .iter()
                                .copied()
                                .filter(|s| !is_root(s) && !is_ghost(s)),
                        );
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

    pub(super) fn check_key_threshold(
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

    pub(super) fn detect_fill_loop(&mut self, rec: Gate, guard: &str, body: &[Stmt]) {
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

    pub(super) fn collect_mutated_names(&self, rec: Gate, stmts: &[Stmt]) -> BTreeSet<String> {
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
