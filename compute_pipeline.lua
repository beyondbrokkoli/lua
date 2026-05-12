local ffi = require("ffi")

local ComputePipeline = {}

local function ReadShaderFile(filename)
    local file = io.open(filename, "rb")
    assert(file, "FATAL: Failed to open shader file: " .. filename)
    local content = file:read("*a")
    file:close()
    return content
end

function ComputePipeline.Init(vk, device, pipelineLayout)
    print("[COMPUTE] Compiling Swarm Intelligence...")

    local compCode = ReadShaderFile("swarm_comp.spv")

    local compInfo = ffi.new("VkShaderModuleCreateInfo", {
        sType = 16, -- VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO
        codeSize = string.len(compCode),
        pCode = ffi.cast("const uint32_t*", compCode)
    })

    local pCompModule = ffi.new("VkShaderModule[1]")
    assert(vk.vkCreateShaderModule(device, compInfo, nil, pCompModule) == 0)

    local stageInfo = ffi.new("VkPipelineShaderStageCreateInfo", {
        sType = 18, -- VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO
        stage = 32, -- VK_SHADER_STAGE_COMPUTE_BIT
        module = pCompModule[0],
        pName = "main"
    })

    local pipelineInfo = ffi.new("VkComputePipelineCreateInfo[1]")
    pipelineInfo[0].sType = 29 -- VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO
    pipelineInfo[0].stage = stageInfo
    pipelineInfo[0].layout = pipelineLayout

    local pPipeline = ffi.new("VkPipeline[1]")
    assert(vk.vkCreateComputePipelines(device, nil, 1, pipelineInfo, nil, pPipeline) == 0)

    print("[COMPUTE] Swarm Pipeline Ready!")

    return {
        pipeline = pPipeline[0],
        module = pCompModule[0],
        pipelineLayout = pipelineLayout
    }
end

function ComputePipeline.Destroy(vk, core_state, comp_state)
    print("[TEARDOWN] Destroying Compute Pipeline...")
    if not comp_state or not core_state then return end
    local device = core_state.device
    if comp_state.pipeline ~= nil then vk.vkDestroyPipeline(device, comp_state.pipeline, nil) end
    if comp_state.module ~= nil then vk.vkDestroyShaderModule(device, comp_state.module, nil) end
end

return ComputePipeline
