-- test_edge.lua — edge-case fixture: negatives + a far key.
-- Stores past len + SPARSE_THRESHOLD (100_000, rt.rs), so the result
-- table carries an overflow map — forcing the bridge's checked
-- fallback path. Arithmetic on arg[] keeps the boundary Integer
-- (a bare `t[0] = arg[0]` copy widens the boundary to Any — the
-- checker only pins Integer when args feed arithmetic).
local t = {}
t[0] = arg[0] * 2
t[100001] = arg[1] * 2
return t
