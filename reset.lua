#!/usr/bin/env lua
-- reset.lua — overwrite IR in lock
dofile("conf.lua")

local function usage()
    print([[
Rewrites every buildable case's IR baseline in ]] .. LOCK_DIR .. [[/.

Usage: lua reset.lua]])
end

local ARGS = {...}
if ARGS[1] and ARGS[1] ~= "run" then
    usage()
    io.stderr:write("\nunknown argument: " .. ARGS[1] .. "\n")
    os.exit(1)
end

do
    local res = os.execute("cargo build --release --quiet > /dev/null 2> " .. BERR)
    if not (res == 0 or res == true) then
        print("\27[31mcargo build failed — refusing to lock:\27[0m\n" .. read_file(BERR))
        os.exit(1)
    end
end

os.execute("mkdir -p '" .. LOCK_DIR .. "'")

local corpus = scan_corpus()
local lockables = {}
for _, n in ipairs(corpus.positive) do table.insert(lockables, n) end
for _, n in ipairs(corpus.panic) do table.insert(lockables, n) end
for _, b in ipairs(corpus.bad) do
    print(string.format("\27[31m✗\27[0m %s — classification conflict, nothing locked: %s", b[1], b[2]))
    os.exit(1)
end

-- No ARGS are passed: the boundary table crosses at runtime, so the IR
-- never depends on it.
local failed = {}
for _, filename in ipairs(lockables) do
    -- One invocation compiles and runs; a panic case exits nonzero on
    -- its scripted death — only the build's success gates the lock.
    run_case(CASES_DIR .. "/" .. filename)
    if not compile_succeeded() then
        print(string.format("\27[31m✗\27[0m %s — compile failed: %s", filename, first_error(ERR)))
        table.insert(failed, filename)
    else
        local code = read_file("out.ll")
        if code == "" then
            print(string.format("\27[31m✗\27[0m %s — compile succeeded but out.ll is empty", filename))
            table.insert(failed, filename)
        else
            local dest = LOCK_DIR .. "/" .. filename:gsub("%.lua$", "") .. ".ll"
            local f = io.open(dest, "w")
            f:write(code)
            f:close()
            print(string.format("\27[32m✓\27[0m locked %s", dest))
        end
    end
end

if #failed > 0 then
    print(string.format("\27[31mLOCKDOWN INCOMPLETE\27[0m — %d/%d failed: %s",
        #failed, #lockables, table.concat(failed, ", ")))
    print("Some lock files were already overwritten — the boss will flag")
    print("the mixed state. Fix the failures and rerun.")
    os.exit(1)
end

-- Orphaned locks (their case was deleted/renamed) outlive their case
-- only until the next fully successful lockdown.
do
    local expected = {}
    for _, n in ipairs(lockables) do expected[n:gsub("%.lua$", "") .. ".ll"] = true end
    local p = io.popen("ls " .. LOCK_DIR .. " 2>/dev/null")
    for line in p:lines() do
        if not expected[line] then
            os.execute("rm -f " .. LOCK_DIR .. "/" .. line)
            print("removed orphan lock: " .. LOCK_DIR .. "/" .. line)
        end
    end
    p:close()
end

io.write(string.format("== milestone locked: %d tests -> %s/ ==\n", #lockables, LOCK_DIR))
do
    local p = io.popen("git rev-parse --short HEAD 2>/dev/null")
    local h = p:read("*l") or "?"
    p:close()
    io.write(string.format("  at commit %s, %s\n", h, os.date("locked %Y-%m-%d %H:%M:%S")))
end
