-- main.lua
io.stdout:setvbuf("no")
package.path = "./lua/?.lua;" .. package.path

local ffi = require("ffi")
local bit = require("bit")
local structs = require("structs") -- Forces SSoT compilation
local net = require("network")
local cfg = require("config_engine")
local FSM = require("fsm_core")
local State = require("sim_world")

-- ==========================================
-- OS AGNOSTIC HIGH-RES TIMER
-- ==========================================
ffi.cdef[[
    void Sleep(uint32_t dwMilliseconds);
    int usleep(uint32_t usec);
    int QueryPerformanceCounter(int64_t *lpPerformanceCount);
    int QueryPerformanceFrequency(int64_t *lpFrequency);
    typedef struct { long tv_sec; long tv_nsec; } timespec;
    int clock_gettime(int clk_id, timespec *tp);
]]

local function sys_sleep(ms)
    if jit.os == "Windows" then ffi.C.Sleep(ms) else ffi.C.usleep(ms * 1000) end
end

local get_time_hires
if jit.os == "Windows" then
    local kernel32 = ffi.load("kernel32")
    local freq = ffi.new("int64_t[1]")
    kernel32.QueryPerformanceFrequency(freq)
    local inv_freq = 1.0 / tonumber(freq[0])
    get_time_hires = function()
        local count = ffi.new("int64_t[1]")
        kernel32.QueryPerformanceCounter(count)
        return tonumber(count[0]) * inv_freq
    end
else
    get_time_hires = function()
        local ts = ffi.new("timespec")
        ffi.C.clock_gettime(1, ts) -- CLOCK_MONOTONIC
        return tonumber(ts.tv_sec) + (tonumber(ts.tv_nsec) * 1e-9)
    end
end

-- ==========================================
-- WEAVER V2 HEADLESS BOOT SEQUENCE
-- ==========================================
print("========================================")
print(" WEAVER ENGINE: V2 DEEP HISTORY TESTBED ")
print("========================================")
print("1. Host (Player 0) - Binds Port 50000")
print("2. Join (Player 1) - Connects to 127.0.0.1:50000 (Binds 50001)")
io.write("> ")
local user_input = io.read("*l")

local is_host = (user_input == "1")
local local_port = is_host and 50000 or 50001
local target_port = is_host and 50001 or 50000
local local_id = is_host and 0 or 1
local peer_id = is_host and 1 or 0

assert(net.Host(local_port), "FATAL: Failed to bind port!")
net.SetPlayerId(local_id)
net.SetSession(0xDEADBEEF) -- Hardcoded session for raw testing
net.Connect(peer_id, "127.0.0.1", target_port)

print(string.format("[SYSTEM] Network Bound. Identity: P%d. Establishing memory arena...", local_id))

-- Allocate absolute state sources
local total_tiles = cfg.world.map_width * cfg.world.map_height
local bytes_terrain = 8 * total_tiles * ffi.sizeof("uint16_t")
local bytes_elevation = 8 * total_tiles * ffi.sizeof("float")

local ctx = {
    session_token = 0xDEADBEEF,
    net_identity = local_id,
    sim_tick_count = 1,
    accumulator = 0.0,
    pending_click = -1,
    total_tiles = total_tiles,
    last_bot_tick = 0,
    
    -- Network topology tracking
    peer_active = ffi.new("bool[8]"),
    peer_highest_tick = ffi.new("uint32_t[8]"),
    
    -- Memory Arenas
    rts_grid = State.init_grid(total_tiles),
    rollback_arena = ffi.new("RollbackBuffer"),
    snapshot_ring = {
        terrain = ffi.new(string.format("uint16_t[128][8][%d]", total_tiles)),
        elevation = ffi.new(string.format("float[128][8][%d]", total_tiles))
    }
}

-- Initialize empty arena slots
local f0 = ctx.rollback_arena.frames[0]
f0.tick = 0
for p = 0, 7 do f0.click_grid_idx[p] = -1 f0.player_input[p] = 0 end
ctx.rollback_arena.head_tick = 0
ctx.rollback_arena.confirmed_tick = 0

-- FSM Constants
local TICK_RATE = 60
local FIXED_DT = 1.0 / TICK_RATE
local last_time = get_time_hires()
local next_debug_print = last_time + 1.0

print("[SYSTEM] Boot sequence complete. Entering strict FSM loop.")
ctx.peer_active[peer_id] = true -- Force active for 2-player local test

-- ==========================================
-- MAIN ENGINE LOOP
-- ==========================================
while true do
    local current_time = get_time_hires()
    local frame_time = math.max(0.001, math.min(current_time - last_time, 0.25))
    last_time = current_time
    ctx.accumulator = ctx.accumulator + frame_time

    -- Execute strict phase isolation
    FSM.tick_playing_state(ctx, FIXED_DT, bytes_terrain, bytes_elevation)

    -- Status Heartbeat
    if current_time >= next_debug_print then
        print(string.format("[HEARTBEAT] SimTick: %d | NetHead: %d | Confirmed: %d | Missing: %d frames", 
            ctx.sim_tick_count, 
            ctx.rollback_arena.head_tick, 
            ctx.rollback_arena.confirmed_tick,
            ctx.sim_tick_count - ctx.rollback_arena.confirmed_tick
        ))
        next_debug_print = current_time + 1.0
    end

    sys_sleep(1) -- Yield to OS
end
