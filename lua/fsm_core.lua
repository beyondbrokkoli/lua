local Pump = require("net_pump")
local State = require("sim_world") -- The deterministic logic execution
local bit = require("bit")
local ffi = require("ffi")

local FSM = {}

function FSM.tick_playing_state(ctx, FIXED_DT, bytes_terrain, bytes_elevation)
    local remote_highest = ctx.rollback_arena.confirmed_tick

    -- ==========================================
    -- PHASE 1: ELASTIC CATCHUP
    -- Fast-forward the local accumulator if remote opponents are significantly ahead.
    -- ==========================================
    if remote_highest > ctx.sim_tick_count + 2 then
        ctx.accumulator = ctx.accumulator + ((remote_highest - ctx.sim_tick_count) * FIXED_DT)
    end

    -- ==========================================
    -- PHASE 2: OUROBOROS STALL (MEMORY CLAMP)
    -- Halt local time if we risk overwriting the 120-tick unconfirmed tail limit.
    -- ==========================================
    if ctx.sim_tick_count > remote_highest + 120 then
        ctx.accumulator = 0 
    end

    -- ==========================================
    -- PHASE 3: BOT / LOCAL INPUT INJECTION
    -- ==========================================
    if ctx.sim_tick_count % 120 == (ctx.net_identity * 10) then
        if ctx.last_bot_tick ~= ctx.sim_tick_count then
            ctx.pending_click = math.random(0, ctx.total_tiles - 1)
            ctx.last_bot_tick = ctx.sim_tick_count
        end
    end

    -- ==========================================
    -- PHASE 4: SIMULATION EXECUTION & ROLLBACK
    -- ==========================================
    while ctx.accumulator >= FIXED_DT do
        local c_idx = bit.band(ctx.sim_tick_count, 127)
        local frame = ctx.rollback_arena.frames[c_idx]

        -- Secure the memory slot
        if frame.tick ~= ctx.sim_tick_count then
            for p = 0, 7 do 
                frame.player_input[p] = 0 
                frame.click_grid_idx[p] = -1 
            end
            frame.state_checksum = 0
            frame.remote_checksum = 0
        end

        frame.tick = ctx.sim_tick_count

        -- Apply pending local input directly to the memory struct
        if ctx.pending_click ~= -1 then
            frame.click_grid_idx[ctx.net_identity] = ctx.pending_click
            ctx.pending_click = -1
        end

        ctx.rollback_arena.head_tick = ctx.sim_tick_count

        -- Execute Data-Driven Network Boundaries
        Pump.send_dynamic_history(ctx)
        Pump.intercept_network(ctx, ctx.sim_tick_count)

        -- Evaluate Rollback Condition
        if ctx.rollback_arena.is_rollback_active == 1 then
            local t_tgt = ctx.rollback_arena.rollback_target
            local r_idx = bit.band(t_tgt - 1, 127)

            -- Snap to past state via FFI memory copy
            ffi.copy(ctx.rts_grid.terrain, ctx.snapshot_ring.terrain[r_idx], bytes_terrain)
            ffi.copy(ctx.rts_grid.elevation, ctx.snapshot_ring.elevation[r_idx], bytes_elevation)

            -- Fast-forward simulation to current tick
            for t = t_tgt, ctx.sim_tick_count - 1 do
                local f_idx = bit.band(t, 127)
                local f = ctx.rollback_arena.frames[f_idx]

                State.update_simulation(ctx.rts_grid, t, f, 8)

                ffi.copy(ctx.snapshot_ring.terrain[f_idx], ctx.rts_grid.terrain, bytes_terrain)
                ffi.copy(ctx.snapshot_ring.elevation[f_idx], ctx.rts_grid.elevation, bytes_elevation)
            end

            ctx.rollback_arena.is_rollback_active = 0
        end

        -- Execute current unconfirmed tick if we haven't hit the predictive ceiling
        if ctx.sim_tick_count <= remote_highest + 4 then
            State.update_simulation(ctx.rts_grid, ctx.sim_tick_count, frame, 8)

            ffi.copy(ctx.snapshot_ring.terrain[c_idx], ctx.rts_grid.terrain, bytes_terrain)
            ffi.copy(ctx.snapshot_ring.elevation[c_idx], ctx.rts_grid.elevation, bytes_elevation)

            ctx.sim_tick_count = ctx.sim_tick_count + 1

            -- Divergence / Desync Verification occurs here
        end

        ctx.accumulator = ctx.accumulator - FIXED_DT
    end
end

return FSM
