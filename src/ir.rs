use crate::ast::StaticType;
use std::marker::PhantomData;

pub type BlockId = usize;

pub type RegId = u32;

#[derive(Debug, Clone, Copy)]
pub struct Int;
#[derive(Debug, Clone, Copy)]
pub struct Float;
#[derive(Debug, Clone, Copy)]
pub struct Bool;
#[derive(Debug, Clone, Copy)]
pub struct Str;
#[derive(Debug, Clone, Copy)]
pub struct Ptr;
#[derive(Debug, Clone, Copy)]
pub struct Byte;

#[derive(Debug, Clone, Copy)]
pub struct Reg<R: Repr + ?Sized> {
    pub id: RegId,
    _repr: PhantomData<R>,
}

impl<R: Repr> Reg<R> {
    pub fn new(id: RegId) -> Self {
        Self {
            id,
            _repr: PhantomData,
        }
    }
}

pub trait Repr {
    fn llvm() -> &'static str;
    fn storage() -> &'static str {
        Self::llvm()
    }
    fn esize() -> u32 {
        8
    }
    const IS_PACKED: bool = false;
    const IS_FLOAT: bool = false;
    fn emit_move(target: &Reg<Self>, source: &Reg<Self>, code: &mut String);
    fn any(reg: Reg<Self>) -> AnyReg;
}

impl Repr for Int {
    fn llvm() -> &'static str {
        "i64"
    }
    fn emit_move(target: &Reg<Self>, source: &Reg<Self>, code: &mut String) {
        code.push_str(&format!("  %v{} = add i64 %v{}, 0\n", target.id, source.id));
    }
    fn any(reg: Reg<Self>) -> AnyReg {
        AnyReg::Int(reg)
    }
}

impl Repr for Float {
    fn llvm() -> &'static str {
        "double"
    }
    const IS_FLOAT: bool = true;
    fn emit_move(target: &Reg<Self>, source: &Reg<Self>, code: &mut String) {
        code.push_str(&format!(
            "  %v{} = fadd double %v{}, 0.0\n",
            target.id, source.id
        ));
    }
    fn any(reg: Reg<Self>) -> AnyReg {
        AnyReg::Float(reg)
    }
}

impl Repr for Bool {
    fn llvm() -> &'static str {
        "i1"
    }
    fn storage() -> &'static str {
        "i8"
    }
    fn esize() -> u32 {
        1
    }
    const IS_PACKED: bool = true;
    fn emit_move(target: &Reg<Self>, source: &Reg<Self>, code: &mut String) {
        code.push_str(&format!("  %v{} = or i1 %v{}, 0\n", target.id, source.id));
    }
    fn any(reg: Reg<Self>) -> AnyReg {
        AnyReg::Bool(reg)
    }
}

impl Repr for Str {
    fn llvm() -> &'static str {
        "ptr"
    }
    fn emit_move(target: &Reg<Self>, source: &Reg<Self>, code: &mut String) {
        code.push_str(&format!(
            "  %v{} = getelementptr i8, ptr %v{}, i64 0\n",
            target.id, source.id
        ));
    }
    fn any(reg: Reg<Self>) -> AnyReg {
        AnyReg::Str(reg)
    }
}

impl Repr for Ptr {
    fn llvm() -> &'static str {
        "ptr"
    }
    fn emit_move(target: &Reg<Self>, source: &Reg<Self>, code: &mut String) {
        code.push_str(&format!(
            "  %v{} = getelementptr i8, ptr %v{}, i64 0\n",
            target.id, source.id
        ));
    }
    fn any(reg: Reg<Self>) -> AnyReg {
        AnyReg::Ptr(reg)
    }
}

impl Repr for Byte {
    fn llvm() -> &'static str {
        "i8"
    }
    fn esize() -> u32 {
        1
    }
    fn emit_move(target: &Reg<Self>, source: &Reg<Self>, code: &mut String) {
        code.push_str(&format!("  %v{} = or i8 %v{}, 0\n", target.id, source.id));
    }
    fn any(reg: Reg<Self>) -> AnyReg {
        AnyReg::Byte(reg)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum AnyReg {
    Int(Reg<Int>),
    Float(Reg<Float>),
    Bool(Reg<Bool>),
    Str(Reg<Str>),
    Ptr(Reg<Ptr>),
    Byte(Reg<Byte>),
}

impl AnyReg {
    pub fn id(&self) -> RegId {
        match self {
            AnyReg::Int(r) => r.id,
            AnyReg::Float(r) => r.id,
            AnyReg::Bool(r) => r.id,
            AnyReg::Str(r) => r.id,
            AnyReg::Ptr(r) => r.id,
            AnyReg::Byte(r) => r.id,
        }
    }

    pub fn into_cell(self) -> CellVal {
        match self {
            AnyReg::Int(r) => CellVal::Int(r),
            AnyReg::Float(r) => CellVal::Float(r),
            AnyReg::Bool(r) => CellVal::Bool(r),
            AnyReg::Str(r) => CellVal::Ptr(Reg::new(r.id)),
            AnyReg::Ptr(r) => CellVal::Ptr(r),
            AnyReg::Byte(r) => CellVal::Byte(r),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegKind {
    Int,
    Float,
    Bool,
    Ptr,
}

impl RegKind {
    pub fn reg(self, id: RegId) -> AnyReg {
        match self {
            RegKind::Int => AnyReg::Int(Reg::new(id)),
            RegKind::Float => AnyReg::Float(Reg::new(id)),
            RegKind::Bool => AnyReg::Bool(Reg::new(id)),
            RegKind::Ptr => AnyReg::Ptr(Reg::new(id)),
        }
    }
}

pub fn repr_of(ty: &StaticType) -> RegKind {
    match ty {
        StaticType::Integer => RegKind::Int,
        StaticType::Float => RegKind::Float,
        StaticType::Boolean => RegKind::Bool,
        StaticType::String | StaticType::Table(_) | StaticType::Unknown(_) => RegKind::Ptr,
    }
}

#[derive(Debug, Clone, Copy)]
pub enum CellVal {
    Int(Reg<Int>),
    Float(Reg<Float>),
    Bool(Reg<Bool>),
    Ptr(Reg<Ptr>),
    Byte(Reg<Byte>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellTy {
    Int,
    Float,
    Bool,
    Ptr,
    Byte,
}

impl CellTy {
    pub fn of(ty: &StaticType) -> Self {
        match ty {
            StaticType::Integer => CellTy::Int,
            StaticType::Float => CellTy::Float,
            StaticType::Boolean => CellTy::Bool,
            StaticType::String | StaticType::Table(_) => CellTy::Ptr,
            StaticType::Unknown(_) => CellTy::Byte,
        }
    }
}

#[derive(Debug, Clone)]
pub enum MoveRegs {
    Int {
        target: Reg<Int>,
        source: Reg<Int>,
    },
    Float {
        target: Reg<Float>,
        source: Reg<Float>,
    },
    Bool {
        target: Reg<Bool>,
        source: Reg<Bool>,
    },
    Str {
        target: Reg<Str>,
        source: Reg<Str>,
    },
    Ptr {
        target: Reg<Ptr>,
        source: Reg<Ptr>,
    },
    Byte {
        target: Reg<Byte>,
        source: Reg<Byte>,
    },
}

#[derive(Debug, Clone)]
pub enum CellGet {
    Int {
        target: Reg<Int>,
        table: Reg<Ptr>,
        index: Reg<Int>,
    },
    Float {
        target: Reg<Float>,
        table: Reg<Ptr>,
        index: Reg<Int>,
    },
    Bool {
        target: Reg<Bool>,
        table: Reg<Ptr>,
        index: Reg<Int>,
    },
    Ptr {
        target: Reg<Ptr>,
        table: Reg<Ptr>,
        index: Reg<Int>,
    },
    Byte {
        target: Reg<Byte>,
        table: Reg<Ptr>,
        index: Reg<Int>,
    },
}

#[derive(Debug, Clone)]
pub enum CellSet {
    Int {
        table: Reg<Ptr>,
        index: Reg<Int>,
        value: Reg<Int>,
    },
    Float {
        table: Reg<Ptr>,
        index: Reg<Int>,
        value: Reg<Float>,
    },
    Bool {
        table: Reg<Ptr>,
        index: Reg<Int>,
        value: Reg<Bool>,
    },
    Ptr {
        table: Reg<Ptr>,
        index: Reg<Int>,
        value: Reg<Ptr>,
    },
    Byte {
        table: Reg<Ptr>,
        index: Reg<Int>,
        value: Reg<Byte>,
    },
}

#[derive(Debug, Clone)]
pub enum CellSetFast {
    Int {
        table: Reg<Ptr>,
        index: Reg<Int>,
        value: Reg<Int>,
        layout: crate::shape::LayoutVerdict,
    },
    Float {
        table: Reg<Ptr>,
        index: Reg<Int>,
        value: Reg<Float>,
        layout: crate::shape::LayoutVerdict,
    },
    Bool {
        table: Reg<Ptr>,
        index: Reg<Int>,
        value: Reg<Bool>,
        layout: crate::shape::LayoutVerdict,
    },
    Ptr {
        table: Reg<Ptr>,
        index: Reg<Int>,
        value: Reg<Ptr>,
        layout: crate::shape::LayoutVerdict,
    },
    Byte {
        table: Reg<Ptr>,
        index: Reg<Int>,
        value: Reg<Byte>,
        layout: crate::shape::LayoutVerdict,
    },
}

#[derive(Debug, Clone)]
pub enum NumRegs {
    Int {
        target: Reg<Int>,
        left: Reg<Int>,
        right: Reg<Int>,
    },
    Float {
        target: Reg<Float>,
        left: Reg<Float>,
        right: Reg<Float>,
    },
}

#[derive(Debug, Clone)]
pub enum NumRegsRhs {
    Int {
        target: Reg<Int>,
        left: Reg<Int>,
        right: Reg<Int>,
        rhs_const: Option<i64>,
    },
    Float {
        target: Reg<Float>,
        left: Reg<Float>,
        right: Reg<Float>,
        rhs_const: Option<i64>,
    },
}

#[derive(Debug, Clone)]
pub enum UnaryNum {
    Int {
        target: Reg<Int>,
        source: Reg<Int>,
    },
    Float {
        target: Reg<Float>,
        source: Reg<Float>,
    },
}

#[derive(Debug, Clone)]
pub enum CmpRegs {
    Int {
        target: Reg<Bool>,
        left: Reg<Int>,
        right: Reg<Int>,
    },
    Float {
        target: Reg<Bool>,
        left: Reg<Float>,
        right: Reg<Float>,
    },
    Bool {
        target: Reg<Bool>,
        left: Reg<Bool>,
        right: Reg<Bool>,
    },
    Str {
        target: Reg<Bool>,
        left: Reg<Str>,
        right: Reg<Str>,
    },
    Ptr {
        target: Reg<Bool>,
        left: Reg<Ptr>,
        right: Reg<Ptr>,
    },
}

#[derive(Debug, Clone)]
pub enum PhiRegs {
    Int {
        target: Reg<Int>,
        args: Vec<(BlockId, Reg<Int>)>,
    },
    Float {
        target: Reg<Float>,
        args: Vec<(BlockId, Reg<Float>)>,
    },
    Bool {
        target: Reg<Bool>,
        args: Vec<(BlockId, Reg<Bool>)>,
    },
    Ptr {
        target: Reg<Ptr>,
        args: Vec<(BlockId, Reg<Ptr>)>,
    },
}

impl PhiRegs {
    pub fn target_id(&self) -> RegId {
        match self {
            PhiRegs::Int { target, .. } => target.id,
            PhiRegs::Float { target, .. } => target.id,
            PhiRegs::Bool { target, .. } => target.id,
            PhiRegs::Ptr { target, .. } => target.id,
        }
    }

    pub fn arg_ids(&self) -> Vec<RegId> {
        fn ids<R: Repr>(args: &[(BlockId, Reg<R>)]) -> Vec<RegId> {
            args.iter().map(|(_, r)| r.id).collect()
        }
        match self {
            PhiRegs::Int { args, .. } => ids(args),
            PhiRegs::Float { args, .. } => ids(args),
            PhiRegs::Bool { args, .. } => ids(args),
            PhiRegs::Ptr { args, .. } => ids(args),
        }
    }

    /// The predecessor blocks of every argument, in arg order — the
    /// edge list a join's phi spans (a tagged call's per-edge class
    /// constants align with it by index).
    pub fn arg_blocks(&self) -> Vec<BlockId> {
        fn blocks<R: Repr>(args: &[(BlockId, Reg<R>)]) -> Vec<BlockId> {
            args.iter().map(|(b, _)| *b).collect()
        }
        match self {
            PhiRegs::Int { args, .. } => blocks(args),
            PhiRegs::Float { args, .. } => blocks(args),
            PhiRegs::Bool { args, .. } => blocks(args),
            PhiRegs::Ptr { args, .. } => blocks(args),
        }
    }
}

pub trait NumRepr: Repr {
    fn num3(target: Reg<Self>, left: Reg<Self>, right: Reg<Self>) -> NumRegs;
    fn num3r(
        target: Reg<Self>,
        left: Reg<Self>,
        right: Reg<Self>,
        rhs_const: Option<i64>,
    ) -> NumRegsRhs;
    fn num2(target: Reg<Self>, source: Reg<Self>) -> UnaryNum;
}

impl NumRepr for Int {
    fn num3(target: Reg<Self>, left: Reg<Self>, right: Reg<Self>) -> NumRegs {
        NumRegs::Int {
            target,
            left,
            right,
        }
    }
    fn num3r(
        target: Reg<Self>,
        left: Reg<Self>,
        right: Reg<Self>,
        rhs_const: Option<i64>,
    ) -> NumRegsRhs {
        NumRegsRhs::Int {
            target,
            left,
            right,
            rhs_const,
        }
    }
    fn num2(target: Reg<Self>, source: Reg<Self>) -> UnaryNum {
        UnaryNum::Int { target, source }
    }
}

impl NumRepr for Float {
    fn num3(target: Reg<Self>, left: Reg<Self>, right: Reg<Self>) -> NumRegs {
        NumRegs::Float {
            target,
            left,
            right,
        }
    }
    fn num3r(
        target: Reg<Self>,
        left: Reg<Self>,
        right: Reg<Self>,
        rhs_const: Option<i64>,
    ) -> NumRegsRhs {
        NumRegsRhs::Float {
            target,
            left,
            right,
            rhs_const,
        }
    }
    fn num2(target: Reg<Self>, source: Reg<Self>) -> UnaryNum {
        UnaryNum::Float { target, source }
    }
}

pub trait CellRepr: Repr {
    const TAG: CellTy;
    fn get(target: Reg<Self>, table: Reg<Ptr>, index: Reg<Int>) -> CellGet;
    fn set(table: Reg<Ptr>, index: Reg<Int>, value: Reg<Self>) -> CellSet;
    fn set_fast(
        table: Reg<Ptr>,
        index: Reg<Int>,
        value: Reg<Self>,
        layout: crate::shape::LayoutVerdict,
    ) -> CellSetFast;
}

impl CellRepr for Int {
    const TAG: CellTy = CellTy::Int;
    fn get(target: Reg<Self>, table: Reg<Ptr>, index: Reg<Int>) -> CellGet {
        CellGet::Int {
            target,
            table,
            index,
        }
    }
    fn set(table: Reg<Ptr>, index: Reg<Int>, value: Reg<Self>) -> CellSet {
        CellSet::Int {
            table,
            index,
            value,
        }
    }
    fn set_fast(
        table: Reg<Ptr>,
        index: Reg<Int>,
        value: Reg<Self>,
        layout: crate::shape::LayoutVerdict,
    ) -> CellSetFast {
        CellSetFast::Int {
            table,
            index,
            value,
            layout,
        }
    }
}

impl CellRepr for Float {
    const TAG: CellTy = CellTy::Float;
    fn get(target: Reg<Self>, table: Reg<Ptr>, index: Reg<Int>) -> CellGet {
        CellGet::Float {
            target,
            table,
            index,
        }
    }
    fn set(table: Reg<Ptr>, index: Reg<Int>, value: Reg<Self>) -> CellSet {
        CellSet::Float {
            table,
            index,
            value,
        }
    }
    fn set_fast(
        table: Reg<Ptr>,
        index: Reg<Int>,
        value: Reg<Self>,
        layout: crate::shape::LayoutVerdict,
    ) -> CellSetFast {
        CellSetFast::Float {
            table,
            index,
            value,
            layout,
        }
    }
}

impl CellRepr for Bool {
    const TAG: CellTy = CellTy::Bool;
    fn get(target: Reg<Self>, table: Reg<Ptr>, index: Reg<Int>) -> CellGet {
        CellGet::Bool {
            target,
            table,
            index,
        }
    }
    fn set(table: Reg<Ptr>, index: Reg<Int>, value: Reg<Self>) -> CellSet {
        CellSet::Bool {
            table,
            index,
            value,
        }
    }
    fn set_fast(
        table: Reg<Ptr>,
        index: Reg<Int>,
        value: Reg<Self>,
        layout: crate::shape::LayoutVerdict,
    ) -> CellSetFast {
        CellSetFast::Bool {
            table,
            index,
            value,
            layout,
        }
    }
}

impl CellRepr for Ptr {
    const TAG: CellTy = CellTy::Ptr;
    fn get(target: Reg<Self>, table: Reg<Ptr>, index: Reg<Int>) -> CellGet {
        CellGet::Ptr {
            target,
            table,
            index,
        }
    }
    fn set(table: Reg<Ptr>, index: Reg<Int>, value: Reg<Self>) -> CellSet {
        CellSet::Ptr {
            table,
            index,
            value,
        }
    }
    fn set_fast(
        table: Reg<Ptr>,
        index: Reg<Int>,
        value: Reg<Self>,
        layout: crate::shape::LayoutVerdict,
    ) -> CellSetFast {
        CellSetFast::Ptr {
            table,
            index,
            value,
            layout,
        }
    }
}

impl CellRepr for Byte {
    const TAG: CellTy = CellTy::Byte;
    fn get(target: Reg<Self>, table: Reg<Ptr>, index: Reg<Int>) -> CellGet {
        CellGet::Byte {
            target,
            table,
            index,
        }
    }
    fn set(table: Reg<Ptr>, index: Reg<Int>, value: Reg<Self>) -> CellSet {
        CellSet::Byte {
            table,
            index,
            value,
        }
    }
    fn set_fast(
        table: Reg<Ptr>,
        index: Reg<Int>,
        value: Reg<Self>,
        layout: crate::shape::LayoutVerdict,
    ) -> CellSetFast {
        CellSetFast::Byte {
            table,
            index,
            value,
            layout,
        }
    }
}

pub trait CmpRepr: Repr {
    fn cmp(target: Reg<Bool>, left: Reg<Self>, right: Reg<Self>) -> CmpRegs;
}

impl CmpRepr for Int {
    fn cmp(target: Reg<Bool>, left: Reg<Self>, right: Reg<Self>) -> CmpRegs {
        CmpRegs::Int {
            target,
            left,
            right,
        }
    }
}

impl CmpRepr for Float {
    fn cmp(target: Reg<Bool>, left: Reg<Self>, right: Reg<Self>) -> CmpRegs {
        CmpRegs::Float {
            target,
            left,
            right,
        }
    }
}

impl CmpRepr for Bool {
    fn cmp(target: Reg<Bool>, left: Reg<Self>, right: Reg<Self>) -> CmpRegs {
        CmpRegs::Bool {
            target,
            left,
            right,
        }
    }
}

impl CmpRepr for Str {
    fn cmp(target: Reg<Bool>, left: Reg<Self>, right: Reg<Self>) -> CmpRegs {
        CmpRegs::Str {
            target,
            left,
            right,
        }
    }
}

impl CmpRepr for Ptr {
    fn cmp(target: Reg<Bool>, left: Reg<Self>, right: Reg<Self>) -> CmpRegs {
        CmpRegs::Ptr {
            target,
            left,
            right,
        }
    }
}

pub trait PhiRepr: Repr {
    fn phi(target: Reg<Self>, args: Vec<(BlockId, Reg<Self>)>) -> PhiRegs;
    fn of(reg: AnyReg) -> Option<Reg<Self>>;
}

impl PhiRepr for Int {
    fn phi(target: Reg<Self>, args: Vec<(BlockId, Reg<Self>)>) -> PhiRegs {
        PhiRegs::Int { target, args }
    }
    fn of(reg: AnyReg) -> Option<Reg<Self>> {
        match reg {
            AnyReg::Int(r) => Some(r),
            _ => None,
        }
    }
}

impl PhiRepr for Float {
    fn phi(target: Reg<Self>, args: Vec<(BlockId, Reg<Self>)>) -> PhiRegs {
        PhiRegs::Float { target, args }
    }
    fn of(reg: AnyReg) -> Option<Reg<Self>> {
        match reg {
            AnyReg::Float(r) => Some(r),
            _ => None,
        }
    }
}

impl PhiRepr for Bool {
    fn phi(target: Reg<Self>, args: Vec<(BlockId, Reg<Self>)>) -> PhiRegs {
        PhiRegs::Bool { target, args }
    }
    fn of(reg: AnyReg) -> Option<Reg<Self>> {
        match reg {
            AnyReg::Bool(r) => Some(r),
            _ => None,
        }
    }
}

impl PhiRepr for Ptr {
    fn phi(target: Reg<Self>, args: Vec<(BlockId, Reg<Self>)>) -> PhiRegs {
        PhiRegs::Ptr { target, args }
    }
    fn of(reg: AnyReg) -> Option<Reg<Self>> {
        match reg {
            AnyReg::Ptr(r) => Some(r),
            AnyReg::Str(r) => Some(Reg::new(r.id)),
            _ => None,
        }
    }
}

impl MoveRegs {
    pub fn target_id(&self) -> RegId {
        match self {
            MoveRegs::Int { target, .. } => target.id,
            MoveRegs::Float { target, .. } => target.id,
            MoveRegs::Bool { target, .. } => target.id,
            MoveRegs::Str { target, .. } => target.id,
            MoveRegs::Ptr { target, .. } => target.id,
            MoveRegs::Byte { target, .. } => target.id,
        }
    }
    pub fn source_id(&self) -> RegId {
        match self {
            MoveRegs::Int { source, .. } => source.id,
            MoveRegs::Float { source, .. } => source.id,
            MoveRegs::Bool { source, .. } => source.id,
            MoveRegs::Str { source, .. } => source.id,
            MoveRegs::Ptr { source, .. } => source.id,
            MoveRegs::Byte { source, .. } => source.id,
        }
    }
}

impl CellGet {
    pub fn target_id(&self) -> RegId {
        match self {
            CellGet::Int { target, .. } => target.id,
            CellGet::Float { target, .. } => target.id,
            CellGet::Bool { target, .. } => target.id,
            CellGet::Ptr { target, .. } => target.id,
            CellGet::Byte { target, .. } => target.id,
        }
    }
}

impl NumRegs {
    pub fn target_id(&self) -> RegId {
        match self {
            NumRegs::Int { target, .. } => target.id,
            NumRegs::Float { target, .. } => target.id,
        }
    }
}

impl NumRegsRhs {
    pub fn target_id(&self) -> RegId {
        match self {
            NumRegsRhs::Int { target, .. } => target.id,
            NumRegsRhs::Float { target, .. } => target.id,
        }
    }
}

impl UnaryNum {
    pub fn target_id(&self) -> RegId {
        match self {
            UnaryNum::Int { target, .. } => target.id,
            UnaryNum::Float { target, .. } => target.id,
        }
    }
}

impl CmpRegs {
    pub fn target_id(&self) -> RegId {
        match self {
            CmpRegs::Int { target, .. } => target.id,
            CmpRegs::Float { target, .. } => target.id,
            CmpRegs::Bool { target, .. } => target.id,
            CmpRegs::Str { target, .. } => target.id,
            CmpRegs::Ptr { target, .. } => target.id,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Terminator {
    Jump(BlockId),
    Branch {
        cond: Reg<Bool>,
        true_block: BlockId,
        false_block: BlockId,
    },
    Halt,
    // The C-ABI boundary: the top-level block yields one table pointer
    // (or null) to the host — `ret ptr %vN` out of @glm_exec.
    Return(Reg<Ptr>),
}

#[derive(Debug, Clone)]
pub enum Instruction {
    LoadInt {
        target: Reg<Int>,
        val: i64,
    },
    LoadFloat {
        target: Reg<Float>,
        val: f64,
    },
    LoadBool {
        target: Reg<Bool>,
        val: bool,
    },
    LoadString {
        target: Reg<Str>,
        val: String,
    },
    LoadNull {
        target: Reg<Ptr>,
    },
    // The one instruction that sees the function parameter: binds the
    // host-passed `%args` pointer to a virtual register (lowered to an
    // identity GEP), so the script's `arg` identifier reads the
    // boundary table like any other SSA value.
    BindArgs {
        target: Reg<Ptr>,
    },
    Move(MoveRegs),

    TableNew {
        target: Reg<Ptr>,
        elem: StaticType,
        flags: u8,
    },
    TableGet(CellGet),
    TableReserve {
        table: Reg<Ptr>,
        bound: Reg<Int>,
    },
    TableSetFast(CellSetFast),
    TableSet(CellSet),
    TableFree {
        table: Reg<Ptr>,
    },
    /// The keep-free: glm_tbl_free_except — the deep free with
    /// pointer-identity skips, so a free can release a base table's
    /// shell and sibling rows while the rows handed to live borrowers
    /// (the keep registers: a return's own value, or outer bindings
    /// holding rows out of the dying tree) survive for their holders.
    /// A singleton slice lowers to the two-operand call; two or more
    /// to glm_tbl_free_except_n.
    TableFreeExcept {
        table: Reg<Ptr>,
        keeps: Vec<Reg<Ptr>>,
    },

    SysAllocCount {
        target: Reg<Int>,
    },

    Add(NumRegs),
    Sub(NumRegs),
    Mul(NumRegs),
    Sitofp {
        target: Reg<Float>,
        source: Reg<Int>,
    },
    Div {
        target: Reg<Float>,
        left: Reg<Float>,
        right: Reg<Float>,
    },
    IntDiv(NumRegsRhs),
    Mod(NumRegsRhs),
    Neg(UnaryNum),
    Less(CmpRegs),
    Leq(CmpRegs),
    Geq(CmpRegs),
    Eq(CmpRegs),
    Not {
        target: Reg<Bool>,
        source: Reg<Bool>,
    },

    Phi(PhiRegs),

    Print {
        operands: Vec<(RegId, StaticType)>,
    },
}

impl Instruction {
    pub fn def_reg(&self) -> Option<RegId> {
        match self {
            Instruction::LoadInt { target, .. } => Some(target.id),
            Instruction::LoadFloat { target, .. } => Some(target.id),
            Instruction::LoadBool { target, .. } => Some(target.id),
            Instruction::LoadString { target, .. } => Some(target.id),
            Instruction::LoadNull { target } => Some(target.id),
            Instruction::BindArgs { target } => Some(target.id),
            Instruction::TableNew { target, .. } => Some(target.id),
            Instruction::Sitofp { target, .. } => Some(target.id),
            Instruction::Div { target, .. } => Some(target.id),
            Instruction::Not { target, .. } => Some(target.id),
            Instruction::SysAllocCount { target } => Some(target.id),
            Instruction::Move(m) => Some(m.target_id()),
            Instruction::TableGet(g) => Some(g.target_id()),
            Instruction::Add(n) | Instruction::Sub(n) | Instruction::Mul(n) => Some(n.target_id()),
            Instruction::IntDiv(n) | Instruction::Mod(n) => Some(n.target_id()),
            Instruction::Neg(u) => Some(u.target_id()),
            Instruction::Less(c)
            | Instruction::Leq(c)
            | Instruction::Geq(c)
            | Instruction::Eq(c) => Some(c.target_id()),
            Instruction::Phi(p) => Some(p.target_id()),
            Instruction::Print { .. }
            | Instruction::TableReserve { .. }
            | Instruction::TableSetFast { .. }
            | Instruction::TableSet { .. }
            | Instruction::TableFree { .. }
            | Instruction::TableFreeExcept { .. } => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BasicBlock {
    pub id: BlockId,
    pub instrs: Vec<Instruction>,
    pub terminator: Option<Terminator>,
}

impl BasicBlock {
    pub fn new(id: BlockId) -> Self {
        Self {
            id,
            instrs: Vec::new(),
            terminator: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct IrProgram {
    pub blocks: Vec<BasicBlock>,
}
