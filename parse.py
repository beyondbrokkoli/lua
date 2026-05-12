import xml.etree.ElementTree as ET
TARGET_FUNCTIONS = {
    # Instance/Device/Queues
    "vkCreateInstance", "vkEnumeratePhysicalDevices", "vkCreateDevice", "vkDestroyInstance", "vkDestroyDevice",
    "vkQueueSubmit", "vkQueueWaitIdle", "vkDeviceWaitIdle", "vkGetPhysicalDeviceQueueFamilyProperties",
    "vkGetDeviceQueue",

    # Memory/Buffers (The ReBAR Allocator)
    "vkGetPhysicalDeviceMemoryProperties", "vkCreateBuffer", "vkDestroyBuffer",
    "vkGetBufferMemoryRequirements", "vkAllocateMemory", "vkBindBufferMemory",
    "vkMapMemory", "vkUnmapMemory", "vkFreeMemory",

    # Descriptors (The Wiring)
    "vkCreateDescriptorSetLayout", "vkCreateDescriptorPool", "vkAllocateDescriptorSets",
    "vkUpdateDescriptorSets", "vkDestroyDescriptorPool", "vkDestroyDescriptorSetLayout",

    # Pipelines & Shaders
    "vkCreateShaderModule", "vkDestroyShaderModule",
    "vkCreateGraphicsPipelines", "vkCreateComputePipelines",
    "vkCreatePipelineLayout", "vkDestroyPipeline", "vkDestroyPipelineLayout",

    # Depth Buffer
    "vkCreateImage", "vkGetImageMemoryRequirements", "vkBindImageMemory",
    "vkDestroyImage",                # <--- ADD THIS

    # Command Buffers
    "vkCreateCommandPool", "vkDestroyCommandPool", "vkAllocateCommandBuffers",
    "vkBeginCommandBuffer", "vkEndCommandBuffer", "vkResetCommandBuffer",
    "vkCmdBindPipeline", "vkCmdBindDescriptorSets", "vkCmdPushConstants",
    "vkCmdDispatch", "vkCmdPipelineBarrier",

    # Graphics/Rendering specific
    "vkCmdBeginRendering", "vkCmdDraw", "vkCmdEndRendering",

    # Swapchain & Sync
    "vkCreateSemaphore", "vkDestroySemaphore",
    "vkAcquireNextImageKHR", "vkCreateSwapchainKHR", "vkDestroySwapchainKHR", "vkQueuePresentKHR",
    "vkGetPhysicalDeviceSurfaceCapabilitiesKHR", "vkGetSwapchainImagesKHR", "vkCreateImageView",
    "vkDestroyImageView", "vkDestroySurfaceKHR" # <--- ADD THIS
}

TARGET_STRUCTS = {
    "VkPhysicalDeviceDynamicRenderingFeatures",
    "VkImageCreateInfo",
    "VkMemoryAllocateInfo",
    "VkPipelineShaderStageCreateInfo",
    "VkVertexInputBindingDescription",
    "VkVertexInputAttributeDescription",
    "VkPipelineVertexInputStateCreateInfo",
    "VkPipelineInputAssemblyStateCreateInfo",
    "VkPipelineViewportStateCreateInfo",
    "VkPipelineRasterizationStateCreateInfo",
    "VkPipelineMultisampleStateCreateInfo",
    "VkPipelineDepthStencilStateCreateInfo",
    "VkPipelineColorBlendAttachmentState",
    "VkPipelineColorBlendStateCreateInfo",
    "VkPipelineDynamicStateCreateInfo",
    "VkGraphicsPipelineCreateInfo",
    "VkPipelineRenderingCreateInfo",
    "VkComputePipelineCreateInfo"
}

def generate_lua_ffi_cdef(xml_path):
    tree = ET.parse(xml_path)
    root = tree.getroot()

    ffi_declarations = []
    seen_funcs = set()

    # 0. Grab API Constants (C Macros like VK_MAX_MEMORY_TYPES)
    api_constants = {}
    for enums_node in root.findall('.//enums[@name="API Constants"]'):
        for enum_tag in enums_node.findall('enum'):
            name = enum_tag.get('name')
            value = enum_tag.get('value')
            if name and value:
                # Strip out C-specific suffixes like 'U' or 'ULL' (e.g. 32U -> 32)
                api_constants[name] = value.replace('U', '').replace('L', '')

    # ========================================================
    # [THE PATCH] 0.5 Grab the actual Enum Values!
    # ========================================================
    ffi_declarations.append("\n// --- Enum Values ---")
    ffi_declarations.append("enum {")

    # Find all enum groups that are actual enums (not bitmasks)
    for enums_node in root.findall('.//enums[@type="enum"]'):
        for enum_tag in enums_node.findall('enum'):
            name = enum_tag.get('name')
            value = enum_tag.get('value')

            # Some Vulkan enums use aliases instead of values, so we check for both
            if not value:
                value = enum_tag.get('alias')

            if name and value:
                ffi_declarations.append(f"    {name} = {value},")

    ffi_declarations.append("};")
    # 1. Grab Handles
    ffi_declarations.append("// --- Handles ---")
    for handle in root.findall('.//types/type[@category="handle"]'):
        name_elem = handle.find('name')
        if name_elem is not None:
            handle_name = name_elem.text
            ffi_declarations.append(f"typedef struct {handle_name}_T* {handle_name};")

    # 1.5 Grab Enums and Bitmasks (Spoofed as ints/uints for LuaJIT memory layout)
    ffi_declarations.append("\n// --- Enums & Bitmasks ---")
    seen_types = set()
    for type_tag in root.findall('.//types/type'):
        category = type_tag.get('category')

        name = type_tag.get('name') or (type_tag.find('name').text if type_tag.find('name') is not None else None)

        if not name or name in seen_types:
            continue

        if category == 'enum':
            ffi_declarations.append(f"typedef int {name};")
            seen_types.add(name)
        elif category == 'bitmask':
            base_type_elem = type_tag.find('type')
            base_type = base_type_elem.text if base_type_elem is not None else "uint32_t"
            ffi_declarations.append(f"typedef {base_type} {name};")
            seen_types.add(name)
        elif category == 'basetype':
            base_type_elem = type_tag.find('type')
            if base_type_elem is not None and name:
                ffi_declarations.append(f"typedef {base_type_elem.text} {name};")
                seen_types.add(name)

    # Dictionary to map struct/union names to their XML element
    all_structs_xml = {}
    for s in root.findall('.//types/type'):
        if s.get('category') in ('struct', 'union'):
            all_structs_xml[s.get('name')] = s

    # 2. Figure out which structs our TARGET_FUNCTIONS need directly
    required_types = set()
    for command in root.findall('.//commands/command'):
        # FIX: We have to look inside <proto> to find the <name>!
        proto = command.find('proto')
        if proto is not None:
            name_elem = proto.find('name')
            if name_elem is not None and name_elem.text in TARGET_FUNCTIONS:
                for param in command.findall('param'):
                    type_elem = param.find('type')
                    if type_elem is not None:
                        required_types.add(type_elem.text)

    # --- SURGICAL PATCH: FORCE-FEED EXPLICIT STRUCTS ---
    for explicit_struct in TARGET_STRUCTS:
        required_types.add(explicit_struct)

    # 3. Recursively find nested structs (Dependencies)
    resolved_structs = set()
    struct_dependencies = {}

    def resolve_dependencies(type_name):
        if type_name in resolved_structs or type_name not in all_structs_xml:
            return

        struct_xml = all_structs_xml[type_name]
        resolved_structs.add(type_name)
        struct_dependencies[type_name] = set()

        for member in struct_xml.findall('member'):
            member_type = member.find('type')
            if member_type is not None and member_type.text in all_structs_xml:
                struct_dependencies[type_name].add(member_type.text)
                resolve_dependencies(member_type.text) # Recurse!

    for t in list(required_types):
        resolve_dependencies(t)

    # 4. Topological Sort (Print base structs before complex structs)
    ffi_declarations.append("\n// --- Structs (Auto-Sorted) ---")
    emitted = set()

    def emit_struct(struct_name):
        if struct_name in emitted:
            return

        # Make sure all dependencies are printed first
        for dep in struct_dependencies.get(struct_name, []):
            emit_struct(dep)

        struct_xml = all_structs_xml[struct_name]
        members = []
        seen_members = set() # Squash duplicate XML artifacts

        for member in struct_xml.findall('member'):
            for comment in member.findall('comment'):
                member.remove(comment)

            member_name = member.find('name').text if member.find('name') is not None else ""
            if member_name in seen_members:
                continue
            seen_members.add(member_name)

            member_text = "".join(member.itertext()).strip()

            # SURGICAL PATCH: Swap out C macros for their raw integer values
            for macro_name, macro_value in api_constants.items():
                if macro_name in member_text:
                    member_text = member_text.replace(macro_name, macro_value)

            members.append(f"    {' '.join(member_text.split())};")

        # Check if this is a struct or a union to emit the correct C syntax
        category = struct_xml.get('category', 'struct')
        ffi_declarations.append(f"typedef {category} {struct_name} {{\n" + "\n".join(members) + f"\n}} {struct_name};")
        emitted.add(struct_name)

    for s in resolved_structs:
        emit_struct(s)

    # 5. Grab Functions
    ffi_declarations.append("\n// --- Functions ---")
    for command in root.findall('.//commands/command'):
        if command.get('alias'):
            continue

        proto = command.find('proto')
        if proto is None:
            continue

        name_elem = proto.find('name')
        if name_elem is None or name_elem.text not in TARGET_FUNCTIONS:
            continue

        if name_elem.text in seen_funcs:
            continue
        seen_funcs.add(name_elem.text)

        signature = "".join(proto.itertext()).strip()
        params = []
        for param in command.findall('param'):
            for comment in param.findall('comment'):
                param.remove(comment)

            param_text = "".join(param.itertext()).strip()
            param_text = " ".join(param_text.split())

            # SURGICAL PATCH: Prevent duplicate parameters in the signature
            if param_text not in params:
                params.append(param_text)

        param_str = ", ".join(params) if params else "void"
        ffi_declarations.append(f"{signature}({param_str});")

    return ffi_declarations

if __name__ == "__main__":
    output_filename = "vulkan_headers.lua"

    with open(output_filename, "w") as f:
        # --- LUA MODULE HEADER ---
        f.write("local ffi = require('ffi')\n")
        f.write("ffi.cdef[[\n")

        # Base types needed to keep LuaJIT happy
        f.write("// --- Base Types ---\n")
        # f.write("typedef uint32_t VkFlags;\n")
        # f.write("typedef uint64_t VkFlags64;\n")
        # f.write("typedef uint64_t VkDeviceSize;\n")
        # f.write("typedef uint32_t VkBool32;\n")
        # f.write("typedef uint64_t VkDeviceAddress;\n")

        f.write("typedef void* PFN_vkVoidFunction;\n")
        f.write("typedef void* PFN_vkAllocationFunction;\n")
        f.write("typedef void* PFN_vkReallocationFunction;\n")
        f.write("typedef void* PFN_vkFreeFunction;\n")
        f.write("typedef void* PFN_vkInternalAllocationNotification;\n")
        f.write("typedef void* PFN_vkInternalFreeNotification;\n\n")

        # Generate and write the parsed declarations
        declarations = generate_lua_ffi_cdef("vk.xml")
        for decl in declarations:
            f.write(decl + "\n")

        # --- LUA MODULE FOOTER ---
        f.write("]]\n")
        f.write("return true\n")

    print(f"[PARSE] Successfully generated {output_filename}!")
