use super::{IrLowerer, LowerError};
use crate::ast::StaticType;
use crate::ir::{
    AnyReg, BlockId, Bool, Byte, CellRepr, CellTy, CellVal, CmpRegs, CmpRepr, Float, Instruction,
    Int, MoveRegs, NumRegs, NumRegsRhs, NumRepr, PhiRegs, PhiRepr, Ptr, Reg, RegKind,
};

pub(super) fn elem_of_ty(ty: &StaticType) -> Result<StaticType, LowerError> {
    match ty {
        StaticType::Table(elem) => Ok((**elem).clone()),
        _ => Err(LowerError(
            "Lower Error: a table operand position received a non-table — \
             the checker and the lowerer disagree"
                .into(),
        )),
    }
}

pub(super) fn int_of(r: AnyReg) -> Result<Reg<Int>, LowerError> {
    match r {
        AnyReg::Int(x) => Ok(x),
        _ => Err(repr_mismatch("an Integer operand position")),
    }
}

pub(super) fn float_of(r: AnyReg) -> Result<Reg<Float>, LowerError> {
    match r {
        AnyReg::Float(x) => Ok(x),
        _ => Err(repr_mismatch("a Float operand position")),
    }
}

pub(super) fn bool_of(r: AnyReg) -> Result<Reg<Bool>, LowerError> {
    match r {
        AnyReg::Bool(x) => Ok(x),
        _ => Err(repr_mismatch("a Bool operand position")),
    }
}

pub(super) fn ptr_of(r: AnyReg) -> Result<Reg<Ptr>, LowerError> {
    match r {
        AnyReg::Ptr(x) => Ok(x),
        _ => Err(repr_mismatch("a table-header operand position")),
    }
}

fn repr_mismatch(where_: &str) -> LowerError {
    LowerError(format!(
        "Lower Error: {where_} received a different repr — \
         the checker and the lowerer disagree"
    ))
}

pub(super) enum NumPair {
    II(Reg<Int>, Reg<Int>),
    FF(Reg<Float>, Reg<Float>),
}

pub(super) fn num_pair(l: AnyReg, r: AnyReg) -> Result<NumPair, LowerError> {
    match (l, r) {
        (AnyReg::Int(a), AnyReg::Int(b)) => Ok(NumPair::II(a, b)),
        (AnyReg::Float(a), AnyReg::Float(b)) => Ok(NumPair::FF(a, b)),
        _ => Err(repr_mismatch("arithmetic on")),
    }
}

pub(super) enum NumSingle {
    I(Reg<Int>),
    F(Reg<Float>),
}

pub(super) fn num_single(r: AnyReg) -> Result<NumSingle, LowerError> {
    match r {
        AnyReg::Int(a) => Ok(NumSingle::I(a)),
        AnyReg::Float(a) => Ok(NumSingle::F(a)),
        _ => Err(repr_mismatch("negation of")),
    }
}

pub(super) enum OrdPair {
    II(Reg<Int>, Reg<Int>),
    FF(Reg<Float>, Reg<Float>),
    BB(Reg<Bool>, Reg<Bool>),
    SS(Reg<crate::ir::Str>, Reg<crate::ir::Str>),
    PP(Reg<Ptr>, Reg<Ptr>),
}

pub(super) fn ord_pair(l: AnyReg, r: AnyReg) -> Result<OrdPair, LowerError> {
    match (l, r) {
        (AnyReg::Int(a), AnyReg::Int(b)) => Ok(OrdPair::II(a, b)),
        (AnyReg::Float(a), AnyReg::Float(b)) => Ok(OrdPair::FF(a, b)),
        (AnyReg::Bool(a), AnyReg::Bool(b)) => Ok(OrdPair::BB(a, b)),
        (AnyReg::Str(a), AnyReg::Str(b)) => Ok(OrdPair::SS(a, b)),
        (AnyReg::Ptr(a), AnyReg::Ptr(b)) => Ok(OrdPair::PP(a, b)),
        // A string that traveled through a cell or a join arrives as
        // Ptr — the id-preserved widening of the Str arm. The checker
        // proved both operands String, so the pair joins to Ptr and
        // compares by pool address.
        (AnyReg::Str(a), AnyReg::Ptr(b)) => Ok(OrdPair::PP(Reg::new(a.id), b)),
        (AnyReg::Ptr(a), AnyReg::Str(b)) => Ok(OrdPair::PP(a, Reg::new(b.id))),
        _ => Err(repr_mismatch("comparison of")),
    }
}

pub(super) fn phi_of(
    kind: RegKind,
    target: crate::ir::RegId,
    arms: Vec<(BlockId, AnyReg)>,
) -> Result<PhiRegs, LowerError> {
    fn narrow<K: PhiRepr>(
        target: crate::ir::RegId,
        arms: Vec<(BlockId, AnyReg)>,
    ) -> Result<PhiRegs, LowerError> {
        let mut typed = Vec::with_capacity(arms.len());
        for (block, reg) in arms {
            let reg = K::of(reg).ok_or_else(|| repr_mismatch("a join phi input position"))?;
            typed.push((block, reg));
        }
        Ok(K::phi(Reg::new(target), typed))
    }
    match kind {
        RegKind::Int => narrow::<Int>(target, arms),
        RegKind::Float => narrow::<Float>(target, arms),
        RegKind::Bool => narrow::<Bool>(target, arms),
        RegKind::Ptr => narrow::<Ptr>(target, arms),
    }
}

pub(super) fn phi_push(phi: &mut PhiRegs, block: BlockId, reg: AnyReg) -> Result<(), LowerError> {
    let mismatch = || repr_mismatch("a join phi input position");
    match phi {
        PhiRegs::Int { args, .. } => {
            args.push((block, <Int as PhiRepr>::of(reg).ok_or_else(mismatch)?))
        }
        PhiRegs::Float { args, .. } => {
            args.push((block, <Float as PhiRepr>::of(reg).ok_or_else(mismatch)?))
        }
        PhiRegs::Bool { args, .. } => {
            args.push((block, <Bool as PhiRepr>::of(reg).ok_or_else(mismatch)?))
        }
        PhiRegs::Ptr { args, .. } => {
            args.push((block, <Ptr as PhiRepr>::of(reg).ok_or_else(mismatch)?))
        }
    }
    Ok(())
}

impl IrLowerer<'_> {
    pub(super) fn num3<N: NumRepr>(
        &mut self,
        ctor: fn(NumRegs) -> Instruction,
        target: crate::ir::RegId,
        l: Reg<N>,
        r: Reg<N>,
    ) {
        self.emit(ctor(N::num3(Reg::new(target), l, r)));
    }

    pub(super) fn num3r<N: NumRepr>(
        &mut self,
        ctor: fn(NumRegsRhs) -> Instruction,
        target: crate::ir::RegId,
        l: Reg<N>,
        r: Reg<N>,
        rhs_const: Option<i64>,
    ) {
        self.emit(ctor(N::num3r(Reg::new(target), l, r, rhs_const)));
    }

    pub(super) fn neg1<N: NumRepr>(&mut self, target: crate::ir::RegId, source: Reg<N>) {
        self.emit(Instruction::Neg(N::num2(Reg::new(target), source)));
    }

    pub(super) fn cmp3<T: CmpRepr>(
        &mut self,
        ctor: fn(CmpRegs) -> Instruction,
        target: crate::ir::RegId,
        l: Reg<T>,
        r: Reg<T>,
    ) {
        self.emit(ctor(T::cmp(Reg::new(target), l, r)));
    }

    pub(super) fn emit_move_into(&mut self, target: crate::ir::RegId, source: AnyReg) -> AnyReg {
        let t = target;
        match source {
            AnyReg::Int(s) => {
                self.emit(Instruction::Move(MoveRegs::Int {
                    target: Reg::new(t),
                    source: s,
                }));
                AnyReg::Int(Reg::new(t))
            }
            AnyReg::Float(s) => {
                self.emit(Instruction::Move(MoveRegs::Float {
                    target: Reg::new(t),
                    source: s,
                }));
                AnyReg::Float(Reg::new(t))
            }
            AnyReg::Bool(s) => {
                self.emit(Instruction::Move(MoveRegs::Bool {
                    target: Reg::new(t),
                    source: s,
                }));
                AnyReg::Bool(Reg::new(t))
            }
            AnyReg::Str(s) => {
                self.emit(Instruction::Move(MoveRegs::Str {
                    target: Reg::new(t),
                    source: s,
                }));
                AnyReg::Str(Reg::new(t))
            }
            AnyReg::Ptr(s) => {
                self.emit(Instruction::Move(MoveRegs::Ptr {
                    target: Reg::new(t),
                    source: s,
                }));
                AnyReg::Ptr(Reg::new(t))
            }
            AnyReg::Byte(s) => {
                self.emit(Instruction::Move(MoveRegs::Byte {
                    target: Reg::new(t),
                    source: s,
                }));
                AnyReg::Byte(Reg::new(t))
            }
        }
    }

    pub(super) fn table_get(
        &mut self,
        target: crate::ir::RegId,
        table: Reg<Ptr>,
        index: Reg<Int>,
        elem: &StaticType,
    ) -> AnyReg {
        match CellTy::of(elem) {
            CellTy::Int => self.get::<Int>(target, table, index),
            CellTy::Float => self.get::<Float>(target, table, index),
            CellTy::Bool => self.get::<Bool>(target, table, index),
            CellTy::Ptr => self.get::<Ptr>(target, table, index),
            CellTy::Byte => self.get::<Byte>(target, table, index),
        }
    }

    fn get<E: CellRepr>(
        &mut self,
        target: crate::ir::RegId,
        table: Reg<Ptr>,
        index: Reg<Int>,
    ) -> AnyReg {
        self.emit(Instruction::TableGet(E::get(
            Reg::new(target),
            table,
            index,
        )));
        E::any(Reg::new(target))
    }

    pub(super) fn table_set(
        &mut self,
        table: Reg<Ptr>,
        index: Reg<Int>,
        value: AnyReg,
        elem: &StaticType,
    ) -> Result<(), LowerError> {
        match value.into_cell() {
            CellVal::Int(v) => self.set(table, index, v, elem),
            CellVal::Float(v) => self.set(table, index, v, elem),
            CellVal::Bool(v) => self.set(table, index, v, elem),
            CellVal::Ptr(v) => self.set(table, index, v, elem),
            CellVal::Byte(v) => self.set(table, index, v, elem),
        }
    }

    pub(super) fn table_set_fast(
        &mut self,
        table: Reg<Ptr>,
        index: Reg<Int>,
        value: AnyReg,
        elem: &StaticType,
        layout: crate::shape::LayoutVerdict,
    ) -> Result<(), LowerError> {
        match value.into_cell() {
            CellVal::Int(v) => self.set_fast(table, index, v, elem, layout),
            CellVal::Float(v) => self.set_fast(table, index, v, elem, layout),
            CellVal::Bool(v) => self.set_fast(table, index, v, elem, layout),
            CellVal::Ptr(v) => self.set_fast(table, index, v, elem, layout),
            CellVal::Byte(v) => self.set_fast(table, index, v, elem, layout),
        }
    }

    fn set<E: CellRepr>(
        &mut self,
        table: Reg<Ptr>,
        index: Reg<Int>,
        value: Reg<E>,
        elem: &StaticType,
    ) -> Result<(), LowerError> {
        if CellTy::of(elem) != E::TAG {
            return Err(LowerError(
                "Lower Error: a stored value's repr disagrees with the table's element type — \
                 the checker and the lowerer disagree"
                    .into(),
            ));
        }
        self.emit(Instruction::TableSet(E::set(table, index, value)));
        Ok(())
    }

    fn set_fast<E: CellRepr>(
        &mut self,
        table: Reg<Ptr>,
        index: Reg<Int>,
        value: Reg<E>,
        elem: &StaticType,
        layout: crate::shape::LayoutVerdict,
    ) -> Result<(), LowerError> {
        if CellTy::of(elem) != E::TAG {
            return Err(LowerError(
                "Lower Error: a stored value's repr disagrees with the table's element type — \
                 the checker and the lowerer disagree"
                    .into(),
            ));
        }
        self.emit(Instruction::TableSetFast(E::set_fast(
            table, index, value, layout,
        )));
        Ok(())
    }

    pub(super) fn promote_to_float(
        &mut self,
        reg: AnyReg,
        ty: &StaticType,
    ) -> Result<Reg<Float>, LowerError> {
        if matches!(ty, StaticType::Integer) {
            let source = int_of(reg)?;
            let target = self.next_reg();
            self.emit(Instruction::Sitofp {
                target: Reg::new(target),
                source,
            });
            Ok(Reg::new(target))
        } else {
            float_of(reg)
        }
    }
}
