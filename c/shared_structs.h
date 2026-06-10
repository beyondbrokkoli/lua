// AUTO-GENERATED SSoT - DO NOT MODIFY
#pragma once
#include <stdint.h>

// --- ENGINE CONSTANTS ---
#define FRAME_STATE_CONFIRMED 2
#define FRAME_STATE_EMPTY 0
#define FRAME_STATE_PREDICTED 1

// --- ENGINE MEMORY STRUCTURES ---
#pragma pack(push, 1)
typedef struct {
    uint64_t session_token;
    uint32_t frame_tick;
    uint32_t checksum_tick;
    uint32_t state_checksum;
    uint32_t ack_tick;
    uint32_t base_tick;
    uint8_t player_id;
    uint8_t history_count;
    uint8_t inputs[64];
    int32_t clicks[64];
} LockstepPacket;
#pragma pack(pop)

typedef struct __attribute__((packed, aligned(4))) {
    uint32_t tick;
    uint8_t state;
    uint8_t _pad_auto_0[3];
    uint32_t state_checksum;
    uint32_t remote_checksum;
    uint8_t remote_peer_id;
    uint8_t player_input[8];
    uint8_t _pad_auto_1[3];
    int32_t click_grid_idx[8];
} NetworkFrame;

typedef struct __attribute__((packed, aligned(64))) {
    uint32_t head_tick;
    uint32_t confirmed_tick;
    uint8_t is_rollback_active;
    uint8_t _pad_auto_0[3];
    uint32_t rollback_target;
    uint8_t _pad_auto_1[44];
    NetworkFrame frames[128];
    uint8_t _pad_tail[4];
} RollbackBuffer;

