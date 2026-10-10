-- test_double.lua — minimal glm module for interop PoC
-- Takes 4 integers via arg[], doubles each, returns result table.
-- The table is built with explicit indices to avoid Lua's 1-indexed {} default.
-- ARGS: 5 10 15 20
-- EXPECT: 10
-- EXPECT: 20
-- EXPECT: 30
-- EXPECT: 40
local t = {}
t[0] = arg[0] * 2
t[1] = arg[1] * 2
t[2] = arg[2] * 2
t[3] = arg[3] * 2
print(t[0])
print(t[1])
print(t[2])
print(t[3])
return t
