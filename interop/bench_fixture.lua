-- bench_fixture.lua — integer doubling, no prints (benchmark fixture)
local t = {}
local i = 0
while i < #arg do
  t[i] = arg[i] * 2
  i = i + 1
end
return t
