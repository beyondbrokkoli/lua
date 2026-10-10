-- bench.lua — steady-state profile of the bridge, phase by phase.
--   luajit bench.lua [path-to-glm-binary] [n_small] [n_large] [iters]
-- Self-building: compiles lib_bench.so from bench_fixture.lua with the
-- debug compiler if it is not next to this script (the fixture is the
-- quiet Integer doubler — test_double.lua with the prints removed, so
-- the numbers measure the bridge and the module, not terminal I/O).

local glm, ai = arg[1], 1
if glm and tonumber(glm) then glm, ai = nil, 0 end  -- no path given

local dir = arg[0]:match("^(.*)/") or "."
package.path = dir .. "/?.lua;" .. package.path

local abi = require "glm_abi"
local ffi = require "ffi"

local n_small = tonumber(arg[ai + 1]) or 10
local n_large = tonumber(arg[ai + 2]) or 10000
local iters   = tonumber(arg[ai + 3]) or 2000
glm = glm or (dir .. "/../target/debug/glm")

local so = dir .. "/lib_bench.so"
do
    local f = io.open(so, "r")
    if f then
        f:close()
    else
        print("building " .. so)
        -- no cd: the compiler emits libglm_out.so in the inherited
        -- cwd; move it over from wherever we happen to run. os.execute
        -- returns a raw exit code (0/256, both truthy) — check == 0.
        local cmd = string.format(
            "%s %s/bench_fixture.lua 1 2 3 4 >/dev/null 2>&1"
                .. " && mv libglm_out.so %s",
            glm, dir, so)
        assert(os.execute(cmd) == 0, "compiler run failed: " .. cmd)
    end
end

local mod = assert(abi.module(so))
assert(mod.kind == abi.GLM_ARG.INT, "bench needs the Integer fixture")

local function mkargs(n)
    local t = {}
    for i = 1, n do t[i] = i end
    return t
end

local function timeit(name, f, n)
    local t0 = os.clock()
    for _ = 1, n do f() end
    local dt = (os.clock() - t0) / n * 1e6
    print(string.format("%-24s %10.1f us/call", name, dt))
    return dt
end

local function bench_round(n, iters)
    local args = mkargs(n)
    assert(mod:run(args)[0] == 2)   -- warmup + sanity

    print(string.format("-- %d-element Integer boundary, %d iters", n, iters))
    timeit("full mod:run", function() mod:run(args) end, iters)

    -- arg fill alone (reserve + one ffi.copy)
    timeit("  build_args", function()
        local t = mod.lib.glm_tbl_new(8, 0)
        mod.lib.glm_tbl_reserve(t, n)
        local span = ffi.cast("GlmTableHeader*", t).data
        local buf = mod.scratch.i64
        for i = 1, n do buf[i - 1] = args[i] end
        ffi.copy(span, buf, 8 * n)
        mod.lib.glm_tbl_free(t)
    end, iters)

    -- exec + free alone (the module's own cost)
    local t = mod.lib.glm_tbl_new(8, 0)
    mod.lib.glm_tbl_reserve(t, n)
    local span = ffi.cast("GlmTableHeader*", t).data
    local buf = mod.scratch.i64
    for i = 1, n do buf[i - 1] = args[i] end
    ffi.copy(span, buf, 8 * n)
    timeit("  glm_exec + frees", function()
        local r = mod.lib.glm_exec(t)
        mod.lib.glm_tbl_free(r)
    end, iters)
    mod.lib.glm_tbl_free(t)
end

bench_round(n_small, iters)
bench_round(n_large, math.max(50, math.floor(iters / 20)))
