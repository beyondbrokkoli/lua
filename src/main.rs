use logos::Logos;
use std::ffi::{CString, c_void};

mod analysis;
mod ast;
mod backend;
mod ir;
mod lexer;
mod lowerer;
mod parser;
mod shape;
mod type_checker;

use ast::Stmt;

// The boundary type, shared by construction: the same #[repr(C)] struct
// the .so's copy of glm_rt uses, so host and callee speak one ABI.
use glm_rt::rt::GlmTable;

// The linker bypass: the whole program is one C-ABI function in a
// shared library — no OS entry point, no internal call stack. The host
// allocates and frees through the .so's own glm_rt symbols (resolved
// here, not linked here), so the runtime's leak census tracks exactly
// the tables the boundary moves.
type GlmExec = unsafe extern "C" fn(args: *mut GlmTable) -> *mut GlmTable;
type GlmTblNew = unsafe extern "C" fn(esize: usize, flags: u8) -> *mut GlmTable;
type GlmTblSet = unsafe extern "C" fn(t: *mut GlmTable, index: i64, val: *const u8);
type GlmTblGet = unsafe extern "C" fn(t: *mut GlmTable, index: i64, dst: *mut u8, esize: usize);
type GlmTblFree = unsafe extern "C" fn(t: *mut GlmTable);

const RTLD_NOW: i32 = 0x2;
const RTLD_LOCAL: i32 = 0x1;

unsafe extern "C" {
    fn dlopen(filename: *const i8, flags: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const i8) -> *mut c_void;
}

unsafe fn resolve_symbol<T>(dylib: *mut c_void, name: &[u8]) -> Result<T, String> {
    let sym = unsafe { dlsym(dylib, name.as_ptr().cast()) };
    if sym.is_null() {
        let name = String::from_utf8_lossy(&name[..name.len() - 1]);
        return Err(format!("dlsym: '{name}' not found in ./libglm_out.so"));
    }
    Ok(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&sym) })
}

/// Whether the script ever names the boundary table. When it does not,
/// the host passes null — the boundary table would otherwise sit live
/// (and counted) for a script that can never touch it, and the
/// script-visible sys_alloc_count() floor would shift for no reason.
fn references_arg(stmts: &[Stmt]) -> bool {
    fn expr(e: &ast::Expr) -> bool {
        match e {
            ast::Expr::Identifier(name) => name == "arg",
            ast::Expr::TableCtor(entries) => entries
                .iter()
                .any(|(k, v)| matches!(k, ast::CtorKey::Expr(ke) if expr(ke)) || expr(v)),
            ast::Expr::Index { obj, key } => expr(obj) || expr(key),
            ast::Expr::BinaryOp { left, right, .. } => expr(left) || expr(right),
            ast::Expr::UnaryOp { expr: inner, .. } => expr(inner),
            ast::Expr::Call { callee, args } => {
                expr(callee) || args.iter().any(expr)
            }
            // An inlined body executes at its call site, inside the
            // script — a read of `arg` in any body needs the table.
            ast::Expr::Function { body, .. } => go(body),
            // Exhaustive over the value leaves: any new Expr variant
            // must be routed through here explicitly, or the build
            // breaks instead of silently missing an `arg` read (a
            // missed read hands the boundary a null and the script
            // segfaults on its first store).
            ast::Expr::Integer(_)
            | ast::Expr::Float(_)
            | ast::Expr::Boolean(_)
            | ast::Expr::String(_)
            | ast::Expr::Nil
            | ast::Expr::SysAllocCount => false,
        }
    }
    fn go(stmts: &[Stmt]) -> bool {
        stmts.iter().any(|s| match s {
            Stmt::LocalDecl { exprs, .. } | Stmt::Print { exprs } => {
                exprs.iter().any(expr)
            }
            Stmt::Assignment { expr: value, .. } => expr(value),
            Stmt::IndexAssign { obj, key, value } => expr(obj) || expr(key) || expr(value),
            Stmt::While { condition, body } => expr(condition) || go(body),
            Stmt::Do { body } => go(body),
            Stmt::If {
                condition,
                then_body,
                else_body,
            } => expr(condition) || go(then_body) || go(else_body),
            Stmt::Expr { expr: inner } => expr(inner),
            Stmt::Return { value } => value.as_ref().is_some_and(expr),
        })
    }
    go(stmts)
}

/// Load the compiled module, hand it the boundary args table, and take
/// ownership of what comes back: the exact invocation path a C host,
/// LuaJIT FFI, or any Rust runner would use.
unsafe fn run_boundary(host_ints: &[i64], pass_args: bool) -> Result<(), String> {
    let path = CString::new("./libglm_out.so").unwrap();
    let dylib = unsafe { dlopen(path.as_ptr(), RTLD_NOW | RTLD_LOCAL) };
    if dylib.is_null() {
        return Err("dlopen: could not load ./libglm_out.so".into());
    }

    let glm_exec: GlmExec = unsafe { resolve_symbol(dylib, b"glm_exec\0")? };
    let glm_tbl_new: GlmTblNew = unsafe { resolve_symbol(dylib, b"glm_tbl_new\0")? };
    let glm_tbl_set: GlmTblSet = unsafe { resolve_symbol(dylib, b"glm_tbl_set\0")? };
    let glm_tbl_get: GlmTblGet = unsafe { resolve_symbol(dylib, b"glm_tbl_get\0")? };
    let glm_tbl_free: GlmTblFree = unsafe { resolve_symbol(dylib, b"glm_tbl_free\0")? };

    // Build the arguments using the exact same C-ABI memory the script
    // reads as its `arg` table (8-byte integer cells) — or pass null
    // when the script never names `arg`.
    let mut args = std::ptr::null_mut::<GlmTable>();
    if pass_args {
        args = unsafe { glm_tbl_new(8, 0) };
        for (i, v) in host_ints.iter().enumerate() {
            unsafe { glm_tbl_set(args, i as i64, (v as *const i64).cast::<u8>()) };
        }
    }

    // Enter the VM.
    let result = unsafe { glm_exec(args) };

    // The host assumes ownership of the returned memory and frees the
    // boundary tables — the script's exit leak census stays dark. A
    // Table<Table> return is part of that contract: the host frees the
    // returned header and the deep free releases every row it names.
    if result.is_null() {
        eprintln!("[glm host] boundary: glm_exec returned null");
    } else {
        let report = unsafe { render_boundary(result, 0, glm_tbl_get) };
        eprintln!("[glm host] boundary: glm_exec returned {report}");
    }

    // Both tables are the host's to free — unless the script handed
    // the boundary table straight back (`return arg`), in which case
    // result IS args and freeing both would double-free one header.
    unsafe {
        if result != args {
            glm_tbl_free(args);
        }
        glm_tbl_free(result);
    }
    Ok(())
}

/// A returned table's cells, as the host reads them. A flat table
/// renders integer (esize 8) or bool (esize 1) cells. A table with the
/// contains-rows flag renders each cell as the row header it names —
/// the one ABI bit that distinguishes Table<Table> from Table<Int>,
/// since both ride 8-byte cells — recursively to two levels, with the
/// runtime's pointer sanity bound so a mis-flagged cell is reported
/// instead of dereferenced.
///
/// # Safety
/// `t` is a live boundary table (or a plausible row of one); only the
/// report reads through it, before the host's own free.
unsafe fn render_boundary(t: *mut GlmTable, depth: u32, glm_tbl_get: GlmTblGet) -> String {
    let body = unsafe { render_cells(t, depth, glm_tbl_get) };
    if depth == 0 {
        format!("a table {body}")
    } else {
        format!("row{body}")
    }
}

/// # Safety
/// See render_boundary.
unsafe fn render_cells(t: *mut GlmTable, depth: u32, glm_tbl_get: GlmTblGet) -> String {
    let (len, esize, contains_tables) = unsafe { ((*t).len, (*t).esize, (*t).contains_tables) };
    let rows = contains_tables != 0 && esize == 8;
    let shown = len.clamp(0, if depth == 0 { 8 } else { 4 }) as usize;
    let mut cells = Vec::with_capacity(shown);
    for i in 0..shown as i64 {
        let mut buf = [0u8; 8];
        unsafe { glm_tbl_get(t, i, buf.as_mut_ptr(), esize) };
        if rows {
            let row = usize::from_ne_bytes(buf) as *mut GlmTable;
            cells.push(if row.is_null() {
                "null".to_string()
            } else if !is_plausible_row(row as usize) {
                format!("row@{:#x}?", row as usize)
            } else if depth < 2 {
                unsafe { render_boundary(row, depth + 1, glm_tbl_get) }
            } else {
                "row…".to_string()
            });
        } else if esize == 8 {
            cells.push(i64::from_ne_bytes(buf).to_string());
        } else {
            cells.push((buf[0] != 0).to_string());
        }
    }
    let flag = if rows { ", contains-rows" } else { "" };
    format!("(len={len}, esize={esize}{flag}): [{}]", cells.join(", "))
}

/// The deep-free path's pointer sanity, mirrored from the runtime: a
/// flagged cell is a row header or null, and anything else must not be
/// dereferenced by the report.
fn is_plausible_row(addr: usize) -> bool {
    #[cfg(target_pointer_width = "64")]
    let is_user_space = addr < 0x0000_7FFF_FFFF_FFFF;
    #[cfg(not(target_pointer_width = "64"))]
    let is_user_space = true;
    addr > 0x0100_0000 && is_user_space && (addr & 7) == 0
}

fn main() {
    glm_rt::trace::compiler_trace_init();

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        glm_rt::trace::compiler_trace_signal(glm_rt::trace::TRACE_BUILD_FAIL);
        eprintln!("GLM_TRACE: slot 1 — build failed");
        default_hook(info);
    }));

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        panic!("Usage: glm <file.lua> [int args...]");
    }
    let source_path = &args[1];
    let source = std::fs::read_to_string(source_path).expect("Failed to read source");

    let mut tokens = Vec::new();
    let mut front_diagnostics = Vec::new();
    for res in lexer::Token::lexer(&source) {
        match res {
            Ok(token) => tokens.push(token),
            Err(_) => {
                front_diagnostics.push("Syntax Error: Lexer error".to_string());
                break;
            }
        }
    }

    let mut parser = parser::Parser::new(tokens);
    let ast = parser.parse_program();
    front_diagnostics.extend(parser.diagnostics);

    if !front_diagnostics.is_empty() {
        for d in &front_diagnostics {
            eprintln!("{d}");
        }
        glm_rt::trace::compiler_trace_signal(glm_rt::trace::TRACE_BUILD_FAIL);
        eprintln!("GLM_TRACE: slot 1 — build failed");
        std::process::exit(1);
    }

    let ctx = analysis::build_context(&ast);

    let mut shape = shape::analyze(&ctx);

    if !shape.diagnostics.is_empty() {
        for d in &shape.diagnostics {
            eprintln!("{d}");
        }
        glm_rt::trace::compiler_trace_signal(glm_rt::trace::TRACE_BUILD_FAIL);
        eprintln!("GLM_TRACE: slot 1 — build failed");
        std::process::exit(1);
    }

    let mut checker = type_checker::TypeChecker::new(&mut shape);
    checker.check_program(&ast);

    if !shape.diagnostics.is_empty() {
        for d in &shape.diagnostics {
            eprintln!("{d}");
        }
        glm_rt::trace::compiler_trace_signal(glm_rt::trace::TRACE_BUILD_FAIL);
        eprintln!("GLM_TRACE: slot 1 — build failed");
        std::process::exit(1);
    }

    let mut ir_lowerer = lowerer::IrLowerer::new(&shape);
    let ir_program = ir_lowerer.lower_program(&ast);

    if !ir_lowerer.diagnostics.is_empty() {
        for d in &ir_lowerer.diagnostics {
            eprintln!("{d}");
        }
        glm_rt::trace::compiler_trace_signal(glm_rt::trace::TRACE_BUILD_FAIL);
        eprintln!("GLM_TRACE: slot 1 — build failed");
        std::process::exit(1);
    }

    let llvm_ir = match backend::generate_llvm_ir(&ir_program) {
        Ok(ir) => ir,
        Err(diagnostics) => {
            for d in &diagnostics {
                eprintln!("{d}");
            }
            glm_rt::trace::compiler_trace_signal(glm_rt::trace::TRACE_BUILD_FAIL);
            eprintln!("GLM_TRACE: slot 1 — build failed");
            std::process::exit(1);
        }
    };
    std::fs::write("out.ll", llvm_ir).expect("Failed to write out.ll");

    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let runtime = format!(
        "{}/target/{}/libglm_rt.a",
        env!("CARGO_MANIFEST_DIR"),
        profile
    );

    // The linker bypass: no @main, no executable — the module is
    // linked as a shared library whose one export is @glm_exec.
    let status = std::process::Command::new("clang")
        .arg("-O3")
        .arg("-shared")
        .arg("-fPIC")
        .arg("out.ll")
        .arg(&runtime)
        .arg("-lpthread")
        .arg("-ldl")
        .arg("-lm")
        .arg("-o")
        .arg("libglm_out.so")
        .status()
        .expect("Failed to execute clang");

    if !status.success() {
        panic!("Clang failed to assemble and link the shared library.");
    }
    println!("Success! Shared library written to ./libglm_out.so");
    glm_rt::trace::compiler_trace_signal(glm_rt::trace::TRACE_COMPILED);
    eprintln!("GLM_TRACE: slot 0 — compiled ok");

    // Host-side invocation: the extra CLI words cross the boundary as
    // the `arg` table's integer cells — only built when the script
    // actually names `arg`.
    let pass_args = references_arg(&ast);
    let mut host_ints = Vec::with_capacity(args.len() - 2);
    if pass_args {
        for raw in &args[2..] {
            match raw.parse::<i64>() {
                Ok(v) => host_ints.push(v),
                Err(_) => {
                    eprintln!(
                        "glm host error: boundary args must be integers (64-bit), got '{raw}' — \
                         the arg table is Table<Integer>"
                    );
                    std::process::exit(1);
                }
            }
        }
    }
    if let Err(e) = unsafe { run_boundary(&host_ints, pass_args) } {
        eprintln!("glm host error: {e}");
        std::process::exit(1);
    }
}
