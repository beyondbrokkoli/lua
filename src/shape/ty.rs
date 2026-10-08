#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum Ty {
    #[default]
    Pending,
    Int,
    Flt,
    Bool,
    Str,
    Tbl(Box<Ty>),
    /// The dynamic cell — the analyzer's face of StaticType::Any. The
    /// written opt-in's landing: a constructor whose entries mix
    /// scalar kinds resolves its site here (a Table in the mix stays
    /// the Conflict it always was — cells hold scalars).
    Any,
    Conflict,
}

use Ty::{Any, Bool, Conflict, Flt, Int, Pending, Str, Tbl};

pub fn scalar(t: &Ty) -> bool {
    matches!(t, Int | Flt | Bool | Str)
}

/// The kinds that may ride a dynamic cell: the four scalars plus Any
/// itself (a mixed ctor's Any entries keep the site dynamic).
pub fn scalarish(t: &Ty) -> bool {
    matches!(t, Int | Flt | Bool | Str | Any)
}

pub fn join_ty(a: &Ty, b: &Ty) -> Ty {
    match (a, b) {
        (Pending, x) | (x, Pending) => x.clone(),
        (Conflict, _) | (_, Conflict) => Conflict,
        // Scalars MIXING — with each other or with an established
        // dynamic cell — choose the dynamic cell: the written opt-in,
        // decided at the join so every carrier and store downstream
        // flows as Table<Any>. A uniform pair never reaches this arm
        // (equal operands answer as themselves below).
        (x, y) if x != y && scalarish(x) && scalarish(y) => Any,
        (Tbl(inner_a), Tbl(inner_b)) => {
            let merged = join_ty(inner_a, inner_b);
            if merged != Conflict {
                Tbl(Box::new(merged))
            } else {
                Conflict
            }
        }
        (x, y) if x == y => x.clone(),
        _ => Conflict,
    }
}

pub fn arith_ty(a: Ty, b: Ty) -> Ty {
    if a == Pending || b == Pending {
        Pending
    } else if a == b && matches!(a, Int | Flt) {
        a
    } else {
        Conflict
    }
}
