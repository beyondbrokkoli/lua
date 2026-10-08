# glm

A Lua-dialect compiler: strict static types, 0-indexed tables, i64
wrapping arithmetic, affine tables, memory composting — and one
deliberate cheat code, the **Any** dynamic cell. `glm` emits LLVM IR
(`out.ll`), links against the C-ABI runtime (`glm_rt`), and produces a
shared library whose one export is the whole program:

```c
ptr glm_exec(ptr args);     /* args: GlmTable*, returns GlmTable* or null */
i32  glm_arg_kind(void);    /* -1 none, 0 int, 1 float, 2 bool, 3 string, 4 any */
```

## Usage

```sh
cargo build --release
./target/release/glm program.lua [args...]   # -> ./libglm_out.so, then runs it
./target/release/glm --exe program.lua       # -> ./glm_out, a standalone executable
```

Inside the script, `arg` names the host-passed table; a top-level
`return` hands a table (or nothing — null) back. Only tables cross the
boundary; scalars and strings stay behind it. `arg` is host-owned and
pinned: it cannot be moved into another binding or table — read its
cells (`arg[i]`) instead; `return arg` is fine. `arg` materializes
only when the script reads it (not reading it means
`sys_alloc_count()` must be 0).

The `arg` table's cell type is **pinned by usage** — the first position
that demands a type unifies the element, and the host parses the CLI
words against exactly that:

| script usage                        | boundary cells     | accepted words     |
| ----------------------------------- | ------------------ | ------------------ |
| `arg[0] + 1`, `arg[0] < 5`, `t[arg[0]]` | `Table<Integer>` | 64-bit integers |
| `arg[0] / 2`                        | `Table<Float>`     | 64-bit floats      |
| `if arg[0] then`                    | `Table<Boolean>`   | `true` / `false`   |
| `arg[0] == "x"`                     | `Table<String>`    | any word           |
| nothing but copies, prints, passes, `#` | `Table<Any>`    | every word         |

One boundary, one cell type — a Float demand after an Int demand is a
compile error. `glm_arg_kind()` exports the contract so any host (C,
LuaJIT FFI, a Rust runner) queries the module instead of guessing.

## The themes

**Strict by default.** Arithmetic, ordering, conditions, and table
keys demand pinned scalars. Every table is monomorphic: one element
repr per table, always. Recursion is rejected at compile time
(inline bodies expand forever).

**Any is the cheat code — written, never silent.** The engine's answer
to a type conflict is a pipeline, not a verdict: the compiler sees a
conflict, it *tries* the Any dynamic cell, and only if the cell cannot
honestly hold the value does it throw the correct compiler error — the
specific refusal for that position, naming the cause. No silent
defaults, no blanket refusals either. Any is opt-in and the opt-in is
written into the script:

- a **mixed scalar constructor** — `local t = {"hello", 5, 2.5, true}`
  is a `Table<Any>`, the script explicitly choosing the dynamic cell;
- an **unconstrained boundary** — a script that only ever copies,
  prints, compares, passes, returns, or measures its `arg` cells;
- **scalar-kind adoption** — a name whose scalar-kind witnesses mix
  (`local t = {1}` then `t[2] = "two"`, rebind chains, nested layers)
  becomes dynamic for the whole script; the typed-position refusals
  name the witness line that caused it beside the line where the
  conflict surfaced.

The dynamic cell is a 16-byte tagged word (payload low, kind tag
high). The tag is read exactly once, inside a `glm_any_*` runtime
helper — the emitted IR is tag-blind. Scalars flow INTO dynamic cells
(packing); an Any value never enters a strictly typed position, and a
table value never rides a cell. Extending Any is extending the
whitelist one operation at a time — the admission protocol, its
borders, and its guards are `ROADMAP.md`'s core.

**`#` is the runtime border.** `#t` reads one past the highest index
ever stored — the table's living extent, maintained by the runtime on
every store and committed by the reserves of proven full-fill loops.
True for grown tables, far keys, row reads, and the host-built `arg`
itself (`#arg` is the word count); a null table answers 0.

**Inline, not called.** Anonymous functions assigned directly to a
variable are callable; each call site inlines the body as a
value-yielding block — the body's `return`s join a phi node that *is*
the call's value. No call stack, no ABI, zero runtime overhead. Calls
are monomorphic (the first call's argument types fix the signature);
arity is exact; the body executes at the call site and sees the
caller's scope plus its parameters.

**Determinism over luck.** No hash function on any access path; string
identity is intern-once-at-first-sight, and key enumeration IS the
identity. Locked baselines are byte-compared on every bench run.

## Tests

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

## The rest of the documentation

- **`ROADMAP.md`** — the engine's contract: the standing laws, the Any
  admission protocol (how to extend the cheat code safely), the desync
  tripwire inventory, and the open horizons (Track A's declaration
  layer and AnyDeepFree, Track B's string keys).
- **`AGENTS.md`** — the GDB tool: one core (`gdb/glm.gdbinit`), two faces
  (`.gdbinit.agent` for agents, `.gdbinit` for humans, both at the repo
  root): launching, the one-line command reference, the header/cell
  decoding rules, and the landmine list. `./gdb/smoke.sh` self-verifies
  both faces (and the corpus diagnostic sweep with `--sweep`) after
  toolchain or config updates.
- **`STRKEYS_STEP12.md`** — Track B's execution sheet for Steps 1–2
  (string-keyed records, check-only); stays until Track B ships.

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
