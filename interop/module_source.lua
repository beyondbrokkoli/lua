-- module_source.lua
-- A simulated data analyzer.
-- Expects an Integer boundary (inferred via arithmetic on `arg`).
-- Returns an Any table containing a mix of Strings, Ints, and Bools.

local out = {}
out[0] = "Batch Analysis Report"  -- Triggers String/Any adoption early

local i = 0
local sum = 0
local max_val = -999999
local count = #arg

-- Process the Integer array
while i < count do
    local val = arg[i]
    sum = sum + val
    if val > max_val then
        max_val = val
    end
    i = i + 1
end

-- Pack the results dynamically
out[1] = count
out[2] = sum
out[3] = max_val

local is_alert = false
if max_val > 50 then
    is_alert = true
end
out[4] = is_alert

if is_alert then
    out[5] = "CRITICAL_THRESHOLD_EXCEEDED"
else
    out[5] = "NOMINAL"
end

return out
