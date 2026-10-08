-- The seam gauntlet: Any cells crossing glm_tbl_get_any /
-- glm_tbl_set_any INSIDE a loop body — the shape where a loop
-- header phi's back-edge predecessor is a fast store's synthetic
-- cont block (bts{n}cont), the exact block naming a desynced seam
-- counter once got wrong. Two tables, both verdicts:
--   dense  — ctor seed + while-loop growth to 69999 cells: realloc
--            doublings and the 1 MiB VM graduation, every span
--            movement aligned by elem_layout and policed by the
--            debug asserts.
--   sparse — the far store's compile-time Sparse verdict: born
--            sparse, every store riding the overflow map's u128
--            lane, the read overlaid back out.
-- ARGS: 5 world
-- EXPECT: world
-- EXPECT: 5
-- EXPECT: world
-- EXPECT: 5
local t = {arg[0]}
local i = 1
while i < 70000 do
  t[i] = arg[i % 2]
  i = i + 1
end
print(t[69999])
print(t[42])
local s = {}
s[200000] = arg[1]
s[0] = arg[0]
print(s[200000])
print(s[0])
return t
