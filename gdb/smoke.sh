#!/usr/bin/env bash
# smoke.sh — the unified GDB tooling smoke: one core (gdb/glm.gdbinit), two
# faces (agent | human), the full face×case matrix. Run it from anywhere —
# it resolves the repo root from its own location — after toolchain or
# config updates; `cargo build` first (every face runs the debug host).
#
#   ./gdb/smoke.sh [--face agent|human|both] [--config <path>] [--sweep]
#
#   --face    which face(s) to drive (default: both)
#   --config  test a candidate config IN PLACE of the per-face shim:
#             a draft SHIM drops in as-is, and a bare draft CORE works too
#             because GLM_GDB_FACE is exported per run (env > shim var is
#             the core's documented precedence).
#   --sweep   corpus-wide diagnostic sweep: compile every cases/*.lua with
#             --exe --debug (link face, no execution) and assert clang
#             emits neither of its invalid-debug-info diagnostics.
#
# The matrix — every (face, case) cell below, ✓/✗ per load-bearing
# marker, exit 1 on any ✗:
#
#   dense — breakpoint on a Lua line, the parg header (agent: one-line
#     pseudo-JSON `{tbl:0x... data:0x... len:8 reserve:0 esize:16
#     mode:dense ct:0 sparse:(nil) border:1}`; human: the annotated
#     multi-line form), decoded cells, the elided backtrace
#     `#1 glm::run_boundary main.rs:...`, igrep's filtered lookahead
#     (agent: exactly 200 walked with the plain summary; human: the
#     call ahead surfaced with the cyan summary line, every line wearing
#     gdb's own disassembler styling rebuilt from `show style` — in a
#     batch run ONLY igrep emits styled instruction lines, so the raw
#     `=> \e[..m0x` grep pins the rebuild itself), the prologue
#     trio: pstack's window from $rsp, the $rbp-anchored view flagging
#     the frame base, and stkwatch catching the `call` push the return
#     address one `si` later (+0 decodes to glm_exec+N, rsp 8 lower),
#     and (human only) the dashboard's [Bytes] opcode row: the
#     instruction line stays native x/1i verbatim (gdb's own disassembler
#     styling and tab placement), with the raw opcode bytes on their own
#     labeled row — x/1i cannot show bytes, so that row is
#     _glm_opcodes_line, pure Python API; sparse pins it at EVERY one of
#     its six stops, proving the top-level definition beats the
#     registration landmine (the first stop already prints it).
#
#   any — the dynamic-cell adoption pipeline (cases/any_adopt_store.lua):
#     't' is born Integer-celled and the String store `t[2] = "five"`
#     flips it to the tagged cell for the whole script. Breakpoint on the
#     first print after the store (pending-breakpoint discipline, same as
#     the dense face); `parg t` shows `esize:16` — the adopted Any cell,
#     not the born one — and now ALSO the runtime border (3 — one past
#     the store, the living `#t`) in both faces; `pcells t 3` decodes the
#     MIXED table in one shot; the raw offset read `*(long*)(t+48)` stays
#     as an extra marker — it pins the +48 offset documentation in
#     AGENTS.md independently of parg; `cbt` anchors the frame at the
#     Lua line.
#
#   sparse — born-sparse Table<Any> (cases/90_smoke_far_any_tag.lua):
#     five glm_tbl_set_any stops before the Lua line — two host
#     boundary fills of the arg table, two ctor stores, then the far
#     store, every Any store riding the register seam — hence run + 4
#     continues (unconditional breakpoint + continue counting; see the
#     landmines in AGENTS.md). On the far stop `xqv val` decodes the
#     tagged string `str u:0x... s:"world"` straight out of the i128
#     argument; at the print line `parg t` shows `mode:sparse` and
#     `pcells` decodes the overflow-map cell back out; `cbt` anchors the
#     frame at the ctor/far Lua line. The human face's many-stop run is
#     also what pins the dashboard surviving multiple stops in one
#     session (six stops, six dashboards).
#
#   join — the module's DWARF surviving an if-join (cases/
#     explore_join_phi.lua), in BOTH faces. The bind flush after a phi
#     group is the load-bearing shape: LLVM's records model attaches
#     each llvm.dbg.value to the instruction that FOLLOWS it, a record
#     on a phi is invalid, and clang's answer is the whole module's
#     debug info being dropped ("PHI Node must not have any attached
#     DbgRecords" — breakpoints and variable views die for every
#     join-bearing script while the module still runs). The cell pins
#     the user-visible property end-to-end per face: the Lua-line
#     breakpoint PAST the join binds and stops, `parg pick`/`pcells
#     pick` read the table through the join phi, and the compile
#     transcript carries none of clang's poison diagnostics.
#
# Agent-only aggregate markers: every agent transcript must be ANSI-free
# (that face's entire reason to exist), carry no per-stop dashboard, and
# carry no [Bytes] opcode row (it is human-only too).
#
# Marker failures map to the config landmines (AGENTS.md): `xqv` dying
# with `No symbol 'unsigned'` = the language sandwich was stripped;
# `pcells` dying with `Invalid type combination in equality test` = the
# `$bufp` init is missing; a missing pending-breakpoint stop =
# `breakpoint pending on` was removed.
#
# History note: on trees before f932c82 the sparse cell cannot complete
# — the far store aborts the process with `misaligned pointer
# dereference` from the debug staticlib (the aligned `*const u128`
# deref that `read_unaligned` fixed). If the third glm_tbl_set_any
# stop dies instead of decoding, check which tree you are on before
# blaming the config.
set -u
FACE=both
CONFIG=""
SWEEP=0
while [ $# -gt 0 ]; do
  case "$1" in
    --face)   FACE="${2:?--face needs a value}"; shift 2 ;;
    --config) CONFIG="${2:?--config needs a path}"; shift 2 ;;
    --sweep)  SWEEP=1; shift ;;
    *)
      echo "usage: $0 [--face agent|human|both] [--config <path>] [--sweep]" >&2
      exit 2 ;;
  esac
done
case "$FACE" in
  agent|human|both) ;;
  *) echo "smoke: bad --face '$FACE' (want agent|human|both)" >&2; exit 2 ;;
esac

# Resolve a relative --config against the invocation CWD (its pre-cd
# meaning), then pin CWD to the repo root this script lives in — everything
# below (the shims, ./target/debug/glm, cases/) is root-relative.
if [ -n "$CONFIG" ]; then
  cfgdir=$(cd "$(dirname "$CONFIG")" && pwd) || {
    echo "smoke: --config '$CONFIG' not found" >&2; exit 2; }
  CONFIG="$cfgdir/$(basename "$CONFIG")"
fi
cd "$(dirname "$0")/.." || exit 2

FAIL=0
SCRATCH=$(mktemp -d)
trap 'rm -rf "$SCRATCH"' EXIT

# One marker helper for both worlds: strip ANSI before grepping (a no-op
# for agent output, whose no-ANSI discipline is itself pinned below).
check() { # check <face-case> <what> <grep -E pattern>
  sed -E 's/\x1B\[[0-9;]*[a-zA-Z]//g' "$SCRATCH/$1.out" > "$SCRATCH/$1.clean" 2>/dev/null
  if grep -qE "$3" "$SCRATCH/$1.clean" 2>/dev/null; then
    printf '  ✓ %s %s\n' "$1" "$2"
  else
    printf '  ✗ %s %s — missing /%s/\n' "$1" "$2" "$3"
    FAIL=1
  fi
}

check_absent() { # check_absent <face-case> <what> <grep -E pattern>
  sed -E 's/\x1B\[[0-9;]*[a-zA-Z]//g' "$SCRATCH/$1.out" > "$SCRATCH/$1.clean" 2>/dev/null
  if grep -qE "$3" "$SCRATCH/$1.clean" 2>/dev/null; then
    printf '  ✗ %s %s — found /%s/\n' "$1" "$2" "$3"
    FAIL=1
  else
    printf '  ✓ %s %s\n' "$1" "$2"
  fi
}

check_count() { # check_count <face-case> <what> <pattern> <min>
  sed -E 's/\x1B\[[0-9;]*[a-zA-Z]//g' "$SCRATCH/$1.out" > "$SCRATCH/$1.clean" 2>/dev/null
  n=$(grep -cE "$3" "$SCRATCH/$1.clean" 2>/dev/null || true)
  n=${n:-0}
  if [ "$n" -ge "$4" ]; then
    printf '  ✓ %s %s (%d ≥ %d)\n' "$1" "$2" "$n" "$4"
  else
    printf '  ✗ %s %s — %d occurrence(s), want ≥ %d\n' "$1" "$2" "$n" "$4"
    FAIL=1
  fi
}

gdb_run() { # gdb_run <face> <label> <script> [glm args...]
  local face=$1 label=$2 script=$3; shift 3
  local config
  if [ -n "$CONFIG" ]; then config=$CONFIG
  elif [ "$face" = agent ]; then config=.gdbinit.agent
  else config=.gdbinit; fi
  # GLM_GDB_FACE pins the face even for a --config that is a bare core.
  GLM_GDB_FACE="$face" rust-gdb -q -nx -x "$config" --batch -x "$script" \
    --args ./target/debug/glm --debug "$@" > "$SCRATCH/$label.out" 2>&1
}

run_agent() { # run_agent <case-label> [glm args...]
  local label=$1; shift
  gdb_run agent "agent-$label" "$SCRATCH/agent-$label.gdb" "$@"
}

run_human() { # run_human <case-label> [glm args...]
  local label=$1; shift
  gdb_run human "human-$label" "$SCRATCH/human-$label.gdb" "$@"
}

FACES=""
[ "$FACE" = agent ] || [ "$FACE" = both ] && FACES="$FACES agent"
[ "$FACE" = human ] || [ "$FACE" = both ] && FACES="$FACES human"

for f in $FACES; do
  echo "=== face: $f (config: ${CONFIG:-default shim}) ==="

  # ---- dense ----------------------------------------------------------
  cat > "$SCRATCH/$f-dense.gdb" <<'EOF'
break 21_interop_alloc_arg_header.lua:15
run
cregs
parg arg
pcells arg 3
cbt 8
EOF
  if [ "$f" = agent ]; then
    { echo "igrep 200 call"
      # The prologue trio: window from $rsp, the anchored frame base, and
      # the watch catching `call` push the return address one `si` later.
      echo "pstack"
      echo "pstack 4 \$rbp"
      echo "stkwatch 1"
      echo "si"
      echo "stkwatch 0"; } >> "$SCRATCH/$f-dense.gdb"
    run_agent dense cases/21_interop_alloc_arg_header.lua 9
    check agent-dense "breakpoint lands on the Lua line"  'glm_exec.*21_interop_alloc_arg_header\.lua:15'
    check agent-dense "cregs one-line register dump"      'rip=0x'
    check agent-dense "parg pseudo-JSON header"           '\{tbl:0x'
    check agent-dense "pcells decoded int cell"           'int i:'
    check agent-dense "cbt elides into the Rust host"     'glm::run_boundary'
    check agent-dense "igrep filters and walks exactly 200" 'igrep: [1-9][0-9]* match\(es\) in 200 instructions'
    check agent-dense "pstack one-line window"            'stk\{rsp:0x[0-9a-f]+ rbp:0x[0-9a-f]+ gap:\+[0-9]+B n:[0-9]+\} \[\+0='
    check agent-dense "pstack \$rbp flags the frame base" '#rbp'
    check agent-dense "si into the call pushes the return address" '\+0=0x[0-9a-f]+\(glm_exec\+[0-9]+\)'
    check_count agent-dense "stkwatch prints on every stop" 'stk\{' 3
  else
    { # Style gate forced on for the igrep color pin: batch gdb disables
      # styling on non-ttys, and _glm_style_codes honors that gate — force
      # it so the rebuild actually emits (interactive sessions have it on).
      echo "set style enabled on"
      echo "igrep 60 call"
      echo "pstack"
      echo "pstack 4 \$rbp"
      echo "stkwatch 1"
      echo "si"
      echo "stkwatch 0"; } >> "$SCRATCH/$f-dense.gdb"
    run_human dense cases/21_interop_alloc_arg_header.lua 9
    check human-dense "breakpoint lands on Lua line"    '21_interop_alloc_arg_header\.lua:15'
    check human-dense "hook-stop dashboard survives"    'EFL: 0x'
    check human-dense "opcode bytes ride their own [Bytes] row" '\[Bytes\] +[0-9A-F]{2}( [0-9A-F]{2})+'
    check human-dense "cregs one-line register dump"    'rip=0x'
    check human-dense "parg shows human header border"  '=== GlmTable at 0x'
    check human-dense "pcells decodes typed cell"       '\[INT\].*9'
    check human-dense "cbt elides into Rust host"       'glm::run_boundary'
    check human-dense "igrep surfaces the call ahead"   'call.*sys_alloc_count@plt'
    check human-dense "igrep human summary line"        'match\(es\) in the next 60 instructions'
    # Raw (un-stripped) pin: batch gdb styles NOTHING natively, so an
    # escape right after the => of an igrep line can only be _glm_style_
    # disasm's rebuild of gdb's own disassembler styling.
    if grep -qP '=> \x1b\[[0-9;]+m0x[0-9a-f]+' "$SCRATCH/human-dense.out" 2>/dev/null; then
      printf '  ✓ %s %s\n' human-dense "igrep lines wear gdb-style colors"
    else
      printf '  ✗ %s %s — none found\n' human-dense "igrep lines wear gdb-style colors"
      FAIL=1
    fi
    check human-dense "pstack pretty window"            '=== Stack window: 8 word\(s\) up from rsp 0x'
    check human-dense "pstack decodes code pointers"    '← glm::run_boundary\+[0-9]+ \(code\)'
    check human-dense "pstack \$rbp flags the frame base" '← \$rbp'
    check_count human-dense "stkwatch appends on every stop" '=== Stack window:' 3
  fi

  # ---- sparse (Table<Any> far-key cell, register seam) -----------------
  cat > "$SCRATCH/$f-sparse.gdb" <<'EOF'
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
EOF
  if [ "$f" = agent ]; then
    { echo "cbt 4"; echo "here"; } >> "$SCRATCH/$f-sparse.gdb"
    run_agent sparse cases/90_smoke_far_any_tag.lua 42 world
    check agent-sparse "far stop decodes the tagged string"               'str u:0x.*s:"world"'
    check agent-sparse "parg shows the sparse header"                     'mode:sparse'
    check agent-sparse "pcells reads the overflow map back"               'i:42'
    check agent-sparse "cbt anchors the frame to the Lua line"            'glm_exec 90_smoke_far_any_tag\.lua'
    check agent-sparse "here prints the source line"                      '90_smoke_far_any_tag\.lua:12'
  else
    echo "here" >> "$SCRATCH/$f-sparse.gdb"
    run_human sparse cases/90_smoke_far_any_tag.lua 42 world
    check human-sparse "far stop decodes tagged string"  '\[STR\].*"world"'
    check human-sparse "parg shows sparse header"        'mode.*\(sparse\)'
    check human-sparse "pcells reads overflow map back"  '\[INT\].*42'
    check human-sparse "xqv prints human border"         '=== 128-bit Cell \(By Value\) ==='
    check human-sparse "here anchors the source line"    '\.lua:12: '
    # Six stops happen in this cell (five set_any + the Lua line); the
    # dashboard must have printed at every one of them — and so must the
    # [Bytes] row, from the FIRST stop (the registration landmine: a
    # hook-body-only python def would print nothing until the second).
    check_count human-sparse "dashboard survives all six stops" 'EFL: 0x' 6
    check_count human-sparse "opcode row at all six stops" '\[Bytes\]' 6
  fi

  # ---- any (dynamic-cell adoption, mixed decode, border) ---------------
  cat > "$SCRATCH/$f-any.gdb" <<'EOF'
break any_adopt_store.lua:13
run
parg t
pcells t 3
p *(long *)((char *)t+48)
cbt 4
here
EOF
  if [ "$f" = agent ]; then
    run_agent any cases/any_adopt_store.lua
    check agent-any "breakpoint lands on the print line"        'glm_exec.*any_adopt_store\.lua:13'
    check agent-any "parg shows the adopted Any cell (esize:16)" 'esize:16 mode:dense'
    check agent-any "pcells decodes int and str cells in one table" '\[0:int i:7 u:0x7 1:[^]]* 2:str u:0x[0-9a-f]+ s:"five"'
    check agent-any "parg shows the runtime border"             'border:3'
    check agent-any "raw +48 read shows the runtime border"     '\$1 = 3\r?$'
    check agent-any "cbt anchors the frame at the Lua line"     'glm_exec any_adopt_store\.lua'
    check agent-any "here prints the source line"               'any_adopt_store\.lua:13'
  else
    run_human any cases/any_adopt_store.lua
    check human-any "breakpoint lands on the print line"        'glm_exec.*any_adopt_store\.lua:13'
    check human-any "parg shows the adopted Any cell (esize:16)" 'esize   :  16'
    check human-any "pcells decodes int and str cells in one table" '\[INT\].*i:7'
    check human-any "pcells decodes the str cell"               '\[STR\].*"five"'
    check human-any "parg shows the runtime border"             'border  :  3'
    check human-any "raw +48 read shows the runtime border"     '\$1 = 3\r?$'
    check human-any "cbt anchors the frame at the Lua line"     'glm_exec any_adopt_store\.lua'
    check human-any "here prints the source line"               'any_adopt_store\.lua:13: print'
  fi

  # ---- join (DWARF survives an if-join) ---------------------------------
  cat > "$SCRATCH/$f-join.gdb" <<'EOF'
break explore_join_phi.lua:17
run
here
parg pick
pcells pick 2
cbt 4
EOF
  if [ "$f" = agent ]; then
    run_agent join cases/explore_join_phi.lua
    check agent-join "breakpoint lands past the join"            'glm_exec.*explore_join_phi\.lua:17'
    check agent-join "parg reads pick through the join phi"      'esize:8 mode:dense'
    check agent-join "pcells decodes the stored cell"            '\[0:i:7 u:0x7'
    check agent-join "cbt anchors the frame at the Lua line"     'glm_exec explore_join_phi\.lua:17'
    check agent-join "here prints the source line"               'explore_join_phi\.lua:17: print'
  else
    run_human join cases/explore_join_phi.lua
    check human-join "breakpoint lands past the join"            'glm_exec.*explore_join_phi\.lua:17'
    check human-join "parg reads pick through the join phi"      'esize   :  8'
    check human-join "pcells decodes the stored cell"            '\[SCALAR\] i:7'
    check human-join "cbt anchors the frame at the Lua line"     'glm_exec explore_join_phi\.lua:17'
    check human-join "here prints the source line"               'explore_join_phi\.lua:17: print'
  fi
  # clang's poison diagnostics must stay absent in BOTH faces — the DWARF
  # validity is a property of the compiled module, and this is the marker
  # that catches a backend regression the moment the module still runs.
  check_absent $f-join "no phi DbgRecords rejection"        'PHI Node must not have any attached DbgRecords'
  check_absent $f-join "no dropped debug info"              'ignoring invalid debug info'
done

# ---- agent-face token discipline (aggregate over its transcripts) --------
if [ "$FACE" != human ]; then
  ansi_bad=$(grep -lP '\x1b\[' "$SCRATCH"/agent-*.out 2>/dev/null || true)
  n_trans=$(ls "$SCRATCH"/agent-*.out 2>/dev/null | wc -l)
  if [ -z "$ansi_bad" ]; then
    printf '  ✓ agent-discipline no ANSI in any transcript (%d files)\n' "$n_trans"
  else
    printf '  ✗ agent-discipline ANSI leaked into: %s\n' "$ansi_bad"
    FAIL=1
  fi
  dash_bad=$(grep -l 'Ret/Stack' "$SCRATCH"/agent-*.out 2>/dev/null || true)
  if [ -z "$dash_bad" ]; then
    printf '  ✓ agent-discipline no per-stop dashboard (%d files)\n' "$n_trans"
  else
    printf '  ✗ agent-discipline dashboard leaked into: %s\n' "$dash_bad"
    FAIL=1
  fi
  opc_bad=$(grep -l '\[Bytes\]' "$SCRATCH"/agent-*.out 2>/dev/null || true)
  if [ -z "$opc_bad" ]; then
    printf '  ✓ agent-discipline no [Bytes] opcode row (%d files)\n' "$n_trans"
  else
    printf '  ✗ agent-discipline [Bytes] row leaked into: %s\n' "$opc_bad"
    FAIL=1
  fi
fi

# ---- corpus-wide diagnostic sweep (optional) ------------------------------
if [ "$SWEEP" -eq 1 ]; then
  echo "=== corpus diagnostic sweep (--exe --debug, compile+link only) ==="
  for c in cases/*.lua; do
    if ./target/debug/glm --exe --debug "$c" 2>&1 \
        | grep -qE 'PHI Node must not have any attached DbgRecords|ignoring invalid debug info'; then
      printf '  ✗ sweep %s — clang debug-info diagnostic\n' "$c"
      FAIL=1
    else
      printf '  ✓ sweep %s\n' "$c"
    fi
  done
fi

if [ "$FAIL" -eq 0 ]; then
  echo "smoke: all green (face: $FACE)"
else
  mkdir -p target/smoke_fail
  cp "$SCRATCH"/*.out target/smoke_fail/ 2>/dev/null || true
  echo "smoke: FAILED (transcripts in target/smoke_fail/)"
  exit 1
fi
