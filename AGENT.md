# AGENT.md — the agentic GDB tool (`.gdbinit.agent`)

This documents `.gdbinit.agent`, the GDB configuration purpose-built for AI-agent
debugging sessions in this repo. It is loaded *instead of* the human-oriented
`./.gdbinit` and is optimized for one thing: maximum diagnostic signal per token.
(The human config's ANSI dashboard, borders, and per-stop register dumps are for
eyes, not context windows.)

## Launching

Run from the repo root (`~/lua`) — the host resolves `./libglm_out.so`, `out.ll`,
and the trace plates relative to it.

```bash
# interactive session (.so / dlopen face)
rust-gdb -nx -x .gdbinit.agent --args ./target/debug/glm --debug cases/<case>.lua <args...>

# fully batched — the form an agent runs autonomously
rust-gdb -nx -x .gdbinit.agent --batch -x /tmp/session.gdb --args ./target/debug/glm --debug cases/<case>.lua <args...>

# standalone exe face (glm_out IS the program)
rust-gdb -nx -x .gdbinit.agent ./glm_out <args...>
```

`-nx` skips `~/.gdbinit` **and** `./.gdbinit` (the human config), while
rust-gdb's own injected pretty-printer `-x` still loads — the two configs can
never bleed into each other. Requires gdb with Python support (already the case
for rust-gdb's pretty printers; no extra packages).

## Stop behavior: built-in, silent, cheap

The config defines **no `hook-stop`** — by design. GDB already echoes the
location and source line on every breakpoint hit and every `next`/`step`:

```
Breakpoint 1, glm_exec (arg=0x55555596af00) at 21_interop_alloc_arg_header.lua:14
14	local t = {1, 2}
```

That one block (~15 tokens) is the entire per-stop output. No registers, no
borders, no ANSI. The 18 `skip`-rule confirmation lines are silenced at startup
by issuing them through Python with `to_string=True`. Pull more context only
when needed, using the commands below — every one prints dense, one-line output.

## Command reference

| Command | Output |
|---|---|
| `cregs` | All 16 GP regs + `rip` on one line, flags as set-flag names only: `rax=0x... rsp=0x... rip=0x... efl=0x202[IF]` |
| `parg [addr\|var]` | `GlmTable` header as pseudo-JSON: `{tbl:0x5555... data:0x... len:8 reserve:0 esize:16 mode:dense ct:0 sparse:(nil)}`. No argument: tries `$rdi`, then `$rbx` (the boundary pointer at the `glm_exec` entry face). Prefer passing the Lua variable directly from a script frame: `parg arg`, `parg t`. |
| `pcells <tbl\|var> [count]` | Decoded cells, one segment per cell: `[0:str u:0x... s:"hello"]`, `[0:int i:9 u:0x9]`, `[0:i:4613937818241073152 u:0x4008000000000000 f:3]`. Count defaults to `min(len, 4)`. |
| `xq <addr>` | One tagged 128-bit cell on one line (int/float/bool/string decode). |
| `pkind` | Inferior-calls `glm_arg_kind()`: `glm_arg_kind=4 (-1 none, 0 int, 1 float, 2 bool, 3 string, 4 any)` — the module's own boundary contract. |
| `cbt [N]` | Backtrace with `std::`/`core::`/`alloc::`/libc/loader frames elided, N=12 default. Frame numbers stay gdb-native, so `frame N` still works: `#0 glm_exec 21_...lua:15` / `#1 glm::run_boundary main.rs:266` |
| `here` | Current position as one line: `21_interop_alloc_arg_header.lua:15: print(sys_alloc_count())` — useful for re-orienting after `up`/`frame`. |
| `blua` | Pending breakpoint on `@glm_exec`, then `run` — lands at the script's first statement. |
| `_cell <addr> <esize> <ct>` | Internal helper (used by `pcells`/`xq`); one decoded cell, no newline. |

## Reading the output

`GlmTable` is `#[repr(C)]` (see `src/rt.rs`) — `parg` reads raw offsets:
`data`+0, `len`+8, `reserve`+16, `esize`+24, `mode`(u8)+32 (0=dense, 1=sparse),
`contains_tables`(u8)+33, `sparse_map`+40.

Cell semantics by `esize`: **1** = bool (`b:`), **8** = scalar — printed as
`i:` (signed), `u:` (hex) and `f:` (bit-cast double) together; the script's type
tells you which view is real, `ct:1` switches the view to row pointers (`row
0x...`) — **16** = tagged `GLM_ARG_ANY` cell: payload in the low 64 bits,
kind tag in the high 64 (`0=int, 1=float, 2=bool, 3=str`; strings print their
interned NUL-terminated content).

`pcells` reads dense buffers directly; for a sparse table (or an unallocated
dense buffer) it falls back to inferior-calling the loaded module's own
`glm_tbl_get` (16 bytes of `malloc` scratch), so far-key overflow-map cells
decode correctly.

## Landmines — do not rediscover these

- **`$es` is the x86 segment register.** `set $es = ...` silently attempts a
  ptrace register write and fails with `Couldn't write registers: Input/output
  error`. The config's convenience vars avoid every segment-register name
  (`$cs/$ds/$es/$fs/$gs/$ss`) — keep it that way.
- **`$eflags` is an opaque flags struct**: no casts, no field access
  (`[ IF ]` pretty-print only). `cregs` parses `info registers eflags` instead.
- **Pending breakpoints**: gdb's default `pending auto` *prompts*, and with
  `confirm off` that auto-declines — `b <case>.lua:14` before `run` would
  silently not exist. The config sets `breakpoint pending on`; never remove it.
- **Python commands inside `define` bodies only register.** A
  `define foo` whose body is a `python` block executes the *registration* when
  first invoked and prints nothing; the Python command itself only works on the
  second invocation. `cregs` is therefore registered at top level in the config.
- **`emissionKind` spelling**: LLVM 22+ accepts `FullDebug`, not the old
  `Full` (`LineTablesOnly` outright crashes clang 23). Relevant if you
  hand-write test IR.
- Variable visibility is SSA-binding-accurate: a local shows `<optimized out>`
  on its own declaration line (it is not born yet) and becomes visible from its
  binding onward — that is correct DWARF, not a bug.

## Smoke test

After toolchain updates, one batched command re-verifies the whole workflow
(expect: breakpoint on a Lua line, one dense register line, JSON header, decoded
cells, elided backtrace):

```bash
cat > /tmp/smoke.gdb <<'EOF'
break 21_interop_alloc_arg_header.lua:15
run
cregs
parg arg
pcells arg 3
cbt 8
EOF
rust-gdb -nx -x .gdbinit.agent --batch -x /tmp/smoke.gdb \
  --args ./target/debug/glm --debug cases/21_interop_alloc_arg_header.lua 9
```
