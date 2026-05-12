local ffi = require("ffi")

local CommandFactory = {}

function CommandFactory.Init(vk, device, queueFamilyIndex, frames_in_flight)
    print("[FACTORY] Forging Triple-Buffered Command Matrix...")
    frames_in_flight = frames_in_flight or 3

    local pools = ffi.new("VkCommandPool[?]", frames_in_flight)
    local poolInfo = ffi.new("VkCommandPoolCreateInfo", {
        sType = 39, -- VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO
        flags = 2, -- VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT
        queueFamilyIndex = queueFamilyIndex
    })

    for i = 0, frames_in_flight - 1 do
        assert(vk.vkCreateCommandPool(device, poolInfo, nil, pools + i) == 0, "FATAL: Failed to allocate Command Pool")
    end

    return {
        pools = pools,
        frames_in_flight = frames_in_flight,
        current_frame = 0
    }
end

function CommandFactory.ResetCurrentFrame(vk, device, cmd_state)
    -- Instantly recycle all command buffers allocated during this frame's previous cycle
    local current_pool = cmd_state.pools[cmd_state.current_frame]
    vk.vkResetCommandPool(device, current_pool, 0)
end

function CommandFactory.AllocateBuffer(vk, device, cmd_state)
    local current_pool = cmd_state.pools[cmd_state.current_frame]
    local allocInfo = ffi.new("VkCommandBufferAllocateInfo", {
        sType = 40, -- VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO
        commandPool = current_pool,
        level = 0,  -- VK_COMMAND_BUFFER_LEVEL_PRIMARY
        commandBufferCount = 1
    })

    local pCmd = ffi.new("VkCommandBuffer[1]")
    assert(vk.vkAllocateCommandBuffers(device, allocInfo, pCmd) == 0, "FATAL: Failed to allocate Command Buffer")
    return pCmd[0]
end

function CommandFactory.AdvanceFrame(cmd_state)
    cmd_state.current_frame = (cmd_state.current_frame + 1) % cmd_state.frames_in_flight
end

function CommandFactory.Destroy(vk, device, cmd_state)
    print("[TEARDOWN] Dismantling Command Matrix...")
    if not cmd_state then return end
    for i = 0, cmd_state.frames_in_flight - 1 do
        vk.vkDestroyCommandPool(device, cmd_state.pools[i], nil)
    end
end

return CommandFactory
