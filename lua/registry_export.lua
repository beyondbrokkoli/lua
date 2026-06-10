-- lua/registry_export.lua
local structs_mod = require("structs")
local cfg = nil
pcall(function() cfg = require("config_engine") end) -- Load safely if it exists

local function get_sorted_keys(t)
    local keys = {}
    for k in pairs(t) do table.insert(keys, k) end
    table.sort(keys)
    return keys
end

local function generate_ssot(c_header_path)
    local c_hdr = io.open(c_header_path, "w")

    c_hdr:write("// AUTO-GENERATED SSoT - DO NOT MODIFY\n")
    c_hdr:write("#pragma once\n#include <stdint.h>\n\n")

    -- If config_engine exists, map Lua config vars to C defines
    if cfg then
        c_hdr:write("// --- ENGINE CONSTANTS ---\n")
        if cfg.mode then
            for _, k in ipairs(get_sorted_keys(cfg.mode)) do
                c_hdr:write(string.format("#define MODE_%s %d\n", string.upper(k), cfg.mode[k]))
            end
        end
        if cfg.net_state then
            for _, k in ipairs(get_sorted_keys(cfg.net_state)) do
                c_hdr:write(string.format("#define FRAME_STATE_%s %d\n", string.upper(k), cfg.net_state[k]))
            end
        end
        c_hdr:write("\n")
    end

    local dynamic_sizes = {
        float = 4, uint32_t = 4, int32_t = 4,
        uint64_t = 8, int64_t = 8,
        uint16_t = 2, int16_t = 2,
        uint8_t = 1, int8_t = 1
    }

    local function resolve_member_size(type_str)
        if dynamic_sizes[type_str] then return dynamic_sizes[type_str] end
        if string.find(type_str, "*") then return 8 end
        if string.find(type_str, "64") then return 8 end
        if string.find(type_str, "32") or type_str == "float" then return 4 end
        if string.find(type_str, "16") then return 2 end
        if string.find(type_str, "8") then return 1 end
        return dynamic_sizes[type_str] or 64
    end

    c_hdr:write("// --- ENGINE MEMORY STRUCTURES ---\n")

    for _, struct in ipairs(structs_mod.specs) do
        local attr = ""
        if struct.wire_format then
            attr = "#pragma pack(push, 1)\n"
            c_hdr:write(attr)
            c_hdr:write(string.format("typedef struct {\n"))
        else
            attr = struct.force_align and "__attribute__((packed, aligned("..struct.align..")))" or "__attribute__((packed))"
            c_hdr:write(string.format("typedef struct %s {\n", attr))
        end

        local offset = 0
        local pad_id = 0

        for _, m in ipairs(struct.members) do
            local m_size = resolve_member_size(m.type)
            
            if not struct.wire_format then
                local rem = offset % m_size
                if rem ~= 0 then
                    local pad_bytes = m_size - rem
                    c_hdr:write(string.format("    uint8_t _pad_auto_%d[%d];\n", pad_id, pad_bytes))
                    offset = offset + pad_bytes
                    pad_id = pad_id + 1
                end
            end

            local arr_str = ""
            local element_count = 1
            if type(m.count) == "table" then
                for _, dim in ipairs(m.count) do
                    arr_str = arr_str .. string.format("[%d]", dim)
                    element_count = element_count * dim
                end
            elseif m.count then
                arr_str = string.format("[%d]", m.count)
                element_count = m.count
            end

            c_hdr:write(string.format("    %s %s%s;\n", m.type, m.name, arr_str))

            local real_size = m_size * element_count
            if dynamic_sizes[m.type] then
                 real_size = dynamic_sizes[m.type] * element_count
            end
            offset = offset + real_size
        end

        if not struct.wire_format then
            local tail_rem = offset % struct.align
            if tail_rem ~= 0 then
                local tail_pad = struct.align - tail_rem
                c_hdr:write(string.format("    uint8_t _pad_tail[%d];\n", tail_pad))
                offset = offset + tail_pad
            end
            c_hdr:write("} " .. struct.name .. ";\n\n")
        else
            c_hdr:write("} " .. struct.name .. ";\n")
            c_hdr:write("#pragma pack(pop)\n\n")
        end

        dynamic_sizes[struct.name] = offset
    end

    c_hdr:close()
    print("[LUA SSOT] V2 Core & Network C-Header Generated.")
end

return { generate = generate_ssot }
