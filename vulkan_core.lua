local ffi = require("ffi")
local bit = require("bit")
require("vulkan_headers")

ffi.cdef[[
    const char** vibe_get_glfw_extensions(uint32_t* count);
    void vibe_inject_validation_layers(void* instance);
    void vibe_eject_validation_layers(void* instance);
]]

local vk

-- 1. Try Windows/Wine standard (vulkan-1.dll)
local success, lib = pcall(ffi.load, "vulkan-1")

-- 2. Try Linux standard (libvulkan.so)
if not success then
    success, lib = pcall(ffi.load, "vulkan")
end

-- 3. Try Linux strict versioning (libvulkan.so.1)
if not success then
    success, lib = pcall(ffi.load, "libvulkan.so.1")
end

assert(success, "FATAL: Could not load the Vulkan dynamic library! Is the Vulkan runtime installed?\nError: " .. tostring(lib))
vk = lib

local core = {}

-- =========================================================
-- PART 1: Instance Creation (Before the Yield)
-- =========================================================
function core.create_instance()
    print("[LUA] Initializing Vulkan Core (Instance Generation)...")

    -- 1. Ask C for GLFW Extensions natively!
    local pCount = ffi.new("uint32_t[1]")
    local glfwExtensions = ffi.C.vibe_get_glfw_extensions(pCount)
    local exts_count = pCount[0]

    -- 1.5. Splice the arrays: GLFW Extensions + Debug Utils + Physical Device Props
    local total_exts = exts_count + 2
    local instanceExtensions = ffi.new("const char*[?]", total_exts)

    for i = 0, exts_count - 1 do
        instanceExtensions[i] = glfwExtensions[i]
    end

    -- Append the TWO Instance extensions
    instanceExtensions[exts_count] = "VK_EXT_debug_utils"
    instanceExtensions[exts_count + 1] = "VK_KHR_get_physical_device_properties2"

    -- 2. Build the Application Info
    local appInfo = ffi.new("VkApplicationInfo", {
        sType = 0, -- VK_STRUCTURE_TYPE_APPLICATION_INFO
        pApplicationName = "VibeEngine Cooking Dish",
        apiVersion = 4194304 -- VK_API_VERSION_1_0
    })

    -- 2.5 Define the Validation Layers
    local validationLayers = ffi.new("const char*[1]", {"VK_LAYER_KHRONOS_validation"})

    -- 3. Build the Instance Info
    local createInfo = ffi.new("VkInstanceCreateInfo", {
        sType = 1, -- VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO
        pApplicationInfo = appInfo,
        enabledExtensionCount = total_exts,
        ppEnabledExtensionNames = instanceExtensions,
        enabledLayerCount = 1,
        ppEnabledLayerNames = validationLayers
    })

    -- 4. Create the Instance
    local pInstance = ffi.new("VkInstance[1]")
    local res = vk.vkCreateInstance(createInfo, nil, pInstance)
    assert(res == 0, "FATAL: vkCreateInstance failed!")
    local instance = pInstance[0]
    print("[LUA] Vulkan Instance Created!")

    -- Optional Validation Layer injection (Requires it to exist in main.c)
    ffi.C.vibe_inject_validation_layers(instance)

    -- Return the base state so main.lua can pass the instance to C and yield
    return {
        vk = vk,
        instance = instance
    }
end

-- =========================================================
-- PART 2: Logical Device & Queue Generation (After the Yield)
-- =========================================================
function core.finalize_device_and_swapchain(vk_state, surface_ptr)
    print("[LUA] Resuming Vulkan Setup. Finalizing Logical Device...")

    local vk = vk_state.vk
    local instance = vk_state.instance

    -- 5. Cast the raw pointer from C back into a VkSurfaceKHR
    local surface = ffi.cast("VkSurfaceKHR", surface_ptr)
    vk_state.surface = surface
    print("[LUA] Window Surface Linked from Main Thread!")

    -- 6. Find the GPU
    local pDeviceCount = ffi.new("uint32_t[1]")
    vk.vkEnumeratePhysicalDevices(instance, pDeviceCount, nil)
    local pDevices = ffi.new("VkPhysicalDevice[?]", pDeviceCount[0])
    vk.vkEnumeratePhysicalDevices(instance, pDeviceCount, pDevices)

    local physicalDevice = pDevices[0] -- Just grab the first GPU for now
    vk_state.physicalDevice = physicalDevice
    print("[LUA] Hardware GPU Selected!")

    -- 7. Find the Graphics/Compute Queue Family
    local pQueueFamilyCount = ffi.new("uint32_t[1]")
    vk.vkGetPhysicalDeviceQueueFamilyProperties(physicalDevice, pQueueFamilyCount, nil)
    local queueFamilies = ffi.new("VkQueueFamilyProperties[?]", pQueueFamilyCount[0])
    vk.vkGetPhysicalDeviceQueueFamilyProperties(physicalDevice, pQueueFamilyCount, queueFamilies)

    local qIndex = -1
    for i = 0, pQueueFamilyCount[0] - 1 do
        -- VK_QUEUE_GRAPHICS_BIT is 1. (It guarantees Compute support too!)
        if bit.band(queueFamilies[i].queueFlags, 1) ~= 0 then
            qIndex = i
            break
        end
    end
    assert(qIndex ~= -1, "FATAL: Could not find a Graphics/Compute queue!")
    vk_state.qIndex = qIndex

    -- 8. Create the Logical Device
    local queuePriority = ffi.new("float[1]", 1.0)
    local queueCreateInfo = ffi.new("VkDeviceQueueCreateInfo", {
        sType = 2, -- VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO
        queueFamilyIndex = qIndex,
        queueCount = 1,
        pQueuePriorities = queuePriority
    })

    -- Enable Swapchain, Dynamic Rendering, and the entire dependency tree!
    local deviceExtensions = ffi.new("const char*[6]", {
        "VK_KHR_swapchain",
        "VK_KHR_dynamic_rendering",
        "VK_KHR_depth_stencil_resolve",
        "VK_KHR_create_renderpass2",
        "VK_KHR_multiview",
        "VK_KHR_maintenance2"
    })

    -- Enable Dynamic Rendering Feature struct
    local dynamicRendering = ffi.new("VkPhysicalDeviceDynamicRenderingFeatures", {
        sType = 1000044003, -- VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DYNAMIC_RENDERING_FEATURES
        dynamicRendering = 1 -- VK_TRUE
    })

    -- Request Physical Device Features (like Large Points)
    local deviceFeatures = ffi.new("VkPhysicalDeviceFeatures")
    ffi.fill(deviceFeatures, ffi.sizeof(deviceFeatures))
    deviceFeatures.largePoints = 1 -- VK_TRUE: Allows gl_PointSize > 1.0!

    -- Hook it into the Device Create Info
    local deviceCreateInfo = ffi.new("VkDeviceCreateInfo", {
        sType = 3, -- VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO
        pNext = dynamicRendering,
        queueCreateInfoCount = 1,
        pQueueCreateInfos = queueCreateInfo,
        enabledExtensionCount = 6,
        ppEnabledExtensionNames = deviceExtensions,
        pEnabledFeatures = deviceFeatures
    })

    local pDevice = ffi.new("VkDevice[1]")
    local res = vk.vkCreateDevice(physicalDevice, deviceCreateInfo, nil, pDevice)
    assert(res == 0, "FATAL: Failed to create Logical Device! Error: " .. tonumber(res))
    local device = pDevice[0]
    vk_state.device = device
    print("[LUA] Logical Device Created!")

    -- 9. Grab the Command Queue
    local pQueue = ffi.new("VkQueue[1]")
    vk.vkGetDeviceQueue(device, qIndex, 0, pQueue)
    vk_state.queue = pQueue[0]

    print("[DEBUG] Device Pointer in core: ", device)

    -- We pass the fully loaded state back to main.lua
    return vk_state
end

-- =========================================================
-- TEARDOWN
-- =========================================================
function core.Destroy(vk_state)
    print("[TEARDOWN] Shutting down Vulkan Core...")
    local vk = vk_state.vk

    -- 1. Destroy Logical Device First
    if vk_state.device ~= nil then
        vk.vkDestroyDevice(vk_state.device, nil)
    end

    -- 2. Destroy the Window Surface
    if vk_state.surface ~= nil then
        vk.vkDestroySurfaceKHR(vk_state.instance, vk_state.surface, nil)
    end

    -- 3. Destroy the Instance Last
    if vk_state.instance ~= nil then
        ffi.C.vibe_eject_validation_layers(vk_state.instance)
        vk.vkDestroyInstance(vk_state.instance, nil)
    end
end

return core
