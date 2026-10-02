use super::ty::{Ty, join_ty};
use std::collections::{BTreeMap, BTreeSet};

pub const SPARSE_THRESHOLD: i64 = glm_rt::rt::SPARSE_THRESHOLD;
pub const BOUNDS_FAIL_THRESHOLD: i64 = i64::MAX / 8;
pub const NULL_ROOT: usize = usize::MAX;
pub const MOVED_ROOT: usize = usize::MAX - 1;
/// The host-seeded boundary table (`arg`): its header is owned by the
/// host across @glm_exe, so it may never be moved into a script
/// binding or carrier. Carried in `TableShape.aliases`, filtered out by
/// `is_root` exactly like the other sentinels — it never enters a site
/// set, and it survives scope merges so the pin is flow-safe.
pub const BOUNDARY_ROOT: usize = usize::MAX - 2;

pub fn is_root(s: &usize) -> bool {
    *s == NULL_ROOT || *s == MOVED_ROOT || *s == BOUNDARY_ROOT
}

/// Row ghosts: first-class ownership tokens minted per row-read AST
/// node (`Expr::Index` over a table base), occupying the id space from
/// `MAX/2` upward — above every real ctor site, below the sentinels,
/// and disjoint from the type checker's `StaticType::Unknown` ghosts,
/// which count DOWN from `usize::MAX` (type_checker.rs BARE_LOCAL_MAX)
/// in their own id space. A ghost in `TableShape.aliases` says
/// "this binding owns the row header it read out of some base"; the
/// base itself stays named in `lineage` (a ghost may also appear as a
/// lineage origin key — a row read out of a ghost-owning binding), so
/// frees stay gated on the base's own death. Ghosts never index the
/// per-site `Vec`s (`site_elem`, `needed`, `elems`), never enter
/// `row_reads`/`row_links`/`child_sites`, and never ride call-site
/// pass-through — see the filters at each of those sites.
pub const ROW_GHOST_FLOOR: usize = usize::MAX / 2;

pub fn is_ghost(s: &usize) -> bool {
    *s >= ROW_GHOST_FLOOR && !is_root(s)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum LayoutVerdict {
    #[default]
    Dense,
    Growing,
    Sparse,
    BoundsFail,
}

impl LayoutVerdict {
    pub fn join(self, other: LayoutVerdict) -> LayoutVerdict {
        use LayoutVerdict::*;
        match (self, other) {
            (BoundsFail, _) | (_, BoundsFail) => BoundsFail,
            (Sparse, _) | (_, Sparse) => Sparse,
            (Growing, _) | (_, Growing) => Growing,
            _ => Dense,
        }
    }
}

pub type RowLineage = BTreeMap<usize, (String, usize)>;

#[derive(Clone, PartialEq, Debug)]
pub struct TableShape {
    pub ty: Ty,
    pub layout: LayoutVerdict,
    pub aliases: BTreeSet<usize>,
    pub lineage: RowLineage,
}

impl TableShape {
    pub fn join(&mut self, other: &Self) -> bool {
        let mut changed = false;

        let new_ty = join_ty(&self.ty, &other.ty);
        if self.ty != new_ty {
            self.ty = new_ty;
            changed = true;
        }

        let new_layout = self.layout.join(other.layout);
        if self.layout != new_layout {
            self.layout = new_layout;
            changed = true;
        }

        let len_before = self.aliases.len();
        self.aliases.extend(&other.aliases);
        if self.aliases.len() > len_before {
            changed = true;
        }

        for (&site, (name, depth)) in &other.lineage {
            match self.lineage.entry(site) {
                std::collections::btree_map::Entry::Vacant(v) => {
                    v.insert((name.clone(), *depth));
                    changed = true;
                }
                std::collections::btree_map::Entry::Occupied(mut o) => {
                    if *depth > o.get().1 {
                        o.get_mut().1 = *depth;
                        changed = true;
                    }
                }
            }
        }

        changed
    }
}
