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
local renderer = require("renderer")
local os = require("os")
local PCOUNT = 800000
-- HIGH-RESOLUTION KERNEL TIMER
local get_time_hires

if jit.os == "Windows" then
    ffi.cdef[[
        int QueryPerformanceCounter(int64_t *lpPerformanceCount);
        int QueryPerformanceFrequency(int64_t *lpFrequency);
    ]]
    local kernel32 = ffi.load("kernel32")
    local freq = ffi.new("int64_t[1]")
    kernel32.QueryPerformanceFrequency(freq)
    local inv_freq = 1.0 / tonumber(freq[0])

    get_time_hires = function()
        local count = ffi.new("int64_t[1]")
        kernel32.QueryPerformanceCounter(count)
        return tonumber(count[0]) * inv_freq
    end
else
    ffi.cdef[[
        typedef struct { long tv_sec; long tv_nsec; } timespec;
        int clock_gettime(int clk_id, timespec *tp);
    ]]
    local CLOCK_MONOTONIC = 1
    get_time_hires = function()
        local ts = ffi.new("timespec")
        ffi.C.clock_gettime(CLOCK_MONOTONIC, ts)
        return tonumber(ts.tv_sec) + (tonumber(ts.tv_nsec) * 1e-9)
    end
end

ffi.cdef[[
    // Core Engine Control
    int vibe_get_is_running();
    void vibe_trigger_shutdown();
    void vibe_mark_lua_finished();

    // GLFW Bridge
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

    // Math & Types
    typedef struct { float m[16]; } mat4_t;

    // Structs
    typedef struct {
        mat4_t viewProj;
        uint32_t pos_x_idx;
        uint32_t pos_y_idx;
        uint32_t pos_z_idx;
        uint32_t particle_count;
        float dt;
        uint32_t vel_x_idx;
        uint32_t vel_y_idx;
        uint32_t vel_z_idx;
        uint32_t target_state;
        uint32_t push_active;
        uint32_t pull_active;
        float mouse_x;
        float mouse_y;
        uint32_t _padding[3];
    } PushConstants;

    typedef struct {
        uint32_t target_state;
        uint32_t push_active;
        uint32_t pull_active;
        float mouse_x;
        float mouse_y;
        uint32_t _padding[3];
    } SwarmCommand;

    typedef struct {
        uint64_t pipeline_id;
        uint64_t descriptor_set;
        uint32_t index_count;       // Swapped from vertex_count
        uint32_t instance_count;
        uint32_t first_index;       // Swapped from first_vertex
        int32_t  vertex_offset;     // NEW: Allows us to shift the starting vertex
        uint32_t first_instance;
        uint32_t _pad_cmd;          // NEW: Keeps the 8-byte alignment perfect
        uint8_t  push_constants[128];
        uint8_t  _padding[24];      // Adjusted to keep exactly 192 bytes
    } DrawCommand;

    typedef struct __attribute__((packed, aligned(64))) {
        uint64_t comp_pipeline;
        uint64_t comp_layout;
        uint64_t comp_desc_set;
        uint64_t gfx_layout;
        uint64_t vertex_buffer;
        uint64_t index_buffer;
        uint64_t swapchain_image;
        uint64_t swapchain_view;
        uint64_t depth_image;
        uint64_t depth_view;
        uint32_t width;
        uint32_t height;
        uint32_t draw_count;
        uint32_t _pad_ptr;
        DrawCommand* draw_queue;
        uint8_t comp_pc_payload[128];
        uint8_t _padding[24];
    } RenderPacket;

    // WSI Bridge
    typedef struct {
        void* device;      // FFI doesn't need strict Vk Types here if passed as pointers
        void* queue;
        void* swapchain;
        uint64_t swapchain_images[10];
        uint64_t swapchain_views[10];
        void* image_available[3];
        void* render_finished[10];
        void* in_flight[3];
        void* vkWaitForFences;
        void* vkAcquireNextImageKHR;
        void* vkResetFences;
        void* vkQueueSubmit;
        void* vkQueuePresentKHR;
        void* pfnBegin;
        void* pfnEnd;
    } RenderThreadInit;

    // Subsystem Interfaces
    void vmath_init_workers(int num_threads);
    void vmath_destroy_workers();
    void vmath_dispatch_swarm(int count, float* px, float* py, float* pz, float* vx, float* vy, float* vz, float* seed, const SwarmCommand* cmd, float time, float dt, float gravity, float blend_metal, float blend_paradox);

    void vibe_stream_positions(int count, float* c_px, float* c_py, float* c_pz, float* g_px, float* g_py, float* g_pz);
    int vibe_ring_get_write_idx();
    RenderPacket* vibe_ring_get_packet(int idx);
    void vibe_ring_submit(int idx);
    void vibe_ring_init_wsi(RenderThreadInit* wsi);
    void vibe_start_render_thread();
    void vibe_kill_render_thread();
    int vibe_get_mouse_btn(int btn);
    int vibe_get_spacebar();

    void Sleep(uint32_t dwMilliseconds);
    int usleep(uint32_t usec);
]]

local function sys_sleep(ms)
    if jit.os == "Windows" then
        ffi.C.Sleep(ms)
    else
        ffi.C.usleep(ms * 1000)
    end
end

local vmath_lib = ffi.load(jit.os == "Windows" and "vibemath.dll" or "./libvibemath.so")

local function main()
    print("[LUA IO] Booting Headless...")
    local vk_state = vulkan_core.create_instance()
    ffi.C.vibe_publish_vk_instance(vk_state.instance)

    print("[LUA IO] Ordering C-Core to Boot GLFW Window...")
    ffi.C.vibe_set_glfw_cmd(1, 1280, 720)

    while ffi.C.vibe_get_vk_surface() == nil do
        -- Spinlock waiting for async C core window creation
    end

    local surface_ptr = ffi.C.vibe_get_vk_surface()
    vulkan_core.finalize_device_and_swapchain(vk_state, surface_ptr)

    local vk = vk_state.vk
    local device = vk_state.device

    local usage_flags = bit.bor(32, 128, 256)
    local INDEX_SIZE = 12000000 * 4
    local idx_usage = bit.bor(64, 256)
    memory.CreateHostVisibleBuffer("MASTER_INDEX_BLOCK", "uint32_t", INDEX_SIZE / 4, idx_usage, vk_state)

    local requested_count = PCOUNT
    local padded_capacity = math.ceil(requested_count / 8) * 8
    memory.AllocateSoA("float", padded_capacity, {"px", "py", "pz", "vx", "vy", "vz", "seed"})

    local cpu_soa = {
        px = memory.AVX_Arrays["px"],
        py = memory.AVX_Arrays["py"],
        pz = memory.AVX_Arrays["pz"],
        vx = memory.AVX_Arrays["vx"],
        vy = memory.AVX_Arrays["vy"],
        vz = memory.AVX_Arrays["vz"],
        seed = memory.AVX_Arrays["seed"]
    }

    local FRAME_FLOAT_COUNT = padded_capacity * 3
    local TOTAL_FLOAT_COUNT = FRAME_FLOAT_COUNT * 3
    local UNIVERSE_SIZE_BYTES = TOTAL_FLOAT_COUNT * ffi.sizeof("float")
    local gpu_usage_flags = bit.bor(32, 128, 256)

    memory.CreateHostVisibleBuffer("MASTER_GPU_BLOCK", "uint8_t", UNIVERSE_SIZE_BYTES, gpu_usage_flags, vk_state)
    local master_ptr = ffi.cast("float*", memory.Mapped["MASTER_GPU_BLOCK"])

    for p = 0, requested_count - 1 do
        cpu_soa.seed[p] = math.random()
        cpu_soa.px[p] = (math.random() - 0.5) * 20000.0
        cpu_soa.py[p] = (math.random() - 0.5) * 10000.0 + 5000.0
        cpu_soa.pz[p] = (math.random() - 0.5) * 20000.0
        cpu_soa.vx[p] = 0.0
        cpu_soa.vy[p] = 0.0
        cpu_soa.vz[p] = 0.0
    end

    vmath_lib.vmath_init_workers(8)

    local pWidth = ffi.new("int[1]")
    local pHeight = ffi.new("int[1]")
    ffi.C.vibe_get_window_size(pWidth, pHeight)

    local sc_state = swapchain_core.Init(vk, vk_state, pWidth[0], pHeight[0])
    local desc_state = descriptors.Init(vk, device, memory.Buffers["MASTER_GPU_BLOCK"])
    local comp_state = compute.Init(vk, device, desc_state.pipelineLayout)
    local gfx_state = graphics.Init(vk, vk_state, pWidth[0], pHeight[0], desc_state.pipelineLayout, sc_state.format)
    local sync_state = renderer.InitSync(vk, device, 3)
    local frame_state = renderer.AllocateFrameState(vk, device, sc_state.extent.width, sc_state.extent.height)

    local wsi = ffi.new("RenderThreadInit")
    wsi.device = device
    wsi.queue = vk_state.queue
    wsi.swapchain = sc_state.handle
    for i=0, sc_state.imageCount-1 do
        wsi.swapchain_images[i] = ffi.cast("uint64_t", sc_state.images[i])
        wsi.swapchain_views[i] = ffi.cast("uint64_t", sc_state.imageViews[i])
        wsi.render_finished[i] = sync_state.renderFinished[i]
    end
    for i=0, 2 do
        wsi.image_available[i] = sync_state.imageAvailable[i]
        wsi.in_flight[i] = sync_state.inFlight[i]
    end
    wsi.vkWaitForFences = ffi.cast("void*", vk.vkGetDeviceProcAddr(device, "vkWaitForFences"))
    wsi.vkAcquireNextImageKHR = ffi.cast("void*", vk.vkGetDeviceProcAddr(device, "vkAcquireNextImageKHR"))
    wsi.vkResetFences = ffi.cast("void*", vk.vkGetDeviceProcAddr(device, "vkResetFences"))
    wsi.vkQueueSubmit = ffi.cast("void*", vk.vkGetDeviceProcAddr(device, "vkQueueSubmit"))
    wsi.vkQueuePresentKHR = ffi.cast("void*", vk.vkGetDeviceProcAddr(device, "vkQueuePresentKHR"))
    wsi.pfnBegin = ffi.cast("void*", vk.vkGetDeviceProcAddr(device, "vkCmdBeginRenderingKHR"))
    wsi.pfnEnd = ffi.cast("void*", vk.vkGetDeviceProcAddr(device, "vkCmdEndRenderingKHR"))

    ffi.C.vibe_ring_init_wsi(wsi)
    ffi.C.vibe_start_render_thread()

    -- HOT PATH OPTIMIZATIONS (Caching Locality)
    local master_gpu_block = memory.Buffers["MASTER_GPU_BLOCK"]
    local master_index_block = memory.Buffers["MASTER_INDEX_BLOCK"]

    local c_px, c_py, c_pz = cpu_soa.px, cpu_soa.py, cpu_soa.pz
    local c_vx, c_vy, c_vz = cpu_soa.vx, cpu_soa.vy, cpu_soa.vz
    local c_seed = cpu_soa.seed

    -- Global Queue Allocation (Triple Buffered)
    local MAX_DRAW_COMMANDS = 1024
    local render_queues = ffi.new("DrawCommand[?]", MAX_DRAW_COMMANDS * 3)

    local frame_count = 0
    local pc = ffi.new("PushConstants")
    pc.pos_x_idx = 0
    pc.pos_y_idx = -16000000
    pc.pos_z_idx = 2000000
    pc.particle_count = PCOUNT
    pc.dt = 0.0

    local proj = ffi.new("mat4_t")
    local view = ffi.new("mat4_t")
    local aspect = sc_state.extent.width / sc_state.extent.height
    vmath.perspective_inf_revz(70.0, aspect, 0.1, proj)

    local cam_pos = {x = 0.0, y = 0.0, z = -600.0}
    local cam_yaw = 0.0
    local cam_pitch = 0.0
    local sensitivity = 0.002
    local move_speed = 320000.0
    local is_resizing = false

    local last_time = get_time_hires()
    local last_resize_time = last_time
    local RESIZE_COOLDOWN = 0.25
    local current_swarm_state = 1
    local MAX_SWARM_STATES = 7
    local swarm_cmd = ffi.new("SwarmCommand")
    local space_was_pressed = false

    -- GEOMETRY COMPILATION
    print("[LUA CO] Compiling Geometry...")
    local idx_ptr = ffi.cast("uint32_t*", memory.Mapped["MASTER_INDEX_BLOCK"])

    local indices = {
        -- SHAPE 0: ICE SHARD (24 indices. Uses vertices 0-5)
        0,2,3, 0,3,4, 0,4,5, 0,5,2, -- Top faces
        1,3,2, 1,4,3, 1,5,4, 1,2,5, -- Bottom faces

        -- SHAPE 1: CUBE (36 indices. Uses vertices 6-13)
        6,7,8, 6,8,9,       -- Front
        7,11,12, 7,12,8,    -- Right
        11,10,13, 11,13,12, -- Back
        10,6,9, 10,9,13,    -- Left
        9,8,12, 9,12,13,    -- Top
        10,11,7, 10,7,6     -- Bottom
    }

    for i, idx in ipairs(indices) do
        idx_ptr[i - 1] = idx
    end

    print("[LUA CO] Entering Flattened Render Loop...")

    -- FLATTENED RENDER LOOP (No Yielding)
    while ffi.C.vibe_get_is_running() == 1 do
        if ffi.C.vibe_get_resize_flag() == 1 then
            is_resizing = true
            last_resize_time = get_time_hires()
        end

        if is_resizing then
            if (get_time_hires() - last_resize_time) > RESIZE_COOLDOWN then
                print("[LUA CO] Window Stable. Initiating Vulkan Rebuild...")
                ffi.C.vibe_kill_render_thread()
                vk.vkDeviceWaitIdle(device)

                local new_w = ffi.new("int[1]")
                local new_h = ffi.new("int[1]")
                ffi.C.vibe_get_window_size(new_w, new_h)

                if new_w[0] > 0 and new_h[0] > 0 then
                    graphics.Destroy(vk, vk_state, gfx_state)
                    swapchain_core.Destroy(vk, vk_state, sc_state)
                    renderer.Destroy(vk, device, sync_state, 3)

                    sc_state = swapchain_core.Init(vk, vk_state, new_w[0], new_h[0])
                    gfx_state = graphics.Init(vk, vk_state, new_w[0], new_h[0], desc_state.pipelineLayout, sc_state.format)

                    local fresh_sync = renderer.InitSync(vk, device, 3)
                    sync_state.imageAvailable = fresh_sync.imageAvailable
                    sync_state.renderFinished = fresh_sync.renderFinished
                    sync_state.inFlight = fresh_sync.inFlight

                    frame_state.viewport[0].width = new_w[0]
                    frame_state.viewport[0].height = new_h[0]
                    frame_state.scissor[0].extent.width = new_w[0]
                    frame_state.scissor[0].extent.height = new_h[0]
                    frame_state.renderInfo[0].renderArea.extent.width = new_w[0]
                    frame_state.renderInfo[0].renderArea.extent.height = new_h[0]

                    local safe_h = math.max(1, new_h[0])
                    aspect = new_w[0] / safe_h
                    vmath.perspective_inf_revz(70.0, aspect, 0.1, proj)

                    local new_wsi = ffi.new("RenderThreadInit")
                    new_wsi.device = device
                    new_wsi.queue = vk_state.queue
                    new_wsi.swapchain = sc_state.handle
                    for i=0, sc_state.imageCount-1 do
                        new_wsi.swapchain_images[i] = ffi.cast("uint64_t", sc_state.images[i])
                        new_wsi.swapchain_views[i] = ffi.cast("uint64_t", sc_state.imageViews[i])
                        new_wsi.render_finished[i] = sync_state.renderFinished[i]
                    end
                    for i=0, 2 do
                        new_wsi.image_available[i] = sync_state.imageAvailable[i]
                        new_wsi.in_flight[i] = sync_state.inFlight[i]
                    end
                    new_wsi.vkWaitForFences = ffi.cast("void*", vk.vkGetDeviceProcAddr(device, "vkWaitForFences"))
                    new_wsi.vkAcquireNextImageKHR = ffi.cast("void*", vk.vkGetDeviceProcAddr(device, "vkAcquireNextImageKHR"))
                    new_wsi.vkResetFences = ffi.cast("void*", vk.vkGetDeviceProcAddr(device, "vkResetFences"))
                    new_wsi.vkQueueSubmit = ffi.cast("void*", vk.vkGetDeviceProcAddr(device, "vkQueueSubmit"))
                    new_wsi.vkQueuePresentKHR = ffi.cast("void*", vk.vkGetDeviceProcAddr(device, "vkQueuePresentKHR"))
                    new_wsi.pfnBegin = ffi.cast("void*", vk.vkGetDeviceProcAddr(device, "vkCmdBeginRenderingKHR"))
                    new_wsi.pfnEnd = ffi.cast("void*", vk.vkGetDeviceProcAddr(device, "vkCmdEndRenderingKHR"))

                    ffi.C.vibe_ring_init_wsi(new_wsi)
                    ffi.C.vibe_start_render_thread()
                end
                print("[LUA CO] Rebuild Complete.")
                is_resizing = false
                last_time = get_time_hires()
            end
        else
            local current_time = get_time_hires()
            local dt = math.max(0.001, math.min(current_time - last_time, 0.033))
            last_time = current_time

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
            local frame_speed = move_speed * dt

            if bit.band(wasd, 1) ~= 0 then cam_pos.x = cam_pos.x + fwd_x * frame_speed; cam_pos.y = cam_pos.y + fwd_y * frame_speed; cam_pos.z = cam_pos.z + fwd_z * frame_speed end
            if bit.band(wasd, 2) ~= 0 then cam_pos.x = cam_pos.x - fwd_x * frame_speed; cam_pos.y = cam_pos.y - fwd_y * frame_speed; cam_pos.z = cam_pos.z - fwd_z * frame_speed end
            if bit.band(wasd, 4) ~= 0 then cam_pos.x = cam_pos.x - right_x * frame_speed; cam_pos.z = cam_pos.z - right_z * frame_speed end
            if bit.band(wasd, 8) ~= 0 then cam_pos.x = cam_pos.x + right_x * frame_speed; cam_pos.z = cam_pos.z + right_z * frame_speed end
            if bit.band(wasd, 16) ~= 0 then cam_pos.y = cam_pos.y + frame_speed end
            if bit.band(wasd, 32) ~= 0 then cam_pos.y = cam_pos.y - frame_speed end

            vmath.lookAt(cam_pos.x, cam_pos.y, cam_pos.z,
                         cam_pos.x + fwd_x, cam_pos.y + fwd_y, cam_pos.z + fwd_z,
                         view)
            pc.dt = pc.dt + dt
            vmath.multiply_mat4(proj, view, pc.viewProj)

            local space_is_down = (ffi.C.vibe_get_spacebar() == 1)
            if space_is_down then
                if not space_was_pressed then
                    current_swarm_state = (current_swarm_state % MAX_SWARM_STATES) + 1
                    space_was_pressed = true
                end
            else
                space_was_pressed = false
            end

            if ffi.C.vibe_get_last_key() == 256 then
                print("[LUA IO] ESCAPE PRESSED. Executing Teardown...")
                ffi.C.vibe_trigger_shutdown()
            end

            swarm_cmd.target_state = current_swarm_state - 1
            swarm_cmd.push_active = ffi.C.vibe_get_mouse_btn(0)
            swarm_cmd.pull_active = ffi.C.vibe_get_mouse_btn(1)
            swarm_cmd.mouse_x = 0.0
            swarm_cmd.mouse_y = 5000.0

            vmath_lib.vmath_dispatch_swarm(
                pc.particle_count,
                c_px, c_py, c_pz,
                c_vx, c_vy, c_vz,
                c_seed,
                swarm_cmd,
                pc.dt, dt, 9.81, 1.0, 1.0
            )

            -- 1. Get the actual ring buffer index for the WSI synchronization
            local write_idx = ffi.C.vibe_ring_get_write_idx()
            local frame_offset = write_idx * FRAME_FLOAT_COUNT

            local gpu_px = master_ptr + frame_offset
            local gpu_py = master_ptr + (frame_offset + padded_capacity)
            local gpu_pz = master_ptr + (frame_offset + (padded_capacity * 2))

            vmath_lib.vibe_stream_positions(
                padded_capacity,
                c_px, c_py, c_pz,
                gpu_px, gpu_py, gpu_pz
            )

            -- 3. Tell the shaders where the positions live
            pc.pos_x_idx = frame_offset
            pc.pos_y_idx = frame_offset + padded_capacity
            pc.pos_z_idx = frame_offset + (padded_capacity * 2)
            pc.vel_x_idx = frame_offset + (padded_capacity * 3)
            pc.vel_y_idx = frame_offset + (padded_capacity * 4)
            pc.vel_z_idx = frame_offset + (padded_capacity * 5)

            -- ==========================================
            -- DATA-ORIENTED QUEUE POPULATION
            -- ==========================================
            local packet = ffi.C.vibe_ring_get_packet(write_idx)
            local current_queue_ptr = render_queues + (write_idx * MAX_DRAW_COMMANDS)

            -- 1. Populate Compute & Global Context (KEEP THIS!)
            -- C still needs to know the global state for the RenderPass
            packet.comp_pipeline = ffi.cast("uint64_t", comp_state.pipeline)
            packet.comp_layout = ffi.cast("uint64_t", comp_state.pipelineLayout)
            packet.comp_desc_set = ffi.cast("uint64_t", desc_state.set0)
            packet.gfx_layout = ffi.cast("uint64_t", gfx_state.pipelineLayout)
            packet.vertex_buffer = ffi.cast("uint64_t", master_gpu_block)
            packet.index_buffer = ffi.cast("uint64_t", master_index_block)
            packet.depth_image = ffi.cast("uint64_t", gfx_state.depthImage)
            packet.depth_view = ffi.cast("uint64_t", gfx_state.depthImageView)
            packet.width = sc_state.extent.width
            packet.height = sc_state.extent.height
            ffi.copy(packet.comp_pc_payload, pc, 128)

            -- 2. Populate Draw Queue Data (DUAL DISPATCH)
            local half_count = math.floor(pc.particle_count / 2)

            -- COMMAND 0: The Ice Shard Swarm (First Half)
            local cmd0 = current_queue_ptr[0]
            cmd0.pipeline_id = ffi.cast("uint64_t", gfx_state.pipeline)
            cmd0.descriptor_set = ffi.cast("uint64_t", desc_state.set0)
            cmd0.index_count = 24
            cmd0.first_index = 0
            cmd0.vertex_offset = 0
            cmd0.instance_count = half_count
            cmd0.first_instance = 0
            ffi.copy(cmd0.push_constants, pc, 128)

            -- COMMAND 1: The Asteroid Cubes (Second Half)
            local cmd1 = current_queue_ptr[1]
            cmd1.pipeline_id = ffi.cast("uint64_t", gfx_state.pipeline)
            cmd1.descriptor_set = ffi.cast("uint64_t", desc_state.set0)
            cmd1.index_count = 36
            cmd1.first_index = 24  -- Start reading after the 24 Ice Shard indices
            cmd1.vertex_offset = 0
            cmd1.instance_count = half_count
            cmd1.first_instance = half_count -- Magic: Reads the second half of physics data!

            -- Tint the cubes differently via PushConstants
            local pc_cube = ffi.new("PushConstants")
            ffi.copy(pc_cube, pc, 128)
            pc_cube.target_state = 99 -- Hacky color flag for the shader
            ffi.copy(cmd1.push_constants, pc_cube, 128)

            -- 3. Bind the queue to the Ring Buffer packet
            packet.draw_queue = current_queue_ptr
            packet.draw_count = 2 -- Now telling C to loop twice!

            -- 4. Cross the boundary ONCE
            ffi.C.vibe_ring_submit(write_idx)

            frame_count = frame_count + 1
        end
        sys_sleep(10)
    end

    print("[LUA CO] Render Loop Terminated. Frames: " .. tostring(frame_count))

    -- TEARDOWN
    print("[TEARDOWN] Terminating Async Render Thread...")
    ffi.C.vibe_kill_render_thread()
    vk.vkDeviceWaitIdle(vk_state.device)
    vmath_lib.vmath_destroy_workers()

    renderer.Destroy(vk, device, sync_state, 3)
    graphics.Destroy(vk, vk_state, gfx_state)
    compute.Destroy(vk, vk_state, comp_state)
    descriptors.Destroy(vk, device, desc_state)
    swapchain_core.Destroy(vk, vk_state, sc_state)

    print("[TEARDOWN] Freeing VRAM Arenas...")
    memory.DestroyBuffer("MASTER_GPU_BLOCK", vk_state)
    if memory.Buffers["MASTER_INDEX_BLOCK"] then
        memory.DestroyBuffer("MASTER_INDEX_BLOCK", vk_state)
    end

    vulkan_core.Destroy(vk_state)
end

main()
ffi.C.vibe_mark_lua_finished()
