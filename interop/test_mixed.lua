-- test_mixed.lua — Any boundary module: takes mixed types
-- ARGS: 42 hello 3.14 true
-- EXPECT: 42
-- EXPECT: hello
-- EXPECT: 3.14
-- EXPECT: true
-- EXPECT: 4
print(arg[0])
print(arg[1])
print(arg[2])
print(arg[3])
print(#arg)
return arg
