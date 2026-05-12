local ffi = require("ffi")
local bit = require("bit")
local vulkan_core = require("vulkan_core")
local memory = require("memory")
local swapchain_core = require("swapchain")
local descriptors = require("descriptors")
local compute = require("compute_pipeline")
local graphics = require("graphics_pipeline")

ffi.cdef[[
    int vibe_get_is_running();
    void vibe_trigger_shutdown();
    void vibe_mark_lua_finished();
    const char** vibe_get_glfw_extensions(uint32_t* count);
    void vibe_publish_vk_instance(void* instance);
    void* vibe_get_vk_surface();
    void vibe_get_window_size(int* width, int* height);
]]

local active_coroutines = {}
local function start_coroutine(func) table.insert(active_coroutines, coroutine.create(func)) end

local function phase_three_bootstrap()
    print("[LUA CO] Bootstrapping Vulkan Instance...")
    local vk_state = vulkan_core.create_instance()
    ffi.C.vibe_publish_vk_instance(vk_state.instance)
    
    local surface_ptr = nil
    while ffi.C.vibe_get_is_running() == 1 do
        surface_ptr = ffi.C.vibe_get_vk_surface()
        if surface_ptr ~= nil then break end
        coroutine.yield()
    end

    vulkan_core.finalize_device_and_swapchain(vk_state, surface_ptr)
    local vk = vk_state.vk
    local device = vk_state.device

    -- Memory Subsystem
    local UNIVERSE_SIZE = 256 * 1024 * 1024
    local usage_flags = bit.bor(32, 256); -- STORAGE + INDIRECT
    memory.CreateHostVisibleBuffer("MASTER_GPU_BLOCK", "uint8_t", UNIVERSE_SIZE, usage_flags, vk_state)

    -- Viewport constraints
    local pWidth = ffi.new("int[1]")
    local pHeight = ffi.new("int[1]")
    ffi.C.vibe_get_window_size(pWidth, pHeight)

    -- Engine Subsystems
    local sc_state = swapchain_core.Init(vk, vk_state, pWidth[0], pHeight[0])
    local desc_state = descriptors.Init(vk, device, memory.Buffers["MASTER_GPU_BLOCK"])
    local comp_state = compute.Init(vk, device, desc_state.pipelineLayout)
    local gfx_state = graphics.Init(vk, vk_state, pWidth[0], pHeight[0], desc_state.pipelineLayout, sc_state.format)

    print("[LUA CO] Phase 3 Pipeline Handshake Complete! Triggering Safe Shutdown...")
    ffi.C.vibe_trigger_shutdown()

    -- Teardown
    graphics.Destroy(vk, vk_state, gfx_state)
    compute.Destroy(vk, vk_state, comp_state)
    descriptors.Destroy(vk, device, desc_state)
    swapchain_core.Destroy(vk, vk_state, sc_state)
    memory.DestroyBuffer("MASTER_GPU_BLOCK", vk_state)
    vulkan_core.Destroy(vk_state)
end

start_coroutine(phase_three_bootstrap)

while ffi.C.vibe_get_is_running() == 1 do
    for i = #active_coroutines, 1, -1 do
        local co = active_coroutines[i]
        if coroutine.status(co) ~= "dead" then
            local success, err = coroutine.resume(co)
            assert(success, "FATAL: COROUTINE CRASHED: " .. tostring(err))
        else
            table.remove(active_coroutines, i)
        end
    end
end
ffi.C.vibe_mark_lua_finished()
