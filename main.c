// main.c
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
// main.c - The Expanded Mailbox
#define CMD_IDLE            0
#define CMD_BOOT_WINDOW     1
#define CMD_KILL_WINDOW     2

// [- REPLACE -] IPC_Mailbox struct with new input fields
typedef struct {
    alignas(64) _Atomic int ready_index;
    _Atomic int is_running;
    _Atomic int lua_finished;
    _Atomic(void*) vk_instance;
    _Atomic(void*) vk_surface;

    // --- Remote Control Hub ---
    _Atomic int glfw_cmd;
    _Atomic int glfw_arg_w;
    _Atomic int glfw_arg_h;
    _Atomic int last_key_pressed;

    // --- NEW: Input State ---
    _Atomic uint32_t wasd_mask;
    _Atomic float mouse_dx;
    _Atomic float mouse_dy;

    // --- NEW: Resize State ---
    _Atomic int window_resized;
    _Atomic int win_w;
    _Atomic int win_h;
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

EXPORT void vibe_set_glfw_cmd(int cmd, int w, int h) {
    atomic_store_explicit(&g_engine.mailbox.glfw_arg_w, w, memory_order_relaxed);
    atomic_store_explicit(&g_engine.mailbox.glfw_arg_h, h, memory_order_relaxed);
    atomic_store_explicit(&g_engine.mailbox.glfw_cmd, cmd, memory_order_release);
}

EXPORT int vibe_get_last_key() {
    return atomic_exchange_explicit(&g_engine.mailbox.last_key_pressed, 0, memory_order_acquire);
}

double last_mx = 0.0, last_my = 0.0;
bool first_mouse = true;

void glfw_cursor_callback(GLFWwindow* window, double xpos, double ypos) {
    if (first_mouse) { last_mx = xpos; last_my = ypos; first_mouse = false; return; }
    float dx = (float)(xpos - last_mx);
    float dy = (float)(ypos - last_my);
    last_mx = xpos; last_my = ypos;

    float current_dx = atomic_load_explicit(&g_engine.mailbox.mouse_dx, memory_order_acquire);
    while (!atomic_compare_exchange_weak_explicit(&g_engine.mailbox.mouse_dx, &current_dx, current_dx + dx, memory_order_release, memory_order_relaxed));

    float current_dy = atomic_load_explicit(&g_engine.mailbox.mouse_dy, memory_order_acquire);
    while (!atomic_compare_exchange_weak_explicit(&g_engine.mailbox.mouse_dy, &current_dy, current_dy + dy, memory_order_release, memory_order_relaxed));
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
        atomic_store_explicit(&g_engine.mailbox.last_key_pressed, GLFW_KEY_ESCAPE, memory_order_release);
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

// [ANCHOR] Right below vibe_eject_validation_layers - [+ ADD +] new EXPORTs
EXPORT uint32_t vibe_get_wasd() { return atomic_load_explicit(&g_engine.mailbox.wasd_mask, memory_order_acquire); }
EXPORT float vibe_get_mouse_dx() { return atomic_exchange_explicit(&g_engine.mailbox.mouse_dx, 0.0f, memory_order_acquire); }
EXPORT float vibe_get_mouse_dy() { return atomic_exchange_explicit(&g_engine.mailbox.mouse_dy, 0.0f, memory_order_acquire); }
EXPORT int vibe_get_resize_flag() { return atomic_exchange_explicit(&g_engine.mailbox.window_resized, 0, memory_order_acquire); }
EXPORT void vibe_get_window_size(int* w, int* h) {
    *w = atomic_load_explicit(&g_engine.mailbox.win_w, memory_order_acquire);
    *h = atomic_load_explicit(&g_engine.mailbox.win_h, memory_order_acquire);
}

void glfw_framebuffer_size_callback(GLFWwindow* window, int width, int height) {
    // If the window is minimized, width/height will be 0. We ignore 0.
    if (width == 0 || height == 0) return;

    atomic_store_explicit(&g_engine.mailbox.win_w, width, memory_order_release);
    atomic_store_explicit(&g_engine.mailbox.win_h, height, memory_order_release);
    atomic_store_explicit(&g_engine.mailbox.window_resized, 1, memory_order_release);
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
    printf("[C-CORE] Booting Headless Worker...\n");

    if (!glfwInit()) return -1;
    vibe_init_mailbox();

    // Ensure new atomic fields start at 0
    atomic_init(&g_engine.mailbox.glfw_cmd, CMD_IDLE);
    atomic_init(&g_engine.mailbox.last_key_pressed, 0);
    atomic_init(&g_engine.mailbox.wasd_mask, 0);
    atomic_init(&g_engine.mailbox.mouse_dx, 0.0f);
    atomic_init(&g_engine.mailbox.mouse_dy, 0.0f);

    // Spawn the Lua Overlord
    vmath_thread_t lua_thread = vmath_thread_start(lua_co_overlord_loop, NULL);

    GLFWwindow* window = NULL;

    // The Subservient Polling Loop
    while (vibe_get_is_running()) {
        if (window) glfwPollEvents();

        // 1. Read Lua's Command (and clear it back to IDLE instantly)
        int cmd = atomic_exchange_explicit(&g_engine.mailbox.glfw_cmd, CMD_IDLE, memory_order_acquire);

        // 2. Execute Lua's Command
        if (cmd == CMD_BOOT_WINDOW && window == NULL) {
            int w = atomic_load_explicit(&g_engine.mailbox.glfw_arg_w, memory_order_relaxed);
            int h = atomic_load_explicit(&g_engine.mailbox.glfw_arg_h, memory_order_relaxed);

            glfwWindowHint(GLFW_CLIENT_API, GLFW_NO_API);
            glfwWindowHint(GLFW_RESIZABLE, GLFW_TRUE);
            window = glfwCreateWindow(w, h, "VibeEngine Remote", NULL, NULL);
            // NATIVE OS CLAMP: Prevent crushing the window into invalid math ranges
            glfwSetWindowSizeLimits(window, 640, 360, GLFW_DONT_CARE, GLFW_DONT_CARE);
            glfwSetFramebufferSizeCallback(window, glfw_framebuffer_size_callback);
            glfwSetKeyCallback(window, glfw_key_callback);

            glfwSetCursorPosCallback(window, glfw_cursor_callback);
            glfwSetInputMode(window, GLFW_CURSOR, GLFW_CURSOR_DISABLED);

            // ---> THE FIX: Seed the mailbox with the initial framebuffer size! <---
            int fb_w, fb_h;
            glfwGetFramebufferSize(window, &fb_w, &fb_h);
            atomic_store_explicit(&g_engine.mailbox.win_w, fb_w, memory_order_release);
            atomic_store_explicit(&g_engine.mailbox.win_h, fb_h, memory_order_release);
            // ----------------------------------------------------------------------

            // Fetch the instance Lua already published
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
            // Wipe the surface from the mailbox so Lua knows it's dead
            atomic_store_explicit(&g_engine.mailbox.vk_surface, NULL, memory_order_release);
            printf("[C-CORE] Window Destroyed. Running Headless...\n");
        }

        // 3. Handle OS-level window close (the 'X' button)
        if (window && glfwWindowShouldClose(window)) {
            // Instead of instantly dying, tell Lua to handle the teardown gracefully
            atomic_store_explicit(&g_engine.mailbox.last_key_pressed, GLFW_KEY_ESCAPE, memory_order_release);
            glfwSetWindowShouldClose(window, GLFW_FALSE); // Prevent infinite loop
        }

        // SLEEP_MS(16); // Low-power sleep while we wait for orders
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
