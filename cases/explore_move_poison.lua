-- EXPECT: 42
-- EXPECT: 0
-- explore/move_poison — affine transfer: `local u = t` poisons t.
local t = {}
t[0] = 42
local u = t
print(u[0])
t = nil
u = nil
print(sys_alloc_count())
