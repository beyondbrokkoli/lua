-- test_interop.lua — end-to-end bridge tests over both boundary kinds.
-- Self-building: compiles the .so fixtures with the debug compiler if
-- they are not next to this script.
--
-- Usage:  luajit test_interop.lua [path-to-glm-binary]

local dir = arg[0]:match("^(.*)/") or "."
package.path = dir .. "/?.lua;" .. package.path

local abi = require "glm_abi"
local ffi = require "ffi"

local glm = arg[1] or (dir .. "/../target/debug/glm")

local function ensure(src, want)
    if want:sub(1, 1) ~= "/" then want = dir .. "/" .. want end
    local src_path = dir .. "/" .. src
    local f = io.open(want, "r")
    if f then
        f:close()
        -- stale when the source is newer than the artifact (POSIX -nt)
        if os.execute(string.format("test %s -nt %s", src_path, want)) ~= 0 then
            return want
        end
        print("rebuilding " .. want .. " (source newer)")
    else
        print("building " .. want)
    end
    -- no cd: the compiler emits libglm_out.so in the inherited cwd.
    -- os.execute returns a raw exit code (0/256, both truthy) — check == 0.
    -- Compiler stderr goes to a log: printed on failure, never swallowed.
    local log = want .. ".log"
    local cmd = string.format(
        "%s %s 1 2 3 4 >/dev/null 2>%s && mv libglm_out.so %s",
        glm, src_path, log, want)
    if os.execute(cmd) ~= 0 then
        local g = io.open(log, "r")
        local msg = g and g:read("*a") or "(no compiler output)"
        if g then g:close() end
        error("compiler run failed:\n" .. msg, 0)
    end
    os.remove(log)
    return want
end

local int_so = ensure("test_double.lua", "lib_int.so")
local any_so = ensure("test_mixed.lua", "lib_any.so")
local edge_so = ensure("test_edge.lua", "lib_edge.so")

-- Integer boundary
local kind = abi.get_arg_kind(int_so)
assert(kind == abi.GLM_ARG.INT, "Integer boundary, got " .. kind)
local out = abi.run(int_so, { 5, 10, 15, 20 })
assert(out[0] == 10 and out[1] == 20 and out[2] == 30 and out[3] == 40,
       "integer doubling: " .. tostring(out[0]))
print("INT  ok: 5 10 15 20 -> 10 20 30 40")

-- Any boundary
local kind = abi.get_arg_kind(any_so)
assert(kind == abi.GLM_ARG.ANY, "Any boundary, got " .. kind)
local out = abi.run(any_so, { 42, "hello", 3.14, true })
assert(out[0] == 42,          "int cell:    " .. tostring(out[0]))
assert(out[1] == "hello",     "string cell: " .. tostring(out[1]))
assert(math.abs(out[2] - 3.14) < 1e-12, "float cell:  " .. tostring(out[2]))
assert(out[3] == true,        "bool cell:   " .. tostring(out[3]))
print("ANY  ok: {42, \"hello\", 3.14, true} roundtrip exact")

-- Object API, cached handle, repeated calls
local mod = assert(abi.module(int_so))
for round = 1, 3 do
    local out = mod:run({ 1 * round, 2 * round })
    assert(out[0] == 2 * round and out[1] == 4 * round, "round " .. round)
end
print("OBJ  ok: 3 rounds on one cached handle")

-- Identity free path (`return arg`)
-- test_mixed returns its arg table: exec must free exactly once
-- (identity check) and still decode. Doing it 100x catches double-free
-- heap corruption the allocator would surface.
for i = 1, 100 do
    local out = abi.run(any_so, { i })
    assert(out[0] == i)
end
print("FREE ok: identity path x100, no heap corruption")

-- Edge: negative ints, both decode paths
-- Fast path (Dense, no map): signed span read.
local out = abi.run(int_so, { -5, 1 })
assert(out[0] == -10, "negative fast path: " .. tostring(out[0]))
-- test_edge stores at t[100001] (past len + SPARSE_THRESHOLD 100_000),
-- so its result table carries an overflow map -> checked fallback path
-- (esize-8 branch: where the unsigned-buffer sign bug lived).
assert(abi.get_arg_kind(edge_so) == abi.GLM_ARG.INT)
local out = abi.run(edge_so, { -42, 7 })
assert(out[0] == -84, "negative fallback: " .. tostring(out[0]))
assert(out[100001] == 14, "far-key fallback: " .. tostring(out[100001]))
print("EDGE ok: negatives exact (fast + fallback paths)")

-- Edge: null string payload decodes to nil, no segfault
-- A hand-crafted Any table: t[0] = STRING-tagged NULL payload, t[1] =
-- int 99, t[100001] = "hi" via glm_tbl_set (far key -> overflow map ->
-- fallback decode). NULL cdata compares equal to nil in LuaJIT.
local m = assert(abi.module(any_so))
local t = m.lib.glm_tbl_new(16, 0)
m.lib.glm_tbl_reserve(t, 2)
local cell = ffi.new("long long[2]")
local span = ffi.cast("long long*", ffi.cast("GlmTableHeader*", t).data)
cell[0], cell[1] = 0, abi.GLM_ARG.STRING      -- STRING tag, NULL payload
span[0], span[1] = cell[0], cell[1]
cell[0], cell[1] = 99, abi.GLM_ARG.INT
span[2], span[3] = cell[0], cell[1]
cell[0] = ffi.cast("long long", m.lib.glm_str_intern("hi", 2))
cell[1] = abi.GLM_ARG.STRING
m.lib.glm_tbl_set(t, 100001, cell)
local out = m:decode(t)
assert(out[0] == nil, "null string -> nil, got " .. tostring(out[0]))
assert(out[1] == 99, "int cell after null string")
assert(out[100001] == "hi", "far string fallback: " .. tostring(out[100001]))
m.lib.glm_tbl_free(t)
print("EDGE ok: null string -> nil, far-key Any fallback")

-- Edge: a huge one-off batch must not pin its buffer
-- SCRATCH_MAX is 2^20 cells; the 1M+1 arg batch rides a transient
-- buffer (GC reclaims it) while the small-call cache stays untouched.
-- test_double only fills t[0..3] regardless of arg count, so the
-- result side stays tiny — that's fine, the buffer is on the arg side.
local big_n = 2^20 + 1
local big = {}
for i = 1, big_n do big[i] = i end
local out = mod:run(big)
assert(out[0] == 2 and out[3] == 8, "1M batch run")
assert((mod.scratch.i64_cap or 0) < big_n, "transient buffer got cached")
print("EDGE ok: 1M-element batch rides a transient buffer")

-- Poison, host side: a string into an Integer boundary.
-- The FFI conversion throws inside build_args — which packs BEFORE it
-- allocates, so the Rust arg table is never created and the live
-- allocation ledger must come back to its baseline.
local baseline = mod.lib.sys_alloc_count()
local ok, err = pcall(mod.run, mod, { 1, "corrupted_read", 3 })
assert(not ok, "poisoned input must throw")
assert(tostring(err):match("cannot convert"),
       "unexpected error: " .. tostring(err))
assert(mod.lib.sys_alloc_count() == baseline,
       "poisoned call leaked: " ..
       tonumber(mod.lib.sys_alloc_count() - baseline) .. " table(s)")
print("POISON ok: host-side violation throws leak-free")

-- Global leak assertion: every table allocated by this suite was freed.
assert(mod.lib.sys_alloc_count() == 0,
       "suite leaked " .. tonumber(mod.lib.sys_alloc_count()) .. " table(s)")

print("ALL INTEROP TESTS PASSED")
