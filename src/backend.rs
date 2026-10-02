use crate::ast::StaticType;
use crate::ir::{
    BlockId, Bool, CellGet, CellSet, CellSetFast, CmpRegs, Instruction, Int, IrProgram, MoveRegs,
    NumRegs, NumRegsRhs, PhiRegs, Ptr, Reg, Repr, Terminator, UnaryNum,
};
use crate::shape::LayoutVerdict;
use glm_rt::trace;
use std::collections::HashMap;

fn elem_size(ty: &StaticType) -> u32 {
    match ty {
        StaticType::Boolean => 1,
        StaticType::Unknown(_) => 1,
        _ => 8,
    }
}

fn llvm_bytes(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        match b {
            b'"' => out.push_str("\\22"),
            b'\\' => out.push_str("\\5C"),
            0x20..=0x7E => out.push(b as char),
            _ => out.push_str(&format!("\\{b:02X}")),
        }
    }
    out
}

pub fn generate_llvm_ir(program: &IrProgram) -> Result<String, Vec<String>> {
    let mut globals = String::new();
    // The compile-time string pool: literal bytes -> global name. Each
    // distinct literal materializes once; every repeat GEPs the same
    // global, so .rodata holds one copy per distinct literal and all
    // uses of that literal share one address. Pool strings are
    // process-immortal constants — no runtime allocation, no free —
    // which is why string cells ride the deep-free exemption.
    let mut str_pool: HashMap<&str, String> = HashMap::new();
    let mut str_idx = 0;
    let mut needs_floor_decl = false;
    let mut needs_div_guard = false;
    let mut needs_tbl_new_decl = false;
    let mut needs_tbl_reserve_decl = false;
    let mut needs_tbl_free_decl = false;
    let mut needs_tbl_free_except_decl = false;
    let mut needs_tbl_free_except_n_decl = false;
    let mut needs_tbl_get_decl = false;
    let mut needs_tbl_set_decl = false;
    let mut needs_sys_alloc_count_decl = false;
    let mut needs_hdr_md = false;
    let mut ts = 0usize;
    // The keep-array scratch counter: one entry alloca per multi-keep
    // call site (never inside a loop body's block — the entry slot is
    // reused by stores before each call).
    let mut ks = 0usize;
    let tail: Vec<String> = {
        let mut probe = 0usize;
        let mut tails: Vec<String> = program
            .blocks
            .iter()
            .map(|b| format!("b{}", b.id))
            .collect();
        for block in &program.blocks {
            let mut last = None;
            for instr in &block.instrs {
                match instr {
                    Instruction::TableSetFast { .. } => {
                        last = Some(probe);
                        probe += 1;
                    }
                    Instruction::TableGet { .. } | Instruction::TableSet { .. } => probe += 1,
                    _ => {}
                }
            }
            if let Some(k) = last {
                tails[block.id] = format!("bts{}cont", k);
            }
        }
        tails
    };

    let mut allocas = String::new();
    let mut code = String::new();

    for block in &program.blocks {
        code.push_str(&format!("\nb{}:\n", block.id));

        for instr in &block.instrs {
            match instr {
                Instruction::LoadInt { target, val } => {
                    code.push_str(&format!("  %v{} = add i64 0, {}\n", target.id, val))
                }
                Instruction::LoadFloat { target, val } => {
                    code.push_str(&format!("  %v{} = fadd double 0.0, {:?}\n", target.id, val))
                }
                Instruction::LoadBool { target, val } => code.push_str(&format!(
                    "  %v{} = or i1 0, {}\n",
                    target.id,
                    if *val { 1 } else { 0 }
                )),
                Instruction::LoadString { target, val } => {
                    let g = match str_pool.get(val.as_str()) {
                        Some(name) => {
                            trace::compiler_trace_signal(trace::TRACE_STR_POOL_HIT);
                            name.clone()
                        }
                        None => {
                            // The one intern path: the only site that
                            // writes a string global.
                            let g = format!("@.str.{}", str_idx);
                            str_idx += 1;
                            globals.push_str(&format!(
                                "{} = private unnamed_addr constant [{} x i8] c\"{}\\00\"\n",
                                g,
                                val.len() + 1,
                                llvm_bytes(val.as_bytes())
                            ));
                            str_pool.insert(val.as_str(), g.clone());
                            g
                        }
                    };
                    code.push_str(&format!(
                        "  %v{} = getelementptr inbounds [{} x i8], ptr {}, i64 0, i64 0\n",
                        target.id,
                        val.len() + 1,
                        g
                    ));
                }
                Instruction::Move(m) => emit_move(m, &mut code),
                Instruction::LoadNull { target } => {
                    code.push_str(&format!("  %v{} = inttoptr i64 0 to ptr\n", target.id));
                }
                Instruction::BindArgs { target } => {
                    // Identity-GEP the function parameter into the SSA
                    // space: the host-passed boundary table becomes the
                    // register behind the script's `arg` identifier.
                    code.push_str(&format!(
                        "  %v{} = getelementptr i8, ptr %args, i64 0\n",
                        target.id
                    ));
                }

                Instruction::TableNew {
                    target,
                    elem,
                    flags,
                } => {
                    trace::compiler_trace_signal(trace::TRACE_OFFLOAD_EMIT);

                    needs_tbl_new_decl = true;
                    needs_hdr_md = true;

                    code.push_str(&format!(
                        "  %v{} = call ptr @glm_tbl_new(i64 {}, i8 {})\n",
                        target.id,
                        elem_size(elem),
                        flags
                    ));
                }
                Instruction::TableGet(g) => {
                    emit_cell_get(g, &mut allocas, &mut code, &mut ts, &mut needs_tbl_get_decl);
                }
                Instruction::TableReserve { table, bound } => {
                    needs_tbl_reserve_decl = true;
                    code.push_str(&format!(
                        "  call void @glm_tbl_reserve(ptr %v{}, i64 %v{})\n",
                        table.id, bound.id
                    ));
                }

                Instruction::TableSetFast(s) => {
                    emit_cell_set_fast(
                        s,
                        &mut allocas,
                        &mut code,
                        &mut ts,
                        &mut needs_hdr_md,
                        &mut needs_tbl_set_decl,
                    );
                }
                Instruction::TableSet(s) => {
                    emit_cell_set(s, &mut allocas, &mut code, &mut ts, &mut needs_tbl_set_decl);
                }
                Instruction::TableFree { table } => {
                    trace::compiler_trace_signal(trace::TRACE_OFFLOAD_EMIT);

                    needs_tbl_free_decl = true;
                    code.push_str(&format!("  call void @glm_tbl_free(ptr %v{})\n", table.id));
                }
                Instruction::TableFreeExcept { table, keeps } => {
                    trace::compiler_trace_signal(trace::TRACE_OFFLOAD_EMIT);

                    if keeps.len() == 1 {
                        // The hot singleton face (the return path): the
                        // two-operand call, byte-identical to the single-keep
                        // era.
                        needs_tbl_free_except_decl = true;
                        code.push_str(&format!(
                            "  call void @glm_tbl_free_except(ptr %v{}, ptr %v{})\n",
                            table.id, keeps[0].id
                        ));
                    } else {
                        // The multi-keep face: one entry scratch array,
                        // filled by stores at the call site, walked by
                        // glm_tbl_free_except_n.
                        needs_tbl_free_except_n_decl = true;
                        let f = ks;
                        ks += 1;
                        allocas.push_str(&format!(
                            "  %kf{f} = alloca [{n} x ptr]\n",
                            f = f,
                            n = keeps.len()
                        ));
                        for (i, k) in keeps.iter().enumerate() {
                            code.push_str(&format!(
                                "  store ptr %v{k}, ptr %kf{f}, i64 {i}\n",
                                k = k.id,
                                f = f,
                                i = i
                            ));
                        }
                        code.push_str(&format!(
                            "  call void @glm_tbl_free_except_n(ptr %v{t}, ptr %kf{f}, i64 {n})\n",
                            t = table.id,
                            f = f,
                            n = keeps.len()
                        ));
                    }
                }
                Instruction::SysAllocCount { target } => {
                    needs_sys_alloc_count_decl = true;
                    code.push_str(&format!(
                        "  %v{} = call i64 @sys_alloc_count()\n",
                        target.id
                    ));
                }
                Instruction::Add(n) => math_regs(n, "add", "fadd", &mut code),
                Instruction::Sub(n) => math_regs(n, "sub", "fsub", &mut code),
                Instruction::Mul(n) => math_regs(n, "mul", "fmul", &mut code),
                Instruction::Div {
                    target,
                    left,
                    right,
                } => {
                    code.push_str(&format!(
                        "  %v{} = fdiv double %v{}, %v{}\n",
                        target.id, left.id, right.id
                    ));
                }
                Instruction::Sitofp { target, source } => {
                    code.push_str(&format!(
                        "  %v{} = sitofp i64 %v{} to double\n",
                        target.id, source.id
                    ));
                }
                Instruction::IntDiv(n) => {
                    floor_div_regs(n, &mut code, &mut needs_floor_decl, &mut needs_div_guard)
                }
                Instruction::Mod(n) => mod_regs(n, &mut code, &mut needs_div_guard),
                Instruction::Neg(u) => neg_regs(u, &mut code),
                Instruction::Less(c) => cmp_regs(c, "slt", "olt", &mut code),
                Instruction::Leq(c) => cmp_regs(c, "sle", "ole", &mut code),
                Instruction::Geq(c) => cmp_regs(c, "sge", "oge", &mut code),
                Instruction::Eq(c) => cmp_regs(c, "eq", "oeq", &mut code),
                Instruction::Not { target, source } => {
                    code.push_str(&format!("  %v{} = xor i1 %v{}, 1\n", target.id, source.id));
                }
                Instruction::Phi(PhiRegs::Int { target, args }) => {
                    emit_phi(target, args, &tail, &mut code)
                }
                Instruction::Phi(PhiRegs::Float { target, args }) => {
                    emit_phi(target, args, &tail, &mut code)
                }
                Instruction::Phi(PhiRegs::Bool { target, args }) => {
                    emit_phi(target, args, &tail, &mut code)
                }
                Instruction::Phi(PhiRegs::Ptr { target, args }) => {
                    emit_phi(target, args, &tail, &mut code)
                }
                Instruction::Print { operands } => {
                    for (i, (r, ty)) in operands.iter().enumerate() {
                        if i > 0 {
                            code.push_str("  call void @glm_print_sep()\n");
                        }
                        let (r, ty) = (*r, ty);
                        match ty {
                            StaticType::Integer => {
                                code.push_str(&format!("  call void @glm_print_int(i64 %v{})\n", r))
                            }
                            StaticType::Float => code.push_str(&format!(
                                "  call void @glm_print_float(double %v{})\n",
                                r
                            )),
                            StaticType::Boolean => {
                                code.push_str(&format!("  call void @glm_print_bool(i1 %v{})\n", r))
                            }
                            StaticType::String => code
                                .push_str(&format!("  call void @glm_print_string(ptr %v{})\n", r)),
                            StaticType::Table(_) => {
                                return Err(vec![
                                    "Type Error: tables cannot be printed".to_string(),
                                ]);
                            }
                            StaticType::Unknown(_) => {
                                code.push_str("  call void @glm_print_int(i64 0)\n")
                            }
                        }
                    }
                    code.push_str("  call void @glm_print_nl()\n");
                }
            }
        }

        match &block.terminator {
            Some(Terminator::Jump(b)) => code.push_str(&format!("  br label %b{}\n", b)),
            Some(Terminator::Branch {
                cond,
                true_block,
                false_block,
            }) => code.push_str(&format!(
                "  br i1 %v{}, label %b{}, label %b{}\n",
                cond.id, true_block, false_block
            )),
            // The boundary value: a returned table pointer crosses to
            // the host; every other fall-out of the block returns null.
            Some(Terminator::Return(val)) => {
                code.push_str(&format!("  ret ptr %v{}\n", val.id))
            }
            Some(Terminator::Halt) | None => code.push_str("  ret ptr null\n"),
        }
    }

    let mut out = String::from(
        "declare void @glm_print_int(i64)\n\
         declare void @glm_print_float(double)\n\
         declare void @glm_print_bool(i1)\n\
         declare void @glm_print_string(ptr)\n\
         declare void @glm_print_sep()\n\
         declare void @glm_print_nl()\n\n\
         define ptr @glm_exec(ptr %args) {\nentry:\n",
    );
    out.push_str(&allocas);
    out.push_str("  br label %b0\n");
    out.push_str(&code);
    out.push_str("}\n");

    let mut head = String::new();
    if needs_floor_decl {
        head.push_str("declare double @llvm.floor.f64(double)\n");
    }
    if needs_tbl_new_decl {
        head.push_str("declare ptr @glm_tbl_new(i64, i8)\n");
    }
    if needs_tbl_reserve_decl {
        head.push_str("declare void @glm_tbl_reserve(ptr, i64)\n");
    }
    if needs_tbl_free_decl {
        head.push_str("declare void @glm_tbl_free(ptr)\n");
    }
    if needs_tbl_free_except_decl {
        head.push_str("declare void @glm_tbl_free_except(ptr, ptr)\n");
    }
    if needs_tbl_free_except_n_decl {
        head.push_str("declare void @glm_tbl_free_except_n(ptr, ptr, i64)\n");
    }
    if needs_tbl_get_decl {
        head.push_str("declare void @glm_tbl_get(ptr, i64, ptr, i64)\n");
    }
    if needs_tbl_set_decl {
        head.push_str("declare void @glm_tbl_set(ptr, i64, ptr)\n");
    }
    if needs_sys_alloc_count_decl {
        head.push_str("declare i64 @sys_alloc_count()\n");
    }
    if needs_div_guard {
        head.push_str("declare void @glm_div_zero_guard(i64)\n");
    }

    let md = if needs_hdr_md {
        "\n!0 = !{!1}\n\
         !1 = distinct !{!\"glm_table_header\", !2}\n\
         !2 = distinct !{!\"glm_table\"}\n"
            .to_string()
    } else {
        String::new()
    };
    Ok(format!("{}{}{}{}", globals, head, out, md))
}

fn emit_move(m: &MoveRegs, code: &mut String) {
    match m {
        MoveRegs::Int { target, source } => Repr::emit_move(target, source, code),
        MoveRegs::Float { target, source } => Repr::emit_move(target, source, code),
        MoveRegs::Bool { target, source } => Repr::emit_move(target, source, code),
        MoveRegs::Str { target, source } => Repr::emit_move(target, source, code),
        MoveRegs::Ptr { target, source } => Repr::emit_move(target, source, code),
        MoveRegs::Byte { target, source } => Repr::emit_move(target, source, code),
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_cell_get(
    g: &CellGet,
    allocas: &mut String,
    code: &mut String,
    ts: &mut usize,
    needs_tbl_get_decl: &mut bool,
) {
    match g {
        CellGet::Int {
            target,
            table,
            index,
        } => emit_table_get(target, table, index, allocas, code, ts, needs_tbl_get_decl),
        CellGet::Float {
            target,
            table,
            index,
        } => emit_table_get(target, table, index, allocas, code, ts, needs_tbl_get_decl),
        CellGet::Bool {
            target,
            table,
            index,
        } => emit_table_get(target, table, index, allocas, code, ts, needs_tbl_get_decl),
        CellGet::Ptr {
            target,
            table,
            index,
        } => emit_table_get(target, table, index, allocas, code, ts, needs_tbl_get_decl),
        CellGet::Byte {
            target,
            table,
            index,
        } => emit_table_get(target, table, index, allocas, code, ts, needs_tbl_get_decl),
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_cell_set_fast(
    s: &CellSetFast,
    allocas: &mut String,
    code: &mut String,
    ts: &mut usize,
    needs_hdr_md: &mut bool,
    needs_tbl_set_decl: &mut bool,
) {
    match s {
        CellSetFast::Int {
            table,
            index,
            value,
            layout,
        } => emit_table_set_fast(
            table,
            index,
            value,
            layout,
            allocas,
            code,
            ts,
            needs_hdr_md,
            needs_tbl_set_decl,
        ),
        CellSetFast::Float {
            table,
            index,
            value,
            layout,
        } => emit_table_set_fast(
            table,
            index,
            value,
            layout,
            allocas,
            code,
            ts,
            needs_hdr_md,
            needs_tbl_set_decl,
        ),
        CellSetFast::Bool {
            table,
            index,
            value,
            layout,
        } => emit_table_set_fast(
            table,
            index,
            value,
            layout,
            allocas,
            code,
            ts,
            needs_hdr_md,
            needs_tbl_set_decl,
        ),
        CellSetFast::Ptr {
            table,
            index,
            value,
            layout,
        } => emit_table_set_fast(
            table,
            index,
            value,
            layout,
            allocas,
            code,
            ts,
            needs_hdr_md,
            needs_tbl_set_decl,
        ),
        CellSetFast::Byte {
            table,
            index,
            value,
            layout,
        } => emit_table_set_fast(
            table,
            index,
            value,
            layout,
            allocas,
            code,
            ts,
            needs_hdr_md,
            needs_tbl_set_decl,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_cell_set(
    s: &CellSet,
    allocas: &mut String,
    code: &mut String,
    ts: &mut usize,
    needs_tbl_set_decl: &mut bool,
) {
    match s {
        CellSet::Int {
            table,
            index,
            value,
        } => emit_table_set(table, index, value, allocas, code, ts, needs_tbl_set_decl),
        CellSet::Float {
            table,
            index,
            value,
        } => emit_table_set(table, index, value, allocas, code, ts, needs_tbl_set_decl),
        CellSet::Bool {
            table,
            index,
            value,
        } => emit_table_set(table, index, value, allocas, code, ts, needs_tbl_set_decl),
        CellSet::Ptr {
            table,
            index,
            value,
        } => emit_table_set(table, index, value, allocas, code, ts, needs_tbl_set_decl),
        CellSet::Byte {
            table,
            index,
            value,
        } => emit_table_set(table, index, value, allocas, code, ts, needs_tbl_set_decl),
    }
}

fn math_regs(n: &NumRegs, int_op: &str, flt_op: &str, code: &mut String) {
    match n {
        NumRegs::Int {
            target,
            left,
            right,
        } => math_op(target, left, right, int_op, flt_op, code),
        NumRegs::Float {
            target,
            left,
            right,
        } => math_op(target, left, right, int_op, flt_op, code),
    }
}

fn floor_div_regs(
    n: &NumRegsRhs,
    code: &mut String,
    needs_floor_decl: &mut bool,
    needs_div_guard: &mut bool,
) {
    match n {
        NumRegsRhs::Int {
            target,
            left,
            right,
            rhs_const,
        } => floor_div_op(
            target,
            left,
            right,
            code,
            needs_floor_decl,
            needs_div_guard,
            *rhs_const,
        ),
        NumRegsRhs::Float {
            target,
            left,
            right,
            rhs_const,
        } => floor_div_op(
            target,
            left,
            right,
            code,
            needs_floor_decl,
            needs_div_guard,
            *rhs_const,
        ),
    }
}

fn mod_regs(n: &NumRegsRhs, code: &mut String, needs_div_guard: &mut bool) {
    match n {
        NumRegsRhs::Int {
            target,
            left,
            right,
            rhs_const,
        } => mod_op(target, left, right, code, needs_div_guard, *rhs_const),
        NumRegsRhs::Float {
            target,
            left,
            right,
            rhs_const,
        } => mod_op(target, left, right, code, needs_div_guard, *rhs_const),
    }
}

fn neg_regs(u: &UnaryNum, code: &mut String) {
    match u {
        UnaryNum::Int { target, source } => neg_op(target, source, code),
        UnaryNum::Float { target, source } => neg_op(target, source, code),
    }
}

fn cmp_regs(c: &CmpRegs, int_cond: &str, flt_cond: &str, code: &mut String) {
    match c {
        CmpRegs::Int {
            target,
            left,
            right,
        } => cmp_op(target, left, right, int_cond, flt_cond, code),
        CmpRegs::Float {
            target,
            left,
            right,
        } => cmp_op(target, left, right, int_cond, flt_cond, code),
        CmpRegs::Bool {
            target,
            left,
            right,
        } => cmp_op(target, left, right, int_cond, flt_cond, code),
        CmpRegs::Str {
            target,
            left,
            right,
        } => cmp_op(target, left, right, int_cond, flt_cond, code),
        CmpRegs::Ptr {
            target,
            left,
            right,
        } => cmp_op(target, left, right, int_cond, flt_cond, code),
    }
}

fn emit_phi<R: Repr>(
    target: &Reg<R>,
    args: &[(BlockId, Reg<R>)],
    tail: &[String],
    code: &mut String,
) {
    let pairs: Vec<String> = args
        .iter()
        .map(|(b, r)| format!("[ %v{}, %{} ]", r.id, tail[*b]))
        .collect();
    code.push_str(&format!(
        "  %v{} = phi {} {}\n",
        target.id,
        R::llvm(),
        pairs.join(", ")
    ));
}

#[allow(clippy::too_many_arguments)]
fn emit_table_get<E: Repr>(
    target: &Reg<E>,
    table: &Reg<Ptr>,
    index: &Reg<Int>,
    allocas: &mut String,
    code: &mut String,
    ts: &mut usize,
    needs_tbl_get_decl: &mut bool,
) {
    *needs_tbl_get_decl = true;
    let ety = E::storage();
    let f = *ts;
    *ts += 1;

    allocas.push_str(&format!("  %ts{f}.dst = alloca {ety}\n", f = f, ety = ety));

    code.push_str(&format!(
        "  call void @glm_tbl_get(ptr %v{table}, i64 %v{index}, ptr %ts{f}.dst, i64 {esize})\n",
        f = f,
        table = table.id,
        index = index.id,
        esize = E::esize()
    ));

    if E::IS_PACKED {
        code.push_str(&format!(
            "  %ts{f}.c = load i8, ptr %ts{f}.dst\n\
               %v{target} = icmp ne i8 %ts{f}.c, 0\n",
            f = f,
            target = target.id
        ));
    } else {
        code.push_str(&format!(
            "  %v{target} = load {ety}, ptr %ts{f}.dst\n",
            target = target.id,
            ety = ety,
            f = f
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_table_set_fast<E: Repr>(
    table: &Reg<Ptr>,
    index: &Reg<Int>,
    value: &Reg<E>,
    layout: &LayoutVerdict,
    allocas: &mut String,
    code: &mut String,
    ts: &mut usize,
    needs_hdr_md: &mut bool,
    needs_tbl_set_decl: &mut bool,
) {
    *needs_hdr_md = true;
    let ety = E::storage();
    let f = *ts;
    *ts += 1;

    let cast_var = format!("%ts{}.c", f);

    let val_cast = if E::IS_PACKED {
        format!(
            "  {cv} = zext i1 %v{value} to i8\n",
            cv = cast_var,
            value = value.id
        )
    } else {
        String::new()
    };
    let val_use = if E::IS_PACKED {
        cast_var
    } else {
        format!("%v{value}", value = value.id)
    };

    allocas.push_str(&format!("  %ts{f}.valp = alloca {ety}\n", f = f, ety = ety));

    match layout {
        LayoutVerdict::Dense => {
            code.push_str(&format!(
                "{val_cast}\
                   %ts{f}.d = load ptr, ptr %v{table}, !alias.scope !0\n\
                   %ts{f}.s = getelementptr inbounds {ety}, ptr %ts{f}.d, i64 %v{index}\n\
                   store {ety} {val_use}, ptr %ts{f}.s, !noalias !0\n\
                   br label %bts{f}cont\n\n\
                 bts{f}cont:\n",
                val_cast = val_cast,
                f = f,
                table = table.id,
                index = index.id,
                ety = ety,
                val_use = val_use
            ));
        }
        LayoutVerdict::Sparse => {
            *needs_tbl_set_decl = true;
            code.push_str(&format!(
                "{val_cast}\
                   store {ety} {val_use}, ptr %ts{f}.valp\n\
                   call void @glm_tbl_set(ptr %v{table}, i64 %v{index}, ptr %ts{f}.valp)\n\
                   br label %bts{f}cont\n\n\
                 bts{f}cont:\n",
                val_cast = val_cast,
                f = f,
                table = table.id,
                index = index.id,
                ety = ety,
                val_use = val_use
            ));
        }
        _ => {
            *needs_tbl_set_decl = true;
            code.push_str(&format!(
                "{val_cast}\
                   %ts{f}.modep = getelementptr inbounds i8, ptr %v{table}, i64 32\n\
                   %ts{f}.mode = load i8, ptr %ts{f}.modep, !alias.scope !0\n\
                   %ts{f}.is_dense = icmp eq i8 %ts{f}.mode, 0\n\
                   br i1 %ts{f}.is_dense, label %bts{f}dense, label %bts{f}sparse\n\n\
                 bts{f}dense:\n\
                   %ts{f}.d = load ptr, ptr %v{table}, !alias.scope !0\n\
                   %ts{f}.s = getelementptr inbounds {ety}, ptr %ts{f}.d, i64 %v{index}\n\
                   store {ety} {val_use}, ptr %ts{f}.s, !noalias !0\n\
                   br label %bts{f}cont\n\n\
                 bts{f}sparse:\n\
                   store {ety} {val_use}, ptr %ts{f}.valp\n\
                   call void @glm_tbl_set(ptr %v{table}, i64 %v{index}, ptr %ts{f}.valp)\n\
                   br label %bts{f}cont\n\n\
                 bts{f}cont:\n",
                val_cast = val_cast,
                f = f,
                table = table.id,
                index = index.id,
                ety = ety,
                val_use = val_use
            ));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_table_set<E: Repr>(
    table: &Reg<Ptr>,
    index: &Reg<Int>,
    value: &Reg<E>,
    allocas: &mut String,
    code: &mut String,
    ts: &mut usize,
    needs_tbl_set_decl: &mut bool,
) {
    *needs_tbl_set_decl = true;
    let ety = E::storage();
    let f = *ts;
    *ts += 1;

    allocas.push_str(&format!("  %ts{f}.valp = alloca {ety}\n", f = f, ety = ety));

    if E::IS_PACKED {
        code.push_str(&format!(
            "  %ts{f}.z = zext i1 %v{value} to i8\n\
               store i8 %ts{f}.z, ptr %ts{f}.valp\n",
            f = f,
            value = value.id
        ));
    } else {
        code.push_str(&format!(
            "  store {ety} %v{value}, ptr %ts{f}.valp\n",
            ety = ety,
            value = value.id,
            f = f
        ));
    }
    code.push_str(&format!(
        "  call void @glm_tbl_set(ptr %v{table}, i64 %v{index}, ptr %ts{f}.valp)\n",
        table = table.id,
        index = index.id,
        f = f
    ));
}

fn math_op<N: Repr>(
    target: &Reg<N>,
    left: &Reg<N>,
    right: &Reg<N>,
    int_op: &str,
    flt_op: &str,
    code: &mut String,
) {
    if N::IS_FLOAT {
        code.push_str(&format!(
            "  %v{} = {} double %v{}, %v{}\n",
            target.id, flt_op, left.id, right.id
        ));
    } else {
        code.push_str(&format!(
            "  %v{} = {} i64 %v{}, %v{}\n",
            target.id, int_op, left.id, right.id
        ));
    }
}

fn neg_op<N: Repr>(target: &Reg<N>, source: &Reg<N>, code: &mut String) {
    if N::IS_FLOAT {
        code.push_str(&format!(
            "  %v{} = fsub double 0.0, %v{}\n",
            target.id, source.id
        ));
    } else {
        code.push_str(&format!("  %v{} = sub i64 0, %v{}\n", target.id, source.id));
    }
}

#[allow(clippy::too_many_arguments)]
fn floor_div_op<N: Repr>(
    target: &Reg<N>,
    left: &Reg<N>,
    right: &Reg<N>,
    code: &mut String,
    needs_floor_decl: &mut bool,
    needs_div_guard: &mut bool,
    rhs_const: Option<i64>,
) {
    if N::IS_FLOAT {
        *needs_floor_decl = true;
        code.push_str(&format!(
            "  %t{}.0 = fdiv double %v{}, %v{}\n",
            target.id, left.id, right.id
        ));
        code.push_str(&format!(
            "  %v{} = call double @llvm.floor.f64(double %t{}.0)\n",
            target.id, target.id
        ));
    } else {
        let divsr: String = match rhs_const {
            Some(k) if k != 0 => format!("%v{right}", right = right.id),
            _ => {
                *needs_div_guard = true;
                code.push_str(&format!(
                    "  call void @glm_div_zero_guard(i64 %v{})\n",
                    right.id
                ));
                code.push_str(&format!(
                    "  %t{}.g0 = icmp eq i64 %v{}, -1\n",
                    target.id, right.id
                ));
                code.push_str(&format!(
                    "  %t{}.g1 = select i1 %t{}.g0, i64 1, i64 %v{}\n",
                    target.id, target.id, right.id
                ));
                format!("%t{target}.g1", target = target.id)
            }
        };
        code.push_str(&format!(
            "  %t{}.0 = sdiv i64 %v{}, {}\n",
            target.id, left.id, divsr
        ));
        code.push_str(&format!(
            "  %t{}.1 = srem i64 %v{}, {}\n",
            target.id, left.id, divsr
        ));
        code.push_str(&format!(
            "  %t{}.2 = icmp ne i64 %t{}.1, 0\n",
            target.id, target.id
        ));
        code.push_str(&format!(
            "  %t{}.3 = xor i64 %v{}, %v{}\n",
            target.id, left.id, right.id
        ));
        code.push_str(&format!(
            "  %t{}.4 = icmp slt i64 %t{}.3, 0\n",
            target.id, target.id
        ));
        code.push_str(&format!(
            "  %t{}.5 = and i1 %t{}.2, %t{}.4\n",
            target.id, target.id, target.id
        ));
        code.push_str(&format!(
            "  %t{}.6 = sub i64 %t{}.0, 1\n",
            target.id, target.id
        ));
        code.push_str(&format!(
            "  %v{} = select i1 %t{}.5, i64 %t{}.6, i64 %t{}.0\n",
            target.id, target.id, target.id, target.id
        ));
    }
}

fn mod_op<N: Repr>(
    target: &Reg<N>,
    left: &Reg<N>,
    right: &Reg<N>,
    code: &mut String,
    needs_div_guard: &mut bool,
    rhs_const: Option<i64>,
) {
    if N::IS_FLOAT {
        code.push_str(&format!(
            "  %t{}.0 = frem double %v{}, %v{}\n",
            target.id, left.id, right.id
        ));
        code.push_str(&format!(
            "  %t{}.1 = fcmp ogt double %t{}.0, 0.0\n",
            target.id, target.id
        ));
        code.push_str(&format!(
            "  %t{}.2 = fcmp olt double %v{}, 0.0\n",
            target.id, right.id
        ));
        code.push_str(&format!(
            "  %t{}.3 = and i1 %t{}.1, %t{}.2\n",
            target.id, target.id, target.id
        ));
        code.push_str(&format!(
            "  %t{}.4 = fcmp olt double %t{}.0, 0.0\n",
            target.id, target.id
        ));
        code.push_str(&format!(
            "  %t{}.5 = fcmp ogt double %v{}, 0.0\n",
            target.id, right.id
        ));
        code.push_str(&format!(
            "  %t{}.6 = and i1 %t{}.4, %t{}.5\n",
            target.id, target.id, target.id
        ));
        code.push_str(&format!(
            "  %t{}.7 = or i1 %t{}.3, %t{}.6\n",
            target.id, target.id, target.id
        ));
        code.push_str(&format!(
            "  %t{}.8 = fadd double %t{}.0, %v{}\n",
            target.id, target.id, right.id
        ));
        code.push_str(&format!(
            "  %v{} = select i1 %t{}.7, double %t{}.8, double %t{}.0\n",
            target.id, target.id, target.id, target.id
        ));
    } else {
        let divsr: String = match rhs_const {
            Some(k) if k != 0 => format!("%v{right}", right = right.id),
            _ => {
                *needs_div_guard = true;
                code.push_str(&format!(
                    "  call void @glm_div_zero_guard(i64 %v{})\n",
                    right.id
                ));
                code.push_str(&format!(
                    "  %t{}.g0 = icmp eq i64 %v{}, -1\n",
                    target.id, right.id
                ));
                code.push_str(&format!(
                    "  %t{}.g1 = select i1 %t{}.g0, i64 1, i64 %v{}\n",
                    target.id, target.id, right.id
                ));
                format!("%t{target}.g1", target = target.id)
            }
        };
        code.push_str(&format!(
            "  %t{}.0 = srem i64 %v{}, {}\n",
            target.id, left.id, divsr
        ));
        code.push_str(&format!(
            "  %t{}.1 = icmp ne i64 %t{}.0, 0\n",
            target.id, target.id
        ));
        code.push_str(&format!(
            "  %t{}.2 = xor i64 %t{}.0, %v{}\n",
            target.id, target.id, right.id
        ));
        code.push_str(&format!(
            "  %t{}.3 = icmp slt i64 %t{}.2, 0\n",
            target.id, target.id
        ));
        code.push_str(&format!(
            "  %t{}.4 = and i1 %t{}.1, %t{}.3\n",
            target.id, target.id, target.id
        ));
        code.push_str(&format!(
            "  %t{}.5 = add i64 %t{}.0, %v{}\n",
            target.id, target.id, right.id
        ));
        code.push_str(&format!(
            "  %v{} = select i1 %t{}.4, i64 %t{}.5, i64 %t{}.0\n",
            target.id, target.id, target.id, target.id
        ));
    }
}

fn cmp_op<T: Repr>(
    target: &Reg<Bool>,
    left: &Reg<T>,
    right: &Reg<T>,
    int_cond: &str,
    flt_cond: &str,
    code: &mut String,
) {
    if T::IS_FLOAT {
        code.push_str(&format!(
            "  %v{} = fcmp {} double %v{}, %v{}\n",
            target.id, flt_cond, left.id, right.id
        ));
    } else {
        code.push_str(&format!(
            "  %v{} = icmp {} {} %v{}, %v{}\n",
            target.id,
            int_cond,
            T::llvm(),
            left.id,
            right.id
        ));
    }
}
