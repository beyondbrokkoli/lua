# glm

The `glm` Lua-dialect compiler emits LLVM IR (`out.ll`),
links it against the C-ABI runtime (`glm_rt`), and produces
a shared library (`./libglm_out.so`) whose one export is the
whole program: `ptr @glm_exec(ptr %args)` — strict static
types, 0-indexed tables, i64 wrapping arithmetic, affine tables
and memory composting.

## Usage

```sh
# Build compiler + runtime (needs cargo and clang)
cargo build --release

# Compile and run a program -> ./libglm_out.so
./target/release/glm path/to/program.lua [args...]

# Compile a standalone executable instead -> ./glm_out
./target/release/glm --exe path/to/program.lua
./glm_out [args...]
```

The compiler writes into the current working directory: `out.ll` (the LLVM
IR), the linked module (`libglm_out.so`, or `glm_out` under `--exe`), and
the trace plate below. In the default mode it then acts as the reference
host: it `dlopen`s the module, resolves `glm_exec`, builds the `arg`
table from the extra CLI words, calls into the script, and frees args
and the returned table. Under `--exe` there is no host at all — the
executable carries its own: `@main(argc, argv)` delegates to the
runtime's `glm_exec_main` with the compile-time pinned element kind,
which parses the words, builds the table, calls `glm_exec`, and frees
both (the `return arg` identity check included).

```c
ptr glm_exec(ptr args); /* args: GlmTable*, returns GlmTable* or null */
```

Inside the script, `arg` names the table the host passed; a top-level
`return` hands a table (or nothing — null) back to the host and ends
execution. Only tables cross the boundary; scalars and strings stay
behind it. `arg` is only materialized when the script reads it (a
shadowing `local arg` takes the name over and the host passes null);
not reading `arg` means that `sys_alloc_count()` must be 0. `arg` is
pinned: the host owns the header, so it cannot be moved into another
binding or table (`local t = arg`, `t[1] = arg` are compile errors, as
is passing `arg` to a function that moves its parameter or returns it
into a move) — read its cells (`arg[i]`) instead; `return arg` is fine.

### Boundary argument types

The `arg` table's cell type is pinned by usage — the first position
that demands a type unifies the element, so the host (or the
executable's own host) parses the CLI words against exactly what the
script's code asked for:

| script usage                          | boundary cells     | accepted words     |
| ------------------------------------- | ------------------ | ------------------ |
| `arg[0] + 1`, `arg[0] < 5`, `-arg[0]`, `arg[0] + arg[1]` | `Table<Integer>` | 64-bit integers |
| `arg[0] + 0.5`, `arg[0] / 2`          | `Table<Float>`     | 64-bit floats      |
| `if arg[0] then`, `not arg[0]`        | `Table<Boolean>`   | `true` / `false`   |
| `arg[0] == "x"`                       | `Table<String>`    | any word           |
| `t[arg[0]]` (any key position: store, ctor entry, read) | `Table<Integer>` | 64-bit integers |
| nothing but copies, prints, and passes | `Table<Any>`      | every word         |

One boundary, one cell type — a Float demand after an Int demand is a
compile error, exactly like inline calls are monomorphic. Cells copied
into constructors and stores (`local u = {arg[0], 2}`) unify the same
way, a boundary read passed into an inline function pins through the
parameter, and the pin survives loops and if/else joins. A `Table`
element cannot be inferred: boundary cells hold scalars.

**No silent defaults — and no refusals either.** An element the script
never demands a type for resolves to **Any**: the boundary's own
dynamic cell. The reasoning is airtight by construction: the script
checked clean without ever pinning the element, which means its cells
were only ever **copied, printed, passed along, or returned** — never
used in a typed position (arithmetic, ordering, conditions, table
keys, stores into typed tables would each have pinned the element to
a concrete scalar first). That exact usage set is what the Any cell
serves, so the script is runnable as-is:

- Each cell is a 16-byte tagged word — payload in the low 64 bits,
  `GLM_ARG_*` tag in the high 64 — riding the same `GlmTable`
  machinery as every other element (one `Table<Any>`, one esize).
- The **host's words** pick each cell's kind at load, under the
  declared precedence **int → float → bool → string**: `5` is an
  Integer cell, `2.5` a Float, `true`/`false` Bools, anything else a
  String cell through the intern. The precedence is a documented
  property of the boundary vocabulary, not a guess about the script.
  Cells may differ in kind within one invocation.
- Inside the script the cell stays opaque: `print(arg[0])` dispatches
  on the tag at runtime, copies (locals, parameters, ctor entries,
  whole-table returns) move the 16 bytes blind, and
  `arg[0] == arg[1]` compares tags and payloads (strings by intern
  identity). Any typed use still pins the boundary monomorphically —
  the dynamic layer only ever covers the usage the static one
  provably left unconstrained.

Dynamics live at the boundary, and only there — inside the script,
strict static monomorphic typing is unchanged (a script with a typed
Int use still rejects a Float word at the host with the same message
as before).

**The contract is exported.** Every linked module — the `.so` and the
`--exe` executable alike — carries:

```c
i32 glm_arg_kind(void); /* -1 none, 0 int, 1 float, 2 bool, 3 string, 4 any */
```

Any host (C, LuaJIT FFI, a Rust runner) queries the module itself for
how to parse its words instead of guessing; the dev-loop host and the
executable's embedded host both key off the same answer (the compiler
resolved them to one parse path). `-1` names an argless module: the
script never reads `arg`, the host passes null, and extra words are
nobody's business. `4` (Any) names the dynamic contract above: every
word parses, each cell carrying its own tag chosen by the declared
precedence.

String words intern through the runtime's pool: the module's distinct
literals are registered at load (the backend emits a string registry
run from an `.init_array` constructor), and a word matching none of
them gets its own immortal copy — so `arg[0] == "literal"` is identity
over the intern space, the same contract as between script literals.

```sh
./target/release/glm example.lua 4 2   # the dev-loop host
./target/release/glm --exe app.lua
./glm_out 4 2                          # the executable's own host
```

## Tests

```sh
lua run.lua                # build + clippy, whole corpus, lock byte-identity
lua run.lua run STR ...    # only cases whose name contains any STR
lua run.lua probe FILE     # one arbitrary file; archives to target/probe/
lua reset.lua              # overwrite diff target
```

Every case classifies itself with magic comments
and the directory is the listing.

```lua
-- EXPECT: <line>            expected stdout line, in order (OPTIONAL:
                             without EXPECT pins the case still gets
                             compile/run/lock coverage)
-- EXPECT_BUILD_FAIL: <text> compile must fail; stderr contains <text>
-- EXPECT_PANIC: <text>      build passes, the script dies with <text>
                             on stderr
-- ARGS: <words...>          boundary arguments, passed through
                             verbatim and parsed against the case's
                             usage-pinned cell type (integers, floats,
                             true/false, or words for String tables);
                             irrelevant for cases that fail to compile
```

The bench's EXE section re-links every case with `--exe` and runs
`./glm_out` with the same ARGS words — the standalone twin of the
whole corpus, so the two hosts (the dev-loop dlopen host and the
executable's embedded `glm_exec_main`) are pinned to identical
behavior.

### Inline Functions

Anonymous functions parse as values — they are not LLVM
functions: each call site inlines the body as a value-yielding
`do...end` block. The body's `return`s join a local phi node that *is*
the call's value — no call stack, no ABI, zero runtime overhead
(and a call to `f` costs the same as pasting `f`'s body at the
call site, every time it runs).

```lua
local abs = function(n)
    if n < 0 then return 0 - n end
    return n
end
print(abs(-5), abs(5))          --> 5   5
```

- Only anonymous functions **assigned directly to a variable**
  (`local f = function(...) ... end`, `f = function(...) ... end`) are
  callable; calls resolve through that variable. Passing a function as
  a value or returning one is rejected — a function has no runtime
  representation.
- **Inlining, not closures**: the body executes at the call site, so
  it sees the caller's scope plus its parameters. A body that reads an
  enclosing name requires that name to be visible at the call site.
- Calls are **monomorphic**: the first call's argument types fix the
  signature; later calls must agree (`id(1)` then `id("s")` is a type
  error). Arity is exact.
- **Recursion is rejected at compile time** — an inline body that
  reaches back into itself would expand forever.
- A function that falls off the end without returning yields its
  return type's zero value (0 / 0.0 / false / null).
- Bodies compose with everything else: early returns, loops, `arg`,
  string returns (pool-identity equality), table returns (the returned
  table carries its constructor's ownership into the caller — release
  it there), and calls that rebind enclosing names inside loops and
  branches (the loop/join phis track them).

# Signals

Every compile writes `.glm_trace.bin` — the event chronology: a u32 total,
a u32 ring capacity, then one slot byte per fired signal in fire order
(oldest overwritten at capacity). Signal names come from
`trace_signals.txt`, `build.rs` turns it into the `TRACE_*` constants
compiled into `src/trace.rs`, so compiler and decoder
cannot drift). The previous plate file is removed before the new one is written.
The compiled program writes its own sidecar plate, `.glm_rt_trace.bin`
(256 sticky bytes, one per slot), fresh per run the same way.

```sh
python3 plate.py                  # event chronology of the last compile
python3 plate.py one.bin two.bin  # event-sequence diff (baseline first)
```

To add a signal: append `<slot> <NAME> <owner> <meaning>` on an unused slot
in `trace_signals.txt`, then poke it through the existing helpers.
