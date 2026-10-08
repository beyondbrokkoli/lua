-- The retroactive adoption, store-witnessed (signal 78): 't' is born
-- Integer-celled and the String store flips the name to the dynamic
-- cell for the whole script — flow-insensitively (the join is total:
-- every read of 't', even one textually earlier, sees Any from
-- here). The Int entry packs retroactively through glm_any_from_int;
-- #t reads the runtime border; #t[2] is the cell's own string length
-- through glm_any_len.
-- EXPECT: five
-- EXPECT: 3
-- EXPECT: 4
local t = {7}
t[2] = "five"
print(t[2])
print(#t)
print(#t[2])
