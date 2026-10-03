#![allow(clippy::missing_safety_doc)]

pub mod rt;
pub mod trace;

impl GlmElem {
    /// The GLM_ARG_* kind of this element — the same mapping rt's
    /// arg_kind_of speaks, on the enum itself for consumers that hold
    /// a GlmElem without reaching into rt.
    pub fn kind(self) -> i32 {
        rt::arg_kind_of(&self)
    }
}

/// The pure halves of the Any word grammar, for the compiler's
/// dev-loop host: classification and cell packing WITHOUT interning —
/// the host interns string words through the .so's own glm_str_intern
/// so the boundary's identity space stays owned by the module's
/// runtime instance (the compiler process holds a second, rlib copy
/// of the runtime whose intern pool and trace sidecar must stay
/// untouched). The exe host uses rt's single-instance any_of_word.
pub use rt::{AnyWord, classify_word, glm_any_pack};

/// The boundary cell vocabulary — the four scalar types the checker's
/// usage inference can pin `arg`'s element to, plus Any: the tagged
/// cell an UNCONSTRAINED element resolves to (the script only copies,
/// prints, passes along, or returns its cells — no typed use anywhere,
/// so the boundary itself carries the choice). This lives in the lib
/// (not rt) because the compiler's backend is its other consumer: the
/// backend writes one of these as the module's exported glm_arg_kind()
/// answer and @main's embedded constant, and the runtime maps it back
/// to its parse — one vocabulary shared across the .so and exe faces
/// of the same module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlmElem {
    Integer,
    Float,
    Boolean,
    String,
    Any,
}
