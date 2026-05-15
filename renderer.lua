local ffi = require("ffi")
local bit = require("bit")
local cmd_factory = require("command_factory")
local math = require("math")

-- ALIAS BRIDGE: Map KHR extension names to Core 1.3 definitions from parse.py
ffi.cdef[[
    typedef VkRenderingAttachmentInfo VkRenderingAttachmentInfoKHR;
    typedef VkRenderingInfo VkRenderingInfoKHR;
    typedef PFN_vkCmdBeginRendering PFN_vkCmdBeginRenderingKHR;
    typedef PFN_vkCmdEndRendering PFN_vkCmdEndRenderingKHR;
]]
local Renderer = {}
Renderer.RenderMode = {
    LUA_NATIVE = 0,
    C_HOST = 1
}
function Renderer.InitSync(vk, device, frames_in_flight)
    print("[RENDERER] Forging Synchronization Primitives...")

    local max_swapchain_images = 10
    local imageAvailable = ffi.new("VkSemaphore[?]", max_swapchain_images)
    local renderFinished = ffi.new("VkSemaphore[?]", max_swapchain_images)
    local inFlight = ffi.new("VkFence[?]", frames_in_flight)

    local semInfo = ffi.new("VkSemaphoreCreateInfo", { sType = 9 })
    local fenceInfo = ffi.new("VkFenceCreateInfo", {
        sType = 8,
        flags = 1
    })

    for i = 0, max_swapchain_images - 1 do
        assert(vk.vkCreateSemaphore(device, semInfo, nil, imageAvailable + i) == 0)
        assert(vk.vkCreateSemaphore(device, semInfo, nil, renderFinished + i) == 0)
    end

    for i = 0, frames_in_flight - 1 do
        assert(vk.vkCreateFence(device, fenceInfo, nil, inFlight + i) == 0)
    end

    return {
        imageAvailable = imageAvailable,
        renderFinished = renderFinished,
        inFlight = inFlight
    }
end

function Renderer.AllocateFrameState(vk, device, width, height)
    local state = {}

    state.pImageIndex = ffi.new("uint32_t[1]")
    state.cmdBeginInfo = ffi.new("VkCommandBufferBeginInfo", { sType = 42 })

    state.computeBarrier = ffi.new("VkMemoryBarrier", {
        sType = 46,
        srcAccessMask = 64,  -- VK_ACCESS_SHADER_WRITE_BIT
        dstAccessMask = 32   -- VK_ACCESS_SHADER_READ_BIT
    })

    state.colorBarrierIn = ffi.new("VkImageMemoryBarrier", {
        sType = 45,
        oldLayout = 0,
        newLayout = 2,
        srcQueueFamilyIndex = 4294967295,
        dstQueueFamilyIndex = 4294967295,
        subresourceRange = { aspectMask = 1, levelCount = 1, layerCount = 1 },
        srcAccessMask = 0,
        dstAccessMask = 256
    })

    state.depthBarrierIn = ffi.new("VkImageMemoryBarrier", {
        sType = 45,
        oldLayout = 0,
        newLayout = 3,
        srcQueueFamilyIndex = 4294967295,
        dstQueueFamilyIndex = 4294967295,
        subresourceRange = { aspectMask = 2, levelCount = 1, layerCount = 1 },
        srcAccessMask = 0,
        dstAccessMask = 1024
    })

    -- Explicitly allocate the array, then copy the structs by value
    state.preBarriers = ffi.new("VkImageMemoryBarrier[2]")
    state.preBarriers[0] = state.colorBarrierIn
    state.preBarriers[1] = state.depthBarrierIn

    state.colorBarrierOut = ffi.new("VkImageMemoryBarrier", {
        sType = 45,
        oldLayout = 2,
        newLayout = 1000001002,
        srcQueueFamilyIndex = 4294967295,
        dstQueueFamilyIndex = 4294967295,
        subresourceRange = { aspectMask = 1, levelCount = 1, layerCount = 1 },
        srcAccessMask = 256,
        dstAccessMask = 0
    })

    state.colorAttachment = ffi.new("VkRenderingAttachmentInfoKHR[1]")
    state.colorAttachment[0].sType = ffi.cast("uint32_t", 1000044001)
    state.colorAttachment[0].imageLayout = 2
    state.colorAttachment[0].loadOp = 1 -- VK_ATTACHMENT_LOAD_OP_CLEAR
    state.colorAttachment[0].storeOp = 0
    state.colorAttachment[0].clearValue.color.float32[0] = 0.01
    state.colorAttachment[0].clearValue.color.float32[1] = 0.01
    state.colorAttachment[0].clearValue.color.float32[2] = 0.02
    state.colorAttachment[0].clearValue.color.float32[3] = 1.0

    state.depthAttachment = ffi.new("VkRenderingAttachmentInfoKHR[1]")
    state.depthAttachment[0].sType = ffi.cast("uint32_t", 1000044001)
    state.depthAttachment[0].imageLayout = 3
    state.depthAttachment[0].loadOp = 1 -- VK_ATTACHMENT_LOAD_OP_CLEAR
    state.depthAttachment[0].storeOp = 1
    state.depthAttachment[0].clearValue.depthStencil.depth = 0.0

    state.renderInfo = ffi.new("VkRenderingInfoKHR[1]")
    state.renderInfo[0].sType = ffi.cast("uint32_t", 1000044000)
    state.renderInfo[0].renderArea.extent.width = width
    state.renderInfo[0].renderArea.extent.height = height
    state.renderInfo[0].layerCount = 1
    state.renderInfo[0].colorAttachmentCount = 1
    state.renderInfo[0].pColorAttachments = state.colorAttachment
    state.renderInfo[0].pDepthAttachment = state.depthAttachment

    state.viewport = ffi.new("VkViewport[1]", {{ 0.0, 0.0, width, height, 0.0, 1.0 }})
    state.scissor = ffi.new("VkRect2D[1]", {{ {0, 0}, {width, height} }})
    state.offsets = ffi.new("VkDeviceSize[1]", {0})

    state.submitInfo = ffi.new("VkSubmitInfo", {
        sType = 4,
        waitSemaphoreCount = 1,
        commandBufferCount = 1,
        signalSemaphoreCount = 1
    })

    state.waitStages = ffi.new("int32_t[1]", { 1024 })
    state.submitInfo.pWaitDstStageMask = state.waitStages
    state.cmdPtr = ffi.new("VkCommandBuffer[1]")

    state.presentInfo = ffi.new("VkPresentInfoKHR", {
        sType = 1000001001,
        waitSemaphoreCount = 1,
        swapchainCount = 1
    })

    state.vkCmdBeginRendering = ffi.cast("PFN_vkCmdBeginRenderingKHR", vk.vkGetDeviceProcAddr(device, "vkCmdBeginRenderingKHR"))
    state.vkCmdEndRendering = ffi.cast("PFN_vkCmdEndRenderingKHR", vk.vkGetDeviceProcAddr(device, "vkCmdEndRenderingKHR"))
    assert(state.vkCmdBeginRendering ~= ffi.NULL and state.vkCmdEndRendering ~= ffi.NULL, "FATAL: KHR Dynamic Rendering Pointers Missing!")

    -- GC-FREE HOISTED ALLOCATIONS
    state.pFence = ffi.new("VkFence[1]")
    state.pDescriptorSets = ffi.new("VkDescriptorSet[1]")
    state.pComputeBarrierArr = ffi.new("VkMemoryBarrier[1]")
    state.pVertexBuffers = ffi.new("VkBuffer[1]")
    state.pColorBarrierOutArr = ffi.new("VkImageMemoryBarrier[1]")
    state.pWaitSemaphoreSubmit = ffi.new("VkSemaphore[1]")
    state.pSignalSemaphoreSubmit = ffi.new("VkSemaphore[1]")
    state.pWaitSemaphorePresent = ffi.new("VkSemaphore[1]")
    state.pSwapchains = ffi.new("VkSwapchainKHR[1]")
    state.pSubmitInfos = ffi.new("VkSubmitInfo[1]")

    -- 1. Slot the initialized structs into the FFI arrays
    state.pComputeBarrierArr[0] = state.computeBarrier
    state.pColorBarrierOutArr[0] = state.colorBarrierOut

    -- 2. Cache Vulkan commands to bypass global table lookups
    state.vkCmdBindPipeline = vk.vkCmdBindPipeline
    state.vkCmdBindDescriptorSets = vk.vkCmdBindDescriptorSets
    state.vkCmdPushConstants = vk.vkCmdPushConstants
    state.vkCmdDispatch = vk.vkCmdDispatch
    state.vkCmdPipelineBarrier = vk.vkCmdPipelineBarrier
    state.vkCmdSetViewport = vk.vkCmdSetViewport
    state.vkCmdSetScissor = vk.vkCmdSetScissor
    state.vkCmdBindVertexBuffers = vk.vkCmdBindVertexBuffers
    state.vkCmdDraw = vk.vkCmdDraw

    return state
end

function Renderer.ExecuteFrame(swapchain, unified_buffer, p_compute, p_gfx, pc_bytes, desc_state)
    local write_idx = ffi.C.vibe_ring_get_write_idx()
    local packet = ffi.C.vibe_ring_get_packet(write_idx)

    packet.comp_pipeline = ffi.cast("uint64_t", p_compute.pipeline)
    packet.comp_layout   = ffi.cast("uint64_t", p_compute.pipelineLayout)
    packet.gfx_pipeline  = ffi.cast("uint64_t", p_gfx.pipeline)
    packet.gfx_layout    = ffi.cast("uint64_t", p_gfx.pipelineLayout)
    packet.desc_set      = ffi.cast("uint64_t", desc_state.set0)
    packet.vertex_buffer = ffi.cast("uint64_t", unified_buffer)
    packet.depth_image   = ffi.cast("uint64_t", p_gfx.depthImage)
    packet.depth_view    = ffi.cast("uint64_t", p_gfx.depthImageView)
    packet.width         = swapchain.extent.width
    packet.height        = swapchain.extent.height

    ffi.copy(packet.pc_payload, pc_bytes, 128)
    ffi.C.vibe_ring_submit(write_idx)
    return true
end

function Renderer.Destroy(vk, device, sync, frames_in_flight)
    print("[TEARDOWN] Dismantling Renderer Sync Objects...")
    vk.vkDeviceWaitIdle(device)
    if not sync then return end

    local max_swapchain_images = 10
    for i = 0, max_swapchain_images - 1 do
        vk.vkDestroySemaphore(device, sync.imageAvailable[i], nil)
        vk.vkDestroySemaphore(device, sync.renderFinished[i], nil)
    end

    for i = 0, frames_in_flight - 1 do
        vk.vkDestroyFence(device, sync.inFlight[i], nil)
    end
end

function Renderer.SubmitHostToDeviceBarrier(vk, device, queue, cmd_state, master_buffer)
    print("[RENDERER] Executing Host-to-Device Memory Barrier...")
    local cmd_buffer = cmd_factory.AllocateBuffer(vk, device, cmd_state)
    local beginInfo = ffi.new("VkCommandBufferBeginInfo", {
        sType = 42,
        flags = 1 -- VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT
    })
    vk.vkBeginCommandBuffer(cmd_buffer, beginInfo)

    local barrier = ffi.new("VkMemoryBarrier[1]")
    barrier[0].sType = 46
    barrier[0].srcAccessMask = 16384 -- VK_ACCESS_HOST_WRITE_BIT
    barrier[0].dstAccessMask = bit.bor(32, 4) -- VK_ACCESS_SHADER_READ_BIT | VK_ACCESS_VERTEX_ATTRIBUTE_READ_BIT

    vk.vkCmdPipelineBarrier(cmd_buffer, 16384, bit.bor(2048, 4), 0, 1, barrier, 0, nil, 0, nil)
    vk.vkEndCommandBuffer(cmd_buffer)

    local submitInfo = ffi.new("VkSubmitInfo[1]")
    submitInfo[0].sType = 4
    submitInfo[0].commandBufferCount = 1
    submitInfo[0].pCommandBuffers = ffi.new("VkCommandBuffer[1]", {cmd_buffer})

    vk.vkQueueSubmit(queue, 1, submitInfo, nil)
    vk.vkQueueWaitIdle(queue)
    print("[RENDERER] VRAM Coherency Secured.")
end

return Renderer
