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
./target/release/glm path/to/program.lua [int args...]
```

The compiler writes into the current working directory: `out.ll` (the LLVM
IR), `libglm_out.so` (the linked module), and the trace plate below. It then
acts as the reference host: it `dlopen`s the module, resolves `glm_exec`,
builds the `arg` table from the extra integer CLI words, calls into the
script, and frees args and the returned table.

```c
ptr glm_exec(ptr args); /* args: GlmTable*, returns GlmTable* or null */
```

Inside the script, `arg` names the table the host passed (8-byte integer
cells, `Table<Integer>`); a top-level `return` hands a table (or nothing —
null) back to the host and ends execution. Only tables cross the boundary;
scalars and strings stay behind it. `arg` is only materialized when the
script names it, not using `arg` means that `sys_alloc_count()` must be 0.
`arg` is pinned: the host owns the header, so it cannot be moved into
another binding or table (`local t = arg`, `t[1] = arg` are compile errors,
as is passing `arg` to a function that moves its parameter or returns it
into a move) — read its cells (`arg[i]`) instead; `return arg` is fine.

```sh
./target/release/glm ~/interop_alloc_arg_header.lua 4 2
```

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
