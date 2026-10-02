use super::*;
impl Analyzer {
    pub(super) fn check_table_use(&mut self, rec: Gate, obj: &Expr) -> Result<(), ShapeError> {
        if let Expr::Identifier(name) = obj {
            signal!(rec.on(), trace::TRACE_CHK_TBL_IDENT);
            let (_, bind) = self.resolve(rec, name)?;
            if bind.aliases.contains(&NULL_ROOT) && rec.on() {
                signal!(rec.on(), trace::TRACE_CHK_TBL_NIL);
                return Err(ShapeError(format!(
                    "Lifetime Error: '{name}' may be nil here — table reads and \
                     stores through a possibly-nil name are rejected at compile time"
                )));
            }
            if bind.aliases.contains(&MOVED_ROOT) && rec.on() {
                signal!(rec.on(), trace::TRACE_CHK_TBL_NIL);
                return Err(ShapeError(format!(
                    "Lifetime Error: '{name}' may be moved here — table reads and \
                     stores through a possibly-moved name are rejected at compile time"
                )));
            }
        }
        Ok(())
    }

    pub(super) fn check_len_operand(&mut self, rec: Gate, expr: &Expr) -> Result<(), ShapeError> {
        // `#s` on a string: the length rides the NUL-terminated intern
        // (strlen at the lowerer), no allocation — the dense-table gates
        // below are table business only.
        if let Expr::Identifier(name) = expr {
            let (_, bind) = self.resolve(rec, name)?;
            if matches!(bind.ty, Str) {
                return Ok(());
            }
            if scalar(&bind.ty) && !matches!(bind.ty, Pending) {
                return Err(ShapeError(format!(
                    "Type Error: '#' requires a Table or String operand — '{name}' is a scalar"
                )));
            }
        } else if let Expr::String(_) = expr {
            return Ok(());
        }
        let (sites, name, row_read) = match expr {
            Expr::TableCtor(_) => (
                self.lattice
                    .sites
                    .get(&(expr as *const Expr))
                    .into_iter()
                    .copied()
                    .collect::<Vec<_>>(),
                "constructor".to_string(),
                false,
            ),
            Expr::Identifier(name) => {
                let (_, bind) = self.resolve(rec, name)?;
                (
                    bind.aliases
                        .iter()
                        .copied()
                        .filter(|&s| !is_root(&s))
                        .collect(),
                    name.clone(),
                    !bind.lineage.is_empty(),
                )
            }
            _ => {
                return Err(ShapeError(
                    "Type Error: '#' requires a named table or a table constructor".to_string(),
                ));
            }
        };
        if row_read || sites.is_empty() {
            return Err(ShapeError(format!(
                "Type Error: '#' requires a provably dense table — '{name}' has \
                 no compile-time border (a row read out of a table carries none)"
            )));
        }
        for &s in &sites {
            if !self.layout.dense_ctor_len.contains_key(&s)
                || self.reads.user_store_sites.contains(&s)
            {
                return Err(ShapeError(format!(
                    "Type Error: '#' requires a provably dense table — '{name}' has \
                     no compile-time border (stores and sparse constructors make \
                     borders unprovable)"
                )));
            }
        }
        let borders: BTreeSet<i64> = sites
            .iter()
            .filter_map(|s| self.layout.dense_ctor_len.get(s).copied())
            .collect();
        if borders.len() > 1 {
            return Err(ShapeError(format!(
                "Type Error: '#' requires one border — '{name}' may hold tables of \
                 differing lengths"
            )));
        }
        Ok(())
    }

    pub(super) fn check_uses(&mut self, rec: Gate, expr: &Expr) -> Result<(), ShapeError> {
        match expr {
            Expr::Identifier(name) => {
                if let Ok((_, bind)) = self.resolve(rec, name)
                    && bind.aliases.contains(&NULL_ROOT)
                {
                    signal!(rec.on(), trace::TRACE_CHK_TBL_NIL);
                    return Err(ShapeError(format!(
                        "Lifetime Error: '{name}' may be nil here — reads through a \
                         possibly-nil name are rejected at compile time"
                    )));
                }
                if let Ok((_, bind)) = self.resolve(rec, name)
                    && bind.aliases.contains(&MOVED_ROOT)
                {
                    signal!(rec.on(), trace::TRACE_CHK_TBL_NIL);
                    return Err(ShapeError(format!(
                        "Lifetime Error: '{name}' may be moved here — reads through a \
                         possibly-moved name are rejected at compile time"
                    )));
                }
            }
            Expr::TableCtor(entries) => {
                signal!(rec.on(), trace::TRACE_CHK_USE_TBL);
                for (key, val) in entries {
                    if let CtorKey::Expr(ke) = key {
                        self.check_uses(rec, ke)?;
                    }
                    self.check_uses(rec, val)?;
                }
            }
            Expr::Index { obj, key } => {
                signal!(rec.on(), trace::TRACE_CHK_USE_IDX);
                self.check_table_use(rec, obj)?;
                self.check_uses(rec, obj)?;
                self.check_uses(rec, key)?;
            }
            Expr::UnaryOp { op, expr } => {
                if matches!(op, UnOp::Len) {
                    self.check_table_use(rec, expr)?;
                    self.check_len_operand(rec, expr)?;
                }
                self.check_uses(rec, expr)?;
            }
            Expr::BinaryOp { left, right, .. } => {
                signal!(rec.on(), trace::TRACE_CHK_USE_BINOP);
                self.check_uses(rec, left)?;
                self.check_uses(rec, right)?;
            }
            Expr::Call { args, .. } => {
                // The callee names a function, not a value — reads
                // through it were already ruled out at its binding.
                for a in args {
                    self.check_uses(rec, a)?;
                }
            }
            Expr::Function { .. } => {
                // The body's uses were checked at the def-site walk.
            }
            _ => {}
        }
        Ok(())
    }
}
