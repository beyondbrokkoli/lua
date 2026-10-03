-- conf.lua — shared harness library. The bench contract (pin grammar,
-- boundary args, verification signatures) has its one home in README.md.

-- The built compiler. ONE invocation compiles and runs: it writes out.ll,
-- links ./libglm_out.so, then dlopens the .so as the boundary host and
-- executes the script in-process.
BIN = "target/release/glm"

-- Scratch captures of one invocation's streams (all under target/):
-- OUT stdout (the script's output plus the compiler's one success note),
-- ERR stderr (diagnostics, runtime deaths, the boundary report), BERR
-- build tooling (cargo/clippy).
OUT = "target/run_out.txt"
ERR = "target/run_err.txt"
BERR = "target/build_err.txt"

CASES_DIR = "cases"
LOCK_DIR = "lock"
DIFF_DIR = "diff"

os.execute("mkdir -p target")

function read_file(path)
    local f = io.open(path, "r")
    if not f then return "" end
    local content = f:read("*a")
    f:close()
    return content
end

function copy_file(src, dst)
    local i = io.open(src, "rb")
    if not i then return false end
    local data = i:read("*a")
    i:close()
    local o = io.open(dst, "wb")
    if not o then return false end
    o:write(data)
    o:close()
    return true
end

-- Pin grammar and classification rules: README.md. Unknown EXPECT_*
-- prefixes are collected so a prefix outside the grammar rots loudly
-- instead of silently checking nothing.
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
    -- One ARGS line per case, passed through verbatim (the host — or
    -- the executable's own host — parses the words against the
    -- usage-pinned cell type at the boundary).
    pins.args = content:match("%-%-%s*ARGS:%s*([^\r\n]+)")
    return pins
end

-- Classifies every CASES_DIR/*.lua by its own pins. Returns
-- positive/negative/panic name arrays, pins keyed by name, bad
-- (classification conflicts, hard failures), odd (unknown prefixes).
function scan_corpus()
    local corpus = { positive = {}, negative = {}, panic = {},
                     bad = {}, odd = {}, pins = {} }
    local p = io.popen("ls " .. CASES_DIR .. " 2>/dev/null")
    for name in p:lines() do
        if name:sub(-4) == ".lua" then
            local pins = parse_pins(read_file(CASES_DIR .. "/" .. name))
            corpus.pins[name] = pins
            local is_neg, is_pan = #pins.build_fail > 0, #pins.panic > 0
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

-- Invokes one case; true iff it exited 0. A panic case exits nonzero
-- even though its build succeeded — compile_succeeded() tells the two
-- deaths apart.
function run_case(src, args, with_backtrace)
    local bt = with_backtrace and "RUST_BACKTRACE=1 " or ""
    local argstr = args and (" " .. args) or ""
    local res = os.execute(string.format(
        "%stimeout 120 %s '%s'%s > %s 2> %s", bt, BIN, src, argstr, OUT, ERR))
    -- Lua 5.1 answers a status number, 5.2+ a boolean; 5.4/5.5 add
    -- "exit"/"signal" + code, both meaning not ok.
    return res == true or res == 0
end

-- True iff the LAST invocation compiled: the success note is the one
-- marker printed after clang links and before the boundary run (the
-- .so and exe spellings both start with 'Success!').
function compile_succeeded()
    return read_file(OUT):find("Success! Shared library written", 1, true) ~= nil
end

-- The --exe twin: the success note names the executable instead.
function compile_succeeded_exe()
    return read_file(OUT):find("Success! executable written", 1, true) ~= nil
end

-- Compile one case standalone (no boundary words). True iff the exe
-- linked; a refusal (the script names `arg`, or words were passed) is
-- reported through the captured streams like any other failure.
function run_case_exe(src)
    local res = os.execute(string.format(
        "timeout 120 %s --exe '%s' > %s 2> %s", BIN, src, OUT, ERR))
    return res == true or res == 0
end

-- Run the linked ./glm_out once with the case's boundary words,
-- capturing both streams.
function run_exe_with(argstr)
    local res = os.execute(string.format(
        "timeout 120 ./glm_out%s > %s 2> %s", argstr, OUT, ERR))
    return res == true or res == 0
end

-- The script's own stdout: OUT minus the success note (the boundary
-- report goes to stderr and never touches EXPECT pins).
function script_stdout()
    local lines = {}
    for line in read_file(OUT):gmatch("[^\r\n]+") do
        if not line:find("Success! Shared library written", 1, true)
            and not line:find("Success! executable written", 1, true) then
            table.insert(lines, line)
        end
    end
    return table.concat(lines, "\n")
end

-- First useful line of a failing invocation: front diagnostics print
-- directly, Rust panics put the message on the line after 'panicked at'.
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

-- glm overwrites out.ll (and libglm_out.so) in the CWD on every
-- invocation — call right after a confirmed compile of THIS case.
function captured_ir()
    local code = read_file("out.ll")
    if code ~= "" then return code end
    return nil
end
