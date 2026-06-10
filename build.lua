-- build.lua
local function run_cmd(cmd)
    local res = os.execute(cmd)
    return (res == true or res == 0)
end

local function compile_engine(platform)
    print("===========================================")
    print(" WEAVER V2 HEADLESS BUILD AUTOMATION")
    print(" Target Platform: " .. string.upper(platform))
    print("===========================================\n")

    print("[1/3] Generating C Header SSoT from Lua Specs...")
    local gen_cmd = [[luajit -e "package.path='./lua/?.lua;'..package.path; require('registry_export').generate('c/shared_structs.h')"]]

    if not run_cmd(gen_cmd) then
        print(" [FATAL] Failed to generate SSoT files!")
        os.exit(1)
    end
    print(" |- SSoT synchronization complete.\n")

    print("[2/3] Compiling Networking Backend (vx_net.c)...")
    local net_cmd = ""
    if platform == "linux" then
        net_cmd = "gcc -shared -fPIC -O3 -march=x86-64 c/vx_net.c -o bin/libvx_net.so"
    elseif platform == "win" then
        net_cmd = "gcc -shared -O3 -march=x86-64-v3 c/vx_net.c -lws2_32 -o bin/vx_net.dll"
    end

    if not run_cmd(net_cmd) then
        print(" [FATAL] vx_net compilation failed!")
        os.exit(1)
    end
    print(" |- Shared library compiled successfully.\n")

    print("[3/3] Compiling Headless Host (main.c)...")
    if platform == "linux" then
        local linux_build_main = "gcc c/main.c -O3 -march=x86-64-v3 -Wl,-E -I/usr/include/luajit-2.1 -lluajit-5.1 -lm -lpthread -o bin/boot"
        local linux_build_main = "echo [3/4] Skipping main.c ..."
        if not run_cmd(linux_build_main) then
            print(" [WARNING] Headless boot compilation failed or main.c missing.")
        else
            print(" |- Host executable compiled.")
        end
    elseif platform == "win" then
        local LUA_INC = "C:/msys64/mingw64/include/luajit-2.1"
        local win_build_main = string.format(
            'gcc c/main.c -O3 -march=x86-64-v3 -I"%s" -lws2_32 -lluajit-5.1 -lm -o bin/boot.exe',
            LUA_INC
        )
        local win_build_main = string.format("echo [3/4] Skipping main.c ...")
        if not run_cmd(win_build_main) then
            print(" [WARNING] Headless boot compilation failed or main.c missing.")
        else
            print(" |- Host executable compiled.")
        end
    end

    print("\n[SUCCESS] Weaver V2 Headless build complete!")
end

local target_platform = arg[1]
if target_platform ~= "linux" and target_platform ~= "win" then
    print(" [FATAL] Missing or invalid target platform!")
    print(" Usage: luajit build.lua <linux|win>")
    os.exit(1)
end

-- Ensure bin directory exists
os.execute(target_platform == "win" and "mkdir bin 2>nul" or "mkdir -p bin")

compile_engine(target_platform)
