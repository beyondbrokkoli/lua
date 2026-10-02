#!/usr/bin/env lua
dofile("conf.lua")

local function usage()
    print([[
Runs cargo build + clippy, then the whole
corpus, then byte-identity of every case's out.ll against existing
IR baselines in ]] .. LOCK_DIR .. [[.
reset.lua is the only writer.

Every case classifies itself with pin
comments; the directory is the listing.
  -- EXPECT: <line>            expected stdout line, in order
  -- EXPECT_BUILD_FAIL: <text> compile must fail with this text
  -- EXPECT_PANIC: <text>      the script must panic with this text
  -- ARGS: <ints...>           integer boundary arguments

One invocation compiles AND runs (the product is ./libglm_out.so and
the compiler process is its boundary host): a positive case must exit
0, a negative case must die in the front, a panic case must link and
then die in the script.

BYTE DRIFT dumps each drifted IR diff.
Relock with (lua reset.lua run).

Usage: lua run.lua run]])
end

local ARGS = {...}
if #ARGS == 0 then usage() os.exit(0) end
if ARGS[1] ~= "run" then
    usage()
    io.stderr:write("\nunknown argument: " .. ARGS[1] .. "\n")
    os.exit(1)
end

-- reporting
local GREEN, RED, YELLOW = "\27[32m", "\27[31m", "\27[33m"
local function c(code, s) return code .. s .. "\27[0m" end

local failed, notices = {}, {}
local function report_fail(section, name, reason)
    table.insert(failed, { section = section, name = name })
    print(string.format("  %s %s — %s", c(RED, "✗"), name, reason))
end
local function report_pass(name, extra)
    print(string.format("  %s %s%s", c(GREEN, "✓"), name, extra and ("   " .. extra) or ""))
end
local function report_notice(text)
    table.insert(notices, text)
    print(string.format("  %s %s", c(YELLOW, "!"), text))
end

local function cap(s, n)
    local lines = {}
    for line in s:gmatch("[^\r\n]+") do table.insert(lines, line) end
    if #lines <= n then return s end
    return table.concat(lines, "\n", 1, n)
        .. string.format("\n       ... (%d more diff lines — full diff: %s vs %s)", #lines - n, DIFF_DIR, LOCK_DIR)
end

local function split_lines(s)
    local t = {}
    for line in s:gmatch("[^\r\n]+") do table.insert(t, line) end
    return t
end

-- EXPECT pins: whole-stdout equality — lines in order, nothing extra.
-- Only enforced when the case carries at least one EXPECT pin; a
-- pinless positive case owes nothing about its stdout (it still gets
-- compile/run/lock coverage — the pin grammar's OPTIONAL promise).
-- The compiler's own success note is already filtered out
-- (script_stdout); what remains must be exactly the script's output.
-- Returns nil on match, else the first divergence.
local function expect_mismatch(out, pins)
    local got = split_lines(out)
    for i = 1, math.max(#pins, #got) do
        local w, g = pins[i], got[i]
        if w == nil then
            return string.format("line %d: unexpected extra output '%s' (want %d lines, got %d)",
                i, g, #pins, #got)
        elseif g == nil then
            return string.format("line %d: want '%s', got nothing (want %d lines, got %d)",
                i, w, #pins, #got)
        elseif w ~= g then
            return string.format("line %d: want '%s', got '%s'", i, w, g)
        end
    end
    return nil
end

-- lock
os.execute("rm -rf '" .. DIFF_DIR .. "'")
os.execute("mkdir -p '" .. DIFF_DIR .. "'")
os.execute("touch " .. DIFF_DIR .. "/.gitkeep")

local lock_identical, lock_drifted, lock_errored, lock_missing = 0, 0, 0, 0

local function lock_name(name) return name:gsub("%.lua$", "") .. ".ll" end

-- Called right after a confirmed compile of THIS case. Returns the tag
-- for the pass line, or nil once the failure is already reported.
local function diff_against_lock(name)
    local lock = LOCK_DIR .. "/" .. lock_name(name)
    if not io.open(lock, "r") then
        lock_missing = lock_missing + 1
        report_notice(name .. " — no lock baseline (new case? relock at the next milestone)")
        return "(no lock)"
    end
    local code = captured_ir()
    if not code then
        lock_errored = lock_errored + 1
        report_fail("lock", name, "compile succeeded but out.ll is empty/missing")
        return nil
    end
    local df = DIFF_DIR .. "/" .. lock_name(name)
    local f = io.open(df, "w")
    f:write(code)
    f:close()
    local p = io.popen(string.format("diff -u '%s' '%s'", lock, df))
    local d = p:read("*a")
    p:close()
    if d == "" then
        lock_identical = lock_identical + 1
        return "lock ✓"
    end
    lock_drifted = lock_drifted + 1
    report_fail("lock", name, "BYTE DRIFT vs " .. LOCK_DIR .. " — user verifies drift by hand, relock only at a milestone:\n" .. cap(d, 60))
    return "lock ✗"
end

-- build
print("== BUILD ==")
do
    local res = os.execute("cargo build --release --quiet > /dev/null 2> " .. BERR)
    if not (res == 0 or res == true) then
        print(c(RED, "  ✗ cargo build failed:") .. "\n" .. read_file(BERR))
        os.exit(1)
    end
    print("  ✓ compiler fresh (target/release/glm)")
    local res = os.execute("cargo clippy --release --quiet -- -D warnings > /dev/null 2> " .. BERR)
    if not (res == 0 or res == true) then
        print(c(RED, "  ✗ cargo clippy failed (-D warnings):") .. "\n" .. read_file(BERR))
        os.exit(1)
    end
    print("  ✓ clippy clean (-D warnings)")
end

-- corpus
local corpus = scan_corpus()
print("\n== CORPUS (self-classified from pin comments) ==")
for _, b in ipairs(corpus.bad) do report_fail("corpus", b[1], b[2]) end
for _, o in ipairs(corpus.odd) do
    report_notice(o[1] .. " — pin prefixes outside the grammar: " .. o[2])
end
print(string.format("  %d positive / %d negative / %d panic",
    #corpus.positive, #corpus.negative, #corpus.panic))

-- positive
print("\n== POSITIVE (compile, run, optional EXPECT, lock) ==")
local pos_passed = 0
for _, name in ipairs(corpus.positive) do
    local pins = corpus.pins[name]
    if not run_case(CASES_DIR .. "/" .. name, pins.args, false) then
        local why = compile_succeeded() and "run failed" or "compile failed"
        report_fail("positive", name, why .. ": " .. first_error(ERR))
    else
        local lock_tag = diff_against_lock(name)
        local bad
        if #pins.expect > 0 then
            bad = expect_mismatch(script_stdout(), pins.expect)
        end
        if bad then
            report_fail("positive", name, "EXPECT mismatch — " .. bad)
        else
            pos_passed = pos_passed + 1
            report_pass(name, lock_tag)
        end
    end
end

-- negative
print("\n== NEGATIVE (compile must fail) ==")
local neg_passed = 0
for _, name in ipairs(corpus.negative) do
    local pins = corpus.pins[name]
    run_case(CASES_DIR .. "/" .. name, pins.args, false)
    if compile_succeeded() then
        report_fail("negative", name, "expected compile failure, but it SUCCEEDED — the guard is gone")
    else
        local berr = read_file(ERR)
        local bad
        for _, msg in ipairs(pins.build_fail) do
            if not berr:find(msg, 1, true) then bad = "missing expected error: " .. msg break end
        end
        if bad then
            report_fail("negative", name, bad)
        else
            neg_passed = neg_passed + 1
            report_pass(name, "[" .. first_error(ERR) .. "]")
        end
    end
end

-- panic
print("\n== PANIC (compile ok, run must panic) ==")
local pan_passed = 0
for _, name in ipairs(corpus.panic) do
    local pins = corpus.pins[name]
    local died = not run_case(CASES_DIR .. "/" .. name, pins.args, true)
    if not compile_succeeded() then
        report_fail("panic", name, "expected compile to succeed: " .. first_error(ERR))
    else
        local lock_tag = diff_against_lock(name)
        if not died then
            report_fail("panic", name, "expected a runtime death, but it ran clean")
        else
            local err = read_file(ERR)
            local bad
            for _, msg in ipairs(pins.panic) do
                if not err:find(msg, 1, true) then bad = "missing expected stderr: " .. msg break end
            end
            if not bad and #pins.expect > 0 then
                bad = expect_mismatch(script_stdout(), pins.expect)
            end
            if bad then
                report_fail("panic", name, bad)
            else
                pan_passed = pan_passed + 1
                report_pass(name, lock_tag or ("[" .. (err:match("([^\r\n]+)") or "died") .. "]"))
            end
        end
    end
end

-- lock wrap
print("\n== LOCK (byte-identity vs " .. LOCK_DIR .. "/) ==")
do
    local expected = {}
    for _, name in ipairs(corpus.positive) do expected[lock_name(name)] = true end
    for _, name in ipairs(corpus.panic) do expected[lock_name(name)] = true end
    local p = io.popen("ls " .. LOCK_DIR .. " 2>/dev/null")
    for line in p:lines() do
        if not expected[line] then
            report_notice("stray lock file (case deleted? lockdown removes orphans): " .. LOCK_DIR .. "/" .. line)
        end
    end
    p:close()
    print(string.format("  lock: %d identical / %d drifted / %d errored / %d missing",
        lock_identical, lock_drifted, lock_errored, lock_missing))
end

-- summary
print("\n== RESULT ==")
print(string.format("  positive : %d/%d", pos_passed, #corpus.positive))
print(string.format("  negative : %d/%d", neg_passed, #corpus.negative))
print(string.format("  panic    : %d/%d", pan_passed, #corpus.panic))
if #notices > 0 then print(c(YELLOW, string.format("  notices  : %d", #notices))) end
if #failed > 0 then
    print(c(RED, string.format("  FAILURES : %d", #failed)))
    for _, f in ipairs(failed) do print(string.format("   [%s] %s", f.section, f.name)) end
    os.exit(1)
end
print(c(GREEN, "  ALL GREEN — invariants intact."))
