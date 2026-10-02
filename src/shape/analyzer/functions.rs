use super::*;
impl Analyzer {
    pub(super) fn resolve_fn(&self, name: &str) -> Option<*const Expr> {
        for scope in self.walk.fn_scopes.iter().rev() {
            if let Some(entry) = scope.get(name) {
                return *entry;
            }
        }
        None
    }

    /// Declare a name's inline-closure binding in the CURRENT scope
    /// (a `local`, shadowing anything outer).
    pub(super) fn declare_fn_binding(&mut self, name: &str, ptr: Option<*const Expr>) {
        self.walk
            .fn_scopes
            .last_mut()
            .unwrap()
            .insert(name.to_string(), ptr);
    }

    /// Bind (or clear) a name's inline-closure binding at its innermost
    /// visible scope (an assignment to an existing binding).
    pub(super) fn assign_fn_binding(&mut self, name: &str, ptr: Option<*const Expr>) {
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
    pub(super) fn walk_fn_def(
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

    /// Which of `params` a returned expression hands straight back
    /// out: the parameter itself, or a call whose callee returns one
    /// of ITS parameters bare at an argument position fed by a
    /// parameter — recursively, so `return g(h(x))` reaches through
    /// the whole chain. Callees' sets are complete because their
    /// definition walks precede this call site.
    pub(super) fn return_reaches_param(&self, expr: &Expr, params: &[String]) -> BTreeSet<usize> {
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
    pub(super) fn record_moved_param(&mut self, name: &str) {
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
    pub(super) fn check_boundary_pin(&self, bind: &TableShape, src: &str) -> Result<(), ShapeError> {
        if bind.aliases.contains(&BOUNDARY_ROOT) {
            return Err(ShapeError(format!(
                "Lifetime Error: the boundary table '{src}' is pinned — the host owns \
                 its header; read its cells ('{src}[i]') instead of moving the table"
            )));
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
    pub(super) fn ret_reach_sites(&self, e: &Expr, out: &mut BTreeSet<usize>) {
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
    pub(super) fn ret_base_temp_sites(&self, e: &Expr, out: &mut Vec<usize>) {
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
}
