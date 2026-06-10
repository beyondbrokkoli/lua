-- lua/sim_world.lua
local ffi = require("ffi")

local World = {}

function World.init_grid(total_tiles)
    local terrain = ffi.new(string.format("uint16_t[8][%d]", total_tiles))
    local elevation = ffi.new(string.format("float[8][%d]", total_tiles))
    return { terrain = terrain, elevation = elevation }
end

function World.update_simulation(grid, tick, frame_data, player_count)
    -- Deterministic State Mutation: Apply clicks to the grid
    for p = 0, player_count - 1 do
        local click_idx = frame_data.click_grid_idx[p]
        if click_idx ~= -1 then
            -- Toggle state based on player click to guarantee diverging hashes if desynced
            if grid.terrain[p][click_idx] == 0 then
                grid.terrain[p][click_idx] = p + 10
                grid.elevation[p][click_idx] = 15.0
            else
                grid.terrain[p][click_idx] = 0
                grid.elevation[p][click_idx] = 0.0
            end
        end
    end
end

return World
