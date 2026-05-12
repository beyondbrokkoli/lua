local ffi = require("ffi")
local bit = require("bit")
-- ============================================================================
-- ALIAS BRIDGE: Map KHR extension names to Core 1.3 definitions from parse.py
-- ============================================================================
ffi.cdef[[
    typedef VkRenderingAttachmentInfo VkRenderingAttachmentInfoKHR;
    typedef VkRenderingInfo VkRenderingInfoKHR;
    typedef PFN_vkCmdBeginRendering PFN_vkCmdBeginRenderingKHR;
    typedef PFN_vkCmdEndRendering PFN_vkCmdEndRenderingKHR;
]]
local Renderer = {}

function Renderer.InitSync(vk, device, frames_in_flight)
    print("[RENDERER] Forging Synchronization Primitives...")
    local imageAvailable = ffi.new("VkSemaphore[?]", frames_in_flight)
    local renderFinished = ffi.new("VkSemaphore[?]", frames_in_flight)
    local inFlight = ffi.new("VkFence[?]", frames_in_flight)

    local semInfo = ffi.new("VkSemaphoreCreateInfo", { sType = 9 })
    local fenceInfo = ffi.new("VkFenceCreateInfo", {
        sType = 8,
        flags = 1
    })

    for i = 0, frames_in_flight - 1 do
        assert(vk.vkCreateSemaphore(device, semInfo, nil, imageAvailable + i) == 0)
        assert(vk.vkCreateSemaphore(device, semInfo, nil, renderFinished + i) == 0)
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
        srcAccessMask = 32,
        dstAccessMask = bit.bor(1, 512)
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

    state.preBarriers = ffi.new("VkImageMemoryBarrier[2]", {state.colorBarrierIn, state.depthBarrierIn})

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

    -- Set to 1000044001 (Attachment Info)
    state.colorAttachment = ffi.new("VkRenderingAttachmentInfoKHR[1]")
    state.colorAttachment[0].sType = ffi.cast("uint32_t", 1000044001)
    state.colorAttachment[0].imageLayout = 2
    state.colorAttachment[0].loadOp = 0
    state.colorAttachment[0].storeOp = 0
    state.colorAttachment[0].clearValue.color.float32[0] = 0.01
    state.colorAttachment[0].clearValue.color.float32[1] = 0.01
    state.colorAttachment[0].clearValue.color.float32[2] = 0.02
    state.colorAttachment[0].clearValue.color.float32[3] = 1.0

    -- Set to 1000044001 (Attachment Info)
    state.depthAttachment = ffi.new("VkRenderingAttachmentInfoKHR[1]")
    state.depthAttachment[0].sType = ffi.cast("uint32_t", 1000044001)
    state.depthAttachment[0].imageLayout = 3
    state.depthAttachment[0].loadOp = 0
    state.depthAttachment[0].storeOp = 1
    state.depthAttachment[0].clearValue.depthStencil.depth = 0.0

    -- Set to 1000044000 (Rendering Info)
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

    -- FIXED: 1024 = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT
    state.waitStages = ffi.new("int32_t[1]", { 1024 })
    state.submitInfo.pWaitDstStageMask = state.waitStages
    state.cmdPtr = ffi.new("VkCommandBuffer[1]")

    state.presentInfo = ffi.new("VkPresentInfoKHR", {
        sType = 1000001001,
        waitSemaphoreCount = 1,
        swapchainCount = 1
    })

    -- Loading KHR Extension Pointers directly
    state.vkCmdBeginRendering = ffi.cast("PFN_vkCmdBeginRenderingKHR", vk.vkGetDeviceProcAddr(device, "vkCmdBeginRenderingKHR"))
    state.vkCmdEndRendering = ffi.cast("PFN_vkCmdEndRenderingKHR", vk.vkGetDeviceProcAddr(device, "vkCmdEndRenderingKHR"))
    assert(state.vkCmdBeginRendering ~= ffi.NULL and state.vkCmdEndRendering ~= ffi.NULL, "FATAL: KHR Dynamic Rendering Pointers Missing!")

    return state
end

function Renderer.ExecuteFrame(vk, device, queue, swapchain, cmd_buffer, current_frame, sync, f_state, unified_buffer, p_compute, p_gfx, pc_bytes, desc_state)
    local inFlightFence = sync.inFlight[current_frame]
    local imageAvailable = sync.imageAvailable[current_frame]
    local renderFinished = sync.renderFinished[current_frame]

    -- [DELETED vkWaitForFences FROM HERE]

    local TIMEOUT_MAX = ffi.cast("uint64_t", -1)
    local res = vk.vkAcquireNextImageKHR(device, swapchain.handle, TIMEOUT_MAX, imageAvailable, nil, f_state.pImageIndex)
    if res ~= 0 and res ~= 1000001004 then return false end

    -- Reset the fence ONLY AFTER we successfully acquire the image
    vk.vkResetFences(device, 1, ffi.new("VkFence[1]", {inFlightFence}))
    vk.vkResetCommandBuffer(cmd_buffer, 0)

    vk.vkBeginCommandBuffer(cmd_buffer, f_state.cmdBeginInfo)

    -- === COMPUTE PASS ===
    vk.vkCmdBindPipeline(cmd_buffer, 1, p_compute.pipeline)
    vk.vkCmdBindDescriptorSets(cmd_buffer, 1, p_compute.pipelineLayout, 0, 1, ffi.new("VkDescriptorSet[1]", {desc_state.set0}), 0, nil)
    vk.vkCmdPushConstants(cmd_buffer, desc_state.pipelineLayout, 33, 0, 64, pc_bytes)
    vk.vkCmdDispatch(cmd_buffer, 1024, 1, 1)

    vk.vkCmdPipelineBarrier(cmd_buffer, 2048, bit.bor(128, 65536), 0, 1, ffi.new("VkMemoryBarrier[1]", {f_state.computeBarrier}), 0, nil, 0, nil)

    -- === GRAPHICS BARRIERS ===
    local imgIndex = f_state.pImageIndex[0]
    f_state.preBarriers[0].image = swapchain.images[imgIndex]
    f_state.preBarriers[1].image = p_gfx.depthImage

    vk.vkCmdPipelineBarrier(cmd_buffer, 1, bit.bor(256, 1024), 0, 0, nil, 0, nil, 2, f_state.preBarriers)

    -- === DYNAMIC RENDERING GRAPHICS PASS ===
    f_state.colorAttachment[0].imageView = swapchain.imageViews[imgIndex]
    f_state.depthAttachment[0].imageView = p_gfx.depthImageView

    f_state.vkCmdBeginRendering(cmd_buffer, f_state.renderInfo)

    vk.vkCmdBindPipeline(cmd_buffer, 0, p_gfx.pipeline)

    -- FIXED: Missing Descriptor Bind for the Graphics Pipeline
    vk.vkCmdBindDescriptorSets(cmd_buffer, 0, p_gfx.pipelineLayout, 0, 1, ffi.new("VkDescriptorSet[1]", {desc_state.set0}), 0, nil)

    vk.vkCmdSetViewport(cmd_buffer, 0, 1, f_state.viewport)
    vk.vkCmdSetScissor(cmd_buffer, 0, 1, f_state.scissor)

    vk.vkCmdBindVertexBuffers(cmd_buffer, 0, 1, ffi.new("VkBuffer[1]", {unified_buffer}), f_state.offsets)
    vk.vkCmdPushConstants(cmd_buffer, p_gfx.pipelineLayout, 33, 0, 64, pc_bytes)

    vk.vkCmdDraw(cmd_buffer, pc_bytes.particle_count, 1, 0, 0)

    f_state.vkCmdEndRendering(cmd_buffer)

    -- === OUTPUT BARRIER ===
    f_state.colorBarrierOut.image = swapchain.images[imgIndex]
    
    -- FIXED: Stage Mask Alignment (1024 = COLOR_ATTACHMENT_OUTPUT_BIT)
    vk.vkCmdPipelineBarrier(cmd_buffer, 1024, 8192, 0, 0, nil, 0, nil, 1, ffi.new("VkImageMemoryBarrier[1]", {f_state.colorBarrierOut}))

    vk.vkEndCommandBuffer(cmd_buffer)

    f_state.cmdPtr[0] = cmd_buffer
    f_state.submitInfo.pWaitSemaphores = ffi.new("VkSemaphore[1]", {imageAvailable})
    f_state.submitInfo.pCommandBuffers = f_state.cmdPtr
    f_state.submitInfo.pSignalSemaphores = ffi.new("VkSemaphore[1]", {renderFinished})

    vk.vkQueueSubmit(queue, 1, ffi.new("VkSubmitInfo[1]", {f_state.submitInfo}), inFlightFence)

    f_state.presentInfo.pWaitSemaphores = ffi.new("VkSemaphore[1]", {renderFinished})
    f_state.presentInfo.pSwapchains = ffi.new("VkSwapchainKHR[1]", {swapchain.handle})
    f_state.presentInfo.pImageIndices = f_state.pImageIndex

    vk.vkQueuePresentKHR(queue, f_state.presentInfo)

    return true
end

function Renderer.Destroy(vk, device, sync, frames_in_flight)
    print("[TEARDOWN] Dismantling Renderer Sync Objects...")
    vk.vkDeviceWaitIdle(device)
    if not sync then return end
    
    for i = 0, frames_in_flight - 1 do
        vk.vkDestroySemaphore(device, sync.imageAvailable[i], nil)
        vk.vkDestroySemaphore(device, sync.renderFinished[i], nil)
        vk.vkDestroyFence(device, sync.inFlight[i], nil)
    end
end

return Renderer
