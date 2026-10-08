-- rebind_nil_slot_adopt.lua — the positive face of the nil-slot law:
-- `t = nil` is ownership-only (the nil frees the header, the type
-- slot survives it), so the rebind after the nil re-opens the name
-- under the SAME element discipline — and the float store meets the
-- Int the {1} witnessed. Under uniform Any adoption that mix is a
-- WITNESS, not a conflict: the name is dynamic from the fixed point
-- on, the Int entry packs retroactively, and the read prints the
-- packed float. The nil never reset types (a nil that reset types
-- would make the slot path-sensitive; the flow-insensitive law
-- depends on it not being so) — adoption just made the surviving
-- slot dynamic.
-- EXPECT: 2.5
local t = {1}
t = nil
t = {}
t[0] = 2.5
print(t[0])
