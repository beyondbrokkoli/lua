# ==============================================================================
# GLM Compile-Phase Exploration: The Affine Ownership Machine
#
# Inspects the COMPILER's Rust state (borrow checker / lifetime pass / AST
# lowering) — not the runtime. One script, three faces; each face arms only
# the breakpoints its case can reach, so nothing needs editing between runs:
#
#   # 1. Affine move & poisoning   (local u = t)
#   rust-gdb -q -nx -x .gdbinit.agent --batch -x gdb/explore_affine.gdb --args \
#       ./target/debug/glm cases/explore_move_poison.lua
#
#   # 2. If-join phis & the faithful handle   (pick = a / pick = b)
#   rust-gdb -q -nx -x .gdbinit.agent --batch -x gdb/explore_affine.gdb --args \
#       ./target/debug/glm cases/explore_join_phi.lua
#
#   # 3. Deep-free transfer   (m[0] = {7, 8})  --debug so the Lua-line stop
#   #    keeps its module DWARF for parg
#   rust-gdb -q -nx -x .gdbinit.agent --batch -x gdb/explore_affine.gdb --args \
#       ./target/debug/glm --debug cases/explore_deep_free.lua
#
# RECON PASS (the tag team — signals as the breakpoint map):
#   ./target/debug/glm cases/<case>.lua >/dev/null && python3 plate.py
# The plate is the recording pass's event chronology. Every line below that
# carries a `signal!` is a plate event; breaking at the poke site turns the
# decoded itinerary into live Rust state. The generic mechanism — stop at ANY
# plate event through the single choke point every fire routes through — is:
#
#   break glm_rt::trace::compiler_trace_signal if slot == 126
#   commands
#     silent
#     printf "[SLOT 126 fired]\n"
#     cbt 4
#     continue
#   end
#
# (slot numbers live in trace_signals.txt; 126 = STMT_IDX_STORED_CTOR.
#  Conditional .so breakpoints are a landmine — these are main-binary
#  breakpoints, loaded at start, so the condition binds immediately.)
#
# The one breakpoint that halts the linear flow is main.rs:458 — the
# ShapeFacts money shot after analyze() + check_program() converged. Every
# other stop auto-inspects and continues via `commands`.
#
# Decoder ring for what you will see:
#   MOVED_ROOT    = 18446744073709551614 (usize::MAX-1) — the moved-from taint
#   NULL_ROOT     = 18446744073709551615 (usize::MAX)    — the nil-drop / bare local
#   BOUNDARY_ROOT = 18446744073709551613 (usize::MAX-2) — the host-owned `arg`
#   small ids     — real ctor sites (deep-free graph nodes); ids >= 2^63 —
#                   row ghosts (first-class ownership tokens)
# ==============================================================================

# ------------------------------------------------------------------------------
# [MOVE] poison_moved — the Owned -> Moved transition, BEFORE and AFTER.
# Two breakpoints bracket the overwrite: entry shows the old TableShape,
# the post-insert line shows the poison seated in the symbol table.
# ------------------------------------------------------------------------------
break glm::shape::analyzer::Analyzer::poison_moved
commands
  silent
  printf "\n=== [MOVE:BEFORE] poison_moved — the Owned -> Moved transition ===\n"
  printf "--- name being poisoned:\n"
  p name
  printf "--- scope depth:\n"
  p depth
  printf "--- caller:\n"
  cbt 4
  printf "--- symbol table BEFORE (scopes[depth][name] still Owned):\n"
  p self.walk.scopes
  continue
end

break rebind.rs:35
commands
  silent
  printf "--- symbol table AFTER (aliases = {MOVED_ROOT}, ty = Pending):\n"
  p self.walk.scopes
  printf "--- recorded for the lowerer (stmt -> poisoned names):\n"
  p self.own.move_poisons
  continue
end

# ------------------------------------------------------------------------------
# [JOIN] TableShape::join — the phi's OWNERSHIP half: the analyzer merging the
# two arms' shapes (aliases union, layout join, lineage deepest-wins). The
# lowerer's register phi (see next stop) must agree with what this converges to.
# Fires on every merge incl. loop back-edges — for these small cases that is a
# handful of stops; skimming the dst/src pairs is the point.
# ------------------------------------------------------------------------------
break glm::shape::core::TableShape::join
commands
  silent
  printf "\n=== [JOIN] scope merge — dst union= src ===\n"
  printf "--- dst (in-place arm):\n"
  p *self
  printf "--- src (incoming arm):\n"
  p *other
  continue
end

# ------------------------------------------------------------------------------
# [JOIN:PHI] lowerer.rs:1468 — the if-join's value-phi loop: per rebinding
# name, then_regs[i] / else_regs[i] are the per-arm SSA registers and the
# emitted Phi names both + the joined (type, layout). This is where
# `pick` becomes a single phi register — the sole faithful handle.
# ------------------------------------------------------------------------------
break lowerer.rs:1468
commands
  silent
  printf "\n=== [JOIN:PHI] if-join value phis ===\n"
  printf "--- rebinding names at this join:\n"
  p phi_order
  printf "--- then-arm registers (name, reg, ty, layout):\n"
  p then_regs
  printf "--- else-arm registers (name, reg, ty, layout):\n"
  p else_regs
  continue
end

# ------------------------------------------------------------------------------
# [DEEP-FREE:GRANT] walk.rs:518 — the analyzer proving the transfer: a
# table-valued store into a cell (STMT_IDX_STORED_CTOR, plate slot 126).
# The flagged site id enters stored_ctor_parents — the deep-free parent set.
# ------------------------------------------------------------------------------
break walk.rs:518
commands
  silent
  printf "\n=== [DEEP-FREE:GRANT] STMT_IDX_STORED_CTOR — a site becomes a deep-free parent\n"
  printf "--- flagged site id:\n"
  p s
  printf "--- stored_ctor_parents so far:\n"
  p self.own.stored_ctor_parents
  continue
end

# ------------------------------------------------------------------------------
# [DEEP-FREE:OR] lowerer.rs:1742 — the moment the proof becomes a bit:
# flags = mode_bit | 0x80 (bit 7) when contains_tables. The two TableNews of
# the deep-free face show the contrast: m ({}; flagged -> 0x80) vs {7, 8}
# (Integer cells; 0x00 — no children to walk).
# ------------------------------------------------------------------------------
break lowerer.rs:1742
commands
  silent
  printf "\n=== [DEEP-FREE:OR] TableNew about to emit ===\n"
  printf "--- elem (cells' type):\n"
  p elem
  printf "--- contains_tables / flags (0x80 = bit 7 = deep-free children):\n"
  p contains_tables
  p flags
  printf "--- the proof set the lowerer consults:\n"
  p self.shape.stored_ctor_parents
  continue
end

# ------------------------------------------------------------------------------
# [RUNTIME CROSS-CHECK] deep-free face only: m's header, live, at the print.
# parg's `ct:` field IS the contains_tables byte at header offset +33 — the
# flag the OR above produced, seen from the other side of the compile.
# On the other faces this breakpoint stays pending forever and never fires.
# ------------------------------------------------------------------------------
break explore_deep_free.lua:6
commands
  silent
  printf "\n=== [RUNTIME] m's header at 'print(m[0][1])' — ct:1 = the deep-free bit ===\n"
  parg m
  pcells m 1
  continue
end

# ------------------------------------------------------------------------------
# THE MONEY SHOT — halts here (no commands): ShapeFacts is complete and
# converged. Every fact the lowerer will consume, in one place:
#   move_poisons         — stmt -> names poisoned there (mechanic 1)
#   stored_ctor_parents  — deep-free parent sites (mechanic 3)
#   join_tags            — mixed joins needing a tag phi (mechanic 2's kin)
#   free_sites / rebind_frees — the compost plan
#   stmt_lines           — maps the raw *const Stmt keys back to Lua lines
# ------------------------------------------------------------------------------
break main.rs:458

# Blast off.
run

printf "\n================ SHAPEFACTS — the converged ownership proof ================\n"
printf "--- move_poisons (mechanic 1: stmt -> moved-from names):\n"
p shape.move_poisons
printf "--- stored_ctor_parents (mechanic 3: deep-free parent site ids):\n"
p shape.stored_ctor_parents
printf "--- join_tags (mechanic 2 kin: stmts whose free needs a tag phi):\n"
p shape.join_tags
printf "--- free_sites (planned drops) / rebind_frees (displaced-value frees):\n"
p shape.free_sites
p shape.rebind_frees
printf "--- stmt_lines (raw stmt pointers -> Lua lines, to read the maps above):\n"
p shape.stmt_lines

continue
