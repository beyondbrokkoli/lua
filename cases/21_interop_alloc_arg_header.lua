-- EXPECT: 1
-- EXPECT: 9
-- EXPECT: 2
-- EXPECT: 1
-- ARGS: 9
-- The one counter shift the boundary introduces, pinned: this script
-- names `arg`, so the host builds the boundary table and its header is
-- live for the script's whole run — every count rides +1 above the
-- executable-world floor. The table is never the script's to free:
-- `return arg` hands the header straight back and the host's identity
-- check frees it exactly once.
print(sys_alloc_count())
print(arg[0])
local t = {1, 2}
print(sys_alloc_count())
t = nil
print(sys_alloc_count())
return arg
