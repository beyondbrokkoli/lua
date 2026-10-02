-- conf.lua

-- The built compiler. ONE invocation compiles and runs: it writes
-- out.ll, links ./libglm_out.so, then dlopens the .so as the boundary
-- host and executes the script in-process. The compiled product is a
-- shared object, not an executable.
BIN = "target/release/glm"

-- Scratch captures of one invocation's streams (all under target/ —
-- gitignored): OUT holds the invocation's stdout (the script's output
-- plus the compiler's one 'Success!' note), ERR its stderr (runtime
-- deaths, the host's boundary report, trace signals). BERR captures
-- build tooling (cargo/clippy) stderr for the BUILD section.
OUT = "target/run_out.txt"
ERR = "target/run_err.txt"
BERR = "target/build_err.txt"

-- Lock baselines and drift captures live OUTSIDE the repo, so the IR
-- boilerplate stays out of git. Lua expands no `~` — the harness does
-- it itself, once, at load.
local function expand_home(p)
    return (p:gsub("^~", os.getenv("HOME") or "."))
end

CASES_DIR = "cases"
LOCK_DIR = "lock"
DIFF_DIR = "diff"

-- The sys_alloc_count contract under the boundary: the host builds the
-- `arg` table ONLY when the script names it (references_arg in
-- main.rs), and that header is live for the script's whole run — an
-- arg-using case's counts ride +1 above the executable-world floors.
-- A script that never names `arg` is handed a null boundary and reads
-- the base floors (0 after the ledger drops), invoked with args or not.

os.execute("mkdir -p target")

-- Reads an entire file into one string ("" when missing).
function read_file(path)
    local f = io.open(path, "r")
    if not f then return "" end
    local content = f:read("*a")
    f:close()
    return content
end

-- The pin grammar. A case carries its own classification in magic
-- comments — the directory IS the listing:
--   -- EXPECT: <line>            one expected stdout line, in order.
--                                OPTIONAL: a case without EXPECT pins
--                                still gets compile/run/lock coverage.
--   -- EXPECT_BUILD_FAIL: <text> the invocation's stderr must contain
--                                this text; the pin's presence makes
--                                the case negative (build must fail)
--   -- EXPECT_PANIC: <text>      the script must die with this text on
--                                stderr; the pin's presence makes the
--                                case a panic case (build must pass)
--   -- ARGS: <ints...>           integer boundary arguments (they
--                                become the script's `arg` table cells)
-- Any other -- EXPECT_*: prefix is collected as unknown so a prefix
-- outside the grammar rots loudly instead of silently checking
-- nothing.
function parse_pins(content)
    local pins = { expect = {}, build_fail = {}, panic = {}, unknown = {} }
    for prefix, rest in content:gmatch("%-%-%s*(EXPECT[_A-Z]*):%s*([^\r\n]+)") do
        if prefix == "EXPECT" then
            table.insert(pins.expect, rest)
        elseif prefix == "EXPECT_BUILD_FAIL" then
            table.insert(pins.build_fail, rest)
        elseif prefix == "EXPECT_PANIC" then
            table.insert(pins.panic, rest)
        else
            table.insert(pins.unknown, prefix)
        end
    end
    -- One ARGS line per case; every word must parse as an integer at
    -- the boundary (the host enforces it) — the harness passes the
    -- line through verbatim.
    pins.args = content:match("%-%-%s*ARGS:%s*([^\r\n]+)")
    return pins
end

-- Classifies every CASES_DIR/*.lua by its own pins. Returns:
--   positive / negative / panic : file-name arrays (ls order)
--   pins                        : per-case parse result, keyed by name
--   bad                         : { {name, why} } classification
--                                 conflicts (hard failures)
--   odd                         : { {name, prefixes} } unknown pin
--                                 prefixes (notices)
function scan_corpus()
    local corpus = { positive = {}, negative = {}, panic = {},
                     bad = {}, odd = {}, pins = {} }
    local p = io.popen("ls " .. CASES_DIR .. " 2>/dev/null")
    for name in p:lines() do
        if name:sub(-4) == ".lua" then
            local pins = parse_pins(read_file(CASES_DIR .. "/" .. name))
            corpus.pins[name] = pins
            local is_neg, is_pan = #pins.build_fail > 0, #pins.panic > 0
            -- EXPECT + EXPECT_PANIC is legal (stdout printed before the
            -- death); EXPECT + EXPECT_BUILD_FAIL is not (nothing runs).
            local why
            if is_neg and is_pan then
                why = "carries both EXPECT_BUILD_FAIL and EXPECT_PANIC pins"
            elseif is_neg and #pins.expect > 0 then
                why = "negative (EXPECT_BUILD_FAIL) but also carries EXPECT pins"
            end
            if why then
                table.insert(corpus.bad, { name, why })
            elseif is_neg then
                table.insert(corpus.negative, name)
            elseif is_pan then
                table.insert(corpus.panic, name)
            else
                table.insert(corpus.positive, name)
            end
            if #pins.unknown > 0 then
                table.insert(corpus.odd, { name, table.concat(pins.unknown, ", ") })
            end
        end
    end
    p:close()
    return corpus
end

-- Invokes one case with the built glm. The single invocation compiles
-- (out.ll + libglm_out.so land in the CWD) and executes the script —
-- the compiler process is the boundary host. Stdout lands in OUT,
-- stderr in ERR. True iff the process exited 0; a panic case exits
-- nonzero even though its build succeeded — ask compile_succeeded()
-- to tell the two deaths apart.
function run_case(src, args, with_backtrace)
    local bt = with_backtrace and "RUST_BACKTRACE=1 " or ""
    local argstr = args and (" " .. args) or ""
    local res = os.execute(string.format(
        "%stimeout 120 %s '%s'%s > %s 2> %s", bt, BIN, src, argstr, OUT, ERR))
    -- Lua 5.1 answers with a status number (0 = ok), 5.2+ with a
    -- boolean; 5.4/5.5 add "exit"/"signal" and the code, and both of
    -- those mean not ok. true and 0 are the only success shapes.
    return res == true or res == 0
end

-- True iff the LAST invocation compiled: the compiler's success note
-- is the one marker printed after clang links and before the boundary
-- run, so a script that dies at runtime still carries it while a
-- build failure never does.
function compile_succeeded()
    return read_file(OUT):find("Success! Shared library written", 1, true) ~= nil
end

-- The script's own stdout: OUT minus the compiler's one success note
-- (the host's boundary report goes to stderr and never touches the
-- EXPECT pins).
function script_stdout()
    local lines = {}
    for line in read_file(OUT):gmatch("[^\r\n]+") do
        if not line:find("Success! Shared library written", 1, true) then
            table.insert(lines, line)
        end
    end
    return table.concat(lines, "\n")
end

-- First useful line of a failing invocation: front diagnostics are
-- printed directly ('Type Error: ...', 'Lifetime Error: ...'), hard
-- compiler crashes are Rust panics (message on the line after
-- 'panicked at'). Falls back to the first nonempty line.
function first_error(path)
    local s = read_file(path)
    local msg = s:match("panicked at.-\n%s*([^\n]+)")
    if not msg then
        for line in s:gmatch("[^\r\n]+") do
            if line:find("Error", 1, true) then msg = line break end
        end
    end
    return msg
        or s:match("([^\r\n]+)")
        or "(no error output)"
end

-- Must be called right after a confirmed compile of THIS case — glm
-- overwrites out.ll (and libglm_out.so) in the CWD on every invocation.
function captured_ir()
    local code = read_file("out.ll")
    if code ~= "" then return code end
    return nil
end
