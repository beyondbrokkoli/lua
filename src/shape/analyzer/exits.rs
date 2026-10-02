use super::*;
/// How a sub-expression's table value leaves the statement: consumed
/// by the enclosing evaluation (dies at the statement's end) or handed
/// to an owner that outlives it (a binding, a cell, the boundary, a
/// stored-through parameter).
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Fate {
    Dies,
    Escapes,
}

impl Analyzer {
    /// The scope-exit frees for the scope at `depth`, signals fired:
    /// the natural-exit decisions (block scopes, inline bodies, the
    /// root chunk) all land here.
    pub(super) fn scope_exit_frees(
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

    pub(super) fn compute_scope_frees(
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
            // live base. Without provenance the
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
        // its borrowed children instead of composting them (a ghost's
        // release is invisible to deepfree, but every borrower NAMES
        // the ghost in its lineage).
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

    pub(super) fn decide_scope_exit(&mut self, rec: Gate, key: (*const Stmt, u8)) -> Result<(), ShapeError> {
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
    pub(super) fn plan_return_exits(&mut self, value: Option<(&Expr, &Ty)>, reach: &BTreeSet<usize>, stmt: *const Stmt) {
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
    pub(super) fn decide_root_exit(&mut self, rec: Gate) -> Result<(), ShapeError> {
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
    pub(super) fn prune_deepfree_children(&self, sites: &mut Vec<usize>) {
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
    pub(super) fn plan_stmt_temp_frees(&mut self, stmts: &[Stmt]) {
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
    pub(super) fn plan_cond_temp_frees(&mut self, cond: &Expr, stmt: &Stmt) {
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
    pub(super) fn plan_arm_temp_frees_in_stmt(&mut self, stmt: &Stmt) {
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

    pub(super) fn plan_arm_temp_frees_in_expr(&mut self, e: &Expr) {
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
    pub(super) fn collect_temp_sites(&self, e: &Expr, fate: Fate, out: &mut Vec<usize>) {
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
    pub(super) fn plan_fn_bodies_in_stmt(&mut self, stmt: &Stmt) {
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

    pub(super) fn plan_fn_bodies_in_expr(&mut self, e: &Expr) {
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
}
