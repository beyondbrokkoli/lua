-- structs.lua
local ffi = require("ffi")

-- V2 LOCKSTEP SSOT
-- Strictly isolated network and FSM boundaries. Zero graphics API pollution.

ffi.cdef[[
    // THE DUMB PIPE PAYLOAD (Over-the-wire UDP Struct)
    #pragma pack(push, 1)
    typedef struct {
        uint64_t session_token;
        uint32_t frame_tick;         // The absolute head tick of the sender
        uint32_t checksum_tick;      // Checksum consensus tick
        uint32_t state_checksum;     // Checksum payload
        uint32_t ack_tick;           // SENDER acknowledges simulating up to this tick from RECEIVER
        uint32_t base_tick;          // Oldest tick in the dynamically packed history
        uint8_t  player_id;
        uint8_t  history_count;      // N frames packed (1 to 64)
        uint8_t  inputs[64];         // Flat SOA array for inputs
        int32_t  clicks[64];         // Flat SOA array for grid clicks
    } LockstepPacket;
    #pragma pack(pop)

    // THE INTERNAL LUA MEMORY ARENA (Ouroboros 128-Slot Ring Buffer)
    typedef struct {
        uint32_t tick;
        uint8_t  state;              // Predicted, Confirmed, or Empty
        uint32_t state_checksum;
        uint32_t remote_checksum;
        uint8_t  remote_peer_id;

        // Unpacked SOA state for local 8-player simulation
        uint8_t  player_input[8];
        int32_t  click_grid_idx[8];
    } NetworkFrame;

    typedef struct {
        uint32_t head_tick;
        uint32_t confirmed_tick;
        uint8_t  is_rollback_active;
        uint32_t rollback_target;
        NetworkFrame frames[128];
    } RollbackBuffer;
]]

return {}
