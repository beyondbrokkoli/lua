#!/usr/bin/env bash
# agent_smoke.sh — self-verifying smoke for the agentic GDB tooling
# (.gdbinit.agent). Run from the repo root after toolchain or config
# updates; `cargo build` first (both faces run the debug host).
#
#   ./agent_smoke.sh
#
# Two faces, ✓/✗ per load-bearing marker, exit 1 on any ✗:
#
#   dense  — breakpoint on a Lua line, one dense register line, the
#     parg pseudo-JSON header `{tbl:0x... data:0x... len:8 reserve:0
#     esize:16 mode:dense ct:0 sparse:(nil)}`, decoded cells
#     `[0:int i:9 u:0x9 ...]`, and the elided backtrace
#     `#1 glm::run_boundary main.rs:...`.
#
#   sparse — born-sparse Table<Any> (cases/90_smoke_far_any_tag.lua):
#     five glm_tbl_set_any stops before the Lua line — two host
#     boundary fills of the arg table, two ctor stores, then the far
#     store, every Any store riding the register seam — hence run + 4
#     continues (unconditional breakpoint + continue counting; see the
#     landmines in AGENT.md). On the far stop `xqv val` decodes the
#     tagged string `str u:0x... s:"world"` straight out of the i128
#     argument; at the
#     print line `parg t` shows `mode:sparse` and `pcells` decodes the
#     overflow-map cell back out (`[0:int i:42 ... 1:str ... s:"world"
#     ...]`); `cbt` anchors the frame at the ctor/far Lua line.
#
# Marker failures map to the config landmines (AGENT.md): `xqv` dying
# with `No symbol 'unsigned'` = the language sandwich was stripped;
# `pcells` dying with `Invalid type combination in equality test` = the
# `$bufp` init is gone; a missing pending-breakpoint stop = `breakpoint
# pending on` was removed.
#
# History note: on trees before f932c82 the sparse face cannot complete
# — the far store aborts the process with `misaligned pointer
# dereference` from the debug staticlib (the aligned `*const u128`
# deref that `read_unaligned` fixed). If the third glm_tbl_set_any
# stop dies instead of decoding, check which tree you are on before
# blaming the config.
set -u
FAIL=0
SCRATCH=$(mktemp -d)
trap 'rm -rf "$SCRATCH"' EXIT

check() { # check <face> <what> <grep -E pattern>
  if grep -qE "$3" "$SCRATCH/$1.out" 2>/dev/null; then
    printf '  ✓ %s %s\n' "$1" "$2"
  else
    printf '  ✗ %s %s — missing /%s/\n' "$1" "$2" "$3"
    FAIL=1
  fi
}

# ---- face 1: dense ---------------------------------------------------
cat > "$SCRATCH/dense.gdb" <<'EOF'
break 21_interop_alloc_arg_header.lua:15
run
cregs
parg arg
pcells arg 3
cbt 8
EOF
rust-gdb -nx -x .gdbinit.agent --batch -x "$SCRATCH/dense.gdb" \
  --args ./target/debug/glm --debug cases/21_interop_alloc_arg_header.lua 9 \
  > "$SCRATCH/dense.out" 2>&1

check dense "breakpoint lands on the Lua line"  'glm_exec.*21_interop_alloc_arg_header\.lua:15'
check dense "cregs one-line register dump"      'rip=0x'
check dense "parg pseudo-JSON header"           '\{tbl:0x'
check dense "pcells decoded int cell"           'int i:'
check dense "cbt elides into the Rust host"     'glm::run_boundary'

# ---- face 2: sparse (Table<Any> far-key cell, register seam) ----------
cat > "$SCRATCH/sparse.gdb" <<'EOF'
break glm_tbl_set_any
break 90_smoke_far_any_tag.lua:12
run
continue
continue
continue
continue
xqv val
continue
parg t
pcells t 3
cbt 4
here
EOF
rust-gdb -nx -x .gdbinit.agent --batch -x "$SCRATCH/sparse.gdb" \
  --args ./target/debug/glm --debug cases/90_smoke_far_any_tag.lua 42 world \
  > "$SCRATCH/sparse.out" 2>&1

check sparse "far stop decodes the tagged string"               'str u:0x.*s:"world"'
check sparse "parg shows the sparse header"                     'mode:sparse'
check sparse "pcells reads the overflow map back"               'i:42'
check sparse "cbt anchors the frame to the Lua line"            'glm_exec 90_smoke_far_any_tag\.lua'
check sparse "here prints the source line"                      '90_smoke_far_any_tag\.lua:12'

if [ "$FAIL" -eq 0 ]; then
  echo "agent smoke: all green"
else
  mkdir -p target/agent_smoke_fail
  cp "$SCRATCH"/*.out target/agent_smoke_fail/
  echo "agent smoke: FAILED (transcripts in target/agent_smoke_fail/)"
  exit 1
fi
