local ffi = require("ffi")
local bit = require("bit")

local Renderer = {}

-- ============================================================================
-- INITIALIZATION
-- ============================================================================

function Renderer.InitSync(vk, device, frames_in_flight)
    print("[RENDERER] Forging Synchronization Primitives...")
    
    local imageAvailable = ffi.new("VkSemaphore[?]", frames_in_flight)
    local renderFinished = ffi.new("VkSemaphore[?]", frames_in_flight)
    local inFlight = ffi.new("VkFence[?]", frames_in_flight)
    
    local semInfo = ffi.new("VkSemaphoreCreateInfo", { sType = 9 }) -- VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO
    local fenceInfo = ffi.new("VkFenceCreateInfo", { 
        sType = 8, -- VK_STRUCTURE_TYPE_FENCE_CREATE_INFO
        flags = 1  -- VK_FENCE_CREATE_SIGNALED_BIT
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

-- ZERO GC MANDATE: Pre-allocate all frame structs here.
function Renderer.AllocateFrameState(vk, device, width, height)
    local state = {}

    -- Execution State
    state.pImageIndex = ffi.new("uint32_t[1]")
    state.cmdBeginInfo = ffi.new("VkCommandBufferBeginInfo", { sType = 42 })

    -- Compute Memory Barrier (Compute Write -> Graphics Read)
    state.computeBarrier = ffi.new("VkMemoryBarrier", {
        sType = 46,
        srcAccessMask = 32, -- VK_ACCESS_SHADER_WRITE_BIT
        dstAccessMask = bit.bor(1, 512) -- VERTEX_ATTRIBUTE_READ | INDIRECT_COMMAND_READ
    })

    -- Image Barriers (Pre-Render)
    state.colorBarrierIn = ffi.new("VkImageMemoryBarrier", {
        sType = 45,
        oldLayout = 0, -- UNDEFINED
        newLayout = 2, -- COLOR_ATTACHMENT_OPTIMAL
        srcQueueFamilyIndex = 4294967295, -- IGNORED
        dstQueueFamilyIndex = 4294967295,
        subresourceRange = { aspectMask = 1, levelCount = 1, layerCount = 1 },
        srcAccessMask = 0,
        dstAccessMask = 256 -- COLOR_ATTACHMENT_WRITE
    })
    
    state.depthBarrierIn = ffi.new("VkImageMemoryBarrier", {
        sType = 45,
        oldLayout = 0,
        newLayout = 252, -- DEPTH_STENCIL_ATTACHMENT_OPTIMAL
        srcQueueFamilyIndex = 4294967295,
        dstQueueFamilyIndex = 4294967295,
        subresourceRange = { aspectMask = 2, levelCount = 1, layerCount = 1 },
        srcAccessMask = 0,
        dstAccessMask = 1024 -- DEPTH_STENCIL_ATTACHMENT_WRITE
    })
    state.preBarriers = ffi.new("VkImageMemoryBarrier[2]", {state.colorBarrierIn, state.depthBarrierIn})

    -- Image Barrier (Post-Render to Present)
    state.colorBarrierOut = ffi.new("VkImageMemoryBarrier", {
        sType = 45,
        oldLayout = 2,
        newLayout = 1000001002, -- PRESENT_SRC_KHR
        srcQueueFamilyIndex = 4294967295,
        dstQueueFamilyIndex = 4294967295,
        subresourceRange = { aspectMask = 1, levelCount = 1, layerCount = 1 },
        srcAccessMask = 256,
        dstAccessMask = 0
    })

    -- Dynamic Rendering Attachments (Explicit Pointer Assignment - Core 1.3)
    state.colorAttachment = ffi.new("VkRenderingAttachmentInfo[1]")
    state.colorAttachment[0].sType = 1000044000
    state.colorAttachment[0].imageLayout = 2
    state.colorAttachment[0].loadOp = 0 -- CLEAR
    state.colorAttachment[0].storeOp = 0 -- STORE
    state.colorAttachment[0].clearValue.color.float32[0] = 0.01
    state.colorAttachment[0].clearValue.color.float32[1] = 0.01
    state.colorAttachment[0].clearValue.color.float32[2] = 0.02
    state.colorAttachment[0].clearValue.color.float32[3] = 1.0

    state.depthAttachment = ffi.new("VkRenderingAttachmentInfo[1]")
    state.depthAttachment[0].sType = 1000044000
    state.depthAttachment[0].imageLayout = 252
    state.depthAttachment[0].loadOp = 0 -- CLEAR
    state.depthAttachment[0].storeOp = 2 -- DONT_CARE
    state.depthAttachment[0].clearValue.depthStencil.depth = 0.0

    state.renderInfo = ffi.new("VkRenderingInfo")
    state.renderInfo.sType = 1000044001
    state.renderInfo.renderArea.extent.width = width
    state.renderInfo.renderArea.extent.height = height
    state.renderInfo.layerCount = 1
    state.renderInfo.colorAttachmentCount = 1
    
    state.renderInfo.pColorAttachments = state.colorAttachment
    state.renderInfo.pDepthAttachment = state.depthAttachment

    -- Pipeline State
    state.viewport = ffi.new("VkViewport[1]", {{ 0.0, 0.0, width, height, 0.0, 1.0 }})
    state.scissor = ffi.new("VkRect2D[1]", {{ {0, 0}, {width, height} }})
    state.offsets = ffi.new("VkDeviceSize[1]", {0})
    
    -- Submit & Present State
    state.submitInfo = ffi.new("VkSubmitInfo", { sType = 4, waitSemaphoreCount = 1, commandBufferCount = 1, signalSemaphoreCount = 1 })
    state.waitStages = ffi.new("int32_t[1]", { 256 }) -- COLOR_ATTACHMENT_OUTPUT
    state.submitInfo.pWaitDstStageMask = state.waitStages
    state.cmdPtr = ffi.new("VkCommandBuffer[1]")

    -- Swapchain Present must remain KHR, as it is strictly an extension
    state.presentInfo = ffi.new("VkPresentInfoKHR", {
        sType = 1000001001,
        waitSemaphoreCount = 1,
        swapchainCount = 1
    })
    
    -- API Function pointers for Core 1.3 Dynamic Rendering (Stripped KHR)
    state.vkCmdBeginRendering = ffi.cast("PFN_vkCmdBeginRendering", vk.vkGetDeviceProcAddr(device, "vkCmdBeginRendering"))
    state.vkCmdEndRendering = ffi.cast("PFN_vkCmdEndRendering", vk.vkGetDeviceProcAddr(device, "vkCmdEndRendering"))
    assert(state.vkCmdBeginRendering and state.vkCmdEndRendering, "FATAL: Core Dynamic Rendering Pointers Missing!")

    return state
end

-- ============================================================================
-- HOT LOOP EXECUTION (ZERO GC)
-- ============================================================================

function Renderer.ExecuteFrame(vk, device, queue, swapchain, cmd_buffer, current_frame, sync, f_state, unified_buffer, p_compute, p_gfx, pc_bytes)
    local inFlightFence = sync.inFlight[current_frame]
    local imageAvailable = sync.imageAvailable[current_frame]
    local renderFinished = sync.renderFinished[current_frame]

    -- 1. Wait and Reset
    vk.vkWaitForFences(device, 1, ffi.new("VkFence[1]", {inFlightFence}), 1, 0xFFFFFFFFFFFFFFFF)

    local res = vk.vkAcquireNextImageKHR(device, swapchain.handle, 0xFFFFFFFFFFFFFFFF, imageAvailable, nil, f_state.pImageIndex)
    if res == 1000001004 then return false end -- VK_ERROR_OUT_OF_DATE_KHR (Resize needed)

    vk.vkResetFences(device, 1, ffi.new("VkFence[1]", {inFlightFence}))
    vk.vkResetCommandBuffer(cmd_buffer, 0)

    -- 2. Record Command Buffer
    vk.vkBeginCommandBuffer(cmd_buffer, f_state.cmdBeginInfo)

    -- === PASS A: COMPUTE ===
    vk.vkCmdBindPipeline(cmd_buffer, 32, p_compute.pipeline) -- COMPUTE
    vk.vkCmdBindDescriptorSets(cmd_buffer, 32, p_compute.pipelineLayout, 0, 1, ffi.new("VkDescriptorSet[1]", {p_compute.set0}), 0, nil)

    -- FORCE exactly 64 bytes to prevent a Vulkan Validation Crash!
    vk.vkCmdPushConstants(cmd_buffer, p_compute.pipelineLayout, 32, 0, 64, pc_bytes)

    vk.vkCmdDispatch(cmd_buffer, 1024, 1, 1) -- Arbitrary generic dispatch size for now

    -- Barrier: Compute Write -> Graphics Read
    vk.vkCmdPipelineBarrier(cmd_buffer, 2048, bit.bor(128, 65536), 0, 1, ffi.new("VkMemoryBarrier[1]", {f_state.computeBarrier}), 0, nil, 0, nil)

    -- === PASS B: GRAPHICS ===
    local imgIndex = f_state.pImageIndex[0]

    -- Pre-Render Barriers (Transition to Color/Depth Optimal)
    f_state.preBarriers[0].image = swapchain.images[imgIndex]
    f_state.preBarriers[1].image = p_gfx.depthImage
    vk.vkCmdPipelineBarrier(cmd_buffer, 1, bit.bor(256, 1024), 0, 0, nil, 0, nil, 2, f_state.preBarriers)

    -- Dynamic Rendering Begin
    f_state.renderInfo.pColorAttachments[0].imageView = swapchain.imageViews[imgIndex]
    f_state.renderInfo.pDepthAttachment[0].imageView = p_gfx.depthImageView
    f_state.vkCmdBeginRendering(cmd_buffer, f_state.renderInfo)

    vk.vkCmdBindPipeline(cmd_buffer, 0, p_gfx.pipeline) -- GRAPHICS
    vk.vkCmdSetViewport(cmd_buffer, 0, 1, f_state.viewport)
    vk.vkCmdSetScissor(cmd_buffer, 0, 1, f_state.scissor)

    -- Bind our single unified SSBO as a vertex buffer (if vertex pulling isn't used)
    vk.vkCmdBindVertexBuffers(cmd_buffer, 0, 1, ffi.new("VkBuffer[1]", {unified_buffer}), f_state.offsets)

    -- FORCE exactly 64 bytes to prevent a Vulkan Validation Crash!
    vk.vkCmdPushConstants(cmd_buffer, p_gfx.pipelineLayout, 1, 0, 64, pc_bytes)

    -- USE the dynamic particle count from the Push Constants!
    vk.vkCmdDraw(cmd_buffer, pc_bytes.particle_count, 1, 0, 0)

    f_state.vkCmdEndRendering(cmd_buffer)

    -- Post-Render Barrier (Transition to Present)
    f_state.colorBarrierOut.image = swapchain.images[imgIndex]
    vk.vkCmdPipelineBarrier(cmd_buffer, 256, 8192, 0, 0, nil, 0, nil, 1, ffi.new("VkImageMemoryBarrier[1]", {f_state.colorBarrierOut}))

    vk.vkEndCommandBuffer(cmd_buffer)

    -- 3. Submit
    f_state.cmdPtr[0] = cmd_buffer
    f_state.submitInfo.pWaitSemaphores = ffi.new("VkSemaphore[1]", {imageAvailable})
    f_state.submitInfo.pCommandBuffers = f_state.cmdPtr
    f_state.submitInfo.pSignalSemaphores = ffi.new("VkSemaphore[1]", {renderFinished})
    
    vk.vkQueueSubmit(queue, 1, ffi.new("VkSubmitInfo[1]", {f_state.submitInfo}), inFlightFence)

    -- 4. Present
    f_state.presentInfo.pWaitSemaphores = ffi.new("VkSemaphore[1]", {renderFinished})
    f_state.presentInfo.pSwapchains = ffi.new("VkSwapchainKHR[1]", {swapchain.handle})
    f_state.presentInfo.pImageIndices = f_state.pImageIndex
    
    vk.vkQueuePresentKHR(queue, f_state.presentInfo)

    return true
end

function Renderer.Destroy(vk, device, sync, frames_in_flight)
    print("[TEARDOWN] Dismantling Renderer Sync Objects...")
    -- User explicitly requested vkDeviceWaitIdle factor-in
    vk.vkDeviceWaitIdle(device)
    
    if not sync then return end
    for i = 0, frames_in_flight - 1 do
        vk.vkDestroySemaphore(device, sync.imageAvailable[i], nil)
        vk.vkDestroySemaphore(device, sync.renderFinished[i], nil)
        vk.vkDestroyFence(device, sync.inFlight[i], nil)
    end
end

return Renderer
