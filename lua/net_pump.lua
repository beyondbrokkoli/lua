local ffi = require("ffi")
local bit = require("bit")
local cfg = require("config_engine")
local net = require("network")

local Pump = {}

-- Flat memory state tracking what opponents know about us
local peer_ack_of_me = ffi.new("uint32_t[8]")

function Pump.send_dynamic_history(ctx)
    local current_tick = ctx.sim_tick_count
    local conf_tick = ctx.rollback_arena.confirmed_tick

    for p = 0, 7 do
        if p ~= ctx.net_identity and ctx.peer_active[p] then
            local pkt = ffi.new("LockstepPacket")
            pkt.session_token = ctx.session_token
            pkt.player_id = ctx.net_identity
            pkt.frame_tick = current_tick

            -- Tell peer 'p' the highest contiguous tick we have verified from them
            pkt.ack_tick = ctx.peer_highest_tick[p]

            if conf_tick > 0 then
                local conf_idx = bit.band(conf_tick, 127) -- 128-tick Ouroboros clamp
                pkt.state_checksum = ctx.rollback_arena.frames[conf_idx].state_checksum
                pkt.checksum_tick = conf_tick
            end

            -- DYNAMIC HISTORY PACKING:
            -- Calculate the exact delta peer 'p' is missing based on their last ACK.
            local needed_base = peer_ack_of_me[p] + 1
            if needed_base == 1 then
                needed_base = math.max(1, current_tick - 63)
            end

            -- Clamp maximum redundancy payload to 64 frames
            local history_len = current_tick - needed_base + 1
            if history_len > 64 then
                history_len = 64
                needed_base = current_tick - 63
            elseif history_len <= 0 then
                history_len = 1
                needed_base = current_tick
            end

            pkt.base_tick = needed_base
            pkt.history_count = history_len

            -- Pack the contiguous block of memory into the flat packet arrays
            for i = 0, history_len - 1 do
                local h_tick = needed_base + i
                local h_idx = bit.band(h_tick, 127)
                local frame = ctx.rollback_arena.frames[h_idx]

                pkt.inputs[i] = frame.player_input[ctx.net_identity]
                pkt.clicks[i] = frame.click_grid_idx[ctx.net_identity]
            end

            -- Fire targeted payload
            net.SendTo(pkt, p)
        end
    end
end

function Pump.intercept_network(ctx, current_tick)
    local in_buffer = ffi.new("LockstepPacket[256]")
    local count = net.RecvAll(in_buffer, 256)

    for i = 0, count - 1 do
        local pkt = in_buffer[i]
        local pid = pkt.player_id

        if pid < 8 and pkt.frame_tick >= 0 then
            ctx.peer_active[pid] = true

            -- Update what THEY know about OUR timeline
            if pkt.ack_tick > peer_ack_of_me[pid] then
                peer_ack_of_me[pid] = pkt.ack_tick
            end

            -- STRICT ALIASING CLAMP:
            -- Protect the 120-tick bounds so memory never corrupts the Ouroboros tail.
            local window_start = math.max(0, current_tick - 60)
            local window_end = math.min(current_tick + 60, ctx.rollback_arena.confirmed_tick + 120)

            -- Unpack the Deep History payload
            for h = 0, pkt.history_count - 1 do
                local h_tick = pkt.base_tick + h

                if h_tick > ctx.rollback_arena.confirmed_tick and h_tick >= window_start and h_tick <= window_end then
                    local h_idx = bit.band(h_tick, 127)
                    local h_frame = ctx.rollback_arena.frames[h_idx]

                    -- Initialize empty FSM state if we leaped into the future
                    if h_frame.tick ~= h_tick then
                        h_frame.tick = h_tick
                        h_frame.state = cfg.net_state.empty
                        for p_scan = 0, 7 do
                            h_frame.player_input[p_scan] = 0
                            h_frame.click_grid_idx[p_scan] = -1
                        end
                        h_frame.state_checksum = 0
                    end

                    -- Trigger Rollback if historical consensus diverges
                    local inc_input = pkt.inputs[h]
                    local inc_click = pkt.clicks[h]

                    if h_frame.player_input[pid] ~= inc_input or h_frame.click_grid_idx[pid] ~= inc_click then
                        if ctx.rollback_arena.is_rollback_active == 0 or h_tick < ctx.rollback_arena.rollback_target then
                            ctx.rollback_arena.is_rollback_active = 1
                            ctx.rollback_arena.rollback_target = h_tick
                        end
                        h_frame.player_input[pid] = inc_input
                        h_frame.click_grid_idx[pid] = inc_click
                    end
                end
            end

            -- Update the highest timeline tick known for this opponent
            if pkt.frame_tick > ctx.peer_highest_tick[pid] then
                ctx.peer_highest_tick[pid] = pkt.frame_tick
            end
        end
    end

    -- Calculate True Consensus (The minimum 'highest_tick' across all active opponents)
    local true_consensus = 0xFFFFFFFF
    for p = 0, 7 do
        if p ~= ctx.net_identity and ctx.peer_active[p] then
            if ctx.peer_highest_tick[p] < true_consensus then
                true_consensus = ctx.peer_highest_tick[p]
            end
        end
    end

    local local_max_valid_tick = math.max(0, current_tick - 1)
    if true_consensus > local_max_valid_tick then
        true_consensus = local_max_valid_tick
    end

    -- Advance lockstep boundary
    if true_consensus ~= 0xFFFFFFFF and true_consensus > ctx.rollback_arena.confirmed_tick then
        ctx.rollback_arena.confirmed_tick = true_consensus
    end
end

return Pump
