-- lua/network.lua
local ffi = require("ffi")

ffi.cdef[[
    int vx_net_host(int port);
    int vx_net_connect(uint8_t peer_id, const char* ip, int port);
    void vx_net_set_session(uint64_t token);
    void vx_net_set_player_id(uint8_t id);
    void vx_net_send_to(void* pkt, uint8_t target_peer);
    int vx_net_recv_all(void* out_buffer, int max_count);
    uint32_t vx_net_hash_state(const void* data, size_t length, uint32_t initial_hash);
]]

-- Dynamically load the binary depending on the host OS
local lib_path = jit.os == "Windows" and "bin/vx_net.dll" or "./bin/libvx_net.so"
local net_lib = ffi.load(lib_path)

local Network = {}

function Network.Host(port)
    return net_lib.vx_net_host(port) == 0
end

function Network.Connect(peer_id, ip, port)
    return net_lib.vx_net_connect(peer_id, ip, port) == 0
end

function Network.SetSession(token)
    net_lib.vx_net_set_session(token)
end

function Network.SetPlayerId(id)
    net_lib.vx_net_set_player_id(id)
end

function Network.SendTo(pkt, peer_id)
    net_lib.vx_net_send_to(pkt, peer_id)
end

function Network.RecvAll(out_buffer, max_count)
    return net_lib.vx_net_recv_all(out_buffer, max_count)
end

function Network.HashState(data_ptr, length, initial_hash)
    return net_lib.vx_net_hash_state(data_ptr, length, initial_hash)
end

return Network
