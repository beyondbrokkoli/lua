-- main.lua
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
local os = require("os")

ffi.cdef[[
    int vibe_get_is_running();
    void vibe_trigger_shutdown();
    void vibe_mark_lua_finished();
    const char** vibe_get_glfw_extensions(uint32_t* count);
    void vibe_publish_vk_instance(void* instance);
    void* vibe_get_vk_surface();
    void vibe_set_glfw_cmd(int cmd, int w, int h);
    int vibe_get_last_key();
    uint32_t vibe_get_wasd();
    float vibe_get_mouse_dx();
    float vibe_get_mouse_dy();
    int vibe_get_resize_flag();
    void vibe_get_window_size(int* w, int* h);
    typedef struct {
        mat4_t viewProj;     // Offset 0
        uint32_t pos_x_idx;     // Offset 64 (4 bytes)
        uint32_t pos_y_idx;     // Offset 68 (4 bytes)
        uint32_t pos_z_idx;     // Offset 72 (4 bytes)
        uint32_t particle_count;// Offset 76 (4 bytes)
        float dt;               // Offset 80 (4 bytes)
    } PushConstants;            // Total: 84 bytes (Well under 128B minimum max)
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
-- We now pass vk_state directly, and drop the standalone device/queue arguments
local function render_fiber(vk, vk_state, sc_state, cmd_state, sync_state, frame_state, master_buf, comp_state, gfx_state, desc_state)
    print("[LUA CO] Render Fiber Weaving...")
    local frame_count = 0

    -- Extract what we need locally
    local device = vk_state.device
    local queue = vk_state.queue

    -- Persistent State Initialization
    local pc = ffi.new("PushConstants")
    pc.pos_x_idx = 0
    pc.pos_y_idx = 1000000
    pc.pos_z_idx = 2000000
    pc.particle_count = 1000000

    local proj = ffi.new("mat4_t")
    local view = ffi.new("mat4_t")

    local aspect = sc_state.extent.width / sc_state.extent.height
    vmath.perspective_inf_revz(70.0, aspect, 0.1, proj)

    local cam_pos = {x = 0.0, y = 0.0, z = -600.0}
    local cam_yaw = 0.0
    local cam_pitch = 0.0
    local sensitivity = 0.002

    local speed = 5.0

    local is_resizing = false
    local last_resize_time = 0.0
    local RESIZE_COOLDOWN = 0.25

    while ffi.C.vibe_get_is_running() == 1 do

        if ffi.C.vibe_get_resize_flag() == 1 then
            is_resizing = true
            last_resize_time = os.clock()
        end

        if is_resizing then
            if (os.clock() - last_resize_time) > RESIZE_COOLDOWN then
                print("[LUA CO] Window Stable. Initiating Vulkan Rebuild...")

                vk.vkDeviceWaitIdle(device)

                local new_w = ffi.new("int[1]")
                local new_h = ffi.new("int[1]")
                ffi.C.vibe_get_window_size(new_w, new_h)

                if new_w[0] > 0 and new_h[0] > 0 then
                    -- 3. Teardown old dependent pipelines & Sync
                    graphics.Destroy(vk, vk_state, gfx_state)
                    swapchain_core.Destroy(vk, vk_state, sc_state)
                    renderer.Destroy(vk, device, sync_state, 3) -- <--- ADD THIS: Nuke cocked semaphores

                    -- 4. Re-forge the chain
                    sc_state = swapchain_core.Init(vk, vk_state, new_w[0], new_h[0])
                    gfx_state = graphics.Init(vk, vk_state, new_w[0], new_h[0], desc_state.pipelineLayout, sc_state.format)

                    -- <--- ADD THIS: Rebuild sync and patch the existing table
                    local fresh_sync = renderer.InitSync(vk, device, 3)
                    sync_state.imageAvailable = fresh_sync.imageAvailable
                    sync_state.renderFinished = fresh_sync.renderFinished
                    sync_state.inFlight       = fresh_sync.inFlight
                    -- --------------------------------------------------------

                    -- 5. Update Frame State (Viewport/Scissor)
                    frame_state.viewport[0].width = new_w[0]
                    frame_state.viewport[0].height = new_h[0]
                    frame_state.scissor[0].extent.width = new_w[0]
                    frame_state.scissor[0].extent.height = new_h[0]
                    frame_state.renderInfo[0].renderArea.extent.width = new_w[0]
                    frame_state.renderInfo[0].renderArea.extent.height = new_h[0]

                    -- 6. Recalculate Projection Aspect Ratio safely
                    local safe_h = math.max(1, new_h[0])
                    aspect = new_w[0] / safe_h
                    vmath.perspective_inf_revz(70.0, aspect, 0.1, proj)
                end

                print("[LUA CO] Rebuild Complete. Resuming Weaver.")
                is_resizing = false
            end
        else
            -- ==========================================================
            -- 1. NORMAL FRAME RENDER (Skipped if Resizing)
            -- ==========================================================
            local inFlightFence = sync_state.inFlight[cmd_state.current_frame]
            local TIMEOUT_MAX = ffi.cast("uint64_t", -1)
            vk.vkWaitForFences(device, 1, ffi.new("VkFence[1]", {inFlightFence}), 1, TIMEOUT_MAX)

            cmd_factory.ResetCurrentFrame(vk, device, cmd_state)

            -- Input Polling & Camera Math
            local dx = ffi.C.vibe_get_mouse_dx()
            local dy = ffi.C.vibe_get_mouse_dy()
            local wasd = ffi.C.vibe_get_wasd()

            cam_yaw = cam_yaw + (dx * sensitivity)
            cam_pitch = math.max(-1.5, math.min(1.5, cam_pitch + (dy * sensitivity)))

            local fwd_x = math.sin(cam_yaw) * math.cos(cam_pitch)
            local fwd_y = -math.sin(cam_pitch)
            local fwd_z = math.cos(cam_yaw) * math.cos(cam_pitch)

            local right_x = math.cos(cam_yaw)
            local right_z = -math.sin(cam_yaw)

            if bit.band(wasd, 1) ~= 0 then cam_pos.x = cam_pos.x + fwd_x * speed; cam_pos.y = cam_pos.y + fwd_y * speed; cam_pos.z = cam_pos.z + fwd_z * speed end
            if bit.band(wasd, 2) ~= 0 then cam_pos.x = cam_pos.x - fwd_x * speed; cam_pos.y = cam_pos.y - fwd_y * speed; cam_pos.z = cam_pos.z - fwd_z * speed end
            if bit.band(wasd, 4) ~= 0 then cam_pos.x = cam_pos.x - right_x * speed; cam_pos.z = cam_pos.z - right_z * speed end
            if bit.band(wasd, 8) ~= 0 then cam_pos.x = cam_pos.x + right_x * speed; cam_pos.z = cam_pos.z + right_z * speed end
            if bit.band(wasd, 16) ~= 0 then cam_pos.y = cam_pos.y + speed end
            if bit.band(wasd, 32) ~= 0 then cam_pos.y = cam_pos.y - speed end

            vmath.lookAt(cam_pos.x, cam_pos.y, cam_pos.z,
                         cam_pos.x + fwd_x, cam_pos.y + fwd_y, cam_pos.z + fwd_z,
                         view)

            pc.dt = frame_count * 0.005;
            vmath.multiply_mat4(proj, view, pc.viewProj)

            local cmd_buffer = cmd_factory.AllocateBuffer(vk, device, cmd_state)

            local success = renderer.ExecuteFrame(
                vk, device, queue, sc_state, cmd_buffer,
                cmd_state.current_frame, sync_state, frame_state,
                master_buf, comp_state, gfx_state, pc, desc_state
            )

            -- If Vulkan natively flags a resize (e.g. Windows snapped the window), trigger the cooldown
            if not success then
                print("[RENDERER] VK_ERROR_OUT_OF_DATE_KHR Triggered! Forcing Rebuild Protocol.")
                is_resizing = true
                last_resize_time = os.clock()
            end

            cmd_factory.AdvanceFrame(cmd_state)
            frame_count = frame_count + 1
        end

        -- ONE unified yield for both paths
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

    -- RENDERER INITIALIZATION
    local sync_state = renderer.InitSync(vk, device, 3)
    local frame_state = renderer.AllocateFrameState(vk, device, sc_state.extent.width, sc_state.extent.height)

    -- THE FIX: Pass vk_state directly, remove the standalone device and vk_state.queue args
    start_fiber(function()
        render_fiber(vk, vk_state, sc_state, cmd_state, sync_state, frame_state, memory.Buffers["MASTER_GPU_BLOCK"], comp_state, gfx_state, desc_state)
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
