local ffi = require("ffi")

local Swapchain = {}

function Swapchain.Init(vk, core_state, width, height)
    print("[SWAPCHAIN] Building the display chain...")

    -- ========================================================
    -- 1. Query Surface Capabilities
    -- ========================================================
    local surfaceCaps = ffi.new("VkSurfaceCapabilitiesKHR")
    vk.vkGetPhysicalDeviceSurfaceCapabilitiesKHR(core_state.physicalDevice, core_state.surface, surfaceCaps)

    -- ========================================================
    -- 2. Create the Swapchain
    -- ========================================================
    local swapchainInfo = ffi.new("VkSwapchainCreateInfoKHR")
    ffi.fill(swapchainInfo, ffi.sizeof(swapchainInfo)) -- ZERO GARBAGE MEMORY
    
    swapchainInfo.sType = 1000001000 -- VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR
    swapchainInfo.surface = core_state.surface
    swapchainInfo.minImageCount = surfaceCaps.minImageCount + 1
    swapchainInfo.imageFormat = 50 -- VK_FORMAT_B8G8R8A8_SRGB
    swapchainInfo.imageColorSpace = 0 -- VK_COLOR_SPACE_SRGB_NONLINEAR_KHR
    
    -- Assign Extent manually from our Bridge width/height
    swapchainInfo.imageExtent.width = width
    swapchainInfo.imageExtent.height = height
    
    swapchainInfo.imageArrayLayers = 1
    swapchainInfo.imageUsage = 16 -- VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT
    swapchainInfo.preTransform = surfaceCaps.currentTransform
    swapchainInfo.compositeAlpha = 1 -- VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR
    swapchainInfo.presentMode = 2 -- VK_PRESENT_MODE_FIFO_KHR (VSync on)
    swapchainInfo.clipped = 1 -- VK_TRUE

    local pSwapchain = ffi.new("VkSwapchainKHR[1]")
    local res = vk.vkCreateSwapchainKHR(core_state.device, swapchainInfo, nil, pSwapchain)
    assert(res == 0, "FATAL: Failed to create Swapchain! Error: " .. tonumber(res))
    local swapchain = pSwapchain[0]

    -- ========================================================
    -- 3. Extract the Swapchain Images
    -- ========================================================
    local pImageCount = ffi.new("uint32_t[1]")
    vk.vkGetSwapchainImagesKHR(core_state.device, swapchain, pImageCount, nil)
    local imageCount = pImageCount[0]

    local images = ffi.new("VkImage[?]", imageCount)
    vk.vkGetSwapchainImagesKHR(core_state.device, swapchain, pImageCount, images)

    -- ========================================================
    -- 4. Create the Image Views
    -- ========================================================
    local imageViews = ffi.new("VkImageView[?]", imageCount)
    
    for i = 0, imageCount - 1 do
        local viewInfo = ffi.new("VkImageViewCreateInfo")
        ffi.fill(viewInfo, ffi.sizeof(viewInfo))
        
        viewInfo.sType = 15 -- VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO
        viewInfo.image = images[i]
        viewInfo.viewType = 1 -- VK_IMAGE_VIEW_TYPE_2D
        viewInfo.format = 50 -- VK_FORMAT_B8G8R8A8_SRGB
        
        viewInfo.subresourceRange.aspectMask = 1 -- VK_IMAGE_ASPECT_COLOR_BIT
        viewInfo.subresourceRange.levelCount = 1
        viewInfo.subresourceRange.layerCount = 1

        -- Notice the pointer arithmetic (imageViews + i) to write directly to the array index!
        assert(vk.vkCreateImageView(core_state.device, viewInfo, nil, imageViews + i) == 0)
    end

    print("[SWAPCHAIN] Created successfully with " .. tonumber(imageCount) .. " images!")

    return {
        handle = swapchain,
        images = images,
        imageViews = imageViews,
        imageCount = imageCount,
        format = 50,
        extent = { width = width, height = height }
    }
end
function Swapchain.Destroy(vk, core_state, sc_state)
    print("[TEARDOWN] Destroying Swapchain & Image Views...")
    if not sc_state then return end
    
    for i = 0, sc_state.imageCount - 1 do
        if sc_state.imageViews[i] ~= nil then
            vk.vkDestroyImageView(core_state.device, sc_state.imageViews[i], nil)
        end
    end
    
    if sc_state.handle ~= nil then
        vk.vkDestroySwapchainKHR(core_state.device, sc_state.handle, nil)
    end
end
return Swapchain
