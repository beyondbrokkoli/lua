local ffi = require("ffi")
local bit = require("bit")

ffi.cdef[[
    void* aligned_alloc(size_t alignment, size_t size);
    void free(void* ptr);
]]

local Memory = {
    Buffers = {},
    DeviceMemory = {},
    Mapped = {},
    AVX_Arrays = {}
}

local function FindSmartBufferMemory(vk, physicalDevice, typeFilter)
    local memProperties = ffi.new("VkPhysicalDeviceMemoryProperties")
    vk.vkGetPhysicalDeviceMemoryProperties(physicalDevice, memProperties)

    -- Prioritize Host Visible + Coherent + Local (ReBAR)
    local rebarFlags = bit.bor(1, 2, 4) -- VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT | HOST_VISIBLE | HOST_COHERENT
    for i = 0, memProperties.memoryTypeCount - 1 do
        if bit.band(typeFilter, bit.lshift(1, i)) ~= 0 and bit.band(memProperties.memoryTypes[i].propertyFlags, rebarFlags) == rebarFlags then
            print("[MEMORY] ReBAR Supported! Streaming directly to VRAM.")
            return i
        end
    end

    -- Fallback: Host Visible + Coherent (Standard System RAM)
    local stdFlags = bit.bor(2, 4)
    for i = 0, memProperties.memoryTypeCount - 1 do
        if bit.band(typeFilter, bit.lshift(1, i)) ~= 0 and bit.band(memProperties.memoryTypes[i].propertyFlags, stdFlags) == stdFlags then
            print("[MEMORY] ReBAR NOT found. Falling back to System RAM.")
            return i
        end
    end
    error("FATAL: Failed to find suitable buffer memory!")
end

function Memory.CreateHostVisibleBuffer(name, cdef_type, element_count, usage_flags, core_state)
    local vk = core_state.vk
    local byte_size = ffi.sizeof(cdef_type) * element_count

    local bufInfo = ffi.new("VkBufferCreateInfo", {
        sType = 12, -- VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO
        size = byte_size,
        usage = usage_flags,
        sharingMode = 0 -- VK_SHARING_MODE_EXCLUSIVE
    })

    local pBuffer = ffi.new("VkBuffer[1]")
    assert(vk.vkCreateBuffer(core_state.device, bufInfo, nil, pBuffer) == 0, "FATAL: vkCreateBuffer failed")
    Memory.Buffers[name] = pBuffer[0]

    local memReqs = ffi.new("VkMemoryRequirements")
    vk.vkGetBufferMemoryRequirements(core_state.device, Memory.Buffers[name], memReqs)

    local allocInfo = ffi.new("VkMemoryAllocateInfo", {
        sType = 5, -- VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO
        allocationSize = memReqs.size,
        memoryTypeIndex = FindSmartBufferMemory(vk, core_state.physicalDevice, memReqs.memoryTypeBits)
    })

    local pMemory = ffi.new("VkDeviceMemory[1]")
    assert(vk.vkAllocateMemory(core_state.device, allocInfo, nil, pMemory) == 0)
    Memory.DeviceMemory[name] = pMemory[0]

    assert(vk.vkBindBufferMemory(core_state.device, Memory.Buffers[name], Memory.DeviceMemory[name], 0) == 0)

    local ppData = ffi.new("void*[1]")
    assert(vk.vkMapMemory(core_state.device, Memory.DeviceMemory[name], 0, byte_size, 0, ppData) == 0)
    Memory.Mapped[name] = ffi.cast(cdef_type .. "*", ppData[0])

    print(string.format("[MEMORY] Allocated & Mapped VRAM Buffer: %s (%.2f MB)", name, byte_size / (1024*1024)))
end

function Memory.AllocateSoA(type_str, count, names)
    local base_type = string.gsub(type_str, "%[.-%]", "")
    local byte_size = ffi.sizeof(base_type) * count

    for i = 1, #names do
        local raw_ptr = ffi.C.aligned_alloc(64, byte_size)
        assert(raw_ptr ~= nil, "FATAL: C-Allocator failed to provide aligned memory!")
        Memory.AVX_Arrays[names[i]] = ffi.cast(base_type .. "*", raw_ptr)
        print(string.format("[MEMORY] Allocated Pure AVX2 SoA: %s (%.2f MB)", names[i], byte_size / (1024*1024)))
    end
end

function Memory.FreeSoA(names)
    for i = 1, #names do
        local ptr = Memory.AVX_Arrays[names[i]]
        if ptr then
            ffi.C.free(ptr)
            Memory.AVX_Arrays[names[i]] = nil
        end
    end
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
