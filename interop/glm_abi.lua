-- glm_abi.lua v2 — LuaJIT FFI bridge for glm-compiled modules
--
-- Canonical home: lua/interop/glm_abi.lua (the compiler repo).
--
-- v2 changes (the "dlopen fix" milestone):
--   * Library handles are cached per path — one dlopen, one symbol
--     resolution, one .init_array run per module per process.
--     LuaJIT's ffi.load has NO unload counterpart (it never dlcloses),
--     so the §6.1 multi-tenant dlclose race cannot occur; the cache
--     exists to avoid repeat dlopen+symbol-resolution cost and to
--     share one module identity across callers.
--   * Arg fill uses the compiler's own fast-fill contract: one
--     glm_tbl_reserve(t, n) (grows the span, commits border = n —
--     exactly what the emitted fill loops rely on), then a single
--     ffi.copy of the packed cells into the span. No i128 ever
--     crosses the FFI register seam: Any cells are packed in Lua as
--     (payload low 64, tag high 64) — any_pack's layout in rt.rs.
--   * Decode reads the dense span directly when the header says
--     Dense with no overflow map (mode@32 == 0, sparse_map@40 == nil)
--     and falls back to per-cell glm_tbl_get otherwise (Sparse mode
--     or far keys present). The header is read through a mirrored
--     cdef struct, ABI-pinned by test_abi_layout_offsets in rt.rs.
--   * Kind-aware decoding: Float results decode as doubles, String
--     results as interned-pointer strings (v1 decoded every esize-8
--     table as integers — a latent bug for Float/String modules).
--
-- USAGE:
--   local abi  = require "glm_abi"
--   local mod  = abi.module("libglm_out.so")     -- handle cached per path
--   local out  = mod:run({ 5, 10, 15 })          -- 0-indexed result table
--   -- or the one-shot form (also cached):
--   local out2 = abi.run("libglm_out.so", { 5, 10, 15 })

local ffi = require "ffi"

ffi.cdef [[
    // Opaque handle for the exported faces
    typedef struct GlmTable GlmTable;

    // Header mirror — offsets ABI-pinned in src/rt.rs (56 bytes total):
    //   data@0 len@8 reserve@16 esize@24 mode@32 contains_tables@33
    //   sparse_map@40 border@48
    typedef struct GlmTableHeader {
        void *data;
        long long len;
        unsigned long long reserve;
        unsigned long long esize;
        unsigned char mode;            // TableMode: 0 Dense, 1 Sparse
        unsigned char contains_tables;
        void *sparse_map;              // far-key overflow map, or NULL
        long long border;              // #t: one past highest stored
    } GlmTableHeader;

    // f64 <-> u64 bit reinterpretation (never tonumber through this path:
    // a Lua number is a double — u64 payload bits above 2^53 would lose
    // precision, which once made 3.14 decode as 3.1399999999998727)
    typedef union { double d; unsigned long long u; } glm_f64_bits;

    // Entry point
    GlmTable *glm_exec(GlmTable *args);
    int       glm_arg_kind(void);

    // Construction / mutation
    GlmTable *glm_tbl_new(unsigned int esize, unsigned char flags);
    void      glm_tbl_reserve(GlmTable *t, long n);
    void      glm_tbl_grow(GlmTable *t, long index);
    void      glm_tbl_set(GlmTable *t, long index, const void *val);

    // Read
    void      glm_tbl_get(GlmTable *t, long index, void *dst,
                          unsigned int esize);

    // Info / free
    long      glm_tbl_len(GlmTable *t);
    void      glm_tbl_free(GlmTable *t);

    // The module's immortal intern pool — one address per distinct
    // content, so pointer equality IS content equality in Any cells.
    const unsigned char *glm_str_intern(const unsigned char *s,
                                         unsigned long len);

    // Live table-header ledger: +1 per glm_tbl_new, -1 per free. A
    // host can self-check leak-freedom around any call sequence.
    long long sys_alloc_count(void);
]]

-- Pin the mirror against drift the moment this file loads.
assert(ffi.sizeof("GlmTableHeader") == 56,
       "GlmTableHeader mirror drifted from the rt.rs ABI pin")

-- Constants (match rt.rs GLM_ARG_*)
local GLM_ARG_NONE    = -1
local GLM_ARG_INT     =  0
local GLM_ARG_FLOAT   =  1
local GLM_ARG_BOOL    =  2
local GLM_ARG_STRING  =  3
local GLM_ARG_ANY     =  4

local KIND_ESIZE = {
    [GLM_ARG_INT]    = 8;
    [GLM_ARG_FLOAT]  = 8;
    [GLM_ARG_BOOL]   = 1;
    [GLM_ARG_STRING] = 8;
    [GLM_ARG_ANY]    = 16;
}

local KIND_NAME = {
    [GLM_ARG_NONE]    = "None";
    [GLM_ARG_INT]     = "Integer";
    [GLM_ARG_FLOAT]   = "Float";
    [GLM_ARG_BOOL]    = "Boolean";
    [GLM_ARG_STRING]  = "String";
    [GLM_ARG_ANY]     = "Any";
}

-- Handle cache — one ffi.load per path per process

-- cache[path] = { lib = <cdata>, kind = <int> }
-- ffi.load never unloads (no dlclose in LuaJIT's FFI), so entries
-- live for the process; drop_cache() only drops references.
local cache = {}

local function load_cached(path)
    local entry = cache[path]
    if entry then return entry.lib, entry.kind end
    local ok, lib = pcall(ffi.load, path)
    if not ok then return nil, nil, lib end
    local kind = lib.glm_arg_kind()
    cache[path] = { lib = lib, kind = kind }
    return lib, kind
end

-- Arg building — reserve + one bulk copy (the fast-fill contract)

-- Packing buffers are cached on the module object and double to fit.
-- Above SCRATCH_MAX cells, a call rides a transient buffer instead —
-- a one-off 10M-element batch must not pin ~80MB on the module until
-- the module object is collected.
local SCRATCH_MAX = 2^20

local function get_buf(scratch, key, ctype, cells)
    local vla = ctype .. "[?]"     -- VLA syntax: without [?], ffi.new's
                                   -- second arg initializes a SCALAR
    if cells > SCRATCH_MAX then
        return ffi.new(vla, cells)            -- transient: let GC reclaim
    end
    local cap = scratch[key .. "_cap"]
    local buf = scratch[key]
    if buf == nil or cap < cells then
        cap = math.max(cells, cap and cap * 2 or 0)
        buf = ffi.new(vla, cap)
        scratch[key], scratch[key .. "_cap"] = buf, cap
    end
    return buf
end

-- Pack the caller's 1-indexed Lua array into a fresh arg table.
-- Safety contract, same as the compiler's emitted fill loops: a fresh
-- glm_tbl_new table is Dense, 0..n-1 is within the reserve, so direct
-- span stores are the exact fast-store path set_core's routing would
-- perform anyway — minus n FFI calls. The array must be dense: sizing
-- rides Lua's `#`, so a nil hole truncates the input at the hole.
--
-- PACK FIRST, ALLOCATE SECOND: every conversion error (a string into
-- an Integer boundary, an unsupported Any type) throws inside the
-- pack loops, while NO Rust memory is live. glm_tbl_new happens only
-- after the cells are clean — an exception can no longer orphan a
-- table (the poisoned-host leak the memory observer caught).
local function build_args(lib, kind, elements, scratch)
    elements = elements or {}
    local n = #elements
    local esize = KIND_ESIZE[kind] or 8
    if n == 0 then return lib.glm_tbl_new(esize, 0) end

    local buf, bytes
    if kind == GLM_ARG_INT then
        buf = get_buf(scratch, "i64", "long long", n)
        for i = 1, n do buf[i - 1] = elements[i] end
        bytes = 8 * n
    elseif kind == GLM_ARG_FLOAT then
        buf = get_buf(scratch, "dbl", "double", n)
        for i = 1, n do buf[i - 1] = elements[i] end
        bytes = 8 * n
    elseif kind == GLM_ARG_BOOL then
        buf = get_buf(scratch, "byte", "unsigned char", n)
        for i = 1, n do buf[i - 1] = elements[i] and 1 or 0 end
        bytes = n
    elseif kind == GLM_ARG_STRING then
        buf = get_buf(scratch, "ptr", "uintptr_t", n)
        for i = 1, n do
            local v = elements[i]
            buf[i - 1] = ffi.cast("uintptr_t",
                lib.glm_str_intern(v, #v))
        end
        bytes = 8 * n
    elseif kind == GLM_ARG_ANY then
        buf = get_buf(scratch, "any", "long long", 2 * n)
        local bits = scratch.bits
        if bits == nil then
            bits = ffi.new("glm_f64_bits")
            scratch.bits = bits
        end
        for i = 1, n do
            local v = elements[i]
            local tag
            if type(v) == "boolean" then
                tag = GLM_ARG_BOOL
                buf[2 * i - 2] = v and 1 or 0
            elseif type(v) == "string" then
                tag = GLM_ARG_STRING
                buf[2 * i - 2] = ffi.cast("long long",
                    lib.glm_str_intern(v, #v))
            elseif type(v) == "number" then
                if v == math.floor(v) and math.abs(v) < 2^53 then
                    tag = GLM_ARG_INT
                    buf[2 * i - 2] = v
                else
                    tag = GLM_ARG_FLOAT
                    bits.d = v
                    buf[2 * i - 2] = ffi.cast("long long", bits.u)
                end
            else
                error("unsupported Any input type: " .. type(v), 3)
            end
            buf[2 * i - 1] = tag            -- tag rides the high 64
        end
        bytes = 16 * n
    else
        error("unsupported boundary kind " .. tostring(kind), 3)
    end

    -- Clean cells: now allocate, reserve (commits border = n), move.
    local t = lib.glm_tbl_new(esize, 0)
    lib.glm_tbl_reserve(t, n)
    ffi.copy(ffi.cast("GlmTableHeader*", t).data, buf, bytes)
    return t
end

-- Result decoding — direct span read, checked fallback

-- Decode one string payload (interned, NUL-terminated). A null payload
-- decodes to nil — NOT via ffi.string: NULL cdata compares equal to nil
-- in LuaJIT, and ffi.string(NULL) is an uncatchable segfault.
local function dec_str(ptr)
    if ptr == nil then return nil end
    return ffi.string(ptr)
end

-- Fast path preconditions, from the header itself: Dense mode and no
-- far-key overflow map. Under those, every store below border went to
-- the span (far keys are the only thing that allocates the map), so a
-- direct read is exact. Anything else: per-cell glm_tbl_get.
local function decode_result(lib, t, kind_hint, scratch)
    if t == nil then return {} end
    local h = ffi.cast("GlmTableHeader*", t)
    local border = tonumber(h.border)
    if border <= 0 then return {} end

    local result = {}
    local direct = (h.mode == 0) and (h.sparse_map == nil)

    -- The esize-8 semantic (int bits? double bits? string ptr?) comes
    -- from the module's boundary kind; a result narrower than the
    -- boundary is an int-style table.
    local esize = tonumber(h.esize)
    local as_float  = (kind_hint == GLM_ARG_FLOAT)  and esize == 8
    local as_string = (kind_hint == GLM_ARG_STRING) and esize == 8

    if direct then
        local span = h.data
        if esize == 16 then
            local w = ffi.cast("const long long*", span)
            local bits = scratch.bits
            if bits == nil then
                bits = ffi.new("glm_f64_bits")
                scratch.bits = bits
            end
            for i = 0, border - 1 do
                local payload = w[2 * i]
                local tag     = tonumber(w[2 * i + 1])
                local v
                if tag == GLM_ARG_INT then
                    v = tonumber(payload)
                elseif tag == GLM_ARG_FLOAT then
                    bits.u = ffi.cast("unsigned long long", payload)
                    v = bits.d
                elseif tag == GLM_ARG_BOOL then
                    v = tonumber(payload) > 0
                elseif tag == GLM_ARG_STRING then
                    v = dec_str(ffi.cast("const char*", payload))
                else
                    v = tonumber(payload)
                end
                result[i] = v
            end
        elseif as_float then
            local w = ffi.cast("const double*", span)
            for i = 0, border - 1 do result[i] = tonumber(w[i]) end
        elseif as_string then
            local w = ffi.cast("const char* const*", span)
            for i = 0, border - 1 do result[i] = dec_str(w[i]) end
        elseif esize == 1 then
            local w = ffi.cast("const unsigned char*", span)
            for i = 0, border - 1 do result[i] = w[i] > 0 end
        else
            local w = ffi.cast("const long long*", span)
            for i = 0, border - 1 do result[i] = tonumber(w[i]) end
        end
        return result
    end

    -- Checked fallback: per-cell through the exported byte seam.
    if esize == 16 then
        local buf = ffi.new("long long[2]")
        local bits = ffi.new("glm_f64_bits")
        for i = 0, border - 1 do
            lib.glm_tbl_get(t, i, buf, 16)
            local tag = tonumber(buf[1])
            local v
            if tag == GLM_ARG_FLOAT then
                bits.u = ffi.cast("unsigned long long", buf[0])
                v = bits.d
            elseif tag == GLM_ARG_BOOL then
                v = tonumber(buf[0]) > 0
            elseif tag == GLM_ARG_STRING then
                v = dec_str(ffi.cast("const char*", buf[0]))
            else
                v = tonumber(buf[0])
            end
            result[i] = v
        end
    elseif esize == 1 then
        local buf = ffi.new("unsigned char[1]")
        for i = 0, border - 1 do
            lib.glm_tbl_get(t, i, buf, 1)
            result[i] = buf[0] > 0
        end
    else
        -- Signed: a negative i64 cell must surface negative, not as
        -- 2^64 + v. The float/string reads below are unaffected — the
        -- int64 -> union's uint64 field (and pointer casts) keep the
        -- raw bits.
        local buf = ffi.new("long long[1]")
        for i = 0, border - 1 do
            lib.glm_tbl_get(t, i, buf, esize)
            if as_float then
                local bits = scratch.bits
                if bits == nil then
                    bits = ffi.new("glm_f64_bits")
                    scratch.bits = bits
                end
                bits.u = buf[0]
                result[i] = bits.d
            elseif as_string then
                result[i] = dec_str(ffi.cast("const char*", buf[0]))
            else
                result[i] = tonumber(buf[0])
            end
        end
    end
    return result
end

-- The module object

local glm_abi = {}

--- Build the arg table and hold it for exec.
local function set_args(mod, elements)
    mod.arg_tbl = build_args(mod.lib, mod.kind, elements, mod.scratch)
    return mod
end

--- Call glm_exec on the held arg table, decode, free both sides
--- (identity-checked: `return arg` frees once).
--- @return table result (possibly empty); nil on a null return
local function exec(mod)
    local lib = mod.lib
    local result_tbl = lib.glm_exec(mod.arg_tbl)
    local result
    if result_tbl ~= nil then
        result = decode_result(lib, result_tbl, mod.kind, mod.scratch)
        if result_tbl ~= mod.arg_tbl then
            lib.glm_tbl_free(mod.arg_tbl)
        end
        lib.glm_tbl_free(result_tbl)
    else
        lib.glm_tbl_free(mod.arg_tbl)
        result = nil
    end
    mod.arg_tbl = nil
    return result
end

local ModuleMT = {
    __index = {
        set_args = set_args;
        exec     = exec;
        run      = function(mod, elements)
            set_args(mod, elements)
            return exec(mod)
        end;
        --- Decode a caller-held GlmTable* (e.g. one kept from a custom
        --- exec path) into a 0-indexed Lua table. Everything is copied
        --- out immediately; the caller keeps ownership and frees.
        decode = function(mod, t)
            return decode_result(mod.lib, t, mod.kind, mod.scratch)
        end;
    };
}

--- Load (or fetch from cache) a glm-compiled module.
--- @return table|nil mod, nil|string error
function glm_abi.module(path)
    local lib, kind, err = load_cached(path)
    if not lib then return nil, "load: " .. tostring(err) end
    return setmetatable({
        lib     = lib;
        path    = path;
        kind    = kind;
        kind_name = KIND_NAME[kind] or "Unknown";
        esize   = KIND_ESIZE[kind] or 8;
        scratch = {};   -- persistent packing/decoding buffers
    }, ModuleMT)
end

--- Drop the cache's reference to a path's handle. The mapping itself
--- stays until process exit — LuaJIT's FFI has no dlclose; this only
--- allows the cdata to be collected and a later ffi.load to re-resolve.
function glm_abi.drop_cache(path) cache[path] = nil end

-- One-shot convenience (cached underneath)

--- @return table|nil result, nil|string error
function glm_abi.run(path, elements)
    local mod, err = glm_abi.module(path)
    if not mod then return nil, err end
    return mod:run(elements)
end

--- Query boundary type without running.
function glm_abi.get_arg_kind(path)
    local _, kind, err = load_cached(path)
    if kind == nil then return GLM_ARG_NONE, tostring(err) end
    return kind
end

glm_abi.GLM_ARG = {
    NONE   = GLM_ARG_NONE;
    INT    = GLM_ARG_INT;
    FLOAT  = GLM_ARG_FLOAT;
    BOOL   = GLM_ARG_BOOL;
    STRING = GLM_ARG_STRING;
    ANY    = GLM_ARG_ANY;
}
glm_abi.kind_name  = KIND_NAME
glm_abi.kind_esize = KIND_ESIZE

return glm_abi
