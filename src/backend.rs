use crate::ast::StaticType;
use crate::ir::{
    BlockId, Bool, CellGet, CellSet, CellSetFast, CmpRegs, DbgBind, EntryKind, Instruction, Int,
    IrProgram, MoveRegs, NumRegs, NumRegsRhs, PhiRegs, Ptr, Reg, RegKind, Repr, SourceLoc,
    Terminator, UnaryNum,
};
use crate::shape::LayoutVerdict;
use glm_rt::trace;
use std::collections::HashMap;
use std::path::Path;

fn elem_size(ty: &StaticType) -> u32 {
    match ty {
        StaticType::Boolean => 1,
        StaticType::Unknown(_) => 1,
        StaticType::Any => 16,
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

// The debug-info node ids. !0..!2 stay reserved for the table-header
// alias-scope nodes the set_fast expansions reference by literal
// number; the numbered metadata space tolerates gaps, so the debug
// nodes start above the reservation whether or not a table was built.
const DI_FILE: usize = 3;
const DI_CU: usize = 4;
const DI_SP: usize = 5;
const DI_FN_TY: usize = 6;
const DI_PTR_TY: usize = 7;
const DI_I64_TY: usize = 8;
const DI_F64_TY: usize = 9;
const DI_BOOL_TY: usize = 10;
const DI_ANY_TY: usize = 11;
const DI_FLAGS: usize = 12; // ..=14: Dwarf Version, Debug Info Version, PIC Level
const DI_VAR_BASE: usize = 15;

/// The module's debug-info builder: fixed nodes for the compile unit,
/// the @glm_exec subprogram, and the value types, then !DILocation and
/// !DILocalVariable nodes interned in first-sight order — the same
/// determinism discipline as the string pool, so two compiles of one
/// script stay byte-identical.
struct DebugMeta {
    /// "!N = ..." definitions, in id order.
    defs: String,
    next: usize,
    locs: HashMap<(u32, u32), usize>,
    vars: HashMap<String, usize>,
}

impl DebugMeta {
    /// None when the program carries no source path — no file to name,
    /// no debug info (the host canonicalizes the path before the
    /// backend runs, so this never fires in the real pipeline).
    fn new(source: &Path) -> Option<Self> {
        let file_name = source.file_name()?.to_string_lossy().into_owned();
        let dir = source
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_string_lossy()
            .into_owned();
        let mut defs = String::new();
        defs.push_str(&format!(
            "!{DI_FILE} = !DIFile(filename: \"{}\", directory: \"{}\")\n",
            llvm_bytes(file_name.as_bytes()),
            llvm_bytes(dir.as_bytes())
        ));
        // The compile unit speaks C (DW_LANG_C99 — the widest debugger
        // support; no DWARF language code exists for Lua) with full
        // emission so the variable binds survive into DWARF.
        defs.push_str(&format!(
            "!{DI_CU} = distinct !DICompileUnit(language: DW_LANG_C99, file: !{DI_FILE}, \
             producer: \"glm\", isOptimized: false, runtimeVersion: 0, emissionKind: FullDebug, \
             enums: !{{}})\n"
        ));
        defs.push_str(&format!(
            "!{DI_SP} = distinct !DISubprogram(name: \"glm_exec\", linkageName: \"glm_exec\", \
             scope: !{DI_FILE}, file: !{DI_FILE}, line: 1, type: !{DI_FN_TY}, scopeLine: 1, \
             spFlags: DISPFlagDefinition, unit: !{DI_CU})\n"
        ));
        defs.push_str(&format!(
            "!{DI_FN_TY} = !DISubroutineType(types: !{{null, !{DI_PTR_TY}, !{DI_PTR_TY}}})\n\
             !{DI_PTR_TY} = !DIDerivedType(tag: DW_TAG_pointer_type, baseType: null, size: 64)\n\
             !{DI_I64_TY} = !DIBasicType(name: \"i64\", size: 64, encoding: DW_ATE_signed)\n\
             !{DI_F64_TY} = !DIBasicType(name: \"f64\", size: 64, encoding: DW_ATE_float)\n\
             !{DI_BOOL_TY} = !DIBasicType(name: \"bool\", size: 8, encoding: DW_ATE_boolean)\n\
             !{DI_ANY_TY} = !DIBasicType(name: \"any\", size: 128, encoding: DW_ATE_unsigned)\n"
        ));
        defs.push_str(&format!(
            "!{DI_FLAGS} = !{{i32 2, !\"Dwarf Version\", i32 4}}\n\
             !{} = !{{i32 2, !\"Debug Info Version\", i32 3}}\n\
             !{} = !{{i32 2, !\"PIC Level\", i32 2}}\n",
            DI_FLAGS + 1,
            DI_FLAGS + 2
        ));
        Some(Self {
            defs,
            next: DI_VAR_BASE,
            locs: HashMap::new(),
            vars: HashMap::new(),
        })
    }

    fn loc_node(&mut self, loc: SourceLoc) -> usize {
        let key = (loc.line, loc.col);
        if let Some(&id) = self.locs.get(&key) {
            return id;
        }
        let id = self.next;
        self.next += 1;
        self.defs.push_str(&format!(
            "!{id} = distinct !DILocation(line: {}, column: {}, scope: !{DI_SP})\n",
            loc.line, loc.col
        ));
        self.locs.insert(key, id);
        id
    }

    fn var_node(&mut self, bind: &DbgBind) -> usize {
        if let Some(&id) = self.vars.get(&bind.name) {
            return id;
        }
        let id = self.next;
        self.next += 1;
        let ty = match bind.kind {
            RegKind::Int => DI_I64_TY,
            RegKind::Float => DI_F64_TY,
            RegKind::Bool => DI_BOOL_TY,
            RegKind::Any => DI_ANY_TY,
            RegKind::Ptr => DI_PTR_TY,
        };
        let arg = bind
            .param
            .map(|n| format!("arg: {n}, "))
            .unwrap_or_default();
        let line = bind.loc.map(|l| l.line).unwrap_or(0);
        self.defs.push_str(&format!(
            "!{id} = !DILocalVariable(name: \"{}\", {arg}scope: !{DI_SP}, file: !{DI_FILE}, \
             line: {line}, type: !{ty})\n",
            llvm_bytes(bind.name.as_bytes())
        ));
        self.vars.insert(bind.name.clone(), id);
        id
    }

    fn tail(&self) -> String {
        format!(
            "\n!llvm.dbg.cu = !{{!{DI_CU}}}\n\
             !llvm.module.flags = !{{!{DI_FLAGS}, !{}, !{}}}\n{}",
            DI_FLAGS + 1,
            DI_FLAGS + 2,
            self.defs
        )
    }
}

/// The !DIBasicType id and LLVM type a debug bind's repr reports with.
fn dbg_ty(kind: RegKind) -> &'static str {
    match kind {
        RegKind::Int => "i64",
        RegKind::Float => "double",
        RegKind::Bool => "i1",
        RegKind::Any => "i128",
        RegKind::Ptr => "ptr",
    }
}

/// Append `, !dbg !N` to every instruction line added since `start`:
/// the backend's instruction lines indent two spaces; block labels and
/// blank separators stay untouched. The multi-block set_fast
/// expansions inherit their statement's position wholesale.
fn attach_dbg(code: &mut String, start: usize, node: usize) {
    let added = code[start..].to_string();
    code.truncate(start);
    for line in added.split_inclusive('\n') {
        if line.starts_with("  ") && !line.trim().is_empty() {
            code.push_str(line.trim_end_matches('\n'));
            code.push_str(&format!(", !dbg !{node}\n"));
        } else {
            code.push_str(line);
        }
    }
}

// === The seam plan ====================================================
// The ONE walk that numbers every table-seam instruction. Before the
// lowerer owned continuations, the emitter split LLVM blocks
// mid-instruction-stream (a fast store wrote `br label %bts{n}cont`
// plus the label itself), so one compiler-IR block mapped to 1–3
// emitted blocks and phis had to name predecessors by labels no
// compiler block ever owned — first a shadow "phi-tail probe"
// re-simulated the counter to predict them (two copies of one
// invariant; the register-seam change edited one copy and loop-header
// phis named bts1cont blocks the emitter never wrote — clang: "use of
// undefined value", pinned by cases/args_any_seam_phi_grow.lua). The
// deep fix moved the split INTO the lowerer: a fast store terminates
// its block into a real continuation block, phis name `b{id}`
// predecessors, and the plan's only remaining job is the per-
// instruction slot numbers — nothing counts during emission, and the
// structural law (a fast store is always its block's last
// instruction) is asserted right here, dying loudly with the exact
// block and index instead of mislabeling the IR.
struct SeamPlan {
    /// The seam number of each table-seam instruction, indexed
    /// [block.id][instruction index] — `None` for non-seam
    /// instructions.
    numbers: Vec<Vec<Option<usize>>>,
}

impl SeamPlan {
    /// The number of the seam instruction at (block, index). Every
    /// TableGet / TableSet / TableSetFast is numbered by `plan_seams`
    /// — a miss here is a new seam instruction the plan walk forgot,
    /// and it dies at the site instead of mislabeling the IR.
    fn number(&self, block: BlockId, ii: usize) -> usize {
        self.numbers[block][ii].expect("the seam plan numbered every table-seam instruction")
    }
}

/// Walk the program once and build the seam plan: uniform numbers for
/// every table-seam instruction (per INSTRUCTION, not per spilled
/// slot — the register face and the byte face number alike; the number
/// is load-bearing only for `%ts{n}` name uniqueness now).
fn plan_seams(program: &IrProgram) -> SeamPlan {
    let mut numbers: Vec<Vec<Option<usize>>> = program
        .blocks
        .iter()
        .map(|b| vec![None; b.instrs.len()])
        .collect();
    let mut probe = 0usize;
    for block in &program.blocks {
        for (ii, instr) in block.instrs.iter().enumerate() {
            match instr {
                Instruction::TableSetFast { .. } => {
                    // The lowerer splits into the continuation the
                    // moment it emits a fast store, so the store is
                    // always its block's last instruction — the
                    // emission below terminates the block on this
                    // instruction's expansion. Anything else means a
                    // new emitter contract is drifting; die here, with
                    // the block and index, not as invalid IR at clang.
                    assert!(
                        ii == block.instrs.len() - 1,
                        "a fast store must end its block (the lowerer \
                         owns the continuation) — block {} carries one \
                         at index {} that is not last",
                        block.id,
                        ii
                    );
                    numbers[block.id][ii] = Some(probe);
                    probe += 1;
                }
                Instruction::TableGet { .. } | Instruction::TableSet { .. } => {
                    numbers[block.id][ii] = Some(probe);
                    probe += 1;
                }
                _ => {}
            }
        }
    }
    SeamPlan { numbers }
}

/// The label-closure check — the belt to the lowerer-owned CFG's
/// suspenders: every block label a `br` or a phi names must be a label
/// the assembled module actually emits. The seam-counter desync this
/// guards against used to surface as clang's bare "use of undefined
/// value '%bts1cont'"; now it dies HERE, at the emitter, naming the
/// dangling block. One direction only (referenced → defined): a
/// defined-but-unreferenced label is valid IR (no function's
/// `entry:` is ever referenced), a referenced-but-undefined one is
/// not. Phis name real `b{id}` blocks since the lowerer owns
/// continuations, so the phi flavor is impossible by construction;
/// this check makes EVERY flavor (a new emitter arm, a hand-routed
/// branch, an emitter-internal diamond like the Hybrid verdict's
/// `bts{n}dense`/`bts{n}sparse` arms) impossible to ship.
fn assert_labels_closed(ir: &str) {
    let mut defined: Vec<&str> = Vec::new();
    let mut referenced: Vec<&str> = Vec::new();
    for line in ir.lines() {
        // Block labels sit at column 0 (`b0:`, `bts3dense:`, `entry:`).
        // Everything else that could masquerade starts with a sigil
        // (`@` globals, `!` metadata, `declare`/`define` never end a
        // line with a bare colon).
        if let Some(name) = line.strip_suffix(':')
            && is_label_name(name)
        {
            defined.push(name);
        } else if line.starts_with(' ') {
            // References live only on indented code lines — br
            // targets and phi predecessor pairs. String-pool constants
            // ride column-0 `@`-lines, so a script literal containing
            // "label %" can never false-positive.
            scan_label_refs(line, &mut referenced);
        }
    }
    for name in &referenced {
        assert!(
            defined.contains(name),
            "IR emitter label desync: a br/phi names block '{name}', \
             which the module never emits — the emitters and the block \
             model disagree (see SeamPlan in backend.rs)"
        );
    }
}

fn is_label_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' || c == '.' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '.')
}

fn is_label_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '.'
}

/// Collect `label %name` (br, both forms) and `[%val, %name ]` (phi
/// predecessor pairs) references off one indented code line.
fn scan_label_refs<'a>(line: &'a str, out: &mut Vec<&'a str>) {
    let mut rest = line;
    while let Some(i) = rest.find("label %") {
        rest = &rest[i + "label %".len()..];
        let end = rest.find(|c: char| !is_label_char(c)).unwrap_or(rest.len());
        out.push(&rest[..end]);
        rest = &rest[end..];
    }
    // Phi pairs: `[ %v13, %b0 ]` — the `, %name ]` shape. Call and GEP
    // argument lists never put `%` directly after the comma-space
    // (a type always sits between), so the pattern is phi-specific.
    let mut rest = line;
    while let Some(i) = rest.find(", %") {
        rest = &rest[i + ", %".len()..];
        let end = rest.find(|c: char| !is_label_char(c)).unwrap_or(rest.len());
        if rest[end..].starts_with(" ]") {
            out.push(&rest[..end]);
        }
        rest = &rest[end..];
    }
}

pub fn generate_llvm_ir(program: &IrProgram) -> Result<String, Vec<String>> {
    let mut globals = String::new();
    let mut dbg = DebugMeta::new(&program.source_file);
    let mut needs_dbg_value_decl = false;
    // The compile-time string pool: literal bytes -> global name. Each
    // distinct literal materializes once; every repeat GEPs the same
    // global, so .rodata holds one copy per distinct literal and all
    // uses of that literal share one address. Pool strings are
    // process-immortal constants — no runtime allocation, no free —
    // which is why string cells ride the deep-free exemption.
    let mut str_pool: HashMap<&str, String> = HashMap::new();
    // The pool's first-sight order: the registry array below walks it,
    // so the emitted IR stays byte-identical across compiles (a
    // HashMap's iteration order is randomized per process — the lock
    // protocol needs the deterministic one).
    let mut str_order: Vec<String> = Vec::new();
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
    let mut needs_tbl_get_any_decl = false;
    let mut needs_tbl_set_any_decl = false;
    let mut needs_str_len_decl = false;
    let mut needs_sys_alloc_count_decl = false;
    let mut needs_any_print_decl = false;
    let mut needs_any_eq_decl = false;
    // Whether the module's own code references ANY runtime symbol —
    // a module that references none (a bare `return arg` passthrough,
    // a pure arithmetic script) would pull no member out of the
    // runtime archive, and the dev-loop host's eager dlsym of the
    // boundary surface (glm_tbl_new and friends) would fail against
    // the thin .so. The anchor below closes that gap.
    let mut has_print = false;
    let mut needs_hdr_md = false;
    // The keep-array scratch counter: one entry alloca per multi-keep
    // call site (never inside a loop body's block — the entry slot is
    // reused by stores before each call).
    let mut ks = 0usize;
    // The seam plan — the single source the seam emitters read for
    // their per-instruction slot numbers (see SeamPlan above). Phis
    // need nothing from it: they name `b{id}` predecessors, real
    // blocks the lowerer created.
    let seam_plan = plan_seams(program);

    let mut allocas = String::new();
    let mut code = String::new();

    for block in &program.blocks {
        code.push_str(&format!("\nb{}:\n", block.id));

        for (ii, instr) in block.instrs.iter().enumerate() {
            let start = code.len();
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
                Instruction::LoadAnyZero { target } => {
                    code.push_str(&format!("  %v{} = add i128 0, 0\n", target.id))
                }
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
                            str_order.push(g.clone());
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
                Instruction::StrLen { target, s } => {
                    needs_str_len_decl = true;
                    code.push_str(&format!(
                        "  %v{} = call i64 @glm_str_len(ptr %v{})\n",
                        target.id, s.id
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
                    emit_cell_get(
                        g,
                        &mut allocas,
                        &mut code,
                        seam_plan.number(block.id, ii),
                        &mut needs_tbl_get_decl,
                        &mut needs_tbl_get_any_decl,
                    );
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
                        seam_plan.number(block.id, ii),
                        &mut needs_hdr_md,
                        &mut needs_tbl_set_decl,
                        &mut needs_tbl_set_any_decl,
                    );
                }
                Instruction::TableSet(s) => {
                    emit_cell_set(
                        s,
                        &mut allocas,
                        &mut code,
                        seam_plan.number(block.id, ii),
                        &mut needs_tbl_set_decl,
                        &mut needs_tbl_set_any_decl,
                    );
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
                Instruction::Less(c) => {
                    cmp_regs(c, "slt", "olt", &mut code, &mut needs_any_eq_decl)
                }
                Instruction::Leq(c) => cmp_regs(c, "sle", "ole", &mut code, &mut needs_any_eq_decl),
                Instruction::Geq(c) => cmp_regs(c, "sge", "oge", &mut code, &mut needs_any_eq_decl),
                Instruction::Eq(c) => cmp_regs(c, "eq", "oeq", &mut code, &mut needs_any_eq_decl),
                Instruction::Not { target, source } => {
                    code.push_str(&format!("  %v{} = xor i1 %v{}, 1\n", target.id, source.id));
                }
                Instruction::Phi(PhiRegs::Int { target, args }) => {
                    emit_phi(target, args, &mut code)
                }
                Instruction::Phi(PhiRegs::Float { target, args }) => {
                    emit_phi(target, args, &mut code)
                }
                Instruction::Phi(PhiRegs::Bool { target, args }) => {
                    emit_phi(target, args, &mut code)
                }
                Instruction::Phi(PhiRegs::Ptr { target, args }) => {
                    emit_phi(target, args, &mut code)
                }
                Instruction::Phi(PhiRegs::Any { target, args }) => {
                    emit_phi(target, args, &mut code)
                }
                Instruction::Print { operands } => {
                    has_print = true;
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
                            // The boundary's dynamic cell: the runtime
                            // switches on the tag the host's word chose
                            // and prints with the matching scalar
                            // printer — the one dispatch an Any value
                            // ever needs.
                            StaticType::Any => {
                                needs_any_print_decl = true;
                                code.push_str(&format!(
                                    "  call void @glm_any_print(i128 %v{})\n",
                                    r
                                ));
                            }
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
            // Stamp the statement's position on every line the
            // instruction expanded to, then flush the variable binds
            // anchored at it (llvm.dbg.value describing a register as
            // a named local from here on).
            if let Some(loc) = block.locs.get(ii).copied().flatten()
                && let Some(d) = &mut dbg
            {
                let node = d.loc_node(loc);
                attach_dbg(&mut code, start, node);
            }
            for bind in block.dbg_values.iter().filter(|b| b.after == ii) {
                let Some(d) = &mut dbg else { continue };
                needs_dbg_value_decl = true;
                let var = d.var_node(bind);
                let loc = bind.loc.unwrap_or(SourceLoc { line: 1, col: 1 });
                let dbg_loc = d.loc_node(loc);
                code.push_str(&format!(
                    "  call void @llvm.dbg.value(metadata {} %v{}, metadata !{}, \
                     metadata !DIExpression()), !dbg !{}\n",
                    dbg_ty(bind.kind),
                    bind.reg,
                    var,
                    dbg_loc
                ));
            }
        }

        // A block ending in a fast store was terminated by that
        // store's expansion (Dense/Sparse branch straight to the
        // continuation; the Hybrid diamond's arms both do) — the IR's
        // own Jump must name the same continuation the lowerer gave
        // the instruction. Assert the agreement and emit nothing:
        // re-emitting the br would land dead code after a terminator.
        if let Some(Instruction::TableSetFast(s)) = block.instrs.last() {
            let cont = s.cont();
            assert!(
                matches!(block.terminator, Some(Terminator::Jump(b)) if b == cont),
                "a fast store's block must jump to its own continuation — \
                 block {} ends in a fast store but does not jump to b{}",
                block.id,
                cont
            );
        } else {
            let term_start = code.len();
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
            if let Some(loc) = block.term_loc
                && let Some(d) = &mut dbg
            {
                let node = d.loc_node(loc);
                attach_dbg(&mut code, term_start, node);
            }
        }
    }

    let mut out = String::from(
        "declare void @glm_print_int(i64)\n\
         declare void @glm_print_float(double)\n\
         declare void @glm_print_bool(i1)\n\
         declare void @glm_print_string(ptr)\n\
         declare void @glm_print_sep()\n\
         declare void @glm_print_nl()\n\n\
         ",
    );
    // The subprogram attachment is the debugger's link: the DISubprogram
    // the whole module's !DILocations scope to IS this function.
    out.push_str(if dbg.is_some() {
        "define ptr @glm_exec(ptr %args) !dbg !5 {\nentry:\n"
    } else {
        "define ptr @glm_exec(ptr %args) {\nentry:\n"
    });
    out.push_str(&allocas);
    out.push_str("  br label %b0\n");
    out.push_str(&code);
    out.push_str("}\n");

    // The exported boundary contract: the pinned cell type as one of
    // glm_rt's GLM_ARG_* constants, ALWAYS emitted — the .so and the
    // exe both carry it, so ANY host (C, LuaJIT FFI, a Rust runner)
    // queries the module itself for how to parse its words instead of
    // guessing. NONE names the argless module (the script never names
    // `arg`).
    let arg_kind = match program.boundary_elem {
        Some(elem) => glm_rt::rt::arg_kind_of(&elem),
        None => glm_rt::rt::GLM_ARG_NONE,
    };
    out.push_str(&format!(
        "\ndefine i32 @glm_arg_kind() {{\n\
         entry:\n\
           ret i32 {arg_kind}\n\
         }}\n"
    ));

    // The standalone entry. The argless form: @main calls the
    // boundary with no table and frees what returns — the whole
    // program in an executable, no host, no dlopen. The args form:
    // @main hands argc/argv to the runtime's glm_exec_main with the
    // compile-time pinned element kind (the same constant
    // glm_arg_kind returns), so the executable carries its own
    // boundary host — the words cross at exec time, parsed against
    // the same usage-inferred cell type the dev-loop host uses.
    let mut needs_exec_main_decl = false;
    if let EntryKind::Exe { args } = program.entry {
        if args {
            out.push_str(&format!(
                "\ndefine i32 @main(i32 %argc, ptr %argv) {{\n\
                 entry:\n\
                   %rc = call i32 @glm_exec_main(i32 %argc, ptr %argv, i32 {arg_kind}, ptr @glm_exec)\n\
                   ret i32 %rc\n\
                 }}\n"
            ));
            needs_exec_main_decl = true;
        } else {
            out.push_str(
                "\ndefine i32 @main() {\n\
                 entry:\n\
                   %r = call ptr @glm_exec(ptr null)\n\
                   call void @glm_tbl_free(ptr %r)\n\
                   ret i32 0\n\
                 }\n",
            );
            // @main's own free runs even when the script frees nothing,
            // so the declaration cannot ride the script's needs flag.
            needs_tbl_free_decl = true;
        }
    }

    // The string registry, emitted only for String boundary cells: the
    // module's distinct literals collected into one pointer array,
    // registered with the runtime at load (an .init_array constructor
    // — runs at dlopen for the .so, at program start for the exe), so
    // the host's boundary words intern to the same pointer identity
    // the script's own literals hold. A miss allocates the runtime's
    // own immortal copy (glm_str_intern), keeping the pool semantics
    // one flat identity space across the boundary.
    let mut registry = String::new();
    if matches!(program.boundary_elem, Some(glm_rt::GlmElem::String)) && !str_order.is_empty() {
        trace::compiler_trace_signal(trace::TRACE_STRTAB_EMIT);
        let ptrs: Vec<String> = str_order.iter().map(|g| format!("ptr {g}")).collect();
        registry.push_str(&format!(
            "@.glm_strtab = global [{} x ptr] [{}]\n",
            ptrs.len(),
            ptrs.join(", ")
        ));
        registry.push_str(&format!(
            "\ndefine internal void @.glm_strreg() {{\n\
             entry:\n\
               call void @glm_str_register(ptr @.glm_strtab, i64 {})\n\
               ret void\n\
             }}\n\
             @llvm.global_ctors = appending global \
             [1 x {{ i32, ptr, ptr }}] \
             [{{ i32, ptr, ptr }} {{ i32 65535, ptr @.glm_strreg, ptr null }}]\n",
            ptrs.len()
        ));
    }

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
    if needs_tbl_get_any_decl {
        head.push_str("declare i128 @glm_tbl_get_any(ptr, i64)\n");
    }
    if needs_tbl_set_any_decl {
        head.push_str("declare void @glm_tbl_set_any(ptr, i64, i128)\n");
    }
    if needs_str_len_decl {
        head.push_str("declare i64 @glm_str_len(ptr)\n");
    }
    if needs_sys_alloc_count_decl {
        head.push_str("declare i64 @sys_alloc_count()\n");
    }
    if needs_any_print_decl {
        head.push_str("declare void @glm_any_print(i128)\n");
    }
    if needs_any_eq_decl {
        head.push_str("declare i32 @glm_any_eq(i128, i128)\n");
    }
    if needs_dbg_value_decl {
        head.push_str("declare void @llvm.dbg.value(metadata, metadata, metadata)\n");
    }
    if needs_div_guard {
        head.push_str("declare void @glm_div_zero_guard(i64)\n");
    }
    if !registry.is_empty() {
        head.push_str("declare void @glm_str_register(ptr, i64)\n");
    }
    if needs_exec_main_decl {
        head.push_str("declare i32 @glm_exec_main(i32, ptr, i32, ptr)\n");
    }

    // The runtime anchor: a .so whose own code references no runtime
    // symbol (a bare `return arg` passthrough) still owes the HOST the
    // boundary surface — the dev-loop host resolves glm_tbl_new,
    // glm_tbl_set, glm_tbl_get, glm_tbl_free, and glm_str_intern from
    // the module so the whole boundary (allocation, census, intern
    // identity) lives in one runtime instance. An internal constant
    // holding the five addresses creates the relocations that pull
    // the archive member in; modules that already reference the
    // runtime emit nothing extra (their IR stays byte-identical).
    let rt_referenced = needs_tbl_new_decl
        || needs_tbl_reserve_decl
        || needs_tbl_free_decl
        || needs_tbl_free_except_decl
        || needs_tbl_free_except_n_decl
        || needs_tbl_get_decl
        || needs_tbl_set_decl
        || needs_tbl_get_any_decl
        || needs_tbl_set_any_decl
        || needs_str_len_decl
        || needs_sys_alloc_count_decl
        || needs_any_print_decl
        || needs_any_eq_decl
        || needs_div_guard
        || needs_exec_main_decl
        || !registry.is_empty()
        || has_print;
    let mut anchor = String::new();
    if !rt_referenced && matches!(program.entry, EntryKind::Lib) {
        // llvm.compiler.used pins the anchor: -O3 may otherwise
        // dead-strip an internal constant nothing references, taking
        // its archive-pulling relocations with it.
        anchor.push_str(concat!(
            "declare ptr @glm_tbl_new(i64, i8)\n",
            "declare void @glm_tbl_set(ptr, i64, ptr)\n",
            "declare void @glm_tbl_get(ptr, i64, ptr, i64)\n",
            "declare void @glm_tbl_free(ptr)\n",
            "declare ptr @glm_str_intern(ptr, i64)\n",
            "@.glm_rt_anchor = internal constant [5 x i64] [\n",
            "  i64 ptrtoint (ptr @glm_tbl_new to i64),\n",
            "  i64 ptrtoint (ptr @glm_tbl_set to i64),\n",
            "  i64 ptrtoint (ptr @glm_tbl_get to i64),\n",
            "  i64 ptrtoint (ptr @glm_tbl_free to i64),\n",
            "  i64 ptrtoint (ptr @glm_str_intern to i64)\n",
            "]\n",
            "@llvm.compiler.used = appending global [1 x ptr] [ptr @.glm_rt_anchor]\n",
        ));
    }

    let md = if needs_hdr_md {
        "\n!0 = !{!1}\n\
         !1 = distinct !{!\"glm_table_header\", !2}\n\
         !2 = distinct !{!\"glm_table\"}\n"
            .to_string()
    } else {
        String::new()
    };
    let dbg_tail = dbg.map(|d| d.tail()).unwrap_or_default();
    let ir = format!(
        "{}{}{}{}{}{}{}",
        globals, registry, head, anchor, out, md, dbg_tail
    );
    assert_labels_closed(&ir);
    Ok(ir)
}

fn emit_move(m: &MoveRegs, code: &mut String) {
    match m {
        MoveRegs::Int { target, source } => Repr::emit_move(target, source, code),
        MoveRegs::Float { target, source } => Repr::emit_move(target, source, code),
        MoveRegs::Bool { target, source } => Repr::emit_move(target, source, code),
        MoveRegs::Str { target, source } => Repr::emit_move(target, source, code),
        MoveRegs::Ptr { target, source } => Repr::emit_move(target, source, code),
        MoveRegs::Byte { target, source } => Repr::emit_move(target, source, code),
        MoveRegs::Any { target, source } => Repr::emit_move(target, source, code),
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_cell_get(
    g: &CellGet,
    allocas: &mut String,
    code: &mut String,
    seam: usize,
    needs_tbl_get_decl: &mut bool,
    needs_tbl_get_any_decl: &mut bool,
) {
    match g {
        CellGet::Int {
            target,
            table,
            index,
        } => emit_table_get(
            target,
            table,
            index,
            allocas,
            code,
            seam,
            needs_tbl_get_decl,
            needs_tbl_get_any_decl,
        ),
        CellGet::Float {
            target,
            table,
            index,
        } => emit_table_get(
            target,
            table,
            index,
            allocas,
            code,
            seam,
            needs_tbl_get_decl,
            needs_tbl_get_any_decl,
        ),
        CellGet::Bool {
            target,
            table,
            index,
        } => emit_table_get(
            target,
            table,
            index,
            allocas,
            code,
            seam,
            needs_tbl_get_decl,
            needs_tbl_get_any_decl,
        ),
        CellGet::Ptr {
            target,
            table,
            index,
        } => emit_table_get(
            target,
            table,
            index,
            allocas,
            code,
            seam,
            needs_tbl_get_decl,
            needs_tbl_get_any_decl,
        ),
        CellGet::Byte {
            target,
            table,
            index,
        } => emit_table_get(
            target,
            table,
            index,
            allocas,
            code,
            seam,
            needs_tbl_get_decl,
            needs_tbl_get_any_decl,
        ),
        CellGet::Any {
            target,
            table,
            index,
        } => emit_table_get(
            target,
            table,
            index,
            allocas,
            code,
            seam,
            needs_tbl_get_decl,
            needs_tbl_get_any_decl,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_cell_set_fast(
    s: &CellSetFast,
    allocas: &mut String,
    code: &mut String,
    seam: usize,
    needs_hdr_md: &mut bool,
    needs_tbl_set_decl: &mut bool,
    needs_tbl_set_any_decl: &mut bool,
) {
    match s {
        CellSetFast::Int {
            table,
            index,
            value,
            layout,
            cont,
        } => emit_table_set_fast(
            table,
            index,
            value,
            layout,
            *cont,
            allocas,
            code,
            seam,
            needs_hdr_md,
            needs_tbl_set_decl,
            needs_tbl_set_any_decl,
        ),
        CellSetFast::Float {
            table,
            index,
            value,
            layout,
            cont,
        } => emit_table_set_fast(
            table,
            index,
            value,
            layout,
            *cont,
            allocas,
            code,
            seam,
            needs_hdr_md,
            needs_tbl_set_decl,
            needs_tbl_set_any_decl,
        ),
        CellSetFast::Bool {
            table,
            index,
            value,
            layout,
            cont,
        } => emit_table_set_fast(
            table,
            index,
            value,
            layout,
            *cont,
            allocas,
            code,
            seam,
            needs_hdr_md,
            needs_tbl_set_decl,
            needs_tbl_set_any_decl,
        ),
        CellSetFast::Ptr {
            table,
            index,
            value,
            layout,
            cont,
        } => emit_table_set_fast(
            table,
            index,
            value,
            layout,
            *cont,
            allocas,
            code,
            seam,
            needs_hdr_md,
            needs_tbl_set_decl,
            needs_tbl_set_any_decl,
        ),
        CellSetFast::Byte {
            table,
            index,
            value,
            layout,
            cont,
        } => emit_table_set_fast(
            table,
            index,
            value,
            layout,
            *cont,
            allocas,
            code,
            seam,
            needs_hdr_md,
            needs_tbl_set_decl,
            needs_tbl_set_any_decl,
        ),
        CellSetFast::Any {
            table,
            index,
            value,
            layout,
            cont,
        } => emit_table_set_fast(
            table,
            index,
            value,
            layout,
            *cont,
            allocas,
            code,
            seam,
            needs_hdr_md,
            needs_tbl_set_decl,
            needs_tbl_set_any_decl,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_cell_set(
    s: &CellSet,
    allocas: &mut String,
    code: &mut String,
    seam: usize,
    needs_tbl_set_decl: &mut bool,
    needs_tbl_set_any_decl: &mut bool,
) {
    match s {
        CellSet::Int {
            table,
            index,
            value,
        } => emit_table_set(
            table,
            index,
            value,
            allocas,
            code,
            seam,
            needs_tbl_set_decl,
            needs_tbl_set_any_decl,
        ),
        CellSet::Float {
            table,
            index,
            value,
        } => emit_table_set(
            table,
            index,
            value,
            allocas,
            code,
            seam,
            needs_tbl_set_decl,
            needs_tbl_set_any_decl,
        ),
        CellSet::Bool {
            table,
            index,
            value,
        } => emit_table_set(
            table,
            index,
            value,
            allocas,
            code,
            seam,
            needs_tbl_set_decl,
            needs_tbl_set_any_decl,
        ),
        CellSet::Ptr {
            table,
            index,
            value,
        } => emit_table_set(
            table,
            index,
            value,
            allocas,
            code,
            seam,
            needs_tbl_set_decl,
            needs_tbl_set_any_decl,
        ),
        CellSet::Byte {
            table,
            index,
            value,
        } => emit_table_set(
            table,
            index,
            value,
            allocas,
            code,
            seam,
            needs_tbl_set_decl,
            needs_tbl_set_any_decl,
        ),
        CellSet::Any {
            table,
            index,
            value,
        } => emit_table_set(
            table,
            index,
            value,
            allocas,
            code,
            seam,
            needs_tbl_set_decl,
            needs_tbl_set_any_decl,
        ),
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

fn cmp_regs(
    c: &CmpRegs,
    int_cond: &str,
    flt_cond: &str,
    code: &mut String,
    needs_any_eq_decl: &mut bool,
) {
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
        // Two Any cells: equality is the runtime's tag-dispatched
        // compare (glm_any_eq answers 0/1), materialized into the i1
        // the branch machinery reads. `~=` composes at the lowerer
        // level (Eq + Not), so this arm is the single Any compare.
        CmpRegs::Any {
            target,
            left,
            right,
        } => {
            *needs_any_eq_decl = true;
            code.push_str(&format!(
                "  %a{}.eq = call i32 @glm_any_eq(i128 %v{}, i128 %v{})\n\
                   %v{} = icmp ne i32 %a{}.eq, 0\n",
                target.id, left.id, right.id, target.id, target.id
            ));
        }
    }
}

/// A join's value: every predecessor is a real block the lowerer
/// created (fast stores included — their continuations are real
/// blocks), so the label is always plain `b{id}`.
fn emit_phi<R: Repr>(target: &Reg<R>, args: &[(BlockId, Reg<R>)], code: &mut String) {
    let pairs: Vec<String> = args
        .iter()
        .map(|(b, r)| format!("[ %v{}, %b{} ]", r.id, b))
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
    seam: usize,
    needs_tbl_get_decl: &mut bool,
    needs_tbl_get_any_decl: &mut bool,
) {
    if E::REG_FACE {
        // The register seam: the tagged cell crosses whole in the i128
        // return — no dst slot, no alignment promise about caller
        // memory. The seam number arrives from the plan whether this
        // branch uses it or not: numbering is per instruction, the
        // plan's one rule.
        *needs_tbl_get_any_decl = true;
        code.push_str(&format!(
            "  %v{target} = call i128 @glm_tbl_get_any(ptr %v{table}, i64 %v{index})\n",
            target = target.id,
            table = table.id,
            index = index.id
        ));
        return;
    }
    *needs_tbl_get_decl = true;
    let ety = E::storage();
    let f = seam;

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

/// The fast store. Its block ends HERE: the lowerer split into the
/// real continuation block `cont` the moment it emitted the
/// instruction, so every edge this expansion writes branches to
/// `%b{cont}` — a label a phi may legally name. The Dense and Sparse
/// verdicts are straight-line stores plus the jump; only the Hybrid
/// verdict branches at runtime, and its diamond arms are
/// emitter-internal (`bts{n}dense` / `bts{n}sparse`) labels no phi
/// ever names — each arm closes into the same real continuation.
#[allow(clippy::too_many_arguments)]
fn emit_table_set_fast<E: Repr>(
    table: &Reg<Ptr>,
    index: &Reg<Int>,
    value: &Reg<E>,
    layout: &LayoutVerdict,
    cont: BlockId,
    allocas: &mut String,
    code: &mut String,
    seam: usize,
    needs_hdr_md: &mut bool,
    needs_tbl_set_decl: &mut bool,
    needs_tbl_set_any_decl: &mut bool,
) {
    *needs_hdr_md = true;
    let ety = E::storage();
    let f = seam;

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

    // The register face never spills: the tagged cell crosses whole
    // in the i128 argument, so no layout verdict needs a valp slot.
    if !E::REG_FACE {
        allocas.push_str(&format!("  %ts{f}.valp = alloca {ety}\n", f = f, ety = ety));
    }

    match layout {
        LayoutVerdict::Dense => {
            // The buffer store names no align clause: LLVM assumes the
            // natural alignment, and the runtime's elem_layout demands
            // exactly that of the span (16 for the i128 cell, 8 for
            // the machine word) — promise and allocation agree by
            // construction.
            code.push_str(&format!(
                "{val_cast}\
                   %ts{f}.d = load ptr, ptr %v{table}, !alias.scope !0\n\
                   %ts{f}.s = getelementptr inbounds {ety}, ptr %ts{f}.d, i64 %v{index}\n\
                   store {ety} {val_use}, ptr %ts{f}.s, !noalias !0\n\
                   br label %b{cont}\n",
                val_cast = val_cast,
                f = f,
                table = table.id,
                index = index.id,
                ety = ety,
                val_use = val_use,
                cont = cont
            ));
        }
        LayoutVerdict::Sparse => {
            code.push_str(&format!(
                "{val_cast}\
                   {checked}\
                   br label %b{cont}\n",
                val_cast = val_cast,
                checked = checked_store::<E>(
                    f,
                    table,
                    index,
                    &val_use,
                    ety,
                    needs_tbl_set_decl,
                    needs_tbl_set_any_decl
                ),
                cont = cont
            ));
        }
        _ => {
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
                   br label %b{cont}\n\n\
                 bts{f}sparse:\n\
                   {checked}\
                   br label %b{cont}\n",
                val_cast = val_cast,
                checked = checked_store::<E>(
                    f,
                    table,
                    index,
                    &val_use,
                    ety,
                    needs_tbl_set_decl,
                    needs_tbl_set_any_decl
                ),
                f = f,
                table = table.id,
                index = index.id,
                ety = ety,
                val_use = val_use,
                cont = cont
            ));
        }
    }
}

/// The checked-store half of the fast path (the Sparse verdict's body
/// and the Hybrid verdict's sparse arm): the register face calls
/// glm_tbl_set_any with the value whole; the byte face spills to its
/// valp slot first. The caller owns the branching scaffolding around
/// the store — these are the store's own lines.
#[allow(clippy::too_many_arguments)]
fn checked_store<E: Repr>(
    f: usize,
    table: &Reg<Ptr>,
    index: &Reg<Int>,
    val_use: &str,
    ety: &str,
    needs_tbl_set_decl: &mut bool,
    needs_tbl_set_any_decl: &mut bool,
) -> String {
    if E::REG_FACE {
        *needs_tbl_set_any_decl = true;
        format!(
            "  call void @glm_tbl_set_any(ptr %v{table}, i64 %v{index}, i128 {v})\n",
            table = table.id,
            index = index.id,
            v = val_use
        )
    } else {
        *needs_tbl_set_decl = true;
        format!(
            "  store {ety} {v}, ptr %ts{f}.valp\n\
               call void @glm_tbl_set(ptr %v{table}, i64 %v{index}, ptr %ts{f}.valp)\n",
            ety = ety,
            v = val_use,
            f = f,
            table = table.id,
            index = index.id
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_table_set<E: Repr>(
    table: &Reg<Ptr>,
    index: &Reg<Int>,
    value: &Reg<E>,
    allocas: &mut String,
    code: &mut String,
    seam: usize,
    needs_tbl_set_decl: &mut bool,
    needs_tbl_set_any_decl: &mut bool,
) {
    if E::REG_FACE {
        // The register seam: the tagged cell crosses whole in the i128
        // argument — no valp spill, no alignment promise about caller
        // memory. The seam number arrives from the plan whether this
        // branch uses it or not: numbering is per instruction.
        *needs_tbl_set_any_decl = true;
        code.push_str(&format!(
            "  call void @glm_tbl_set_any(ptr %v{table}, i64 %v{index}, i128 %v{value})\n",
            table = table.id,
            index = index.id,
            value = value.id
        ));
        return;
    }
    *needs_tbl_set_decl = true;
    let ety = E::storage();
    let f = seam;

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

#[cfg(test)]
mod label_closure_tests {
    use super::assert_labels_closed;

    #[test]
    fn closed_module_passes() {
        // The seam shape exactly: a loop-header phi whose back-edge
        // predecessor is a fast store's REAL continuation block, the
        // hybrid diamond's emitter-internal arms closing into it, plus
        // the entry label nothing references — closed in both scan
        // directions.
        assert_labels_closed(
            "define ptr @f() {\n\
             entry:\n\
             \x20 br label %b0\n\
             \nb0:\n\
             \x20 %v1 = phi i64 [ 0, %entry ], [ 1, %b3 ]\n\
             \x20 br i1 %v2, label %bts0dense, label %bts0sparse\n\
             \nbts0dense:\n\
             \x20 store i64 1, ptr %p\n\
             \x20 br label %b3\n\
             \nbts0sparse:\n\
             \x20 call void @glm_tbl_set(ptr %t, i64 1, ptr %v)\n\
             \x20 br label %b3\n\
             \nb3:\n\
             \x20 br label %b0\n\
             }\n",
        );
    }

    #[test]
    fn string_pool_literals_cannot_false_positive() {
        // A script literal containing the reference spellings rides a
        // column-0 @-line: the scanner must ignore it.
        assert_labels_closed(
            "@.str = constant [9 x i8] c\"label %x\\00\"\n\
             define ptr @f() {\n\
             entry:\n\
             \x20 ret ptr null\n\
             }\n",
        );
    }

    #[test]
    #[should_panic(expected = "names block 'b1'")]
    fn dangling_reference_panics() {
        // The desync's exact shape: the phi names a block the emitter
        // never wrote — whatever emitter arm let it slip.
        assert_labels_closed(
            "define ptr @f() {\n\
             entry:\n\
             \x20 br label %b0\n\
             \nb0:\n\
             \x20 %v1 = phi i64 [ 0, %entry ], [ 1, %b1 ]\n\
             \x20 ret ptr null\n\
             }\n",
        );
    }
}

#[cfg(test)]
mod fast_store_tail_tests {
    use super::{assert_labels_closed, emit_table_set_fast};
    use crate::ir::{Int, Ptr, Reg};
    use crate::shape::LayoutVerdict;

    /// The Growing verdict's diamond — the only fast-store arm no
    /// corpus case reaches today (a reserved loop's fills all refine
    /// to Dense), so its rewrite is pinned here: the emitter-internal
    /// arms close into the REAL continuation block, and no `bts…cont`
    /// tail label exists anywhere a phi could name.
    #[test]
    fn growing_diamond_closes_into_the_real_cont_block() {
        let table = Reg::<Ptr>::new(1);
        let index = Reg::<Int>::new(2);
        let value = Reg::<Int>::new(3);
        let mut allocas = String::new();
        let mut code = String::new();
        let mut hdr_md = false;
        let mut set_decl = false;
        let mut set_any_decl = false;
        emit_table_set_fast::<Int>(
            &table,
            &index,
            &value,
            &LayoutVerdict::Growing,
            5,
            &mut allocas,
            &mut code,
            7,
            &mut hdr_md,
            &mut set_decl,
            &mut set_any_decl,
        );
        assert_eq!(code.matches("br label %b5").count(), 2);
        assert!(!code.contains("cont:"));
        // And the fragment is label-closed once wrapped in a module.
        assert_labels_closed(&format!(
            "define ptr @f() {{\n\
             entry:\n\
             \x20 br label %b9\n\
             \nb9:\n{code}\
             b5:\n\
             \x20 ret ptr null\n\
             }}\n"
        ));
    }
}
