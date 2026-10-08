-- EXPECT: 8
-- EXPECT: 0
-- explore/deep_free — m[0] = {7, 8}: the deep-free ownership transfer.
local m = {}
m[0] = {7, 8}
print(m[0][1])
m = nil
print(sys_alloc_count())
