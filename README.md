# glm

A Lua-dialect compiler: strict static types, 0-indexed tables, i64 wrapping
arithmetic, affine tables, memory composting — and one deliberate cheat
code, the **Any** dynamic cell. `glm` emits LLVM IR (`out.ll`), links
against the C-ABI runtime (`glm_rt`), and produces a shared library whose
one export is the whole program (or a standalone executable with `--exe`):

```c
ptr glm_exec(ptr args);     /* args: GlmTable*, returns GlmTable* or null */
i32  glm_arg_kind(void);    /* -1 none, 0 int, 1 float, 2 bool, 3 string, 4 any */
```

## The dependency: `logos`

`Cargo.toml` declares exactly **one** external crate —
[`logos`](https://github.com/maciejhirsz/logos) (`0.16.1`), a derive-macro
lexer generator. The token enum in `src/lexer.rs` is `#[derive(Logos)]`,
with `#[logos(skip ...)]` rules for whitespace, `--` line comments,
`--[[ ... ]]` block comments, and the shebang; logos compiles those
patterns into a state machine *at compile time*, so lexing is a zero-cost
walk with no runtime dependency.

## The language

### Types and literals

- **Int** — i64, decimal literals, wrapping arithmetic.
- **Float** — f64, point literals (`3.14`, `2.0`). Prints
  shortest-round-trip: `-4.0` appears as `-4`.
- **Bool** — `true` / `false`.
- **String** — double-quoted **literals only**: interned once at first
  sight, NUL-terminated C pointers. No concat, no indexing, no
  comparison; `#s` is the interned length (a variable lowers to one
  `glm_str_len` call; a directly visible literal folds to a constant).
- **Table** — first-class, see below. **Function** — first-class, see
  below. There is no `nil` *value*: `x = nil` is a release assignment
  that frees the binding (reading a table binding after its release is a
  compile-time lifetime error).

### Strict arithmetic

Floats and integers never mix implicitly — `a + pi` with `a` an Int is a
compile error, not a coercion. The lanes:

- `+ - * // %` on two Ints stay Int; Floats have their own lane.
- `/` is the one operator that leaves the Int lane: `17 / 5` prints
  `3.4` (a Float result from Int operands).
- `//` floors toward −infinity (`-7 // 2` = `-4`); `%` takes the
  divisor's sign (`7 % -2` = `-1`) — Lua's law, floats included.
- Comparisons `< > <= ==` on numbers, `==` on booleans. `and` / `or` /
  `not` short-circuit and work in expressions, `print` args, and `while`
  headers.

### Control flow and functions

- `if / elseif / else`, `while`, and `do ... end` pure scopes. All
  iteration is `while`-based (no numeric `for`).
- Functions are anonymous expressions assigned to locals
  (`local f = function(x) ... end`). Calls are **inline, not called**:
  each call site inlines the body as a value-yielding block whose
  `return`s join a phi node that *is* the call's value. No call stack,
  no ABI. Calls are monomorphic (the first call fixes the signature),
  arity is exact, the body sees the caller's scope plus its parameters —
  and recursion is rejected at compile time.

### Tables — monomorphic by first touch, Any for mixed stores

Indices are **0-based**. Constructors: `{}` and positional lists
(`{7}`, `{1, 2}`). An empty table's cell kind is decided by its first
store — one element repr per table, always (Int, Float, Bool byte-packed
1-byte cells, or String pointer cells). A heterogeneous store into a
typed table is a checker conflict *by default* — and the engine's answer
is a pipeline, not a verdict: it tries the Any dynamic cell, and only if
the cell cannot honestly hold the value does it throw the specific
refusal naming the cause. Any is opt-in and written into the script:

- a **mixed scalar constructor** — `local t = {"hello", 5, 2.5, true}`
  is a `Table<Any>`;
- an **unconstrained boundary** (see the `arg` table below);
- **scalar-kind adoption** — recently this includes *storing* a
  different type into an already-born table:

```lua
local t = {7}          -- t is born Integer-celled
t[2] = "five"          -- the String store flips t to the dynamic Any
                        -- cell for the WHOLE script — flow-insensitively
print(t[2])             -- five (the packed String cell)
print(#t)               -- 3  (the runtime border)
print(#t[2])            -- 4  (the cell's own string length)
```

The pre-existing Int entry packs retroactively through
`glm_any_from_int`. The join is total: every read of `t`, even one
textually *earlier* than the adopting store, sees Any — which is also
why a typed read before the store still refuses. The dynamic cell is a
16-byte tagged word (payload low, kind tag high); the tag is read
exactly once, inside a `glm_any_*` runtime helper — the emitted IR is
tag-blind. Scalars flow INTO dynamic cells; an Any value never enters a
strictly typed position, and a table value never rides a cell.

Nested tables (`local m = {}; m[0] = {7, 8}`) make Table-of-Table rows;
releasing the outer binding (`m = nil`) deep-frees both.

### `#` — the runtime border

`#t` reads one past the highest index ever stored — the table's living
extent, maintained by the runtime on every store and committed by the
reserves of proven full-fill loops. True for grown tables, far keys, row
reads, and the host-built `arg` itself (`#arg` is the word count); a
null table answers 0. One compile-time case: a dense constructor's
border (`#{1,2,3}`) folds to the entry count.

### The host boundary: `arg`, `print`, `sys_alloc_count`

Inside the script, `arg` names the host-passed table; a top-level
`return` hands a table (or nothing — null) back. Only tables cross the
boundary. `arg` is host-owned and pinned: it cannot be moved into
another binding or table — read its cells (`arg[i]`) instead;
`return arg` is fine. `arg` materializes only when the script reads it
(not reading it means `sys_alloc_count()` must be 0).

The `arg` table's cell type is **pinned by usage** — the first position
that demands a type unifies the element, and the host parses the CLI
words against exactly that:

| script usage                            | boundary cells     | accepted words     |
| --------------------------------------- | ------------------ | ------------------ |
| `arg[0] + 1`, `arg[0] < 5`, `t[arg[0]]` | `Table<Integer>`   | 64-bit integers    |
| `arg[0] / 2`                            | `Table<Float>`     | 64-bit floats      |
| `if arg[0] then`                        | `Table<Boolean>`   | `true` / `false`   |
| `arg[0] == "x"`                         | `Table<String>`    | any word           |
| nothing but copies, prints, passes, `#` | `Table<Any>`       | every word         |

One boundary, one cell type — a Float demand after an Int demand is a
compile error. `glm_arg_kind()` exports the contract so any host (C,
LuaJIT FFI, a Rust runner) queries the module instead of guessing.
`print(...)` is variadic and tab-separates its arguments;
`sys_alloc_count()` is the live-allocation ledger.

## Using the human tools

### Build and run

```sh
cargo build                     # debug build — the debugging harness uses this
./target/debug/glm program.lua [args...]   # -> ./libglm_out.so, then runs it
cargo build --release
./target/release/glm program.lua           # the fast path
./target/debug/glm --exe program.lua       # -> ./glm_out, a standalone executable
```

### The steering shell (`tools/shell.lua`)

The comfortable way to drive everything, from the repo root:

```sh
luajit tools/shell.lua                     # verbose
luajit tools/shell.lua face=agent          # minimal
```

Line-mode commands (TAB completes case names):

| command | what it does |
|---|---|
| `case [name]` | list the corpus (`cases/`), or one case's `ARGS` pin |
| `run <case> [args...]` | one-shot batch gdb run; prints the transcript, lands at the `glm_exec` boundary |
| `debug <case> [args...]` | **live gdb on a pty** — single-key hotkeys |
| `smoke [agent\|human\|both]` | `./gdb/smoke.sh` streamed through |
| `face [agent\|human]` | switch the face subsequent spawns load |
| `keys [KEY [gdb cmd...]]` | list / rebind the hotkey map |
| `help`, `exit` | as advertised |

Hotkey mode (inside `debug`) — the default verbose mode
prints the full register dashboard at every stop:

| key | sends |
|---|---|
| `→` / `←` | `si` / `finish` |
| `↑` / `↓` | `up` / `down` |
| `space` | `continue` |
| `b` | prompt for a `break` argument (set after the first stop — see landmines) |
| `c` / `s` / `h` | `cregs` / `pstack` / `here` |
| `r` | `run` (the pending `glm_exec` breakpoint lands again) |
| `q` / `ESC` / `Ctrl+C` | back to line mode |

Case names accept every spelling — bare, `.lua`-less, `cases/`-prefixed,
absolute. Ctrl+C always restores the terminal; no gdb is left orphaned.

### Setup

GDB's auto-load safe-path gate declines a local `./.gdbinit` until its
path has been granted.
`~/.gdbinit`:

```gdb
add-auto-load-safe-path /home/halim/lua/.gdbinit
```

Then, from the repo root (the host resolves `./libglm_out.so` and the
shims relative to it):

```sh
cargo build
rust-gdb -q --args ./target/debug/glm --debug cases/<case>.lua <args...>
```

No dotfile edit? The explicit form always works, gate or no gate (`-nx`
skips both init files, `-x` loads the shim directly), and `-q` silences the
version greeting. One border to respect: gdb's own options end at `--args` —
everything after it is the case's boundary argv, so a flag placed there rides
into the script, never into gdb.

```sh
rust-gdb -q -nx -x .gdbinit --args ./target/debug/glm --debug cases/<case>.lua <args...>
```

Every stop prints the dashboard: `[Ret/Stack]` (RAX/RSP/RBP), `[Args
1-3]`/`[Args 4-6]`, `[Scratch]` (R10/R11/RIP), `[Saved]` (RBX–R15 +
EFL), a `[Bytes]` row with the next instruction's raw opcode bytes, and
gdb's own styled `x/1i` line as the anchor. `blua` (or `to_script`)
runs straight to the script's first statement; no default breakpoints
are set. The core's commands, verbose output:

| Command | Output |
|---|---|
| `cregs` | All 16 GP regs + `rip` on one line, flags as set-flag names (identical in both faces). |
| `parg [addr\|var]` | `GlmTable` header as an annotated multi-line block; `border` is `#t`'s answer. No argument: tries `$rdi`, then `$rbx`. |
| `pcells <tbl\|var> [count]` | Decoded cells, one `[INT]`-labeled line per cell; count defaults to `min(len, 4)`. Born-sparse tables fall back to the module's `glm_tbl_get`, so far-key cells decode correctly. |
| `xq <addr>` | One tagged 128-bit cell: tag/payload words plus the decode. |
| `xqv <i128 expr>` | Same decode for a cell passed BY VALUE (the register seam: `xqv val` at a `glm_tbl_set_any` stop). |
| `pkind` | Inferior-calls `glm_arg_kind()` — the module's own boundary contract. |
| `cbt [N]` | Backtrace with `std::`/libc frames elided, renumbered over the printed frames (use native `bt` for native numbering). |
| `igrep <count> <pattern> [addr]` | Disassembles `count` instructions ahead, prints only regex matches — gdb's own disassembler styling, matches bold, cyan summary line. |
| `here` | Current position as one line, the `file:line` anchor in green. |
| `pstack [count] [addr]` | The live stack window: one colored row per word, annotated (`symbol+offset` for code pointers, the saved-rbp chain, `pstack 4 $rbp` for the prologue pair). |
| `stkwatch [on\|off]` | Opt in to a pstack window on EVERY stop. |
| `blua` / `to_script` | Run straight into the script boundary. |
| `xd`, `regs` | gdb-native `x/4gx`, `info registers`. |

### Reading table output

`GlmTable` is `#[repr(C)]` (see `src/rt.rs`): `data`+0, `len`+8,
`reserve`+16, `esize`+24, `mode`(u8)+32 (0=dense, 1=sparse),
`contains_tables`(u8)+33, `sparse_map`+40, `border`(i64)+48 — `border`
is `#t`'s answer, one past the highest index ever stored (`len` is the
span watermark, not it). Cell semantics by `esize`: **1** = bool
(`b:`), **8** = scalar, printed as `i:` (signed), `u:` (hex) and `f:`
(bit-cast double) together — `ct:1` switches the view to row pointers —
**16** = tagged Any cell: payload in the low 64 bits, kind tag in the
high 64 (`0=int, 1=float, 2=bool, 3=str`; strings print their interned
content).

### Known issues

- `$es` is the x86 segment register; the config's convenience variables
  avoid every segment-register name. `$eflags` is an opaque flags
  struct: no casts, no field access.
- `breakpoint pending on` is load-bearing — never remove it (a pending
  breakpoint with `confirm off` would silently not exist otherwise).
- Conditional breakpoints on the dlopen'd `.so` never fire if set
  before `run`. Set them after the first stop.
- `finish` from inside a not-yet-resolved PLT stub aborts ("Cannot
  insert breakpoint 0") — step `ni` over PLT calls or `si` into the
  resolved function first. `finish` from inside the ld-linux resolver
  itself works, and runs the rest of the call.
- The full list (language sandwiches, void convenience vars, SSA
  variable visibility) is in `AGENTS.md` — the core is shared, so are
  its traps.

### Smoke test

After toolchain or config updates: `./gdb/smoke.sh` (both faces; cargo
build happens first). It drives the full face×case matrix batched and
greps the transcripts for content pins.
