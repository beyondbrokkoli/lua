use crate::ast::{BinOp, CtorKey, Expr, StaticType, Stmt, UnOp};
use crate::shape::{FnDef, ShapeFacts};
use glm_rt::{signal, trace};
use std::collections::BTreeMap;

const BARE_LOCAL_MAX: usize = usize::MAX;

/// What `return` must hand over in the current scope. The root context
/// is the @glm_exec boundary contract; an internal scope (one per
/// inline closure, later) carries its own expected type so its returns
/// validate against the caller's contract, not the host's.
#[derive(Clone)]
enum ReturnCtx {
    /// The FFI boundary: a table crosses to the host, or nil for null.
    /// Scalars and strings stay behind the boundary.
    Boundary,
    /// An internal scope's contract: every `return` agrees with the
    /// expected type (unify widens Unknowns as divergent returns join).
    Internal(StaticType),
}

pub struct TypeChecker<'a> {
    scopes: Vec<BTreeMap<String, StaticType>>,
    // Inline-closure bindings, parallel to `scopes`: name -> the
    // `Expr::Function` node it holds, or None where a non-function
    // binding shadows one (a tombstone, so shadowing hides the fn).
    fn_scopes: Vec<BTreeMap<String, Option<*const Expr>>>,
    shape: &'a mut ShapeFacts,
    substitutions: BTreeMap<usize, StaticType>,
    bare_next: usize,
    bare_ids: BTreeMap<(*const Stmt, String), usize>,
    // One entry per return-bearing scope, innermost last. Pushed by
    // check_program (the boundary), by push_return_ctx (inline closure
    // bodies, at their call sites); never popped past empty.
    return_ctxs: Vec<ReturnCtx>,
    // The chain of function bodies currently being inline-checked — a
    // call reaching back into itself cannot be inlined.
    inline_stack: Vec<*const Expr>,
}

impl<'a> TypeChecker<'a> {
    pub fn new(shape: &'a mut ShapeFacts) -> Self {
        Self {
            scopes: vec![BTreeMap::new()],
            fn_scopes: vec![BTreeMap::new()],
            shape,
            substitutions: BTreeMap::new(),
            bare_next: BARE_LOCAL_MAX,
            bare_ids: BTreeMap::new(),
            return_ctxs: vec![ReturnCtx::Boundary],
            inline_stack: Vec::new(),
        }
    }

    fn fresh_unknown(&mut self) -> StaticType {
        let id = self.bare_next;
        self.bare_next -= 1;
        StaticType::Unknown(id)
    }

    fn declare_fn_binding(&mut self, name: &str, ptr: Option<*const Expr>) {
        self.fn_scopes
            .last_mut()
            .unwrap()
            .insert(name.to_string(), ptr);
    }

    fn assign_fn_binding(&mut self, name: &str, ptr: Option<*const Expr>) {
        let depth = self
            .fn_scopes
            .iter()
            .rposition(|s| s.contains_key(name))
            .unwrap_or(self.fn_scopes.len() - 1);
        self.fn_scopes[depth].insert(name.to_string(), ptr);
    }

    /// Register a directly-bound anonymous function: the signature
    /// (params + cloned body + a fresh return Unknown shared by every
    /// call site) goes to the facts bus; the variable itself gets a
    /// placeholder type no value ever reads.
    fn bind_function(&mut self, name: &str, expr: &Expr) -> StaticType {
        let Expr::Function { params, body } = expr else {
            unreachable!("bind_function on a non-function");
        };
        let ret = self.fresh_unknown();
        self.shape.fn_defs.insert(
            expr as *const Expr,
            FnDef {
                params: params.clone(),
                body: body.as_slice(),
                ret: ret.clone(),
                signature: None,
            },
        );
        self.declare_fn_binding(name, Some(expr as *const Expr));
        self.fresh_unknown()
    }

    /// Enter an internal scope whose `return`s must agree with
    /// `expected`. Called around each inline closure body at its call
    /// sites; the boundary context is pushed in `new` and never popped.
    fn push_return_ctx(&mut self, expected: StaticType) {
        self.return_ctxs.push(ReturnCtx::Internal(expected));
    }

    pub fn check_program(&mut self, stmts: &[Stmt]) {
        // The FFI boundary: `arg` names the table the host passes
        // across @glm_exec's parameter — 8-byte integer cells, the
        // one contract both sides of the boundary share. Seeded, not
        // declared: an explicit `local arg` shadows it (root scope).
        self.scopes[0].insert(
            "arg".to_string(),
            StaticType::Table(Box::new(StaticType::Integer)),
        );
        self.check_block(stmts);
        self.resolve_all_scopes();
        self.shape.check_row_reads();
    }

    fn check_block(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            if let Err(msg) = self.check_stmt(stmt) {
                signal!(trace::TRACE_GHOST_BAIL_CHECKER);
                let line = self
                    .shape
                    .stmt_lines
                    .get(&(stmt as *const Stmt))
                    .copied();
                self.shape.diagnostics.push(match line {
                    Some(l) => format!("line {l}: {msg}"),
                    None => msg,
                });
                return;
            }
        }
    }

    fn resolve_all_scopes(&mut self) {
        for scope in self.scopes.iter_mut() {
            for ty in scope.values_mut() {
                *ty = Self::resolve_through(&self.substitutions, ty);
            }
        }
        for ((stmt, name), id) in &self.bare_ids {
            let resolved = Self::resolve_through(&self.substitutions, &StaticType::Unknown(*id));
            self.shape
                .local_types
                .insert((*stmt, name.clone()), resolved);
        }
    }

    fn begin_scope(&mut self) {
        self.scopes.push(BTreeMap::new());
        self.fn_scopes.push(BTreeMap::new());
    }
    fn end_scope(&mut self) {
        self.fn_scopes.pop();
        self.scopes.pop().expect("Cannot pop global scope");
    }

    fn declare_var(&mut self, name: String, ty: StaticType) -> Result<(), String> {
        // The seeded boundary `arg` is shadowable at the root, where it
        // was seeded — a script's own `local arg` takes the name over.
        let boundary_seed = name == "arg" && self.scopes.len() == 1;
        let current_scope = self.scopes.last_mut().unwrap();
        if current_scope.contains_key(&name) && !boundary_seed {
            return Err(format!(
                "Scope Error: variable '{}' already declared in this scope",
                name
            ));
        }
        current_scope.insert(name, ty);
        Ok(())
    }

    fn var_type(&self, name: &str) -> Result<StaticType, String> {
        for scope in self.scopes.iter().rev() {
            if let Some(ty) = scope.get(name) {
                return Ok(self.resolve_var(ty));
            }
        }
        Err(format!(
            "Scope Error: reference to undeclared variable '{}'",
            name
        ))
    }

    fn check_stmt(&mut self, stmt: &Stmt) -> Result<(), String> {
        match stmt {
            Stmt::LocalDecl { names, exprs } => {
                let mut expr_types = Vec::with_capacity(exprs.len());
                for (name, expr) in names.iter().zip(exprs.iter()) {
                    if let Expr::Function { .. } = expr {
                        expr_types.push(self.bind_function(name, expr));
                    } else {
                        self.declare_fn_binding(name, None);
                        expr_types.push(self.check_expr(expr)?);
                    }
                }
                for name in &names[exprs.len()..] {
                    self.declare_fn_binding(name, None);
                }
                for (name, ty) in names.iter().zip(expr_types) {
                    self.declare_var(name.clone(), ty)?;
                }
                for name in &names[exprs.len()..] {
                    let id = self.bare_next;
                    self.bare_next -= 1;
                    self.declare_var(name.clone(), StaticType::Unknown(id))?;
                    self.bare_ids
                        .insert((stmt as *const Stmt, name.clone()), id);
                }
            }
            Stmt::Assignment { name, expr } => {
                if let Expr::Function { .. } = expr {
                    // Rebinding to a fresh function: the signature and
                    // the fn binding both move; the variable keeps its
                    // placeholder type.
                    let ty = self.bind_function(name, expr);
                    for scope in self.scopes.iter_mut().rev() {
                        if scope.contains_key(name) {
                            scope.insert(name.clone(), ty);
                            break;
                        }
                    }
                    return Ok(());
                }
                self.assign_fn_binding(name, None);
                let expected = self.var_type(name)?;
                if matches!(expr, Expr::Nil) {
                    if !matches!(expected, StaticType::Table(_) | StaticType::Unknown(_)) {
                        return Err(format!(
                            "Type Error: 'nil' releases tables — '{}' is of type {}",
                            name,
                            type_name(&expected)
                        ));
                    }
                    return Ok(());
                }
                let actual = self.check_expr(expr)?;
                if expected != actual {
                    // A rebind unifies: an unwitnessed ctor's element
                    // binds to the name's, so `t = {}` re-opens the
                    // name under its own element type — the displaced
                    // header's fate is the analyzer's rebind free, not
                    // this slot's business. Monomorphism lives on the
                    // name: a later store through it still polices.
                    self.unify(&expected, &actual)?;
                    let resolved = self.resolve_var(&actual);
                    for scope in self.scopes.iter_mut().rev() {
                        if scope.contains_key(name) {
                            scope.insert(name.clone(), resolved);
                            break;
                        }
                    }
                }
            }
            Stmt::IndexAssign { obj, key, value } => {
                let (elem, _obj_desc) = self.check_index_base(obj)?;
                let key_ty = self.check_expr(key)?;
                if key_ty != StaticType::Integer {
                    return Err(format!(
                        "Type Error: table index must be an Integer, got {}",
                        type_name(&key_ty)
                    ));
                }

                let val_ty = self.check_expr(value)?;

                if matches!(elem, StaticType::Table(_)) && matches!(val_ty, StaticType::Table(_)) {
                    let expected_inner = match &elem {
                        StaticType::Table(inner) => inner.as_ref(),
                        _ => unreachable!(),
                    };
                    let actual_inner = match &val_ty {
                        StaticType::Table(inner) => inner.as_ref(),
                        _ => unreachable!(),
                    };
                    self.unify(expected_inner, actual_inner)?;
                } else {
                    self.unify(&elem, &val_ty)?;
                }
            }
            Stmt::While { condition, body } => {
                self.check_condition(condition, "while")?;
                self.begin_scope();
                self.check_block(body);
                self.end_scope();
            }
            Stmt::If {
                condition,
                then_body,
                else_body,
            } => {
                self.check_condition(condition, "if")?;
                self.begin_scope();
                self.check_block(then_body);
                self.end_scope();
                self.begin_scope();
                self.check_block(else_body);
                self.end_scope();
            }
            Stmt::Do { body } => {
                self.begin_scope();
                self.check_block(body);
                self.end_scope();
            }
            Stmt::Print { exprs } => {
                for e in exprs {
                    let ty = self.check_expr(e)?;
                    if matches!(ty, StaticType::Table(_)) {
                        return Err("Type Error: cannot print a table — print its cells instead"
                            .to_string());
                    }
                }
            }
            Stmt::Expr { expr } => {
                // A call on its own line: checked, value discarded.
                let _ = self.check_expr(expr)?;
            }
            Stmt::Return { value } => {
                match self.return_ctxs.last().cloned() {
                    // The C-ABI boundary returns one pointer: a table
                    // the host owns from here on, or null. Scalars and
                    // strings stay behind the boundary.
                    Some(ReturnCtx::Boundary) => match value {
                        None | Some(Expr::Nil) => {}
                        Some(expr) => match self.check_expr(expr)? {
                            StaticType::Table(_) => {}
                            other => {
                                return Err(format!(
                                    "Type Error: 'return' hands a value to the host — \
                                     return a table (or nil for null), got {}",
                                    type_name(&other)
                                ));
                            }
                        },
                    },
                    // An internal scope's contract: every return
                    // agrees with the expected type; the bare and nil
                    // forms belong to the host boundary, not a caller.
                    Some(ReturnCtx::Internal(expected)) => {
                        let expr = match value {
                            Some(expr) if !matches!(expr, Expr::Nil) => expr,
                            _ => {
                                return Err(
                                    "Type Error: an internal 'return' carries a value — \
                                     the bare and nil forms belong to the script's host \
                                     boundary"
                                        .to_string(),
                                );
                            }
                        };
                        let actual = self.check_expr(expr)?;
                        self.unify(&expected, &actual)?;
                        let resolved = self.resolve_var(&actual);
                        if let Some(ReturnCtx::Internal(ty)) = self.return_ctxs.last_mut() {
                            *ty = resolved;
                        }
                    }
                    None => unreachable!("new pushes the root return context"),
                }
            }
        }
        Ok(())
    }

    fn check_index_base(&mut self, obj: &Expr) -> Result<(StaticType, String), String> {
        let ty = self.check_expr(obj)?;
        match ty {
            StaticType::Table(elem) => {
                let desc = type_name(&StaticType::Table(elem.clone()));
                Ok((*elem, desc))
            }
            _ => Err(format!(
                "Type Error: cannot index {} — only tables support '[]'",
                type_name(&ty)
            )),
        }
    }

    fn check_condition(&mut self, condition: &Expr, kw: &str) -> Result<(), String> {
        let ty = self.check_expr(condition)?;
        if ty != StaticType::Boolean {
            return Err(format!(
                "Type Error: '{}' condition must be a Boolean, got {}",
                kw,
                type_name(&ty)
            ));
        }
        Ok(())
    }

    fn check_expr(&mut self, expr: &Expr) -> Result<StaticType, String> {
        match expr {
            Expr::Integer(_) => Ok(StaticType::Integer),
            Expr::Float(_) => Ok(StaticType::Float),
            Expr::Boolean(_) => Ok(StaticType::Boolean),
            Expr::String(_) => Ok(StaticType::String),
            Expr::Nil => {
                signal!(trace::TRACE_CHK_NIL_EXPR);
                Err(
                    "Type Error: 'nil' is only valid as the right-hand side of 't = nil' — \
                     it releases a table's memory, it is not a value"
                        .to_string(),
                )
            }
            Expr::TableCtor(entries) => {
                let elem = self.shape.elem_of(expr);
                for (key, e) in entries {
                    if let CtorKey::Expr(ke) = key {
                        let key_ty = self.check_expr(ke)?;
                        if key_ty != StaticType::Integer {
                            return Err(format!(
                                "Type Error: table index must be an Integer, got {}",
                                type_name(&key_ty)
                            ));
                        }
                    }
                    let ty = self.check_expr(e)?;
                    if matches!(elem, StaticType::Table(_)) && matches!(ty, StaticType::Table(_)) {
                        let expected_inner = match &elem {
                            StaticType::Table(inner) => inner.as_ref(),
                            _ => unreachable!(),
                        };
                        let actual_inner = match &ty {
                            StaticType::Table(inner) => inner.as_ref(),
                            _ => unreachable!(),
                        };
                        if expected_inner != actual_inner {
                            self.unify(expected_inner, actual_inner)?;
                        }
                    } else if ty != elem {
                        return Err(format!(
                            "Type Error: mixed table constructor elements — {} after {}",
                            type_name(&ty),
                            type_name(&elem)
                        ));
                    }
                }
                Ok(StaticType::Table(Box::new(elem)))
            }
            Expr::Index { obj, key } => {
                let (elem, _) = self.check_index_base(obj)?;
                let key_ty = self.check_expr(key)?;
                if key_ty != StaticType::Integer {
                    return Err(format!(
                        "Type Error: table index must be an Integer, got {}",
                        type_name(&key_ty)
                    ));
                }
                Ok(elem)
            }
            Expr::Identifier(name) => self.var_type(name),
            Expr::BinaryOp { op, left, right } => {
                let l = self.check_expr(left)?;
                let r = self.check_expr(right)?;
                match op {
                    BinOp::And | BinOp::Or => {
                        if l == StaticType::Boolean && r == StaticType::Boolean {
                            Ok(StaticType::Boolean)
                        } else {
                            Err(format!(
                                "Type Error: '{}' requires Boolean operands on both sides",
                                match op {
                                    BinOp::And => "and",
                                    _ => "or",
                                }
                            ))
                        }
                    }
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::IntDiv | BinOp::Mod => {
                        self.numeric_operand(&l, &r, op)
                    }
                    BinOp::Div => {
                        if !matches!(
                            (l, r),
                            (
                                StaticType::Integer | StaticType::Float,
                                StaticType::Integer | StaticType::Float
                            )
                        ) {
                            return Err("Type Error: '/' requires numeric operands".to_string());
                        }
                        Ok(StaticType::Float)
                    }
                    BinOp::LessThan | BinOp::GreaterThan | BinOp::LessEq | BinOp::GreaterEq => {
                        self.numeric_operand(&l, &r, op)?;
                        Ok(StaticType::Boolean)
                    }
                    BinOp::Equal | BinOp::NotEqual => {
                        // String equality is identity over the pool intern
                        // space: each distinct literal holds one address, so
                        // the compare is an icmp on the shared Ptr repr. A
                        // String against anything else — including an
                        // Unknown, whose cell bits are no trustworthy
                        // pointer — stays rejected.
                        if l == StaticType::String && r == StaticType::String {
                            signal!(trace::TRACE_CHK_STR_EQ);
                            return Ok(StaticType::Boolean);
                        }
                        if l == StaticType::String || r == StaticType::String {
                            return Err(format!(
                                "Type Error: '{}' compares String with {}",
                                match op {
                                    BinOp::Equal => "==",
                                    _ => "~=",
                                },
                                type_name(if l == StaticType::String { &r } else { &l })
                            ));
                        }
                        if !types_compatible(&l, &r) {
                            return Err(format!(
                                "Type Error: '{}' compares {} with {}",
                                match op {
                                    BinOp::Equal => "==",
                                    _ => "~=",
                                },
                                type_name(&l),
                                type_name(&r)
                            ));
                        }
                        Ok(StaticType::Boolean)
                    }
                }
            }
            Expr::UnaryOp { op, expr } => {
                let t = self.check_expr(expr)?;
                match op {
                    UnOp::Neg => match t {
                        StaticType::Integer | StaticType::Float => Ok(t),
                        _ => Err("Type Error: unary '-' requires a numeric operand".to_string()),
                    },
                    UnOp::Not => match t {
                        StaticType::Boolean => Ok(StaticType::Boolean),
                        _ => Err("Type Error: 'not' requires a Boolean operand".to_string()),
                    },
                    UnOp::Len => match self.resolve_var(&t) {
                        StaticType::Table(_) | StaticType::String => Ok(StaticType::Integer),
                        _ => Err(format!(
                            "Type Error: '#' requires a Table or String operand, got {}",
                            type_name(&t)
                        )),
                    },
                }
            }
            Expr::SysAllocCount => Ok(StaticType::Integer),
            Expr::Function { .. } => Err(
                "Type Error: a function value binds directly to a variable — \
                 'local f = function(a, b) ... end'"
                    .to_string(),
            ),
            Expr::Call { callee, args } => {
                // Inline closure call: the body is checked right here,
                // with the arguments unified into the parameters and
                // the returns joined through an Internal context — the
                // type-checking twin of the lowerer's inline expansion.
                let name = match callee.as_ref() {
                    Expr::Identifier(n) => n.clone(),
                    _ => {
                        return Err(
                            "Type Error: calls go through a function variable — only \
                             anonymous functions assigned directly to variables are callable"
                                .to_string(),
                        );
                    }
                };
                let fn_ptr = self
                    .fn_scopes
                    .iter()
                    .rev()
                    .find_map(|s| s.get(&name).copied())
                    .flatten()
                    .ok_or_else(|| {
                        format!(
                            "Type Error: '{name}' is not a function — only anonymous \
                             functions assigned directly to variables are callable"
                        )
                    })?;
                if self.inline_stack.contains(&fn_ptr) {
                    return Err(format!(
                        "Type Error: '{name}' is recursive — inline closures expand at \
                         their call sites, so a call chain may not reach back into itself"
                    ));
                }
                let def = self
                    .shape
                    .fn_defs
                    .get(&fn_ptr)
                    .cloned()
                    .ok_or_else(|| {
                        format!(
                            "Type Error: '{name}' holds no function body — \
                             the checker and the analyzer disagree"
                        )
                    })?;
                if args.len() != def.params.len() {
                    return Err(format!(
                        "Type Error: '{name}' takes {} argument(s), got {}",
                        def.params.len(),
                        args.len()
                    ));
                }
                let mut arg_tys = Vec::with_capacity(args.len());
                for a in args {
                    arg_tys.push(self.check_expr(a)?);
                }

                // Calls are monomorphic: the first call's argument
                // types fix the signature; later calls must agree.
                // Fresh parameter unknowns per call would otherwise
                // make `f(1)` then `f("s")` pass whenever the body
                // doesn't conflict on its own.
                match &def.signature {
                    None => {
                        if let Some(d) = self.shape.fn_defs.get_mut(&fn_ptr) {
                            d.signature = Some(arg_tys.clone());
                        }
                    }
                    Some(fixed) => {
                        for (i, (f, a)) in fixed.iter().zip(arg_tys.iter()).enumerate() {
                            if self.unify(f, a).is_err() {
                                let want = type_name(&self.resolve_var(f));
                                let got = type_name(&self.resolve_var(a));
                                return Err(format!(
                                    "Type Error: argument {} of '{name}' is {got} where \
                                     the first call fixed {want} — calls are monomorphic",
                                    i + 1
                                ));
                            }
                        }
                    }
                }

                // The original AST nodes: pointer-keyed facts (ctor
                // sites, frees) must line up with what the analyzer
                // walked at the definition site.
                let body = unsafe { def.body() };

                self.inline_stack.push(fn_ptr);
                self.begin_scope();
                for (p, aty) in def.params.iter().zip(arg_tys) {
                    let pty = self.fresh_unknown();
                    self.declare_var(p.clone(), pty.clone())?;
                    self.unify(&pty, &aty)?;
                }
                self.push_return_ctx(def.ret.clone());
                self.check_block(body);
                let ret = match self.return_ctxs.pop() {
                    Some(ReturnCtx::Internal(ty)) => self.resolve_var(&ty),
                    _ => unreachable!("push_return_ctx just pushed an Internal context"),
                };
                self.end_scope();
                self.inline_stack.pop();

                if let Some(d) = self.shape.fn_defs.get_mut(&fn_ptr) {
                    d.ret = ret.clone();
                }
                self.shape.call_defs.insert(expr as *const Expr, fn_ptr);
                Ok(ret)
            }
        }
    }

    fn numeric_operand(
        &self,
        l: &StaticType,
        r: &StaticType,
        op: &BinOp,
    ) -> Result<StaticType, String> {
        match (l, r) {
            (StaticType::Integer, StaticType::Integer) => Ok(StaticType::Integer),
            (StaticType::Float, StaticType::Float) => Ok(StaticType::Float),
            (StaticType::Integer, StaticType::Float) | (StaticType::Float, StaticType::Integer) => {
                Err(format!(
                    "Type Error: '{}' does not support mixed Integer and Float operands",
                    bin_op_name(op)
                ))
            }
            _ => Err(format!(
                "Type Error: '{}' requires numeric operands",
                bin_op_name(op)
            )),
        }
    }
}

impl<'a> TypeChecker<'a> {
    fn resolve_var(&self, ty: &StaticType) -> StaticType {
        Self::resolve_through(&self.substitutions, ty)
    }

    fn resolve_through(substitutions: &BTreeMap<usize, StaticType>, ty: &StaticType) -> StaticType {
        match ty {
            StaticType::Unknown(id) => match substitutions.get(id) {
                Some(resolved) => Self::resolve_through(substitutions, resolved),
                None => ty.clone(),
            },
            StaticType::Table(inner) => {
                StaticType::Table(Box::new(Self::resolve_through(substitutions, inner)))
            }
            other => other.clone(),
        }
    }

    fn occurs(&self, id: usize, ty: &StaticType) -> bool {
        match ty {
            StaticType::Unknown(other) => {
                *other == id
                    || self
                        .substitutions
                        .get(other)
                        .is_some_and(|bound| self.occurs(id, bound))
            }
            StaticType::Table(inner) => self.occurs(id, inner),
            _ => false,
        }
    }

    fn bind(&mut self, id: usize, ty: StaticType) -> Result<(), String> {
        if self.occurs(id, &ty) {
            signal!(trace::TRACE_FAIL_OCCURS_CHECK);
            return Err("Type Error: cyclic table type — a table's element type \
                 may not contain the table itself (glm's element types are finite)"
                .to_string());
        }
        signal!(trace::TRACE_CHK_UNIFY);
        self.substitutions.insert(id, ty.clone());
        self.shape.substitutions.insert(id, ty);
        Ok(())
    }

    fn unify(&mut self, expected: &StaticType, actual: &StaticType) -> Result<(), String> {
        let expected = self.resolve_var(expected);
        let actual = self.resolve_var(actual);
        match (&expected, &actual) {
            (a, b) if a == b => Ok(()),

            (StaticType::Unknown(id_a), StaticType::Unknown(id_b)) if id_a != id_b => {
                self.bind(*id_b, StaticType::Unknown(*id_a))
            }

            (StaticType::Unknown(_), _) | (_, StaticType::Unknown(_)) => {
                let (u_id, c_ty) = match (&expected, &actual) {
                    (StaticType::Unknown(id), ty) => (*id, ty.clone()),
                    (ty, StaticType::Unknown(id)) => (*id, ty.clone()),
                    _ => unreachable!(),
                };
                self.bind(u_id, c_ty)
            }

            (StaticType::Table(e1), StaticType::Table(e2)) => self.unify(e1, e2),

            _ => Err(format!(
                "Type Error: type conflict — {} vs {}",
                type_name(&expected),
                type_name(&actual)
            )),
        }
    }
}

fn types_compatible(l: &StaticType, r: &StaticType) -> bool {
    match (l, r) {
        (a, b) if a == b => true,
        (StaticType::Unknown(_), StaticType::Unknown(_)) => true,
        (StaticType::Unknown(_), _) | (_, StaticType::Unknown(_)) => true,
        (StaticType::Table(e1), StaticType::Table(e2)) => types_compatible(e1, e2),
        _ => false,
    }
}

fn type_name(ty: &StaticType) -> String {
    match ty {
        StaticType::Integer => "Integer".to_string(),
        StaticType::Float => "Float".to_string(),
        StaticType::Boolean => "Boolean".to_string(),
        StaticType::String => "String".to_string(),
        StaticType::Table(elem) => match **elem {
            StaticType::Integer => "IntTable".to_string(),
            StaticType::Float => "FloatTable".to_string(),
            StaticType::Boolean => "BoolTable".to_string(),
            StaticType::String => "StringTable".to_string(),
            StaticType::Unknown(_) => "Table<?>".to_string(),
            _ => format!("Table of {}", type_name(elem)),
        },        StaticType::Unknown(_) => "?".to_string(),
    }
}

fn bin_op_name(op: &BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::IntDiv => "//",
        BinOp::Mod => "%",
        BinOp::LessThan => "<",
        BinOp::GreaterThan => ">",
        BinOp::LessEq => "<=",
        BinOp::GreaterEq => ">=",
        BinOp::Equal => "==",
        BinOp::NotEqual => "~=",
        BinOp::And => "and",
        BinOp::Or => "or",
    }
}
