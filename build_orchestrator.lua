-- build_orchestrator.lua

-- Define your Vulkan SDK Path for Windows here!
-- (Change this to match your exact LunarG folder name on your laptop)
local VULKAN_SDK_PATH = "C:/VulkanSDK/1.4.341.1"

-- Native, OS-Agnostic File Copier
local function copy_file(source, destination)
    local infile = io.open(source, "rb")
    if not infile then
        print("  [ERROR] Could not find: " .. source)
        return false
    end
    local content = infile:read("*all")
    infile:close()

    local outfile = io.open(destination, "wb")
    if not outfile then
        print("  [ERROR] Could not write to: " .. destination)
        return false
    end
    outfile:write(content)
    outfile:close()
    return true
end

local function compile_engine(platform)
    print("========================================")
    print("   VULKAN ENGINE BUILD ORCHESTRATOR")
    print("   Target Platform: " .. string.upper(platform))
    print("========================================")

    if platform == "linux" then
        -- ==========================================
        -- LINUX BUILD PIPELINE
        -- ==========================================
        print("\n[1/2] Compiling SPIR-V Shaders...")
        -- os.execute("glslc render.vert -o render_vert.spv")
        -- os.execute("glslc render.frag -o render_frag.spv")
        -- os.execute("glslc swarm.comp -o swarm_comp.spv")

        print("\n[2/2] Compiling Vulkan-Engine (Monolithic AVX2 Host) ...")

        -- THE FIX:
        -- 1. Compile main.c and vibemath.c together.
        -- 2. Added -mavx -mavx2 -mfma for the SIMD math.
        -- 3. Added -lm (Math) and -lpthread (Threading).
        -- 4. Added -Wl,-E (CRITICAL: Tells GCC to expose the executable's functions to LuaJIT FFI).
        local linux_build = "gcc main.c -O3 -mavx -mavx2 -mfma -Wl,-E -I/usr/include/luajit-2.1 -lglfw -lvulkan -lluajit-5.1 -lm -lpthread -o boot"
        os.execute(linux_build)

    elseif platform == "win" then
        -- ==========================================
        -- WINDOWS BUILD PIPELINE
        -- ==========================================
        print("\n[1/3] Compiling SPIR-V Shaders...")
        local glslc = VULKAN_SDK_PATH .. "/Bin/glslc.exe"
        -- os.execute(glslc .. " render.vert -o render_vert.spv")
        -- os.execute(glslc .. " render.frag -o render_frag.spv")
        -- os.execute("glslc swarm.comp -o swarm_comp.spv")

        print("\n[2/3] Compiling Vulkan-Engine.exe (Monolithic AVX2 Host) ...")

        -- THE FIX: Define the MSYS2 LuaJIT include path
        local LUA_INC = "C:/msys64/mingw64/include/luajit-2.1"

        -- Injected LUA_INC as the first -I flag
        local win_build = string.format(
            'gcc main.c -O3 -mavx -mavx2 -mfma -I"%s" -I"%s/Include" -L"%s/Lib" -lws2_32 -lglfw3 -lvulkan-1 -lluajit-5.1 -lm -o boot.exe',
            LUA_INC, VULKAN_SDK_PATH, VULKAN_SDK_PATH
        )
        os.execute(win_build)

        -- ==========================================
        -- AUTOMATIC DEPENDENCY PACKING
        -- ==========================================
        print("\n[3/3] Packing Windows Dependencies (DLLs)...")
        copy_file("C:/msys64/mingw64/bin/glfw3.dll", "glfw3.dll")
        copy_file("C:/msys64/mingw64/bin/lua51.dll", "lua51.dll")
        print("  |- DLLs copied successfully.")
    else
        print("ERROR: Unknown platform. Use 'linux' or 'win'.")
        os.exit(1)
    end

    print("\n[SUCCESS] Engine build complete!\n")
end
local function minify_c(content)
    content = content:gsub("/%*.-%*/", "")
    local minified_string = ""
    local in_multiline_macro = false

    for line in content:gmatch("[^\r\n]+") do
        local clean_line = line
        local s = clean_line:find("//", 1, true)
        if s then
            local prefix = clean_line:sub(1, s - 1)
            local _, quote_count = prefix:gsub('"', '"')
            if quote_count % 2 == 0 then clean_line = prefix end
        end

        clean_line = clean_line:gsub("[ \t]+", " ")
        clean_line = clean_line:match("^%s*(.-)%s*$")

        if clean_line ~= "" then
            if clean_line:sub(1, 1) == "#" or in_multiline_macro then
                minified_string = minified_string .. clean_line .. "\n"
                in_multiline_macro = (clean_line:sub(-1) == "\\")
            else
                minified_string = minified_string .. clean_line .. " "
            end
        end
    end
    if minified_string == "" then return "/* [EMPTY] */" end
    return minified_string
end

local function minify_lua(content)
    local lines = {}
    local d = "\45" .. "\45"
    for line in content:gmatch("[^\r\n]+") do
        local s = line:find(d, 1, true)
        local clean_line = line
        if s then
            local prefix = line:sub(1, s - 1)
            local _, quote_count = prefix:gsub('"', '"')
            if quote_count % 2 == 0 then clean_line = prefix end
        end
        clean_line = clean_line:gsub("[ \t]+", " ")
        clean_line = clean_line:match("^%s*(.-)%s*$")
        if clean_line ~= "" then table.insert(lines, clean_line) end
    end
    if #lines == 0 then return "-- [EMPTY] --" end
    return table.concat(lines, "; ")
end

local function get_sorted_files()
    local sorted = {}
    local visited = {}

    local function visit(file)
        if visited[file] then return end
        visited[file] = true

        local f = io.open(file, "r")
        if f then
            local content = f:read("*all")
            f:close()
            for dep_match in content:gmatch('require%s*%(?%s*["\'](.-)["\']%s*%)?') do
                local dep_name = dep_match:gsub("%.", "/")
                if not dep_name:find("%.lua$") then dep_name = dep_name .. ".lua" end
                visit(dep_name)
            end
        end
        table.insert(sorted, file)
    end

    -- Automatically trace dependencies starting from main.lua
    visit("main.lua")
    return sorted
end

-- ==========================================================
-- EXECUTION
-- ==========================================================

-- Grab the first argument passed to the script (e.g., lua build.lua linux)
-- Default to "linux" if the user forgets to type it.
local target_platform = arg[1] or "linux"

compile_engine(target_platform)

print("\n--- AI SNAPSHOT ---")
local order = get_sorted_files()

-- Explicitly add C backend to the snapshot since require() won't find it
table.insert(order, "main.c")
--table.insert(order, "vibemath.c")

for _, src in ipairs(order) do
    local f = io.open(src, "r")
    if f then
        local content = f:read("*all")
        local minified_content = ""

        if src:match("%.c$") or src:match("%.h$") then
            minified_content = minify_c(content)
        else
            minified_content = minify_lua(content)
        end

        print("@@@ FILE: " .. src .. " @@@\n" .. minified_content)
        f:close()
    end
end
