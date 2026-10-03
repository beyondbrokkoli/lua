#![allow(clippy::missing_safety_doc)]

pub mod rt;
pub mod trace;

/// The boundary cell vocabulary — the four scalar types the checker's
/// usage inference can pin `arg`'s element to. This lives in the lib
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
}
