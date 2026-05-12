local ffi = require("ffi")
local bit = require("bit")
local math = require("math")
local vmath = require("vmath")
local vulkan_core = require("vulkan_core")
local memory = require("memory")
local swapchain_core = require("swapchain")
local descriptors = require("descriptors")
local compute = require("compute_pipeline")
local graphics = require("graphics_pipeline")
local cmd_factory = require("command_factory")
local renderer = require("renderer")

ffi.cdef[[
    int vibe_get_is_running();
    void vibe_trigger_shutdown();
    void vibe_mark_lua_finished();
    const char** vibe_get_glfw_extensions(uint32_t* count);
    void vibe_publish_vk_instance(void* instance);
    void* vibe_get_vk_surface();
    void vibe_get_window_size(int* width, int* height);
    void vibe_set_glfw_cmd(int cmd, int w, int h);
    int vibe_get_last_key();

    typedef struct {
        uint32_t pos_x_idx;
        uint32_t pos_y_idx;
        uint32_t pos_z_idx;
        uint32_t particle_count;

        float dt;
        uint32_t _pad[3];

        float viewProj[16];
    } PushConstants;
]]

local active_coroutines = {}
local co_blockers = {}

local function start_fiber(func)
    local co = coroutine.create(func)
    table.insert(active_coroutines, co)
    co_blockers[co] = function() return true end
end

local function run_weaver()
    while ffi.C.vibe_get_is_running() == 1 do
        for i = #active_coroutines, 1, -1 do
            local co = active_coroutines[i]
            local blocker = co_blockers[co]

            if not blocker or blocker() then
                local success, next_blocker = coroutine.resume(co)
                assert(success, "FATAL: FIBER CRASH -> " .. tostring(next_blocker))

                if coroutine.status(co) == "dead" then
                    table.remove(active_coroutines, i)
                    co_blockers[co] = nil
                else
                    co_blockers[co] = next_blocker
                end
            end
        end
        if #active_coroutines == 0 then break end
    end
end
local function render_fiber(vk, device, sc_state, queue, cmd_state, sync_state, frame_state, master_buf, comp_state, gfx_state, desc_state)
    print("[LUA CO] Render Fiber Weaving...")
    local frame_count = 0

    -- Persistent State Initialization
    local pc = ffi.new("PushConstants")
    pc.pos_x_idx = 0
    pc.pos_y_idx = 1000000
    pc.pos_z_idx = 2000000
    pc.particle_count = 1000000

    local proj = ffi.new("float[16]")
    local view = ffi.new("float[16]")

    local aspect = sc_state.extent.width / sc_state.extent.height
    vmath.perspective_inf_revz(70.0, aspect, 0.1, proj)

    while ffi.C.vibe_get_is_running() == 1 do
        -- ==========================================================
        -- 1. ABSOLUTE BARRICADE: Wait for GPU to finish this frame
        -- ==========================================================
        local inFlightFence = sync_state.inFlight[cmd_state.current_frame]
        local TIMEOUT_MAX = ffi.cast("uint64_t", -1)
        vk.vkWaitForFences(device, 1, ffi.new("VkFence[1]", {inFlightFence}), 1, TIMEOUT_MAX)

        -- ==========================================================
        -- 2. SAFE ZONE: Reset & Rebuild
        -- ==========================================================
        cmd_factory.ResetCurrentFrame(vk, device, cmd_state)

        pc.dt = frame_count * 0.016
        local orbit_x = math.sin(pc.dt) * 400.0
        local orbit_z = math.cos(pc.dt) * 400.0

        vmath.lookAt(orbit_x, 200.0, orbit_z, 0.0, 0.0, 0.0, view)
        vmath.multiply_mat4(proj, view, pc.viewProj)

        local cmd_buffer = cmd_factory.AllocateBuffer(vk, device, cmd_state)

        local success = renderer.ExecuteFrame(
            vk, device, queue, sc_state, cmd_buffer,
            cmd_state.current_frame, sync_state, frame_state,
            master_buf, comp_state, gfx_state, pc, desc_state
        )

        if not success then
            print("[RENDERER] Swapchain out of date! Rebuild required.")
        end

        cmd_factory.AdvanceFrame(cmd_state)
        frame_count = frame_count + 1
        coroutine.yield(function() return true end)
    end
    print("[LUA CO] Render Fiber Terminated. Frames: " .. tostring(frame_count))
end
local function command_glfw_fiber()
    print("[LUA IO] Booting Headless...")

    local vk_state = vulkan_core.create_instance()
    ffi.C.vibe_publish_vk_instance(vk_state.instance)

    print("[LUA IO] Ordering C-Core to Boot GLFW Window...")
    ffi.C.vibe_set_glfw_cmd(1, 1280, 720)

    coroutine.yield(function()
        return ffi.C.vibe_get_vk_surface() ~= nil
    end)

    local surface_ptr = ffi.C.vibe_get_vk_surface()
    vulkan_core.finalize_device_and_swapchain(vk_state, surface_ptr)

    local vk = vk_state.vk
    local device = vk_state.device

    local UNIVERSE_SIZE = 256 * 1024 * 1024
    local usage_flags = bit.bor(32, 128, 256) -- Added 128 (VERTEX_BUFFER_BIT)
    memory.CreateHostVisibleBuffer("MASTER_GPU_BLOCK", "uint8_t", UNIVERSE_SIZE, usage_flags, vk_state)

    local pWidth = ffi.new("int[1]")
    local pHeight = ffi.new("int[1]")
    ffi.C.vibe_get_window_size(pWidth, pHeight)

    local sc_state = swapchain_core.Init(vk, vk_state, pWidth[0], pHeight[0])
    local desc_state = descriptors.Init(vk, device, memory.Buffers["MASTER_GPU_BLOCK"])
    local comp_state = compute.Init(vk, device, desc_state.pipelineLayout)
    local gfx_state = graphics.Init(vk, vk_state, pWidth[0], pHeight[0], desc_state.pipelineLayout, sc_state.format)
    local cmd_state = cmd_factory.Init(vk, device, vk_state.qIndex, 3)

    -- >>> NEW: RENDERER INITIALIZATION <<<
    local sync_state = renderer.InitSync(vk, device, 3)
    local frame_state = renderer.AllocateFrameState(vk, device, sc_state.extent.width, sc_state.extent.height)

    start_fiber(function()
        render_fiber(vk, device, sc_state, vk_state.queue, cmd_state, sync_state, frame_state, memory.Buffers["MASTER_GPU_BLOCK"], comp_state, gfx_state, desc_state)
    end)

    local window_active = true
    while window_active do
        local key = ffi.C.vibe_get_last_key()

        if key == 256 then
            print("[LUA IO] ESCAPE PRESSED. Executing Teardown...")
            ffi.C.vibe_trigger_shutdown()
            window_active = false
        end

        coroutine.yield(function() return true end)
    end

    cmd_factory.Destroy(vk, device, cmd_state)
    renderer.Destroy(vk, device, sync_state, 3)
    graphics.Destroy(vk, vk_state, gfx_state)
    compute.Destroy(vk, vk_state, comp_state)
    descriptors.Destroy(vk, device, desc_state)
    swapchain_core.Destroy(vk, vk_state, sc_state)
    memory.DestroyBuffer("MASTER_GPU_BLOCK", vk_state)
    vulkan_core.Destroy(vk_state)
end

start_fiber(command_glfw_fiber)
run_weaver()
ffi.C.vibe_mark_lua_finished()
