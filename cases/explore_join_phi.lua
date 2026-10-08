-- EXPECT: 7
-- EXPECT: 0
-- explore/join_phi — if-join: pick = a / pick = b across arms;
-- `pick = nil` is the sole faithful handle (frees through the join phi).
-- The do-block bounds the census: a/b's block-exit frees land before
-- the final print instead of in the root tail after it.
do
    local a = {}
    local b = {}
    local pick
    if 1 < 2 then
        pick = a
    else
        pick = b
    end
    pick[0] = 7
    print(pick[0])
    pick = nil
end
print(sys_alloc_count())
