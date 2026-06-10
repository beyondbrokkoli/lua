// main.c
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <stdbool.h>
#include <stdatomic.h>
#include <stdalign.h>

#define GLFW_INCLUDE_VULKAN
#include <GLFW/glfw3.h>

// Activate the Vulkan structs for the main thread
#define VX_ENABLE_VULKAN_STRUCTS
#include "shared_structs.h"

#include <lua.h>
#include <lualib.h>
#include <lauxlib.h>

#if defined(_WIN32)
#define EXPORT __declspec(dllexport)
#else
#define EXPORT __attribute__((visibility("default")))
#endif

#include <pthread.h>
#include <unistd.h>
#define SLEEP_MS(ms) usleep((ms) * 1000)

typedef pthread_t vmath_thread_t;
#define THREAD_FUNC void*
#define THREAD_RETURN_VAL NULL
#if defined(_WIN32) || defined(_WIN64)
    #include <windows.h>
    #include <timeapi.h>
    #pragma comment(lib, "winmm.lib")
#endif
static vmath_thread_t vmath_thread_start(void* (*func)(void*), void* arg) {
    pthread_t thread;
    pthread_create(&thread, NULL, func, arg);
    return thread;
}

static void vmath_thread_join(vmath_thread_t thread) {
    pthread_join(thread, NULL);
}

#define CMD_IDLE 0
#define CMD_BOOT_WINDOW 1
#define CMD_KILL_WINDOW 2

static bool s_is_fullscreen = false;
static int s_win_x = 0, s_win_y = 0;
static int s_win_w = 1280, s_win_h = 720;

typedef struct {
    alignas(64) _Atomic int ready_index;
    _Atomic int is_running;
    _Atomic int lua_finished;
    _Atomic(void*) vk_instance;
    _Atomic(void*) vk_surface;
    _Atomic int glfw_cmd;
    _Atomic int glfw_arg_w;
    _Atomic int glfw_arg_h;
    _Atomic int last_key_pressed;
    _Atomic uint32_t wasd_mask;
    _Atomic float mouse_dx;
    _Atomic float mouse_dy;

    _Atomic float mouse_x;  // Add this
    _Atomic float mouse_y;  // Add this
    _Atomic float click_x;  // NEW: Hardware-latched X
    _Atomic float click_y;  // NEW: Hardware-latched Y
    _Atomic int mouse_captured; // NEW: Track the F10 toggle state

    _Atomic int window_resized;
    _Atomic int win_w;
    _Atomic int win_h;
    _Atomic int mouse_left;
    _Atomic int mouse_right;
    _Atomic int key_space;
} IPC_Mailbox;

typedef struct {
    IPC_Mailbox mailbox;
    int render_index;
    int write_index;
} EngineState;

static EngineState g_engine;

EXPORT int vx_core_is_running() { return atomic_load_explicit(&g_engine.mailbox.is_running, memory_order_relaxed); }
EXPORT void vx_core_shutdown() { atomic_store_explicit(&g_engine.mailbox.is_running, 0, memory_order_release); }
EXPORT void vx_core_mark_finished() { atomic_store_explicit(&g_engine.mailbox.lua_finished, 1, memory_order_release); }
EXPORT const char** vx_sys_glfw_extensions(uint32_t* count) { return glfwGetRequiredInstanceExtensions(count); }
EXPORT void vx_sys_publish_instance(void* instance) { atomic_store_explicit(&g_engine.mailbox.vk_instance, instance, memory_order_release); }
EXPORT void* vx_sys_get_surface() { return atomic_load_explicit(&g_engine.mailbox.vk_surface, memory_order_acquire); }

EXPORT void vx_sys_set_cmd(int cmd, int w, int h) {
    atomic_store_explicit(&g_engine.mailbox.glfw_arg_w, w, memory_order_relaxed);
    atomic_store_explicit(&g_engine.mailbox.glfw_arg_h, h, memory_order_relaxed);
    atomic_store_explicit(&g_engine.mailbox.glfw_cmd, cmd, memory_order_release);
}

EXPORT int vx_input_last_key() {
    return atomic_exchange_explicit(&g_engine.mailbox.last_key_pressed, 0, memory_order_acquire);
}

double last_mx = 0.0, last_my = 0.0;
bool first_mouse = true;
static bool s_mouse_captured = false;

void glfw_cursor_callback(GLFWwindow* window, double xpos, double ypos) {
    // Record absolute coordinates for Lua edge-panning
    atomic_store_explicit(&g_engine.mailbox.mouse_x, (float)xpos, memory_order_release);
    atomic_store_explicit(&g_engine.mailbox.mouse_y, (float)ypos, memory_order_release);

    // 3. Normal Delta Calculation
    float dx = (float)(xpos - last_mx);
    float dy = (float)(ypos - last_my);
    last_mx = xpos; last_my = ypos;

    float current_dx = atomic_load_explicit(&g_engine.mailbox.mouse_dx, memory_order_acquire);
    while (!atomic_compare_exchange_weak_explicit(&g_engine.mailbox.mouse_dx, &current_dx, current_dx + dx, memory_order_release, memory_order_relaxed));

    float current_dy = atomic_load_explicit(&g_engine.mailbox.mouse_dy, memory_order_acquire);
    while (!atomic_compare_exchange_weak_explicit(&g_engine.mailbox.mouse_dy, &current_dy, current_dy + dy, memory_order_release, memory_order_relaxed));
}

void glfw_mouse_button_callback(GLFWwindow* window, int button, int action, int mods) {
    if (button == GLFW_MOUSE_BUTTON_LEFT) {
        if (action == GLFW_PRESS) {
            double cx, cy;
            glfwGetCursorPos(window, &cx, &cy); // Get exact coordinate of the event
            atomic_store_explicit(&g_engine.mailbox.click_x, (float)cx, memory_order_release);
            atomic_store_explicit(&g_engine.mailbox.click_y, (float)cy, memory_order_release);
            atomic_store_explicit(&g_engine.mailbox.mouse_left, 1, memory_order_release);
        } else {
            atomic_store_explicit(&g_engine.mailbox.mouse_left, 0, memory_order_release);
        }
    } else if (button == GLFW_MOUSE_BUTTON_RIGHT) {
        // DO NOT LOSE THIS!
        atomic_store_explicit(&g_engine.mailbox.mouse_right, (action == GLFW_PRESS) ? 1 : 0, memory_order_release);
    }
}

EXPORT int vx_input_mouse_btn(int btn) {
    if (btn == 0) return atomic_load_explicit(&g_engine.mailbox.mouse_left, memory_order_acquire);
    if (btn == 1) return atomic_load_explicit(&g_engine.mailbox.mouse_right, memory_order_acquire);
    return 0;
}

EXPORT float vx_input_mouse_x() { return atomic_load_explicit(&g_engine.mailbox.mouse_x, memory_order_acquire); }
EXPORT float vx_input_mouse_y() { return atomic_load_explicit(&g_engine.mailbox.mouse_y, memory_order_acquire); }

EXPORT float vx_input_click_x() {
    return atomic_load_explicit(&g_engine.mailbox.click_x, memory_order_acquire);
}
EXPORT float vx_input_click_y() {
    return atomic_load_explicit(&g_engine.mailbox.click_y, memory_order_acquire);
}

EXPORT int vx_input_is_captured() {
    return atomic_load_explicit(&g_engine.mailbox.mouse_captured, memory_order_acquire);
}

void glfw_key_callback(GLFWwindow* window, int key, int scancode, int action, int mods) {
    if (action == GLFW_PRESS || action == GLFW_RELEASE) {
        uint32_t bit = 0;
        if (key == GLFW_KEY_W) bit = 1; else if (key == GLFW_KEY_S) bit = 2;
        else if (key == GLFW_KEY_A) bit = 4; else if (key == GLFW_KEY_D) bit = 8;
        else if (key == GLFW_KEY_E) bit = 16; else if (key == GLFW_KEY_Q) bit = 32;
        if (bit) {
            uint32_t mask = atomic_load_explicit(&g_engine.mailbox.wasd_mask, memory_order_acquire);
            uint32_t new_mask;
            do {
                new_mask = (action == GLFW_PRESS) ? (mask | bit) : (mask & ~bit);
            } while(!atomic_compare_exchange_weak_explicit(&g_engine.mailbox.wasd_mask, &mask, new_mask, memory_order_release, memory_order_relaxed));
        }
    }
    if (key == GLFW_KEY_ESCAPE && action == GLFW_PRESS) {
        // Instantly trigger shutdown, no mouse-capture gatekeeping needed
        atomic_store_explicit(&g_engine.mailbox.last_key_pressed, GLFW_KEY_ESCAPE, memory_order_release);
    }
    if (key == GLFW_KEY_SPACE) {
        // 1 means pressed or held, 0 means released
        atomic_store_explicit(&g_engine.mailbox.key_space, (action != GLFW_RELEASE) ? 1 : 0, memory_order_release);
    }
    // === F11 NATIVE FULLSCREEN TOGGLE ===
    if (key == GLFW_KEY_F11 && action == GLFW_PRESS) {
        if (!s_is_fullscreen) {
            // 1. Save the exact window position and size before maximizing
            glfwGetWindowPos(window, &s_win_x, &s_win_y);
            glfwGetWindowSize(window, &s_win_w, &s_win_h);

            // 2. Get the primary monitor and its native resolution
            GLFWmonitor* monitor = glfwGetPrimaryMonitor();
            const GLFWvidmode* mode = glfwGetVideoMode(monitor);

            // 3. Switch to borderless fullscreen on that monitor
            glfwSetWindowMonitor(window, monitor, 0, 0, mode->width, mode->height, mode->refreshRate);
            s_is_fullscreen = true;
            printf("[C-CORE] Native Fullscreen Engaged (%dx%d @ %dHz)\n", mode->width, mode->height, mode->refreshRate);
        } else {
            // Restore back to the exact windowed state
            glfwSetWindowMonitor(window, NULL, s_win_x, s_win_y, s_win_w, s_win_h, 0);
            s_is_fullscreen = false;
            printf("[C-CORE] Windowed Mode Restored\n");
        }
    }
    // THE MOUSE RELAY TOGGLE
    if (key == GLFW_KEY_F10 && action == GLFW_PRESS) {
        int is_cap = atomic_load_explicit(&g_engine.mailbox.mouse_captured, memory_order_acquire);
        is_cap = !is_cap; // Flip the state
        atomic_store_explicit(&g_engine.mailbox.mouse_captured, is_cap, memory_order_release);

        if (is_cap) {
            glfwSetInputMode(window, GLFW_CURSOR, GLFW_CURSOR_CAPTURED);
            printf("[C-CORE] Mouse Clamped to Window (F10)\n");
        } else {
            glfwSetInputMode(window, GLFW_CURSOR, GLFW_CURSOR_NORMAL);
            printf("[C-CORE] Mouse Freed (F10)\n");
        }
    }
    if (action == GLFW_PRESS) {
        if (key == GLFW_KEY_1 || key == GLFW_KEY_2 || key == GLFW_KEY_3 || key == GLFW_KEY_4 || key == GLFW_KEY_F5 || key == GLFW_KEY_ENTER || key == GLFW_KEY_KP_ENTER) {
            atomic_store_explicit(&g_engine.mailbox.last_key_pressed, key, memory_order_release);
        }
    }
}

VkDebugUtilsMessengerEXT g_debugMessenger = VK_NULL_HANDLE;

static VKAPI_ATTR VkBool32 VKAPI_CALL vulkan_debug_callback(
    VkDebugUtilsMessageSeverityFlagBitsEXT messageSeverity,
    VkDebugUtilsMessageTypeFlagsEXT messageType,
    const VkDebugUtilsMessengerCallbackDataEXT* pCallbackData,
    void* pUserData) {

    if (messageSeverity < VK_DEBUG_UTILS_MESSAGE_SEVERITY_WARNING_BIT_EXT) {
        return VK_FALSE;
    }
    printf("\n[VULKAN LAYER ENFORCER]\nSEVERITY: %d\nMESSAGE: %s\n\n",
           messageSeverity, pCallbackData->pMessage);
    fflush(stdout);
    return VK_FALSE;
}

EXPORT void vx_sys_inject_validation(void* instance_ptr) {
    VkInstance instance = (VkInstance)instance_ptr;
    VkDebugUtilsMessengerCreateInfoEXT createInfo = {0};
    createInfo.sType = VK_STRUCTURE_TYPE_DEBUG_UTILS_MESSENGER_CREATE_INFO_EXT;
    createInfo.messageSeverity = VK_DEBUG_UTILS_MESSAGE_SEVERITY_VERBOSE_BIT_EXT |
                                 VK_DEBUG_UTILS_MESSAGE_SEVERITY_WARNING_BIT_EXT |
                                 VK_DEBUG_UTILS_MESSAGE_SEVERITY_ERROR_BIT_EXT;
    createInfo.messageType = VK_DEBUG_UTILS_MESSAGE_TYPE_GENERAL_BIT_EXT |
                             VK_DEBUG_UTILS_MESSAGE_TYPE_VALIDATION_BIT_EXT |
                             VK_DEBUG_UTILS_MESSAGE_TYPE_PERFORMANCE_BIT_EXT;
    createInfo.pfnUserCallback = vulkan_debug_callback;

    PFN_vkCreateDebugUtilsMessengerEXT func = (PFN_vkCreateDebugUtilsMessengerEXT)
        glfwGetInstanceProcAddress(instance, "vkCreateDebugUtilsMessengerEXT");

    if (func != NULL) {
        func(instance, &createInfo, NULL, &g_debugMessenger);
        printf("[C-CORE] Validation Layer Enforcer Injected Successfully!\n");
    } else {
        printf("[C-FATAL] Failed to setup debug messenger (VK_EXT_debug_utils not found).\n");
    }
}

EXPORT void vx_sys_eject_validation(void* instance) {
    PFN_vkDestroyDebugUtilsMessengerEXT destroyFn =
        (PFN_vkDestroyDebugUtilsMessengerEXT)vkGetInstanceProcAddr(
            (VkInstance)instance,
            "vkDestroyDebugUtilsMessengerEXT"
        );

    if (destroyFn != NULL) {
        destroyFn((VkInstance)instance, g_debugMessenger, NULL);
    }
}

EXPORT uint32_t vx_input_wasd() { return atomic_load_explicit(&g_engine.mailbox.wasd_mask, memory_order_acquire); }
EXPORT float vx_input_mouse_dx() { return atomic_exchange_explicit(&g_engine.mailbox.mouse_dx, 0.0f, memory_order_acquire); }
EXPORT float vx_input_mouse_dy() { return atomic_exchange_explicit(&g_engine.mailbox.mouse_dy, 0.0f, memory_order_acquire); }
EXPORT int vx_sys_resize_flag() { return atomic_exchange_explicit(&g_engine.mailbox.window_resized, 0, memory_order_acquire); }
EXPORT void vx_sys_window_size(int* w, int* h) {
    *w = atomic_load_explicit(&g_engine.mailbox.win_w, memory_order_acquire);
    *h = atomic_load_explicit(&g_engine.mailbox.win_h, memory_order_acquire);
}

void glfw_framebuffer_size_callback(GLFWwindow* window, int width, int height) {
    if (width == 0 || height == 0) return;
    atomic_store_explicit(&g_engine.mailbox.win_w, width, memory_order_release);
    atomic_store_explicit(&g_engine.mailbox.win_h, height, memory_order_release);
    atomic_store_explicit(&g_engine.mailbox.window_resized, 1, memory_order_release);
}
EXPORT int vx_input_spacebar() {
    return atomic_load_explicit(&g_engine.mailbox.key_space, memory_order_acquire);
}


#define RING_SIZE 4
#define LOAD(var) atomic_load_explicit(&(var), memory_order_acquire)
#define STORE(var, val) atomic_store_explicit(&(var), (val), memory_order_release)

// FFI-CONTRACT: RenderPacket mapping
typedef struct {
    alignas(64) RenderPacket packets[RING_SIZE];
    alignas(64) _Atomic int ready_idx;
    alignas(64) _Atomic uint32_t locked_mask; // REPLACES read_idx
} RenderRing;

// INSTANTIATE WITH MASK
static RenderRing g_ring = { .ready_idx = -1, .locked_mask = 0 };
static RenderThreadInit g_wsi;
static vmath_thread_t g_render_thread;
static atomic_int g_render_thread_active = 0;

// [NEW] Global Command Pool Tracking
static VkCommandPool g_render_cmd_pool = VK_NULL_HANDLE;
static VkCommandPool g_transfer_cmd_pool = VK_NULL_HANDLE;

// DIRECTIVE ZETA: ASYNC TRANSFER OVERLORD
typedef struct {
    uint64_t src_buffer;
    uint64_t dst_buffer;
    uint64_t size;
    uint64_t timeline_sem;
    uint64_t signal_val;
    alignas(64) _Atomic int status; // 0 = FREE, 1 = PENDING, 2 = COMPLETE
} TransferJob;

#define TRANSFER_RING_SIZE 4
static TransferJob g_transfer_ring[TRANSFER_RING_SIZE];
static uint32_t g_transfer_family_idx = 0;
static vmath_thread_t g_transfer_thread;
static atomic_int g_transfer_thread_active = 0;

EXPORT void vx_transfer_setup(uint32_t q_family_index) {
    g_transfer_family_idx = q_family_index;
    for(int i = 0; i < TRANSFER_RING_SIZE; i++) {
        atomic_init(&g_transfer_ring[i].status, 0);
    }
}

EXPORT int vx_transfer_request(uint64_t src, uint64_t dst, uint64_t size, uint64_t t_sem, uint64_t sig_val) {
    for(int i = 0; i < TRANSFER_RING_SIZE; i++) {
        int expected = 0; // FREE
        if (atomic_compare_exchange_strong_explicit(&g_transfer_ring[i].status, &expected, 1, memory_order_acquire, memory_order_relaxed)) {
            g_transfer_ring[i].src_buffer = src;
            g_transfer_ring[i].dst_buffer = dst;
            g_transfer_ring[i].size = size;
            g_transfer_ring[i].timeline_sem = t_sem;
            g_transfer_ring[i].signal_val = sig_val;

            // Mark as PENDING for the transfer thread to pick up
            atomic_store_explicit(&g_transfer_ring[i].status, 2, memory_order_release);
            return 1;
        }
    }
    return 0; // Mailbox full, Lua must yield
}

THREAD_FUNC transfer_thread_loop(void* arg) {
    printf("[C-CORE] Async Transfer Overlord Online.\n");

    VkCommandPool cmd_pool;
    VkCommandPoolCreateInfo pool_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
        .queueFamilyIndex = g_transfer_family_idx // Strict Domain Isolation
    };
    // Replace the local pool declaration
    vkCreateCommandPool(g_wsi.device, &pool_info, NULL, &g_transfer_cmd_pool);

    VkCommandBuffer cmd;
    VkCommandBufferAllocateInfo alloc_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = g_transfer_cmd_pool, // Use the global
        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
        .commandBufferCount = 1
    };
    vkAllocateCommandBuffers(g_wsi.device, &alloc_info, &cmd);

    PFN_vkQueueSubmit pfnSubmit = (PFN_vkQueueSubmit)g_wsi.vkQueueSubmit;

    while (atomic_load_explicit(&g_transfer_thread_active, memory_order_acquire) && atomic_load_explicit(&g_engine.mailbox.is_running, memory_order_acquire)) {
        bool worked = false;

        for(int i = 0; i < TRANSFER_RING_SIZE; i++) {
            if (atomic_load_explicit(&g_transfer_ring[i].status, memory_order_acquire) == 2) {
                TransferJob* job = &g_transfer_ring[i];

                vkResetCommandBuffer(cmd, 0);
                VkCommandBufferBeginInfo beginInfo = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
                vkBeginCommandBuffer(cmd, &beginInfo);

                // Hardware DMA Copy Command
                VkBufferCopy copyRegion = { .srcOffset = 0, .dstOffset = 0, .size = job->size };
                vkCmdCopyBuffer(cmd, (VkBuffer)job->src_buffer, (VkBuffer)job->dst_buffer, 1, &copyRegion);

                vkEndCommandBuffer(cmd);

                // Vulkan 1.2 Timeline Linkage
                VkTimelineSemaphoreSubmitInfo timelineInfo = {
                    .sType = 1000207003, // VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO
                    .signalSemaphoreValueCount = 1,
                    .pSignalSemaphoreValues = &job->signal_val
                };

                VkSubmitInfo submitInfo = {
                    .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                    .pNext = &timelineInfo,
                    .commandBufferCount = 1,
                    .pCommandBuffers = &cmd,
                    .signalSemaphoreCount = 1,
                    .pSignalSemaphores = (VkSemaphore*)&job->timeline_sem
                };

                // Submit directly to the dedicated Transfer Queue!
                printf("[C-CORE] Submitting DMA Transfer. Timeline Signal Value: %llu\n", (unsigned long long)job->signal_val);
                pfnSubmit(g_wsi.transfer_queue, 1, &submitInfo, VK_NULL_HANDLE);

                // Free the mailbox slot immediately.
                // Lua checks the Semaphore value, not this mailbox, to know when it's done!
                atomic_store_explicit(&job->status, 0, memory_order_release);
                worked = true;
            }
        }

        // Zero-overhead sleep if no jobs exist
        if (!worked) SLEEP_MS(1);
    }

    printf("[C-CORE] Async Transfer Thread gracefully terminated.\n");
    return NULL;
}

EXPORT void vx_stream_init(RenderThreadInit* wsi) {
    g_wsi = *wsi;

    // Purge stale packets and zero the lock mask across WSI rebuilds
    atomic_store_explicit(&g_ring.ready_idx, -1, memory_order_release);
    atomic_store_explicit(&g_ring.locked_mask, 0, memory_order_release);
}

EXPORT RenderPacket* vx_stream_packet(int idx) {
    return &g_ring.packets[idx];
}

EXPORT int vx_stream_acquire() {
    uint32_t mask = LOAD(g_ring.locked_mask);
    int ready = LOAD(g_ring.ready_idx);

    // Search forward from the last ready index for the ONE free slot
    for (int i = 1; i <= RING_SIZE; i++) {
        int idx = (ready + i) % RING_SIZE;
        if ((mask & (1 << idx)) == 0) {
            return idx; // Found the safe slot!
        }
    }
    // THE FIX: Do not return 0. Return -1 to indicate ring saturation.
    return -1;
}

EXPORT void vx_stream_commit(int idx) {
    // FORCE HARDWARE MEMORY FLUSH:
    // Guarantees all previous ffi.copy and pointer assignments from Lua
    // are visible in RAM before the C-Core is allowed to read this slot.
    atomic_thread_fence(memory_order_release);
    STORE(g_ring.ready_idx, idx);
}

EXPORT void vx_record_commands(VkCommandBuffer cmd, RenderPacket* p, DrawCommand* queue, uint32_t count, PFN_vkCmdBeginRenderingKHR pfnBegin, PFN_vkCmdEndRenderingKHR pfnEnd) {
    VkCommandBufferBeginInfo beginInfo = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
    vkBeginCommandBuffer(cmd, &beginInfo);

    // 2. Setup Render Pass Barriers (Cleansed of ID Buffer logic)
    VkImageMemoryBarrier preBarriers[2] = {0};
    preBarriers[0].sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER;
    preBarriers[0].oldLayout = VK_IMAGE_LAYOUT_UNDEFINED;
    preBarriers[0].newLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL;
    preBarriers[0].image = (VkImage)p->swapchain_image;
    preBarriers[0].subresourceRange = (VkImageSubresourceRange){VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
    preBarriers[0].dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT;

    preBarriers[1].sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER;
    preBarriers[1].oldLayout = VK_IMAGE_LAYOUT_UNDEFINED;
    preBarriers[1].newLayout = VK_IMAGE_LAYOUT_DEPTH_ATTACHMENT_OPTIMAL;
    preBarriers[1].image = (VkImage)p->depth_image;
    preBarriers[1].subresourceRange = (VkImageSubresourceRange){VK_IMAGE_ASPECT_DEPTH_BIT, 0, 1, 0, 1};
    preBarriers[1].dstAccessMask = VK_ACCESS_DEPTH_STENCIL_ATTACHMENT_WRITE_BIT;

    // Standard Fast-Path Barrier: Wait on TOP_OF_PIPE
    vkCmdPipelineBarrier(cmd,
        VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
        VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT | VK_PIPELINE_STAGE_EARLY_FRAGMENT_TESTS_BIT,
        0, 0, NULL, 0, NULL, 2, preBarriers);

    VkRenderingAttachmentInfoKHR colorAttachment = {0};
    colorAttachment.sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO_KHR;
    colorAttachment.imageView = (VkImageView)p->swapchain_view;
    colorAttachment.imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL;
    colorAttachment.loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR;
    colorAttachment.storeOp = VK_ATTACHMENT_STORE_OP_STORE;
    colorAttachment.clearValue.color.float32[0] = 0.01f;
    colorAttachment.clearValue.color.float32[1] = 0.01f;
    colorAttachment.clearValue.color.float32[2] = 0.02f;
    colorAttachment.clearValue.color.float32[3] = 1.0f;

    VkRenderingAttachmentInfoKHR depthAttachment = {
        .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO_KHR,
        .imageView = (VkImageView)p->depth_view,
        .imageLayout = VK_IMAGE_LAYOUT_DEPTH_ATTACHMENT_OPTIMAL,
        .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
        .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
        .clearValue.depthStencil = {1.0f, 0}
    };

    VkRenderingInfoKHR renderInfo = {
        .sType = VK_STRUCTURE_TYPE_RENDERING_INFO_KHR,
        .renderArea.extent = {p->width, p->height},
        .layerCount = 1,
        .colorAttachmentCount = 1,
        .pColorAttachments = &colorAttachment,
        .pDepthAttachment = &depthAttachment
    };
    pfnBegin(cmd, &renderInfo);

    // 3. Global Graphics State Setup
    VkViewport viewport = {0.0f, 0.0f, (float)p->width, (float)p->height, 0.0f, 1.0f};
    VkRect2D scissor = {{0, 0}, {p->width, p->height}};
    vkCmdSetViewport(cmd, 0, 1, &viewport);
    vkCmdSetScissor(cmd, 0, 1, &scissor);

    VkDeviceSize offset = 0;
    VkBuffer vbo = (VkBuffer)p->vertex_buffer;
    vkCmdBindVertexBuffers(cmd, 0, 1, &vbo, &offset);

    // --- BIND THE INDEX BUFFER ---
    VkBuffer ibo = (VkBuffer)p->index_buffer;
    vkCmdBindIndexBuffer(cmd, ibo, 0, VK_INDEX_TYPE_UINT32);

    PFN_vkCmdSetCullModeEXT vkCmdSetCullModeEXT = (PFN_vkCmdSetCullModeEXT)g_wsi.pfnSetCullMode;
    PFN_vkCmdSetFrontFaceEXT vkCmdSetFrontFaceEXT = (PFN_vkCmdSetFrontFaceEXT)g_wsi.pfnSetFrontFace;
    PFN_vkCmdSetPrimitiveTopologyEXT vkCmdSetPrimitiveTopologyEXT = (PFN_vkCmdSetPrimitiveTopologyEXT)g_wsi.pfnSetPrimitiveTopology;
    PFN_vkCmdSetDepthTestEnableEXT vkCmdSetDepthTestEnableEXT = (PFN_vkCmdSetDepthTestEnableEXT)g_wsi.pfnSetDepthTestEnable;
    PFN_vkCmdSetDepthWriteEnableEXT vkCmdSetDepthWriteEnableEXT = (PFN_vkCmdSetDepthWriteEnableEXT)g_wsi.pfnSetDepthWriteEnable;
    PFN_vkCmdSetDepthCompareOpEXT vkCmdSetDepthCompareOpEXT = (PFN_vkCmdSetDepthCompareOpEXT)g_wsi.pfnSetDepthCompareOp;

    // 4. Data-Oriented Queue Execution
    uint64_t current_pipeline = 0;
    uint64_t current_descriptor = 0;

    for (uint32_t i = 0; i < count; i++) {
        DrawCommand* draw = &queue[i];

        if (draw->pipeline_id != current_pipeline) {
            vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, (VkPipeline)draw->pipeline_id);
            current_pipeline = draw->pipeline_id;
        }

        if (draw->descriptor_set != current_descriptor) {
            VkDescriptorSet dset = (VkDescriptorSet)draw->descriptor_set;
            vkCmdBindDescriptorSets(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, (VkPipelineLayout)p->gfx_layout, 0, 1, &dset, 0, NULL);
            current_descriptor = draw->descriptor_set;
        }

        VkRect2D scissor = {
            .offset = { (int32_t)draw->scissor_x, (int32_t)draw->scissor_y },
            .extent = { (uint32_t)draw->scissor_w, (uint32_t)draw->scissor_h }
        };

        vkCmdSetScissor(cmd, 0, 1, &scissor);
        vkCmdSetCullModeEXT(cmd, draw->cull_mode);
        vkCmdSetFrontFaceEXT(cmd, draw->front_face);
        vkCmdSetPrimitiveTopologyEXT(cmd, draw->topology);
        vkCmdSetDepthTestEnableEXT(cmd, draw->depth_test);
        vkCmdSetDepthWriteEnableEXT(cmd, draw->depth_write);
        vkCmdSetDepthCompareOpEXT(cmd, draw->depth_compare_op);

        vkCmdPushConstants(
            cmd, (VkPipelineLayout)p->gfx_layout,
            VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT,
            draw->pc_offset, draw->pc_size, draw->push_constants + draw->pc_offset
        );

        vkCmdDrawIndexed(cmd,
            draw->index_count,
            draw->instance_count,
            draw->first_index,
            draw->vertex_offset,
            draw->first_instance
        );
    }

    pfnEnd(cmd);

    // 5. Present Barrier
    VkImageMemoryBarrier presentBarrier = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
        .oldLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
        .newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR,
        .image = (VkImage)p->swapchain_image,
        .subresourceRange = (VkImageSubresourceRange){VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
        .srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
        .dstAccessMask = 0
    };
    vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, NULL, 0, NULL, 1, &presentBarrier);
    vkEndCommandBuffer(cmd);
}

THREAD_FUNC render_thread_loop(void* arg) {
    printf("[C-CORE] Async Render Thread Online.\n");

    // 1. C-Owned Command Pool Setup
    VkCommandPool cmd_pool;
    VkCommandPoolCreateInfo pool_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
        .queueFamilyIndex = 0 // Assuming Graphics queue index is 0 in your setup
    };
    // Replace the local pool declaration
    vkCreateCommandPool(g_wsi.device, &pool_info, NULL, &g_render_cmd_pool);

    VkCommandBuffer cmd_buffers[3];
    VkCommandBufferAllocateInfo alloc_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = g_render_cmd_pool, // Use the global
        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
        .commandBufferCount = 3
    };
    vkAllocateCommandBuffers(g_wsi.device, &alloc_info, cmd_buffers);

    uint32_t current_frame = 0;
    int local_read = -1;
    int active_ring_slots[3] = {-1, -1, -1}; // NEW: Tracks VRAM in flight

    // Added array to map Swapchain Image Indices to Fences
    VkFence image_fences[10];
    for (int i = 0; i < 10; i++) {
        image_fences[i] = VK_NULL_HANDLE;
    }

    // Typecast Vulkan WSI Pointers
    PFN_vkWaitForFences pfnWait = (PFN_vkWaitForFences)g_wsi.vkWaitForFences;
    PFN_vkAcquireNextImageKHR pfnAcquire = (PFN_vkAcquireNextImageKHR)g_wsi.vkAcquireNextImageKHR;
    PFN_vkResetFences pfnReset = (PFN_vkResetFences)g_wsi.vkResetFences;
    PFN_vkQueueSubmit pfnSubmit = (PFN_vkQueueSubmit)g_wsi.vkQueueSubmit;
    PFN_vkQueuePresentKHR pfnPresent = (PFN_vkQueuePresentKHR)g_wsi.vkQueuePresentKHR;

    while (atomic_load_explicit(&g_render_thread_active, memory_order_acquire) &&
           atomic_load_explicit(&g_engine.mailbox.is_running, memory_order_acquire)) {

        // 1. Grab the latest frame from Lua
        int ready = atomic_load_explicit(&g_ring.ready_idx, memory_order_acquire);
        if (ready == -1 || ready == local_read) {
            SLEEP_MS(1);
            continue;
        }
        local_read = ready;

        // THE TEMPORAL SEAL: INSTANT LOCK
        // Lock the slot immediately so Lua cannot lap the ring
        // and overwrite it while we sleep on the Vulkan Fence!
        atomic_fetch_or_explicit(&g_ring.locked_mask, (1 << local_read), memory_order_release);

        // 2. Safe to sleep on the GPU WSI
        pfnWait(g_wsi.device, 1, &g_wsi.in_flight[current_frame], VK_TRUE, UINT64_MAX);

        // 3. UNLOCK: The GPU has finished drawing the oldest frame. Give the slot back to Lua.
        int finished_slot = active_ring_slots[current_frame];
        if (finished_slot != -1 && finished_slot != local_read) {
            atomic_fetch_and_explicit(&g_ring.locked_mask, ~(1 << finished_slot), memory_order_release);
        }

        active_ring_slots[current_frame] = local_read;

        // 4. Safely read the sealed packet
        RenderPacket* p = &g_ring.packets[local_read];
        VkCommandBuffer cmd = cmd_buffers[current_frame];

        // 1. Wait on CPU ring buffer fence for THIS command buffer slot
        pfnWait(g_wsi.device, 1, &g_wsi.in_flight[current_frame], VK_TRUE, UINT64_MAX);

        // 2. Acquire Image (Signaling the CPU's current_frame semaphore)
        uint32_t img_idx;
        VkResult res = pfnAcquire(g_wsi.device, g_wsi.swapchain, UINT64_MAX, g_wsi.image_available[current_frame], VK_NULL_HANDLE, &img_idx);

        if (res == VK_ERROR_OUT_OF_DATE_KHR) {
            atomic_store_explicit(&g_engine.mailbox.window_resized, 1, memory_order_release);
            SLEEP_MS(10);
            continue;
        }

        // 3. Reset the fence for this slot
        pfnReset(g_wsi.device, 1, &g_wsi.in_flight[current_frame]);

        p->swapchain_image = g_wsi.swapchain_images[img_idx];
        p->swapchain_view = g_wsi.swapchain_views[img_idx];

        vkResetCommandBuffer(cmd, 0);
        vx_record_commands(cmd, p, p->draw_queue, p->draw_count, (PFN_vkCmdBeginRenderingKHR)g_wsi.pfnBegin, (PFN_vkCmdEndRenderingKHR)g_wsi.pfnEnd);

        VkPipelineStageFlags waitStage = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;

        // 4. Submit (Wait on CPU frame semaphore, Signal the GPU IMAGE semaphore)
        VkSubmitInfo submitInfo = {
            .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
            .waitSemaphoreCount = 1,
            .pWaitSemaphores = &g_wsi.image_available[current_frame],
            .pWaitDstStageMask = &waitStage,
            .commandBufferCount = 1,
            .pCommandBuffers = &cmd,
            .signalSemaphoreCount = 1,
            .pSignalSemaphores = &g_wsi.render_finished[img_idx] // <-- TIED TO HARDWARE IMAGE
        };
        pfnSubmit(g_wsi.queue, 1, &submitInfo, g_wsi.in_flight[current_frame]);

        // 5. Present (Wait on the GPU IMAGE semaphore)
        VkPresentInfoKHR presentInfo = {
            .sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR,
            .waitSemaphoreCount = 1,
            .pWaitSemaphores = &g_wsi.render_finished[img_idx], // <-- TIED TO HARDWARE IMAGE
            .swapchainCount = 1,
            .pSwapchains = &g_wsi.swapchain,
            .pImageIndices = &img_idx
        };
        pfnPresent(g_wsi.queue, &presentInfo);

        // Keep CPU Ring Buffer locked to 3 slots
        current_frame = (current_frame + 1) % 3;

    }

    printf("[C-CORE] Async Render Thread gracefully terminated and pool destroyed.\n");
    return NULL;
}

EXPORT void vx_thread_start() {
    atomic_store_explicit(&g_render_thread_active, 1, memory_order_release);
    atomic_store_explicit(&g_transfer_thread_active, 1, memory_order_release);
    g_render_thread = vmath_thread_start(render_thread_loop, NULL);
    g_transfer_thread = vmath_thread_start(transfer_thread_loop, NULL);
}

EXPORT void vx_thread_kill() {
    // 1. Signal threads to stop
    atomic_store_explicit(&g_render_thread_active, 0, memory_order_release);
    atomic_store_explicit(&g_transfer_thread_active, 0, memory_order_release);

    // 2. Wait for them to physically exit
    vmath_thread_join(g_render_thread);
    vmath_thread_join(g_transfer_thread);

    // 3. NOW it is 100% safe to lock the device
    vkDeviceWaitIdle(g_wsi.device);

    // 4. Sweep memory
    if (g_render_cmd_pool) {
        vkDestroyCommandPool(g_wsi.device, g_render_cmd_pool, NULL);
        g_render_cmd_pool = VK_NULL_HANDLE;
    }
    if (g_transfer_cmd_pool) {
        vkDestroyCommandPool(g_wsi.device, g_transfer_cmd_pool, NULL);
        g_transfer_cmd_pool = VK_NULL_HANDLE;
    }

    printf("[C-CORE] Async Threads joined, Device idled, and Pools destroyed.\n");
}

void vx_init_mailbox() {
    atomic_init(&g_engine.mailbox.ready_index, 0);
    atomic_init(&g_engine.mailbox.is_running, 1);
    atomic_init(&g_engine.mailbox.lua_finished, 0);
    atomic_init(&g_engine.mailbox.vk_instance, NULL);
    atomic_init(&g_engine.mailbox.vk_surface, NULL);
    atomic_init(&g_engine.mailbox.mouse_x, 0.0f);
    atomic_init(&g_engine.mailbox.mouse_y, 0.0f);
    atomic_init(&g_engine.mailbox.click_x, -1.0f); // Initialize to -1
    atomic_init(&g_engine.mailbox.click_y, -1.0f); // Initialize to -1
    atomic_init(&g_engine.mailbox.mouse_captured, 0); // Start Free
}

THREAD_FUNC lua_co_overlord_loop(void* arg) {
    printf("[LUA-OS-THREAD] Booting Lua VM...\n");
    lua_State* L = luaL_newstate();
    luaL_openlibs(L);
    if (luaL_dofile(L, "main.lua") != LUA_OK) {
        printf("\n[LUA FATAL ERROR] %s\n", lua_tostring(L, -1));
    }
    lua_close(L);
    printf("[LUA-OS-THREAD] VM Destroyed.\n");
    return THREAD_RETURN_VAL;
}

int main(int argc, char** argv) {
    printf("[C-CORE] Booting Headless Worker...\n");

    if (!glfwInit()) return -1;
    vx_init_mailbox();

    atomic_init(&g_engine.mailbox.glfw_cmd, CMD_IDLE);
    atomic_init(&g_engine.mailbox.last_key_pressed, 0);
    atomic_init(&g_engine.mailbox.wasd_mask, 0);
    atomic_init(&g_engine.mailbox.mouse_dx, 0.0f);
    atomic_init(&g_engine.mailbox.mouse_dy, 0.0f);
    atomic_init(&g_engine.mailbox.mouse_left, 0);
    atomic_init(&g_engine.mailbox.mouse_right, 0);
    atomic_init(&g_engine.mailbox.key_space, 0);

    vmath_thread_t lua_thread = vmath_thread_start(lua_co_overlord_loop, NULL);

    GLFWwindow* window = NULL;

    while (vx_core_is_running()) {
        if (window) glfwPollEvents();

        int cmd = atomic_exchange_explicit(&g_engine.mailbox.glfw_cmd, CMD_IDLE, memory_order_acquire);

        if (cmd == CMD_BOOT_WINDOW && window == NULL) {
            int w = atomic_load_explicit(&g_engine.mailbox.glfw_arg_w, memory_order_relaxed);
            int h = atomic_load_explicit(&g_engine.mailbox.glfw_arg_h, memory_order_relaxed);

            glfwWindowHint(GLFW_CLIENT_API, GLFW_NO_API);
            glfwWindowHint(GLFW_RESIZABLE, GLFW_TRUE);
            window = glfwCreateWindow(w, h, "VX Engine Remote", NULL, NULL);
            glfwSetWindowSizeLimits(window, 640, 360, GLFW_DONT_CARE, GLFW_DONT_CARE);

            // --- THE WINDOWS FOCUS OVERRIDE HACK ---
            glfwShowWindow(window);
            glfwSetWindowAttrib(window, GLFW_FLOATING, GLFW_TRUE);  // Force OS to overlay it
            glfwFocusWindow(window);                                // Grab the input lock
            glfwSetWindowAttrib(window, GLFW_FLOATING, GLFW_FALSE); // Sink it back to normal
            glfwPollEvents();                                       // Flush the OS event queue instantly

            glfwSetFramebufferSizeCallback(window, glfw_framebuffer_size_callback);
            glfwSetKeyCallback(window, glfw_key_callback);
            glfwSetCursorPosCallback(window, glfw_cursor_callback);
            glfwSetMouseButtonCallback(window, glfw_mouse_button_callback);

            int fb_w, fb_h;
            glfwGetFramebufferSize(window, &fb_w, &fb_h);
            atomic_store_explicit(&g_engine.mailbox.win_w, fb_w, memory_order_release);
            atomic_store_explicit(&g_engine.mailbox.win_h, fb_h, memory_order_release);

            void* instance = atomic_load_explicit(&g_engine.mailbox.vk_instance, memory_order_acquire);
            if (instance != NULL) {
                VkSurfaceKHR surface;
                if (glfwCreateWindowSurface((VkInstance)instance, window, NULL, &surface) == VK_SUCCESS) {
                    atomic_store_explicit(&g_engine.mailbox.vk_surface, (void*)surface, memory_order_release);
                    printf("[C-CORE] Window & Surface Created on Lua's Demand!\n");
                }
            }
        }
        else if (cmd == CMD_KILL_WINDOW && window != NULL) {
            glfwDestroyWindow(window);
            window = NULL;
            atomic_store_explicit(&g_engine.mailbox.vk_surface, NULL, memory_order_release);
            printf("[C-CORE] Window Destroyed. Running Headless...\n");
        }

        if (window && glfwWindowShouldClose(window)) {
            atomic_store_explicit(&g_engine.mailbox.last_key_pressed, GLFW_KEY_ESCAPE, memory_order_release);
            glfwSetWindowShouldClose(window, GLFW_FALSE);
        }
        SLEEP_MS(1);
    }

    printf("\n[C-CORE] Shutdown triggered. Waiting for Lua VM...\n");
    while (atomic_load_explicit(&g_engine.mailbox.lua_finished, memory_order_acquire) == 0) {
        SLEEP_MS(1);
    }

    vmath_thread_join(lua_thread);
    if (window) glfwDestroyWindow(window);
    glfwTerminate();
    printf("[C-CORE] Clean Exit.\n");
    return 0;
}
