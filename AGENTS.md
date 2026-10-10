# AGENTS.md — the agentic tooling: verify with the bench, debug with the agent-face GDB

This file documents the agent-side surface of the repo: the corpus bench
and its pin discipline, the trace-signal plates, the standing workflow
rules, and the **agent face** of the GDB harness. The human-side tools
(the LuaJIT steering shell, the human GDB face with the register
dashboard and colors) are documented in `README.md`.

## The dev loop: run.lua

```sh
lua run.lua                # build + clippy, whole corpus, lock byte-identity
lua run.lua run --fmt      # same, also gating on cargo fmt (opt-in)
lua run.lua run STR ...    # only cases whose name contains any STR
lua run.lua probe FILE     # one arbitrary file; archives to target/probe/
lua reset.lua              # overwrite diff target (milestone relock only)
```

Every case classifies itself with magic comments and the directory is
the listing:

```lua
-- EXPECT: <line>            expected stdout line, in order (a case
                             without EXPECT still gets compile/run/lock
                             coverage)
-- EXPECT_BUILD_FAIL: <text> compile must fail; stderr contains <text>
-- EXPECT_PANIC: <text>      build passes, the script dies with <text>
-- ARGS: <words...>          boundary arguments, parsed against the
                             case's usage-pinned cell type
```

The bench's EXE section re-links every case with `--exe` and runs it
with the same ARGS — pinning the two hosts (the dev-loop dlopen host
and the executable's embedded `glm_exec_main`) to identical behavior.

## Signals and plates

Every compile writes `.glm_trace.bin` — the event chronology.
`trace_signals.txt` is the single source of truth: `build.rs` turns it
into the `TRACE_*` constants, `plate.py` decodes from the same file,
so compiler and decoder cannot drift. Every checker widening ships a
trace signal plus at least one positive and one negative case — no
silent relaxations.

```sh
python3 plate.py                  # event chronology of the last compile
python3 plate.py one.bin two.bin  # event-sequence diff (baseline first)
```

To add a signal: append `<slot> <NAME> <owner> <meaning>` on an unused
slot in `trace_signals.txt`, then poke it through the existing helpers.

## Repo workflow rules (standing)

- `cases/`, `lock/`, `diff/`, and every root `.md` except `README.md`
  and `AGENTS.md` are **untracked by policy** — never `git add` them;
  `git status` showing them is correct. Relocking (`lua reset.lua`) is
  a milestone-only discipline, never mid-work.
- The bench runner is **single-owner**: `out.ll`, `libglm_out.so`,
  `.glm_trace.bin` are per-invocation CWD artifacts — two concurrent
  drivers corrupt each other's output.
- Adoption poisons retroactively: a typed read BEFORE the dynamic
  store in the source text still refuses (the join is
  flow-insensitive). Write test cases accordingly — a positive case
  with a typed read must not share a name with an adopting store.
- Clippy is an error gate inside `lua run.lua run`; rustfmt is opt-in
  (`--fmt`).

# The GDB tool: one core, two faces

The repo's GDB tooling: **`gdb/glm.gdbinit`**, the single core
that defines every command once, plus two thin shims at the repo root that
pick a face. (Layout law: the root keeps exactly the two face-picking
dotfiles — `.gdbinit` must, since GDB's local auto-load only ever looks for
`./.gdbinit` in the CWD — and `gdb/` holds everything else: the core, the
smoke, the exploration scripts.) The agent
face is optimized for token efficiency (it exists for AI-agent debugging
sessions); the human face adds the per-stop register dashboard, ANSI colors,
and annotated output.

## Face selection

The core resolves its face at load time, first match wins:

1. `GLM_GDB_FACE=agent|human` in the environment — lets a draft core be
   driven directly, no shim: `GLM_GDB_FACE=human rust-gdb -q -nx -x gdb/glm.gdbinit ...`
2. the `$glm_face_human` convenience variable, set by a shim (`.gdbinit`
   sets 1, `.gdbinit.agent` sets 0);
3. default: **agent** — silence is the safer fallback for an unspecified face.

The core re-publishes the verdict as `$glm_face_human` (always initialized
before any command body can run), and Python commands read the `GLM_HUMAN`
flag left in the interpreter globals.

## Launching (the agent face)

Run from the repo root (`~/lua`) — the host resolves `./libglm_out.so`,
`out.ll`, and the trace plates relative to it, and the shims `source gdb/glm.gdbinit` by
relative path.

```bash
# agent face, interactive session (.so / dlopen face)
rust-gdb -q -nx -x .gdbinit.agent --args ./target/debug/glm --debug cases/<case>.lua <args...>

# agent face, standalone exe face (glm_out IS the program)
rust-gdb -q -nx -x .gdbinit.agent --args ./glm_out <args...>
```

The fully batched form — the one an agent runs autonomously — takes a second
`-x`: a **command file you write yourself**, one GDB command per line (exactly
what you would type interactively, the custom commands below included). That
file does not exist until you create it, and any path works — `/tmp/session.gdb`
is just the convention used here:

```bash
cat > /tmp/session.gdb <<'EOF'
break 21_interop_alloc_arg_header.lua:15
run
cregs
parg arg
pcells arg 3
cbt 8
EOF
rust-gdb -q -nx -x .gdbinit.agent --batch -x /tmp/session.gdb \
  --args ./target/debug/glm --debug cases/21_interop_alloc_arg_header.lua 9
```

`-nx` skips `~/.gdbinit` **and** `./.gdbinit` (the human shim), while
rust-gdb's own injected pretty-printer `-x` still loads — the two faces can
never bleed into each other. `-q` silences the version greeting: batch mode
prints none anyway, so the flag exists for the live-pty faces — and it must
ride the command line, because the banner prints before any `-x` file is
sourced, so no config setting can retract it. Requires gdb with Python
support (already the case for rust-gdb's pretty printers; no extra packages).

The launch line has one hard border: gdb's own options end at `--args`.
Everything after it — `--debug <case>.lua <args...>` — is the debuggee's
argv, the case's boundary arguments: a flag dropped there is silently eaten
by the program, never by gdb (`-q` included). And `--args` is what carries
the case's args at all: without it, trailing arguments after the program are
not passed to the program — gdb attempts the first as a core file and
reports the rest as excess. Every form above therefore keeps `-q` with the
gdb options and puts nothing but the program and its args after `--args`.

## Stop behavior: built-in, silent, cheap (agent face)

The agent face prints **nothing per stop** — the core's `hook-stop` body is
gated off entirely for it. GDB already echoes the location and source line on
every breakpoint hit and every `next`/`step`:

```
Breakpoint 1, glm_exec (arg=0x55555596af00) at 21_interop_alloc_arg_header.lua:14
14	local t = {1, 2}
```

That one block (~15 tokens) is the entire per-stop output. No registers, no
borders, no ANSI. The human face's per-stop dashboard (registers, opcode
bytes, styled disassembly) is described in `README.md`. Pull more context only
when needed, using the commands below — every one prints dense, one-line
output.

## Command reference (agent-face output shapes)

| Command | Output |
|---|---|
| `cregs` | All 16 GP regs + `rip` on one line, flags as set-flag names only: `rax=0x... rsp=0x... rip=0x... efl=0x202[IF]` (identical in both faces). |
| `parg [addr\|var]` | `GlmTable` header as pseudo-JSON: `{tbl:0x5555... data:0x... len:8 reserve:0 esize:16 mode:dense ct:0 sparse:(nil) border:1}` — `border` is `#t`'s answer, printed in BOTH faces. No argument: tries `$rdi`, then `$rbx` (the boundary pointer at the `glm_exec` entry face). Prefer passing the Lua variable directly from a script frame: `parg arg`, `parg t`. |
| `pcells <tbl\|var> [count]` | Decoded cells, one segment per cell: `[0:str u:0x... s:"hello"]`, `[0:int i:9 u:0x9]`, `[0:i:4613937818241073152 u:0x4008000000000000 f:3]`. Count defaults to `min(len, 4)`; an explicit count overrides `len` (born-sparse tables have `len 0`). |
| `xq <addr>` | One tagged 128-bit cell on one line (int/float/bool/string decode). |
| `xqv <i128 expr>` | Same decode for a cell passed BY VALUE — the register seam's decoder (`xqv val` at a `glm_tbl_set_any` stop). |
| `pkind` | Inferior-calls `glm_arg_kind()`: `glm_arg_kind=4 (-1 none, 0 int, 1 float, 2 bool, 3 string, 4 any)` — the module's own boundary contract. One wording in both faces. |
| `cbt [N]` | Backtrace with `std::`/`core::`/`alloc::`/libc/loader frames elided, N=12 default. Agent face: frame numbers stay gdb-native, so `frame N` still works: `#0 glm_exec 21_...lua:15` / `#1 glm::run_boundary main.rs:266`. |
| `igrep <count> <pattern> [addr]` | Disassembles `count` instructions ahead of `addr` (default `$pc`), prints only the lines matching the case-insensitive regex, one summary line (`igrep: 20 match(es) in 200 instructions`). The walk goes in 64-instruction chunks with a one-instruction lookahead, so crossing into unmapped memory keeps the partial output instead of raising it away. The native `pipe x/500i $pc \| grep call` is the quick one-off; `igrep` stays inside GDB and bounds the loss. |
| `here` | Current position as one line: `21_interop_alloc_arg_header.lua:15: print(sys_alloc_count())` — useful for re-orienting after `up`/`frame`. |
| `blua` / `to_script` | Pending breakpoint on `@glm_exec`, then `run` — lands at the script's first statement. Two names, one body (`to_script` was the human config's name, `blua` the agent's). |
| `xd <addr>` | 4 consecutive 64-bit words (32 bytes), gdb-native `x/4gx`. |
| `pstack [count] [addr]` | The live stack window: `count` words (default 8, max 64) read upward from `$rsp` (or `addr`). Agent: one line, `stk{rsp:0x... rbp:0x... gap:+7040B n:8} [+0=0x... +56=0x...(glm::run_boundary+2217)]`. Annotations: code pointers decode to `symbol+offset`, values equal to `$rbp` mark the saved-frame-pointer chain (`^rbp`), the word AT `$rbp` is flagged (`#rbp`) when in window. The prologue companion: `pstack 4 $rbp` shows `[saved rbp][return address][caller stack]`. Pure Python. |
| `stkwatch [on\|off]` | Opt-in: print a pstack window on EVERY stop (via hook-stop). `stkwatch on` + `si` through a prologue is the real-time view. Deliberately breaks the agent face's silent-stop contract, one dense line per stop. |
| `regs` | `info registers` over all GP regs + `rip` + `eflags`, gdb-native formatting. |
| `_cell <addr> <esize> <ct>` | Internal helper (used by `pcells`/`xq`); one decoded cell, no newline. |

## Reading the output

`GlmTable` is `#[repr(C)]` (see `src/rt.rs`) — `parg` reads raw offsets:
`data`+0, `len`+8, `reserve`+16, `esize`+24, `mode`(u8)+32 (0=dense, 1=sparse),
`contains_tables`(u8)+33, `sparse_map`+40, `border`(i64)+48 (`#t`'s answer —
one past the highest index ever stored; `len` is the span watermark, not it).
`parg` prints `border` in both faces; a raw `p *(long*)((char*)t+48)` still
works and is kept in the smoke as the pin on the offset itself.

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
  first invoked and prints nothing; the Python command itself only works on
  the second invocation. `cregs` is therefore registered at top level in the config.
- **`emissionKind` spelling**: LLVM 22+ accepts `FullDebug`, not
  `Full` (`LineTablesOnly` outright crashes clang 23). Relevant if you
  hand-write test IR.
- **Define-body expressions parse in the current frame's language.** The
  config's commands cast with C syntax (`(char*)`, `(unsigned long)`,
  function-pointer calls); at the `glm_exec` face that is harmless — the
  hand-written DWARF declares no Rust language, so the frame is C — but
  inside `rt.rs` frames (DWARF Rust) the casts die: `xq`/`pkind` with
  `No symbol 'unsigned' in current context`, `parg` with `unexpected
  token`. The cast-bearing commands therefore wrap their bodies in
  language sandwiches — but the switches go through Python,
  `gdb.execute("set language c", to_string=True)` … `set language auto`,
  never as bare `set language` lines: a bare switch that flips the working
  language away from the selected frame's prints `Warning: the current
  language does not match this frame`, which plagued every Rust-frame stop
  (the state change is identical, only the warning is swallowed). `_cell`
  sets C without restoring so the nested sandwich cannot flip the language
  back mid-body of its caller. Never strip the sandwich.
- **A convenience var compared before its first assignment is void**, and
  the equality test dies with `Invalid type combination in equality
  test`. `pcells` initializes `$bufp` at body top for exactly this
  reason: its sparse fallback (inferior-call of `glm_tbl_get`) never
  runs on the dense smoke face, so only sparse tables ever saw the trap.
- **Pending breakpoints with conditions silently never fire.** `break
  glm_tbl_set if index == 200000` issued before `run` resolves at the
  dlopen but stops nothing (the same breakpoint without `if` fires
  fine). Set conditional `.so` breakpoints after the first stop — the
  module is loaded then, the breakpoint binds immediately, and the
  condition works — or use an unconditional breakpoint plus `continue`
  counting when the call sequence is known.
- Variable visibility is SSA-binding-accurate: a local shows `<optimized out>`
  on its own declaration line (it is not born yet) and becomes visible from its
  binding onward — that is correct DWARF, not a bug.
- **Face selection must precede the commands.** The core's Python block is
  what initializes `$glm_face_human` — a shim that sources the core before
  setting the variable silently gets the agent face, and a define body
  branching on an unset convenience var hits the void-comparison landmine
  above. Precedence is env (`GLM_GDB_FACE`) > shim variable > agent default,
  and the core re-publishes the verdict so command bodies never see it void.
- **`finish` from a not-yet-resolved PLT stub aborts** (`Cannot insert
  breakpoint 0` / `Command aborted`): the stub frame carries no unwind
  info, so gdb computes a null return address. Step over the `call` with
  `ni`, or `si` into the resolved function first. From inside the ld-linux
  resolver itself `finish` works — and note it runs the *rest of the call*
  (the resolver is entered by `jmp`, so its stack's return address is the
  original caller's).

## Smoke test

After toolchain or config updates, run `./gdb/smoke.sh` (it resolves the
repo root from its own location; `cargo build` first):

    ./gdb/smoke.sh                          # both faces (the default)
    ./gdb/smoke.sh --face agent|human       # one face
    ./gdb/smoke.sh --config <draft>         # test a candidate config in place
    ./gdb/smoke.sh --sweep                  # + corpus-wide diagnostic sweep

It drives the full face×case matrix batched — dense (breakpoint on a Lua
line, `cregs`, `parg`, `pcells`, elided `cbt`, igrep's filtered lookahead,
and in the human face the dashboard's `[Bytes]` opcode row beside the
native `x/1i` line), sparse (born-sparse `Table<Any>` far-key cell: the
`glm_tbl_set_any` stop sequence over the register seam, `xqv`'s tagged-Any
decode out of the i128 argument, the overflow-map `pcells` fallback, the
Lua-line anchor; the human cell's six stops also pin the dashboard
surviving every one of them — `[Bytes]` row included, from the first
stop, which is the registration landmine's pin), any (the adoption pipeline on
`cases/any_adopt_store.lua`: a store
adopting the tagged cell for a born-Integer table — `parg` proves
`esize:16` and the runtime `border:3`, `pcells` decodes int and str cells
side by side, and the raw `*(long*)((char*)t+48)` read stays as the offset
pin), and join (the module's DWARF surviving an if-join, on
`cases/explore_join_phi.lua`: the backend banks dbg binds anchored at a
phi and flushes them before the block's first real instruction, because
LLVM's records model attaches each `llvm.dbg.value` to the *following*
instruction and a DbgRecord on a phi is invalid — clang answers by
dropping the whole module's debug info, so Lua-line breakpoints and
variable views die for every join-bearing script while the module still
runs; the cell pins the breakpoint stop past the join, `parg`/`pcells`
reading `pick` through the join phi, and the absence of clang's
`PHI Node must not have any attached DbgRecords` / `ignoring invalid
debug info` diagnostics — **in both faces**: DWARF validity is a property
of the compiled module, not of the config looking at it). Agent-only
aggregate markers assert the token discipline itself: no ANSI codes, no
per-stop dashboard, and no `[Bytes]` opcode row (it is human-only too) in
any agent transcript. One `check`/`check_absent`
pair serves both worlds, grepping ANSI-stripped text (a no-op for agent
output). A `--config` argument replaces the per-face shim with a draft —
a draft shim drops in as-is, and a bare draft core works too because the
smoke exports `GLM_GDB_FACE` per run (the core's documented env-first
precedence). The script's header carries the expected shapes; a ✗ maps
to the landmines above (`xqv` dying with `No symbol 'unsigned'` =
language sandwich stripped; `pcells` dying with `Invalid type
combination in equality test` = the `$bufp` init missing; a missing
pending-breakpoint stop = `breakpoint pending on` removed).

`--sweep` is the corpus-wide form of the join cell's absent checks: it
compiles every `cases/*.lua` with `--exe --debug` (the standalone link
face — compile and link only, no host, no execution) and asserts neither
of clang's invalid-debug-info diagnostics appears, so a backend DWARF
regression is caught on the whole corpus rather than one join-bearing
case.

## The steering shell and the e2e drivers

`tools/shell.lua` (LuaJIT + ffi, Linux only) is the **human** steering
surface — line-mode commands (`case`/`run`/`debug`/`smoke`/`face`/`keys`)
plus a single-key hotkey mode over a forkpty'd gdb child. Its default
face is human; the agent face stays reachable (`face agent`,
`--face agent`). Agents do not need the shell: the pure-gdb batched form
above is the agentic flow. The shell's own regression surface is the
three pty drivers — run them after any `tools/` change:

```sh
python3 tools/e2e_shell.py && python3 tools/e2e_extra.py && python3 tools/e2e_smoke.py
```

(the smoke one takes minutes; transcripts land in `target/shell/`).
They drive the REAL shell on a python pty, raw bytes in/out;
`expect` matches against ACCUMULATED, ANSI-stripped bytes — gdb on a tty
styles its own stop lines (the function name, even the `arg` parameter
name), and a styled `glm_exec` splits a raw-byte marker; the stripping is
the same convention smoke.sh's `check()` applies to its transcripts.
"Breakpoint 1 (glm_exec)
pending." (blua's notice) vs "Breakpoint 1, glm_exec (arg=" (the real
stop) is the discriminator; matching the bare "glm_exec" matches the
pending line instantly and skips the stop.
