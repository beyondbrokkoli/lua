local ffi = require("ffi")
local bit = require("bit")

local Descriptors = {}

function Descriptors.Init(vk, device, master_gpu_buffer)
    print("[DESCRIPTORS] Wiring Master VRAM Arena as a Unified SSBO...")

    -- Stage Bits
    local STAGE_VERTEX = 1
    local STAGE_COMPUTE = 32
    local STAGE_ALL = bit.bor(STAGE_VERTEX, STAGE_COMPUTE)

    -- ========================================================
    -- 1. Descriptor Set Layout Binding (Single SSBO)
    -- ========================================================
    local ssboBinding = ffi.new("VkDescriptorSetLayoutBinding[1]")
    ssboBinding[0].binding = 0
    ssboBinding[0].descriptorType = 7 -- VK_DESCRIPTOR_TYPE_STORAGE_BUFFER
    ssboBinding[0].descriptorCount = 1
    ssboBinding[0].stageFlags = STAGE_ALL
    ssboBinding[0].pImmutableSamplers = nil

    local layoutInfo = ffi.new("VkDescriptorSetLayoutCreateInfo")
    ffi.fill(layoutInfo, ffi.sizeof(layoutInfo))
    layoutInfo.sType = 32 -- VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO
    layoutInfo.bindingCount = 1
    layoutInfo.pBindings = ssboBinding

    local pLayout = ffi.new("VkDescriptorSetLayout[1]")
    assert(vk.vkCreateDescriptorSetLayout(device, layoutInfo, nil, pLayout) == 0, "FATAL: Layout Creation Failed")
    local unifiedSetLayout = pLayout[0]

    -- ========================================================
    -- 2. Push Constant Range (64-Byte Router)
    -- ========================================================
    local pushRange = ffi.new("VkPushConstantRange[1]")
    pushRange[0].stageFlags = STAGE_ALL
    pushRange[0].offset = 0
    pushRange[0].size = 64

    -- ========================================================
    -- 3. Pipeline Layout (Unified Router)
    -- ========================================================
    local pipeLayoutInfo = ffi.new("VkPipelineLayoutCreateInfo")
    ffi.fill(pipeLayoutInfo, ffi.sizeof(pipeLayoutInfo))
    pipeLayoutInfo.sType = 30 -- VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO
    pipeLayoutInfo.setLayoutCount = 1
    pipeLayoutInfo.pSetLayouts = ffi.new("VkDescriptorSetLayout[1]", {unifiedSetLayout})
    pipeLayoutInfo.pushConstantRangeCount = 1
    pipeLayoutInfo.pPushConstantRanges = pushRange

    local pPipeLayout = ffi.new("VkPipelineLayout[1]")
    assert(vk.vkCreatePipelineLayout(device, pipeLayoutInfo, nil, pPipeLayout) == 0, "FATAL: Pipeline Layout Failed")
    local unifiedPipelineLayout = pPipeLayout[0]

    -- ========================================================
    -- 4. Descriptor Pool
    -- ========================================================
    local poolSize = ffi.new("VkDescriptorPoolSize[1]")
    poolSize[0].type = 7 -- VK_DESCRIPTOR_TYPE_STORAGE_BUFFER
    poolSize[0].descriptorCount = 1

    local poolInfo = ffi.new("VkDescriptorPoolCreateInfo")
    ffi.fill(poolInfo, ffi.sizeof(poolInfo))
    poolInfo.sType = 33 -- VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO
    poolInfo.maxSets = 1
    poolInfo.poolSizeCount = 1
    poolInfo.pPoolSizes = poolSize

    local pPool = ffi.new("VkDescriptorPool[1]")
    assert(vk.vkCreateDescriptorPool(device, poolInfo, nil, pPool) == 0)
    local descriptorPool = pPool[0]

    -- ========================================================
    -- 5. Allocate and Update Descriptor Set
    -- ========================================================
    local allocInfo = ffi.new("VkDescriptorSetAllocateInfo")
    ffi.fill(allocInfo, ffi.sizeof(allocInfo))
    allocInfo.sType = 34 -- VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO
    allocInfo.descriptorPool = descriptorPool
    allocInfo.descriptorSetCount = 1
    allocInfo.pSetLayouts = ffi.new("VkDescriptorSetLayout[1]", {unifiedSetLayout})

    local pSet = ffi.new("VkDescriptorSet[1]")
    assert(vk.vkAllocateDescriptorSets(device, allocInfo, pSet) == 0)

    local VK_WHOLE_SIZE = ffi.cast("uint64_t", -1)
    local bufInfo = ffi.new("VkDescriptorBufferInfo[1]")
    bufInfo[0].buffer = master_gpu_buffer
    bufInfo[0].offset = 0
    bufInfo[0].range = VK_WHOLE_SIZE

    local write = ffi.new("VkWriteDescriptorSet[1]")
    ffi.fill(write, ffi.sizeof(write))
    write[0].sType = 35 -- VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET
    write[0].dstSet = pSet[0]
    write[0].dstBinding = 0
    write[0].descriptorCount = 1
    write[0].descriptorType = 7
    write[0].pBufferInfo = bufInfo

    vk.vkUpdateDescriptorSets(device, 1, write, 0, nil)

    print("[DESCRIPTORS] Unified Memory Matrix successfully bound!")
    return {
        setLayout = unifiedSetLayout,
        pipelineLayout = unifiedPipelineLayout,
        pool = descriptorPool,
        set0 = pSet[0]
    }
end

function Descriptors.Destroy(vk, device, desc_state)
    print("[TEARDOWN] Deconstructing Descriptors...")
    if not desc_state then return end
    if desc_state.pool then vk.vkDestroyDescriptorPool(device, desc_state.pool, nil) end
    if desc_state.setLayout then vk.vkDestroyDescriptorSetLayout(device, desc_state.setLayout, nil) end
    if desc_state.pipelineLayout then vk.vkDestroyPipelineLayout(device, desc_state.pipelineLayout, nil) end
end

return Descriptors
