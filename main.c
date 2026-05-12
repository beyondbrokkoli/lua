#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <stdbool.h>
#include <stdatomic.h>
#include <stdalign.h>

#define GLFW_INCLUDE_VULKAN
#include <GLFW/glfw3.h>

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

static vmath_thread_t vmath_thread_start(void* (*func)(void*), void* arg) {
    pthread_t thread;
    pthread_create(&thread, NULL, func, arg);
    return thread;
}

static void vmath_thread_join(vmath_thread_t thread) {
    pthread_join(thread, NULL);
}

typedef struct {
    alignas(64) _Atomic int ready_index;
    _Atomic int is_running;
    _Atomic int lua_finished;
    _Atomic(void*) vk_instance;
    _Atomic(void*) vk_surface;
} IPC_Mailbox;

typedef struct {
    IPC_Mailbox mailbox;
    int render_index;
    int write_index;
} EngineState;

static EngineState g_engine;

EXPORT int vibe_get_is_running() { return atomic_load_explicit(&g_engine.mailbox.is_running, memory_order_relaxed); }
EXPORT void vibe_trigger_shutdown() { atomic_store_explicit(&g_engine.mailbox.is_running, 0, memory_order_release); }
EXPORT void vibe_mark_lua_finished() { atomic_store_explicit(&g_engine.mailbox.lua_finished, 1, memory_order_release); }

EXPORT const char** vibe_get_glfw_extensions(uint32_t* count) { return glfwGetRequiredInstanceExtensions(count); }
EXPORT void vibe_publish_vk_instance(void* instance) { atomic_store_explicit(&g_engine.mailbox.vk_instance, instance, memory_order_release); }
EXPORT void* vibe_get_vk_surface() { return atomic_load_explicit(&g_engine.mailbox.vk_surface, memory_order_acquire); }
// INJECT THIS BLOCK
EXPORT void vibe_get_window_size(int* width, int* height) { 
    *width = 1280; 
    *height = 720; 
}

// ==========================================
// 3. VULKAN VALIDATION LAYER ENFORCER
// ==========================================
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

EXPORT void vibe_inject_validation_layers(void* instance_ptr) { 
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
EXPORT void vibe_eject_validation_layers(void* instance) {
    PFN_vkDestroyDebugUtilsMessengerEXT destroyFn = 
        (PFN_vkDestroyDebugUtilsMessengerEXT)vkGetInstanceProcAddr(
            (VkInstance)instance, 
            "vkDestroyDebugUtilsMessengerEXT"
        );
    
    if (destroyFn != NULL) {
        destroyFn((VkInstance)instance, g_debugMessenger, NULL);
    }
}
void vibe_init_mailbox() {
    atomic_init(&g_engine.mailbox.ready_index, 0);
    atomic_init(&g_engine.mailbox.is_running, 1);
    atomic_init(&g_engine.mailbox.lua_finished, 0);
    atomic_init(&g_engine.mailbox.vk_instance, NULL);
    atomic_init(&g_engine.mailbox.vk_surface, NULL);
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
    printf("[C-CORE] Booting Phase 1: Instance & Surface Handshake...\n");

    if (!glfwInit()) return -1;
    glfwWindowHint(GLFW_CLIENT_API, GLFW_NO_API);
    glfwWindowHint(GLFW_RESIZABLE, GLFW_FALSE);
    GLFWwindow* window = glfwCreateWindow(1280, 720, "VibeEngine Phase 1", NULL, NULL);

    vibe_init_mailbox();
    vmath_thread_t lua_thread = vmath_thread_start(lua_co_overlord_loop, NULL);

    bool surface_created = false;

    while (vibe_get_is_running()) {
        glfwPollEvents();
        if (glfwWindowShouldClose(window)) vibe_trigger_shutdown();

        if (!surface_created) {
            void* instance = atomic_load_explicit(&g_engine.mailbox.vk_instance, memory_order_acquire);
            if (instance != NULL) {
                VkSurfaceKHR surface;
                if (glfwCreateWindowSurface((VkInstance)instance, window, NULL, &surface) == VK_SUCCESS) {
                    atomic_store_explicit(&g_engine.mailbox.vk_surface, (void*)surface, memory_order_release);
                    surface_created = true;
                    printf("[C-CORE] Window Surface Created & Published to Lua!\n");
                    printf("[C-CORE] Phase 1 Handshake Complete. Triggering Safe Teardown...\n");
                    vibe_trigger_shutdown();
                } else {
                    printf("[C-FATAL] Failed to create Vulkan Surface.\n");
                    vibe_trigger_shutdown();
                }
            }
        }
        SLEEP_MS(16);
    }

    printf("\n[C-CORE] Shutdown triggered. Waiting for Lua VM...\n");
    while (atomic_load_explicit(&g_engine.mailbox.lua_finished, memory_order_acquire) == 0) {
        SLEEP_MS(1);
    }

    vmath_thread_join(lua_thread);
    glfwDestroyWindow(window);
    glfwTerminate();
    printf("[C-CORE] Clean Exit.\n");
    return 0;
}
