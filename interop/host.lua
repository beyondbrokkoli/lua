-- interop/host.lua
-- Run with: luajit interop/host.lua (from the project root)

local abi = require("interop.glm_abi")

local so_path = "interop/lib_module.so"
local source_path = "interop/module_source.lua"

local f = io.open(so_path, "r")
local stale = false
if f then
    f:close()
    -- rebuild when the source is newer than the artifact (POSIX -nt):
    -- no more manual .so deletion to pick up module edits
    stale = os.execute(string.format("test %s -nt %s", source_path, so_path)) == 0
end
if not f or stale then
    print((stale and "Rebuilding " or "Building ") .. so_path .. "...")
    -- target/release/glm (no ../) since we run from the project root.
    -- os.execute returns a raw exit code (0/256, both truthy) — check == 0.
    -- Compiler stderr goes to a log: printed on failure, never swallowed.
    local log = so_path .. ".log"
    local cmd = string.format("target/release/glm %s >/dev/null 2>%s && mv libglm_out.so %s",
                              source_path, log, so_path)
    if os.execute(cmd) ~= 0 then
        local g = io.open(log, "r")
        error("Compiler run failed:\n" .. (g and g:read("*a") or "(no output)"), 0)
    end
    os.remove(log)
end

print("Loading module via ABI...")
local mod = assert(abi.module(so_path))
print("Inferred Input Boundary: " .. mod.kind_name)

-- 3. Prepare our 1-indexed Lua input data
local sensor_data = { 12, 18, 42, 55, 31, 9, 27 }
print("Passing " .. #sensor_data .. " integers to module...\n")

-- 4. Execute the boundary crossing
local result = mod:run(sensor_data)

-- 5. Read the 0-indexed Any result
if result then
    print("--- Decode Successful ---")
    print("Header:  " .. tostring(result[0]))
    print("Count:   " .. tostring(result[1]))
    print("Sum:     " .. tostring(result[2]))
    print("Max:     " .. tostring(result[3]))
    print("Alert:   " .. tostring(result[4]))
    print("Status:  " .. tostring(result[5]))
end

-- 6. The self-check: the runtime's live table-header ledger must be
-- back to zero — every glm_tbl_new was matched by a free.
print("Alloc ledger: " .. tonumber(mod.lib.sys_alloc_count()))
