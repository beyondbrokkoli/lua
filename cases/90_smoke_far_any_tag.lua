-- Smoke-test repro (imaginary user report): an audit script keeps
-- tagged cells in an Any table. Entries past the far-key threshold
-- ride the overflow map; storing a string entry at a far key and
-- reading it back must return the string with its tag intact.
-- ARGS: 42 world
-- EXPECT: 42
-- EXPECT: world
-- EXPECT: world
local t = {arg[0], arg[1]}
t[200000] = arg[1]
print(t[0])
print(t[200000])
print(arg[1])
return t
