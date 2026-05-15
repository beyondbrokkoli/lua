local ffi = require("ffi")
local bit = require("bit")

local Memory = {
    Buffers = {},
    DeviceMemory = {},
    Mapped = {}
}

local function FindSmartBufferMemory(vk, physicalDevice, typeFilter)
    local memProperties = ffi.new("VkPhysicalDeviceMemoryProperties")
    vk.vkGetPhysicalDeviceMemoryProperties(physicalDevice, memProperties)

    -- Prioritize Host Visible + Coherent + Local (ReBAR)
    local rebarFlags = bit.bor(1, 2, 4) 
    for i = 0, memProperties.memoryTypeCount - 1 do
        if bit.band(typeFilter, bit.lshift(1, i)) ~= 0 and bit.band(memProperties.memoryTypes[i].propertyFlags, rebarFlags) == rebarFlags then
            print("[MEMORY] ReBAR Supported! Streaming directly to VRAM.")
            return i
        end
    end

    -- Fallback: Force Write-Combining (Reject HOST_CACHED_BIT)
    local stdFlags = bit.bor(2, 4)
    local cachedFlag = 8 
    for i = 0, memProperties.memoryTypeCount - 1 do
        local flags = memProperties.memoryTypes[i].propertyFlags
        local has_std = bit.band(flags, stdFlags) == stdFlags
        local not_cached = bit.band(flags, cachedFlag) == 0
        if bit.band(typeFilter, bit.lshift(1, i)) ~= 0 and has_std and not_cached then
            print("[MEMORY] ReBAR NOT found. Falling back to System RAM (Write-Combining).")
            return i
        end
    end
    error("FATAL: Failed to find suitable buffer memory!")
end

function Memory.CreateHostVisibleBuffer(name, cdef_type, element_count, usage_flags, core_state)
    local vk = core_state.vk
    local byte_size = ffi.sizeof(cdef_type) * element_count

    local bufInfo = ffi.new("VkBufferCreateInfo", {
        sType = 12, size = byte_size, usage = usage_flags, sharingMode = 0
    })

    local pBuffer = ffi.new("VkBuffer[1]")
    assert(vk.vkCreateBuffer(core_state.device, bufInfo, nil, pBuffer) == 0, "FATAL: vkCreateBuffer failed")
    Memory.Buffers[name] = pBuffer[0]

    local memReqs = ffi.new("VkMemoryRequirements")
    vk.vkGetBufferMemoryRequirements(core_state.device, Memory.Buffers[name], memReqs)

    local allocInfo = ffi.new("VkMemoryAllocateInfo", {
        sType = 5, allocationSize = memReqs.size,
        memoryTypeIndex = FindSmartBufferMemory(vk, core_state.physicalDevice, memReqs.memoryTypeBits)
    })

    local pMemory = ffi.new("VkDeviceMemory[1]")
    assert(vk.vkAllocateMemory(core_state.device, allocInfo, nil, pMemory) == 0)
    Memory.DeviceMemory[name] = pMemory[0]

    assert(vk.vkBindBufferMemory(core_state.device, Memory.Buffers[name], Memory.DeviceMemory[name], 0) == 0)

    local ppData = ffi.new("void*[1]")
    assert(vk.vkMapMemory(core_state.device, Memory.DeviceMemory[name], 0, byte_size, 0, ppData) == 0)

    -- === AVX2 ALIGNMENT GUARANTEE ===
    local ptr_addr = tonumber(ffi.cast("uint64_t", ppData[0]))
    assert(bit.band(ptr_addr, 31) == 0, "FATAL: Vulkan memory is not 32-byte aligned.")
    -- ================================

    Memory.Mapped[name] = ffi.cast(cdef_type .. "*", ppData[0])
    print(string.format("[MEMORY] Allocated & Mapped VRAM Buffer: %s (%.2f MB)", name, byte_size / (1024*1024)))
end

function Memory.DestroyBuffer(name, core_state)
    local vk = core_state.vk
    if Memory.Buffers[name] then
        vk.vkDestroyBuffer(core_state.device, Memory.Buffers[name], nil)
    end
    if Memory.DeviceMemory[name] then
        vk.vkUnmapMemory(core_state.device, Memory.DeviceMemory[name])
        vk.vkFreeMemory(core_state.device, Memory.DeviceMemory[name], nil)
    end
end

return Memory
