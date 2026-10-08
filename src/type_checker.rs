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
    // The boundary `arg` seed's unknown id: `local arg` shadows the
    // seed and the seed then never unifies, so the finalizer needs to
    // know the unknown it owns (None) from one that merely stayed
    // unconstrained.
    arg_elem_id: Option<usize>,
    // Set the moment a use of the name `arg` resolves to the ROOT
    // scope's seeded binding (a cell read or a whole-table read): the
    // seed's read flag. Cleared never — later shadows cannot un-read
    // the seed.
    seed_read: bool,
    // The B-path ledger: '#' operands admitted PROVISIONALLY (their
    // type was an unresolved unknown at the use site — a boundary
    // cell, a parameter, a bare local). The whole script's inference
    // gets the last word: the replay after the finalizer resolves
    // each recorded type — Any admits (signal 150), a retroactive
    // scalar pin refuses at the recorded line.
    len_deferrals: Vec<(StaticType, Option<usize>)>,
    // The statement currently being checked — the deferral's error
    // anchor. An inline body checked inside a call site anchors to
    // the call's own statement (the body is checked within it).
    cur_stmt: Option<*const Stmt>,
}

/// The head unknown of a freshly seeded boundary element — fresh_unknown
/// mints descending ids, so this is the id the finalizer binds on default.
fn arg_elem_id(ty: &StaticType) -> usize {
    match ty {
        StaticType::Unknown(id) => *id,
        _ => unreachable!("the boundary seed is minted as an unknown"),
    }
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
            arg_elem_id: None,
            seed_read: false,
            len_deferrals: Vec::new(),
            cur_stmt: None,
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
        // across @glm_exec's parameter. The element type starts as a
        // fresh unknown and is pinned by usage — the first position
        // that demands a type (an arithmetic operand, a comparison, a
        // condition, an equality, a table key) unifies it, so
        // `arg[0] + 1` makes Integer cells and `arg[0] == "x"` String
        // cells. An element nothing pins resolves to Any — the
        // boundary's own dynamic cell — because a clean check with an
        // unread-by-typed-positions seed means the cells were only
        // copied, printed, passed along, or returned, exactly the
        // usage the Any cell serves. Seeded, not declared: an explicit
        // `local arg` shadows it (root scope) — the seed is then never
        // read and no boundary table materializes.
        let arg_elem = self.fresh_unknown();
        self.arg_elem_id = Some(arg_elem_id(&arg_elem));
        self.scopes[0].insert("arg".to_string(), StaticType::Table(Box::new(arg_elem)));
        self.check_block(stmts);
        self.finalize_boundary_elem();
        self.resolve_all_scopes();
        self.replay_len_deferrals();
        self.shape.check_row_reads();
    }

    /// The B-path's last word. Each '#' whose operand the walk could
    /// not pin was admitted provisionally and recorded; the finished
    /// substitution set now names what the operand REALLY is — Any
    /// (the boundary's resolution or a mixed ctor's cell) admits with
    /// the signal, a scalar a later typed use retroactively pinned
    /// refuses at the recorded line, tables and strings were already
    /// fine, and a never-pinned unknown stays admitted (it lowers as
    /// the Ptr dialect and reads the runtime border — total, like
    /// every read in this engine).
    fn replay_len_deferrals(&mut self) {
        for (ty, line) in std::mem::take(&mut self.len_deferrals) {
            match self.resolve_var(&ty) {
                StaticType::Table(_) | StaticType::String | StaticType::Unknown(_) => {}
                StaticType::Any => {
                    signal!(trace::TRACE_CHK_ANY_LEN);
                }
                other => {
                    let msg = format!(
                        "Type Error: '#' requires a Table, String, or Any operand, got {}",
                        type_name(&other)
                    );
                    self.shape.diagnostics.push(match line {
                        Some(l) => format!("line {l}: {msg}"),
                        None => msg,
                    });
                }
            }
        }
    }

    /// Resolve the boundary element to the type the script's own code
    /// demanded and write it where the lowerer, the backend, and the
    /// hosts read it. An element nothing pinned resolves to ANY — the
    /// boundary's own dynamic cell — not an error and not a default:
    /// the script checked clean with the seed unread by any typed
    /// position, which means its cells were only copied, printed,
    /// passed along, or returned. That is exactly the usage set the
    /// Any cell supports, so the script is runnable as-is and the
    /// HOST's words pick each cell's kind at load (the one place
    /// dynamics are allowed). The script still defines its type
    /// whenever it has one: a typed use pins the seed to a concrete
    /// scalar long before this point. A Table demand is rejected:
    /// boundary cells hold scalars the host parses off the command
    /// line, never tables.
    fn finalize_boundary_elem(&mut self) {
        let Some(id) = self.arg_elem_id else { return };
        let elem = match self.resolve_var(&StaticType::Unknown(id)) {
            // Follow the chain to its head unknown: a unify against a
            // constructor's element unknown may have left the arg
            // unknown pointing at it. Only a script that truly reads
            // the seed owes a contract: a shadowing `local arg` (the
            // seed never read) and a script that never mentions it
            // stay silent — the host passes null and the boundary
            // never materializes. A read seed that stayed unresolved
            // is the Any contract.
            StaticType::Unknown(head) => {
                if !self.seed_read {
                    return;
                }
                signal!(trace::TRACE_BOUNDARY_ELEM_ANY);
                self.bind(head, StaticType::Any).ok();
                self.shape.boundary_elem = Some(StaticType::Any);
                self.resolve_fn_defs_through_subs();
                return;
            }
            StaticType::Table(_) => {
                self.shape.diagnostics.push(
                    "Type Error: the boundary 'arg' table's cells hold scalars — \
                     a table element cannot be inferred from 'arg[i]' usage"
                        .to_string(),
                );
                return;
            }
            // The seed was already bound to Any by the script itself —
            // a mixed constructor carried a boundary cell into the
            // dynamic cell (the written opt-in). Same destination as
            // the unresolved seed, same signal: the boundary is Any.
            StaticType::Any => {
                signal!(trace::TRACE_BOUNDARY_ELEM_ANY);
                self.shape.boundary_elem = Some(StaticType::Any);
                self.resolve_fn_defs_through_subs();
                return;
            }
            concrete => concrete,
        };
        signal!(trace::TRACE_BOUNDARY_ELEM_PINNED);
        self.shape.boundary_elem = Some(elem);
        self.resolve_fn_defs_through_subs();
    }

    /// Fn signatures and returns captured unknowns at their call
    /// sites, before the finalizer's bind — re-resolve them through
    /// the now-complete substitutions so the lowerer's inline
    /// expansion sees the same concrete types every other position
    /// does (a pinned scalar, or Any for the unconstrained boundary).
    fn resolve_fn_defs_through_subs(&mut self) {
        for def in self.shape.fn_defs.values_mut() {
            def.ret = Self::resolve_through(&self.substitutions, &def.ret);
            if let Some(sig) = &mut def.signature {
                for ty in sig.iter_mut() {
                    *ty = Self::resolve_through(&self.substitutions, ty);
                }
            }
        }
    }

    fn check_block(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            if let Err(msg) = self.check_stmt(stmt) {
                signal!(trace::TRACE_GHOST_BAIL_CHECKER);
                let line = self.shape.stmt_lines.get(&(stmt as *const Stmt)).copied();
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

    fn var_type(&mut self, name: &str) -> Result<StaticType, String> {
        for (depth, scope) in self.scopes.iter().enumerate().rev() {
            if let Some(ty) = scope.get(name) {
                // The seed's read flag: a use resolving to the ROOT
                // scope's `arg` binding that still carries the seed's
                // unknown (the boundary table itself — a shadowing
                // `local arg` replaced the binding with its own type,
                // so the head id is the discriminator). Only such a
                // read gives the script a boundary contract to pin.
                if depth == 0
                    && name == "arg"
                    && let Some(head) = self.arg_elem_id_head()
                    && let StaticType::Table(inner) = ty
                    && let StaticType::Unknown(id) = self.resolve_var(inner)
                    && id == head
                {
                    self.seed_read = true;
                }
                return Ok(self.resolve_var(ty));
            }
        }
        Err(format!(
            "Scope Error: reference to undeclared variable '{}'",
            name
        ))
    }

    /// The seed unknown's CURRENT head id — the seed may itself have
    /// been unified onto another unknown (a ctor's element), so the
    /// head, not the raw seed id, is what a read resolves to.
    fn arg_elem_id_head(&self) -> Option<usize> {
        self.arg_elem_id
            .map(|id| match self.resolve_var(&StaticType::Unknown(id)) {
                StaticType::Unknown(head) => head,
                _ => id,
            })
    }

    fn check_stmt(&mut self, stmt: &Stmt) -> Result<(), String> {
        // The deferral anchor: any provisional '#' admission inside
        // this statement (an inline body included — it is checked
        // within its call's statement) reports at this line.
        self.cur_stmt = Some(stmt as *const Stmt);
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
                    // The rebind admission: the name's element and the
                    // incoming table's element are both concrete but
                    // join to a THIRD element (a scalar-kind mix —
                    // either side already dynamic, or two kinds
                    // differing) — the lattice's rebind-linked join
                    // speaking. Rebind links are witnesses like any
                    // other, so the sites linked and the name stays
                    // dynamic. Mirrors the ctor arm's admission: the
                    // joined type types the name honestly Table<Any>;
                    // it never pretends a dynamic object is scalar
                    // (that direction has no admission anywhere).
                    if let (StaticType::Table(e1), StaticType::Table(e2)) = (&expected, &actual)
                        && let Some(joined) = static_join(e1, e2)
                        && joined != **e1
                        && joined != **e2
                    {
                        signal!(trace::TRACE_CHK_ANY_REBIND);
                        let resolved = StaticType::Table(Box::new(joined));
                        for scope in self.scopes.iter_mut().rev() {
                            if scope.contains_key(name) {
                                scope.insert(name.clone(), resolved);
                                break;
                            }
                        }
                        return Ok(());
                    }
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
                self.check_index_key(key)?;

                let val_ty = self.check_expr(value)?;

                // A dynamic-cell table absorbs scalar stores — the
                // written opt-in's store half: the lowerer packs the
                // value through glm_any_from_*. An unknown value
                // (a boundary cell) unifies into Any and binds; a
                // TABLE value never rides a dynamic cell; and the
                // reverse direction — an Any value into a typed table
                // — stays the unify conflict below (no unpack: the
                // interpreter cliff stays closed).
                if matches!(self.resolve_var(&elem), StaticType::Any) {
                    match self.resolve_var(&val_ty) {
                        StaticType::Integer
                        | StaticType::Float
                        | StaticType::Boolean
                        | StaticType::String
                        | StaticType::Any
                        | StaticType::Unknown(_) => return Ok(()),
                        StaticType::Table(_) => {
                            return Err(
                                "Type Error: an Any table's cells hold scalars — a table value \
                                 cannot enter a dynamic cell"
                                    .to_string(),
                            );
                        }
                    }
                }

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
                                return Err("Type Error: an internal 'return' carries a value — \
                                     the bare and nil forms belong to the script's host \
                                     boundary"
                                    .to_string());
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

    /// The key position of an index read, a store, or a constructor
    /// entry: a still-unknown key (a boundary cell, or a value a
    /// parameter carried from one) is DEMANDED to Integer — `t[arg[0]]`
    /// means Integer cells — while any other concrete type stays the
    /// long-standing rejection. A prior conflicting demand already
    /// resolved the unknown, so the compare names it instead.
    fn check_index_key(&mut self, key: &Expr) -> Result<(), String> {
        let key_ty = self.check_expr(key)?;
        let key_ty = self.resolve_var(&key_ty);
        let key_ty = if let StaticType::Unknown(id) = key_ty {
            self.bind(id, StaticType::Integer)?;
            StaticType::Integer
        } else {
            key_ty
        };
        if key_ty != StaticType::Integer {
            let note = self.any_provenance_note(&key_ty, key);
            return Err(format!(
                "Type Error: table index must be an Integer, got {}{}",
                type_name(&key_ty),
                note.unwrap_or_default()
            ));
        }
        Ok(())
    }

    fn check_condition(&mut self, condition: &Expr, kw: &str) -> Result<(), String> {
        let ty = self.check_expr(condition)?;
        // Usage inference: an unknown condition operand (a boundary
        // cell) pins to Boolean — `if arg[0] then` means Bool cells.
        let ty = self.pin_bool(&ty)?;
        if ty != StaticType::Boolean {
            let note = self.any_provenance_note(&ty, condition);
            return Err(format!(
                "Type Error: '{}' condition must be a Boolean, got {}{}",
                kw,
                type_name(&ty),
                note.unwrap_or_default()
            ));
        }
        Ok(())
    }

    fn check_expr(&mut self, expr: &Expr) -> Result<StaticType, String> {
        self.check_expr_inner(expr)
    }

    fn check_expr_inner(&mut self, expr: &Expr) -> Result<StaticType, String> {
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
                        self.check_index_key(ke)?;
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
                        // A dynamic-cell site absorbs scalar entries —
                        // the written opt-in: the lowerer packs each
                        // through glm_any_from_* (the analyzer's mixed
                        // join made the site Any). A concrete scalar
                        // simply packs; an unresolved unknown (a
                        // boundary cell) unifies so it BINDS to Any;
                        // a table value never rides a dynamic cell.
                        if matches!(elem, StaticType::Any) {
                            if matches!(ty, StaticType::Table(_)) {
                                return Err(format!(
                                    "Type Error: an Any table's cells hold scalars — a table \
                                     value cannot enter a dynamic cell ({} after Any)",
                                    type_name(&ty)
                                ));
                            }
                            if matches!(ty, StaticType::Unknown(_)) {
                                self.unify(&elem, &ty)?;
                            }
                            continue;
                        }
                        // A ctor element disagreeing with the site's
                        // running element is a conflict — unless one
                        // side is an unresolved unknown (a boundary
                        // cell, a bare local): usage inference unifies
                        // it, so `local u = {arg[0], 2}` pins the
                        // boundary to Integer cells and the ctor to a
                        // Table<Integer>.
                        self.unify(&elem, &ty)?;
                        let resolved = self.resolve_var(&ty);
                        if resolved != self.resolve_var(&elem) {
                            return Err(format!(
                                "Type Error: mixed table constructor elements — {} after {}",
                                type_name(&ty),
                                type_name(&elem)
                            ));
                        }
                    }
                }
                Ok(StaticType::Table(Box::new(elem)))
            }
            Expr::Index { obj, key } => {
                let (elem, _) = self.check_index_base(obj)?;
                self.check_index_key(key)?;
                Ok(elem)
            }
            Expr::Identifier(name) => self.var_type(name),
            Expr::BinaryOp { op, left, right } => {
                let l = self.check_expr(left)?;
                let r = self.check_expr(right)?;
                match op {
                    BinOp::And | BinOp::Or => {
                        // Usage inference: an unknown operand pins to
                        // Boolean — `arg[0] and arg[1]` means Bool cells.
                        let l = self.pin_bool(&l)?;
                        let r = self.pin_bool(&r)?;
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
                        self.numeric_operand(&l, &r, op, left, right)
                    }
                    BinOp::Div => {
                        // An unknown operand pins to its partner before
                        // the numeric demand — `arg[0] / 2` is Integer
                        // cells, `arg[0] / 2.0` Float.
                        self.infer_numeric_pair(&l, &r)?;
                        let l = self.resolve_var(&l);
                        let r = self.resolve_var(&r);
                        if !matches!(
                            (&l, &r),
                            (
                                StaticType::Integer | StaticType::Float,
                                StaticType::Integer | StaticType::Float
                            )
                        ) {
                            let note = self
                                .any_provenance_note(&l, left)
                                .or_else(|| self.any_provenance_note(&r, right));
                            return Err(format!(
                                "Type Error: '/' requires numeric operands{}",
                                note.unwrap_or_default()
                            ));
                        }
                        Ok(StaticType::Float)
                    }
                    BinOp::LessThan | BinOp::GreaterThan | BinOp::LessEq | BinOp::GreaterEq => {
                        self.numeric_operand(&l, &r, op, left, right)?;
                        Ok(StaticType::Boolean)
                    }
                    BinOp::Equal | BinOp::NotEqual => {
                        // Usage inference: an unknown against a concrete
                        // scalar pins to it — `arg[0] == "x"` makes String
                        // cells, `arg[0] == true` Boolean cells. A Table
                        // partner stays un-pinned (params may be tables;
                        // the boundary's cells never are — the finalizer
                        // rejects a Table elem).
                        self.pin_scalar_pair(&l, &r)?;
                        let l = self.resolve_var(&l);
                        let r = self.resolve_var(&r);
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
                        // A boundary cell compared with a table: the
                        // cell can never hold one (the finalizer rejects
                        // a Table element), so name it now instead of
                        // leaving an unknown that would lower as a
                        // repr mismatch. A bare local keeps today's
                        // null-pointer compare.
                        let l_unk = matches!(l, StaticType::Unknown(_));
                        let r_unk = matches!(r, StaticType::Unknown(_));
                        if (l_unk && matches!(r, StaticType::Table(_))
                            || r_unk && matches!(l, StaticType::Table(_)))
                            && (self.is_boundary_chained(&l) || self.is_boundary_chained(&r))
                        {
                            return Err(
                                "Type Error: '==' compares a boundary cell with a table — \
                                 boundary cells hold scalars"
                                    .to_string(),
                            );
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
                    UnOp::Neg => {
                        // Usage inference: an unknown operand of unary
                        // '-' pins to Integer (the numeric default).
                        let t = self.resolve_var(&t);
                        let t = if let StaticType::Unknown(id) = t {
                            self.bind(id, StaticType::Integer)?;
                            StaticType::Integer
                        } else {
                            t
                        };
                        match t {
                            StaticType::Integer | StaticType::Float => Ok(t),
                            _ => {
                                Err("Type Error: unary '-' requires a numeric operand".to_string())
                            }
                        }
                    }
                    UnOp::Not => {
                        // Usage inference: `not arg[0]` means Bool cells.
                        let t = self.pin_bool(&t)?;
                        match t {
                            StaticType::Boolean => Ok(StaticType::Boolean),
                            _ => Err("Type Error: 'not' requires a Boolean operand".to_string()),
                        }
                    }
                    UnOp::Len => match self.resolve_var(&t) {
                        StaticType::Table(_) | StaticType::String => Ok(StaticType::Integer),
                        // The first Any operation: a dynamic cell in
                        // '#' reads its length through glm_any_len —
                        // strlen for the string kind, a named runtime
                        // death for every other (the checker admits
                        // the operand tag-blind; the host's word
                        // picked the kind at load).
                        StaticType::Any => {
                            signal!(trace::TRACE_CHK_ANY_LEN);
                            Ok(StaticType::Integer)
                        }
                        // The B-path: the operand cannot be pinned
                        // YET — a boundary cell, a parameter, a bare
                        // local. Admit provisionally and record; the
                        // replay after the finalizer gives the whole
                        // script's inference the last word (a later
                        // typed use pins the cell and the honest
                        // refusal lands at this line).
                        unknown @ StaticType::Unknown(_) => {
                            let line = self
                                .cur_stmt
                                .and_then(|s| self.shape.stmt_lines.get(&s).copied());
                            self.len_deferrals.push((unknown, line));
                            Ok(StaticType::Integer)
                        }
                        _ => Err(format!(
                            "Type Error: '#' requires a Table, String, or Any operand, got {}",
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
                        return Err("Type Error: calls go through a function variable — only \
                             anonymous functions assigned directly to variables are callable"
                            .to_string());
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
                let def = self.shape.fn_defs.get(&fn_ptr).cloned().ok_or_else(|| {
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
        &mut self,
        l: &StaticType,
        r: &StaticType,
        op: &BinOp,
        l_expr: &Expr,
        r_expr: &Expr,
    ) -> Result<StaticType, String> {
        // Usage inference: an unknown operand (a boundary cell, a bare
        // local) takes its partner's numeric type before the demand.
        let (l, r) = self.infer_numeric_pair(l, r)?;
        match (&l, &r) {
            (StaticType::Integer, StaticType::Integer) => Ok(StaticType::Integer),
            (StaticType::Float, StaticType::Float) => Ok(StaticType::Float),
            (StaticType::Integer, StaticType::Float) | (StaticType::Float, StaticType::Integer) => {
                Err(format!(
                    "Type Error: '{}' does not support mixed Integer and Float operands",
                    bin_op_name(op)
                ))
            }
            _ => {
                // An adopted operand lands here: the read surfaces the
                // conflict, the adopting store caused it — name both.
                let note = self
                    .any_provenance_note(&l, l_expr)
                    .or_else(|| self.any_provenance_note(&r, r_expr));
                Err(format!(
                    "Type Error: '{}' requires numeric operands{}",
                    bin_op_name(op),
                    note.unwrap_or_default()
                ))
            }
        }
    }
}

impl<'a> TypeChecker<'a> {
    fn resolve_var(&self, ty: &StaticType) -> StaticType {
        Self::resolve_through(&self.substitutions, ty)
    }

    /// Whether an unresolved unknown is chained to the boundary seed —
    /// an `arg` cell read, or a value a parameter carried from one
    /// (unification links unknowns into chains; two unknowns share a
    /// head iff they are chained). Bare locals mint their own unknowns
    /// and stay outside.
    fn is_boundary_chained(&self, ty: &StaticType) -> bool {
        let Some(seed) = self.arg_elem_id else {
            return false;
        };
        matches!(
            (
                self.resolve_var(ty),
                self.resolve_var(&StaticType::Unknown(seed)),
            ),
            (StaticType::Unknown(a), StaticType::Unknown(b)) if a == b
        )
    }

    /// Pin an unknown operand to Boolean — a condition, `and`/`or`, or
    /// `not` demanding a Bool makes the boundary cell type Bool.
    fn pin_bool(&mut self, ty: &StaticType) -> Result<StaticType, String> {
        let resolved = self.resolve_var(ty);
        if let StaticType::Unknown(id) = resolved {
            self.bind(id, StaticType::Boolean)?;
            return Ok(StaticType::Boolean);
        }
        Ok(resolved)
    }

    /// Pin a pair of numeric operands: an unknown takes its concrete
    /// numeric partner's type; two unknowns take Integer, the boundary
    /// default — `arg[0] + 1` is Integer cells, `arg[0] + 0.0` Float.
    fn infer_numeric_pair(
        &mut self,
        l: &StaticType,
        r: &StaticType,
    ) -> Result<(StaticType, StaticType), String> {
        let lr = self.resolve_var(l);
        let rr = self.resolve_var(r);
        let num = |t: &StaticType| matches!(t, StaticType::Integer | StaticType::Float);
        let pin = |me: &mut Self, t: &StaticType, to: &StaticType| -> Result<(), String> {
            if let StaticType::Unknown(id) = t {
                me.bind(*id, to.clone())?;
            }
            Ok(())
        };
        let unknown = |t: &StaticType| matches!(t, StaticType::Unknown(_));
        match (&lr, &rr) {
            (a, b) if num(a) && num(b) => Ok((lr.clone(), rr.clone())),
            (a, b) if num(a) && unknown(b) => {
                pin(self, b, a)?;
                Ok((a.clone(), a.clone()))
            }
            (a, b) if num(b) && unknown(a) => {
                pin(self, a, b)?;
                Ok((b.clone(), b.clone()))
            }
            (a, b) if unknown(a) && unknown(b) => {
                // Neither side concrete numeric: two boundary reads
                // (`arg[0] + arg[1]`) or bare locals — the numeric
                // default is Integer, exactly what an unconstrained
                // boundary element already means.
                pin(self, a, &StaticType::Integer)?;
                pin(self, b, &StaticType::Integer)?;
                Ok((StaticType::Integer, StaticType::Integer))
            }
            // A concrete non-numeric operand (Bool after a condition
            // pinned the boundary) passes through untouched — the
            // caller's match rejects it with its own message.
            _ => Ok((lr, rr)),
        }
    }

    /// Pin an unknown operand to its concrete scalar partner in an
    /// equality — `arg[0] == "x"` makes String cells, `arg[0] == true`
    /// Boolean. A Table partner pins nothing (cells are never tables;
    /// the compatible check answers) and two unknowns stay open.
    fn pin_scalar_pair(&mut self, l: &StaticType, r: &StaticType) -> Result<(), String> {
        let lr = self.resolve_var(l);
        let rr = self.resolve_var(r);
        let scalar = |t: &StaticType| {
            matches!(
                t,
                StaticType::Integer | StaticType::Float | StaticType::Boolean | StaticType::String
            )
        };
        match (&lr, &rr) {
            (a, StaticType::Unknown(id)) if scalar(a) => {
                self.bind(*id, a.clone())?;
            }
            (StaticType::Unknown(id), b) if scalar(b) => {
                self.bind(*id, b.clone())?;
            }
            _ => {}
        }
        Ok(())
    }

    /// The adoption provenance note for a typed-position refusal: the
    /// operand resolves to Any through a name the analyzer watched go
    /// dynamic, so the error names the witness line too — the CAUSE
    /// (the adopting store or mixed constructor), not just where the
    /// conflict surfaced (this read). None when the operand is not
    /// Any-through-a-name or the name carries no recorded witness
    /// (the boundary's own Any resolution names no line — the
    /// boundary IS the opt-in).
    fn any_provenance_note(&self, ty: &StaticType, expr: &Expr) -> Option<String> {
        if !matches!(self.resolve_var(ty), StaticType::Any) {
            return None;
        }
        let name = match expr {
            Expr::Identifier(n) => n,
            Expr::Index { obj, .. } => match obj.as_ref() {
                Expr::Identifier(n) => n,
                _ => return None,
            },
            _ => return None,
        };
        let line = self.shape.adopt_lines.get(name)?;
        signal!(trace::TRACE_CHK_ANY_PROVENANCE);
        Some(format!(
            " — '{name}' went dynamic at line {line} (the scalar-kind mix on its cells; \
             adoption is name-global)"
        ))
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

/// The checker's mirror of the analyzer's element join (join_ty): two
/// concrete elements that mix scalar kinds — directly or through
/// table layers — join to the dynamic cell instead of conflicting.
/// None = a side is unresolved (the caller's unify path binds it) or
/// the join is a genuine conflict (shapes disagree); Some(side) for
/// equal/absorption cases (the caller's inequality checks decide
/// whether an admission is happening at all).
fn static_join(l: &StaticType, r: &StaticType) -> Option<StaticType> {
    use crate::shape::{Ty, join_ty};
    fn to_ty(t: &StaticType) -> Option<Ty> {
        Some(match t {
            StaticType::Integer => Ty::Int,
            StaticType::Float => Ty::Flt,
            StaticType::Boolean => Ty::Bool,
            StaticType::String => Ty::Str,
            StaticType::Any => Ty::Any,
            StaticType::Table(inner) => Ty::Tbl(Box::new(to_ty(inner)?)),
            StaticType::Unknown(_) => return None,
        })
    }
    fn to_static(t: &Ty) -> Option<StaticType> {
        Some(match t {
            Ty::Int => StaticType::Integer,
            Ty::Flt => StaticType::Float,
            Ty::Bool => StaticType::Boolean,
            Ty::Str => StaticType::String,
            Ty::Any => StaticType::Any,
            Ty::Tbl(inner) => StaticType::Table(Box::new(to_static(inner)?)),
            Ty::Pending | Ty::Conflict => return None,
        })
    }
    let joined = join_ty(&to_ty(l)?, &to_ty(r)?);
    to_static(&joined)
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
            StaticType::Any => "AnyTable".to_string(),
            _ => format!("Table of {}", type_name(elem)),
        },
        StaticType::Unknown(_) => "?".to_string(),
        StaticType::Any => "Any".to_string(),
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
