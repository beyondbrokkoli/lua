use super::*;
pub(super) fn deepfree_children(
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
pub(super) fn origin_housed(
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
/// a birth-site projection — but borrowers NAME
/// the ghost in their lineage directly, so the harm test below still
/// sees through it at every chain depth.
pub(super) fn release_set(
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
/// binding's register points into the tree being composted. Value-kind
/// bindings (the four scalars plus Any) are excluded — their register
/// copied a cell value, not a row pointer. `upto` bounds the scan: scope-exit batches spare only
/// OUTER borrowers (batch-mates die in the same instant, their ghosts
/// deferring through batch coverage); drops and rebinds run
/// mid-statement, where every live binding counts.
pub(super) fn borrowers_of(
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
            if scalarish(&ts.ty) {
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
pub(super) fn ghost_origin_covered(
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

pub(super) fn holds_reaches(holds: &HoldsState, from: usize, to: usize) -> bool {
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

impl Analyzer {
    /// Project a node onto the site-keyed physical graph: a ghost
    /// maps to its row's birth site when resolved; real sites map to
    /// themselves; roots and unresolved ghosts map to nothing.
    pub(super) fn proj_node(&self, n: usize) -> Option<usize> {
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
    pub(super) fn ghost_base_closure(&self, g: usize, out: &mut BTreeSet<usize>) {
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
    pub(super) fn birth_closure(&self, site: usize) -> BTreeSet<usize> {
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
    pub(super) fn row_links_closure(&self, site: usize) -> BTreeSet<usize> {
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
    pub(super) fn node_release(&self, node: usize) -> BTreeSet<usize> {
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
    pub(super) fn harm_set(&self, node: usize) -> BTreeSet<usize> {
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
    pub(super) fn mark_claimed(&mut self, node: usize, keeps: &[super::facts::Keep]) {
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
    pub(super) fn housed_in_release(&self, node: usize) -> (Vec<usize>, Vec<usize>) {
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
    pub(super) fn ghost_row_elem(&self, ghost: usize) -> Ty {
        let Some(mint) = self.lattice.ghost_mints.get(&ghost) else {
            return Pending;
        };
        let mut r = Pending;
        for &b in &mint.bases {
            if is_ghost(&b) {
                r = join_ty(&r, &self.ghost_row_elem(b));
            } else if !is_root(&b)
                && let Tbl(inner) = &self.lattice.site_elem[b]
            {
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
    pub(super) fn same_row(&self, a: usize, b: usize) -> bool {
        if a == b {
            return true;
        }
        match (
            self.lattice.ghost_mints.get(&a),
            self.lattice.ghost_mints.get(&b),
        ) {
            (Some(ma), Some(mb)) => ma.key == mb.key && !ma.bases.is_disjoint(&mb.bases),
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
    pub(super) fn check_row_ownership(
        &self,
        name: &str,
        depth: usize,
        shape: &TableShape,
        moved_srcs: &[(usize, String)],
    ) -> Result<(), ShapeError> {
        let new_ghosts: Vec<usize> = shape.aliases.iter().copied().filter(is_ghost).collect();
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

    /// Affine sole-housing: a row stored into a cell refuses when
    /// another LIVE binding already owns the same header (both would
    /// plan independent fires) or another cell already houses it (both
    /// housings would claim it). The value's own Identifier source is
    /// exempt — its ownership moves into the cell.
    pub(super) fn check_row_housing(&self, g: usize, value: &Expr) -> Result<(), ShapeError> {
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

    pub(super) fn reachable_rows(&self, origin: usize, depth: usize) -> BTreeSet<usize> {
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
}
