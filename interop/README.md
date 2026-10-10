# interop — calling glm modules from LuaJIT

`glm_abi.lua` is the host-side bridge: it loads a compiled
`libglm_out.so` via `ffi.load`, builds the `arg` table for the module's
boundary type, calls `glm_exec`, decodes the result, and frees both
tables — with **zero changes to the glm runtime** (`src/rt.rs`).

## Usage

```lua
local abi = require "glm_abi"          -- from this directory

-- one-shot (handle cached per path)
local out = abi.run("lib_bench.so", { 5, 10, 15 })  -- 0-indexed result

-- object form for repeated calls on one cached handle
local mod = assert(abi.module("lib_bench.so"))
local out = mod:run({ 5, 10, 15 })
print(mod.kind_name)                    -- "Integer" / "Float" / "Any" / ...

-- decode a caller-held GlmTable* (custom exec paths); the caller
-- keeps ownership — everything is copied out immediately
local out2 = mod:decode(some_glm_table_ptr)
```

All five boundary kinds work in both directions, including **Any**:
LuaJIT's FFI cannot pass i128 in registers, so the bridge never tries.
It packs the 16-byte tagged cell itself (payload low 64, `GLM_ARG_*`
tag high 64 — `any_pack`'s layout) and moves it through
`glm_tbl_set`/`glm_tbl_get`'s **byte seam**, which memcpy-reads
esize-16 tables. Strings are interned through the module's own
`glm_str_intern`, preserving pool-identity equality.

## How the fast paths stay legal

- **Arg fill** uses the compiler's own fast-fill contract: one
  `glm_tbl_reserve(t, n)` (grows the span, commits `border = n` —
  exactly what emitted fill loops rely on) then a single `ffi.copy`
  of the packed cells. A fresh `glm_tbl_new` table is Dense, so
  0..n-1 direct span stores are the same stores `set_core` would
  perform, minus n FFI calls.
- **Decode** reads the dense span directly only when the header says
  Dense with no overflow map (`mode@32 == 0`, `sparse_map@40 == NULL`)
  — under those conditions every store below `border` went to the
  span. Otherwise it falls back to per-cell `glm_tbl_get`.
- The header is accessed through a mirrored cdef struct, pinned at
  load time (`assert(ffi.sizeof("GlmTableHeader") == 56)`) against
  `test_abi_layout_offsets` in `src/rt.rs`. If the header layout ever
  changes, this file fails loudly instead of misreading.
- **Handles** are cached per path. LuaJIT's FFI has no `dlclose`, so
  a module stays mapped for the process — one dlopen, one symbol
  resolution, one `.init_array` run per module.

## The exception path cannot leak

`build_args` packs **before** it allocates: every conversion error (a
string into an Integer boundary, an unsupported Any type) throws while
no Rust memory is live, so a poisoned host input can never orphan an
arg table. Hosts can self-check leak-freedom around any call sequence:

```lua
local before = tonumber(mod.lib.sys_alloc_count())
local ok, err = pcall(mod.run, mod, { 1, "corrupted_read", 3 })
assert(not ok and tonumber(mod.lib.sys_alloc_count()) == before)
```

(`sys_alloc_count` is the runtime's live table-header ledger: +1 per
`glm_tbl_new`, −1 per free. Careful with cdata: an `int64_t` will not
concatenate with strings — wrap in `tonumber`.) The runtime also
self-reports at exit via the trace sidecar ("slot 90 — leak").

`host.lua` is a runnable demo of the whole story: an Integer-boundary
input analyzed by `module_source.lua`, decoded from an Any result
table — run `luajit interop/host.lua` from the project root (the
project's strict invocation scheme). It rebuilds the module when the
source is newer than the `.so` and surfaces compiler errors instead of
swallowing them.

## Files

- `glm_abi.lua` — the bridge (canonical copy; `interop_lab/` in the
  parent workspace holds the audited v1 and the proposal document)
- `test_interop.lua` — self-building end-to-end tests (compiles the
  fixtures with `../target/debug/glm` if missing)
- `bench.lua` — self-building per-phase steady-state profile (run /
  build_args / glm_exec); `bench_fixture.lua` is its quiet Integer
  doubler source — test_double with the prints removed, so the
  numbers measure the bridge and module, not terminal I/O
- `test_double.lua`, `test_mixed.lua`, `bench_fixture.lua`,
  `test_edge.lua` — glm module sources for the fixtures (Integer /
  Any / quiet-Integer / far-key negatives)
- `module_source.lua` + `host.lua` — the runnable demo (Any result
  over an Integer boundary, self-building, stale-detecting)

## Caveats and edge semantics

- Args must be a **dense** 1-indexed Lua array: sizing rides Lua's `#`,
  so a nil hole truncates the input at the hole.
- A string cell with a **null payload** decodes to `nil`, not a crash
  (and `ffi.string(NULL)` would be an uncatchable segfault, so the
  guard is load-bearing).
- Lua numbers above 2^53 lose precision as Any INT cells (LuaJIT has
  no i64 literal lane through this path); int/float disambiguation for
  Any inputs is integral-and-<2^53 → INT.
- A Float-boundary module's args and results are doubles end-to-end.
- The result is a plain 0-indexed Lua table: iterate `0..count-1`,
  never with `#` or `ipairs` (Lua's `#` stops at the first nil).
- Packing buffers are cached per module object and double to fit, but
  batches above 2^20 cells ride a transient buffer instead — a one-off
  10M-element call does not pin ~80MB forever.

## Boundary kinds are inferred by the compiler

`glm_arg_kind()` is baked at compile time from how the module uses
`arg[]` — arithmetic keeps the boundary Integer, while a bare
`local t = {}; t[0] = arg[0]` copies the element verbatim and widens
the boundary to **Any** (the checker admits the element without
pinning a kind). If a module's kind surprises you, look for a direct
`arg[i]` pass-through into a table. `test_edge.lua` shows both shapes.
