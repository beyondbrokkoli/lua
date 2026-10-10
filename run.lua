#!/usr/bin/env lua
-- run.lua — the bench: build + clippy, the corpus, lock byte-identity.
-- Workflow, pin grammar, and verification signatures: README.md.
dofile("conf.lua")

local function usage()
    print([[
Usage: lua run.lua [run [substr]...] | probe <file.lua> [--fmt]
  (no args)   run the whole corpus
  run         run the whole corpus
  run SUB...  run only cases whose name contains any SUB
  probe FILE  run one file, archive out.ll + plates to target/probe/
  --fmt       also gate on cargo fmt --check (rustfmt is opt-in by
              design: cargo fmt rewrites source files mid-session and
              forces read-before-edit churn in agentic flows; milestone
              relocks and review passes should carry it)

The bench always includes the EXE section: every case is re-linked
with --exe and run as ./glm_out with its ARGS words — the executable
carries its own boundary host.]])
end

local ARGS = {...}
local mode = ARGS[1] or "run"
if mode ~= "run" and mode ~= "probe" then
    usage()
    io.stderr:write("\nunknown argument: " .. ARGS[1] .. "\n")
    os.exit(1)
end

-- The rustfmt flag, parsed anywhere after the mode.
local FMT = true
for i = 2, #ARGS do
    if ARGS[i] == "--fmt" then FMT = true end
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
-- Only enforced when the case carries at least one EXPECT pin; a pinless
-- positive case still gets compile/run/lock coverage. Returns nil on
-- match, else the first divergence.
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
    report_fail("lock", name, "BYTE DRIFT vs " .. LOCK_DIR .. " — relock only at a milestone:\n" .. cap(d, 60))
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
    if FMT then
        local res = os.execute("cargo fmt --check > /dev/null 2> " .. BERR)
        if not (res == 0 or res == true) then
            print(c(RED, "  ✗ cargo fmt --check failed (run `cargo fmt` and re-stage):") .. "\n" .. read_file(BERR))
            os.exit(1)
        end
        print("  ✓ rustfmt clean (cargo fmt --check)")
    else
        report_notice("rustfmt gate skipped — pass --fmt to enforce it")
    end
    local res = os.execute("cargo clippy --release --quiet -- -D warnings > /dev/null 2> " .. BERR)
    if not (res == 0 or res == true) then
        print(c(RED, "  ✗ cargo clippy failed (-D warnings):") .. "\n" .. read_file(BERR))
        os.exit(1)
    end
    print("  ✓ clippy clean (-D warnings)")
end

-- probe: one arbitrary file through the same capture pipeline, archiving
-- the products (out.ll and both trace plates) under target/probe/ so
-- they survive the next invocation.
if mode == "probe" then
    local file = ARGS[2]
    if not file then usage() os.exit(1) end
    local ok = run_case(file, nil, true)
    local compiled = compile_succeeded()
    local name = file:gsub("%.lua$", ""):gsub("(.*/)", "")
    local dir = "target/probe"
    os.execute("mkdir -p " .. dir)
    if compiled then copy_file("out.ll", dir .. "/" .. name .. ".ll") end
    copy_file(".glm_trace.bin", dir .. "/" .. name .. ".bin")
    copy_file(".glm_rt_trace.bin", dir .. "/" .. name .. ".rt.bin")
    print("\n== PROBE " .. file .. " ==")
    print("  compile: " .. (compiled and "ok" or ("FAILED — " .. first_error(ERR))))
    print("  exit   : " .. (ok and "0" or "nonzero"))
    local out = script_stdout()
    if out ~= "" then print("  stdout :\n" .. out) end
    print("  plates : " .. dir .. "/" .. name .. ".bin (.rt.bin sidecar"
        .. (compiled and ", .ll IR" or "") .. ") — decode: python3 plate.py <plate>")
    os.exit(0)
end

-- run: optional substring filters over case names (--fmt is not one)
local filters = {}
for i = 2, #ARGS do
    if ARGS[i] ~= "--fmt" then table.insert(filters, ARGS[i]) end
end
local function selected(name)
    if #filters == 0 then return true end
    for _, s in ipairs(filters) do
        if name:find(s, 1, true) then return true end
    end
    return false
end

-- corpus
local corpus = scan_corpus()
local total = #corpus.positive + #corpus.negative + #corpus.panic
if #filters > 0 then
    local function keep(list)
        local t = {}
        for _, n in ipairs(list) do if selected(n) then table.insert(t, n) end end
        return t
    end
    corpus.positive = keep(corpus.positive)
    corpus.negative = keep(corpus.negative)
    corpus.panic = keep(corpus.panic)
end
print("\n== CORPUS (self-classified from pin comments) ==")
for _, b in ipairs(corpus.bad) do report_fail("corpus", b[1], b[2]) end
for _, o in ipairs(corpus.odd) do
    report_notice(o[1] .. " — pin prefixes outside the grammar: " .. o[2])
end
local matched = #corpus.positive + #corpus.negative + #corpus.panic
print(string.format("  %d positive / %d negative / %d panic%s",
    #corpus.positive, #corpus.negative, #corpus.panic,
    #filters > 0 and string.format("  (filtered: %d/%d by '%s')", matched, total, table.concat(filters, "' '")) or ""))

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

-- exe: the standalone twin of every compiled case — the same corpus
-- through `glm --exe`, then ./glm_out itself with the case's ARGS
-- words (the executable carries its own boundary host: the pinned
-- element kind is embedded in @main, the runtime parses the words at
-- exec time). Positive cases must run clean and match their EXPECT
-- pins; panic cases must die the same scripted death inside the
-- executable.
print("\n== EXE (standalone twin: --exe compile + ./glm_out) ==")
local exe_passed = 0
do
    local function exe_twin(name, must_die)
        if not run_case_exe(CASES_DIR .. "/" .. name) then
            report_fail("exe", name, "standalone compile failed: " .. first_error(ERR))
            return
        end
        if not compile_succeeded_exe() then
            report_fail("exe", name, "standalone compile failed: " .. first_error(ERR))
            return
        end
        local pins = corpus.pins[name]
        local argstr = pins.args and (" " .. pins.args) or ""
        local ok = run_exe_with(argstr)
        if must_die then
            if ok then
                report_fail("exe", name, "expected the scripted death inside the executable, it ran clean")
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
                    report_fail("exe", name, bad)
                else
                    exe_passed = exe_passed + 1
                    report_pass(name .. " [exe]", "[" .. (err:match("([^\r\n]+)") or "died") .. "]")
                end
            end
        else
            if not ok then
                report_fail("exe", name, "executable exited nonzero: " .. first_error(ERR))
            else
                local bad
                if #pins.expect > 0 then
                    bad = expect_mismatch(script_stdout(), pins.expect)
                end
                if bad then
                    report_fail("exe", name, "EXPECT mismatch (exe) — " .. bad)
                else
                    exe_passed = exe_passed + 1
                    report_pass(name .. " [exe]")
                end
            end
        end
    end
    for _, name in ipairs(corpus.positive) do exe_twin(name, false) end
    for _, name in ipairs(corpus.panic) do exe_twin(name, true) end
end

-- lock wrap (orphan scan is whole-corpus; a filtered run sees strays
-- everywhere and would cry wolf)
print("\n== LOCK (byte-identity vs " .. LOCK_DIR .. "/) ==")
if #filters == 0 then
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
end
print(string.format("  lock: %d identical / %d drifted / %d errored / %d missing",
    lock_identical, lock_drifted, lock_errored, lock_missing))

-- summary
print("\n== RESULT ==")
print(string.format("  positive : %d/%d", pos_passed, #corpus.positive))
print(string.format("  negative : %d/%d", neg_passed, #corpus.negative))
print(string.format("  panic    : %d/%d", pan_passed, #corpus.panic))
if #filters == 0 then
    print(string.format("  exe      : %d twin(s) clean", exe_passed))
end
if #notices > 0 then print(c(YELLOW, string.format("  notices  : %d", #notices))) end
if #failed > 0 then
    print(c(RED, string.format("  FAILURES : %d", #failed)))
    for _, f in ipairs(failed) do print(string.format("   [%s] %s", f.section, f.name)) end
    os.exit(1)
end
if #filters > 0 then
    print(c(YELLOW, string.format("  GREEN (filtered %d/%d) — a full `lua run.lua run` is still owed", matched, total)))
else
    print(c(GREEN, "  ALL GREEN — invariants intact."))
end
