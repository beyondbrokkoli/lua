/* c/vx_net.c - The Brainless UDP Pipe */
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>

#include "shared_structs.h"

#if defined(_WIN32)
    #define EXPORT __declspec(dllexport)
    #include <winsock2.h>
    #include <ws2tcpip.h>
    #include <mstcpip.h>
    #pragma comment(lib, "ws2_32.lib")
    typedef int socklen_t;
    typedef SSIZE_T ssize_t;
    typedef SOCKET vx_socket_t;
    static int net_wsa_initialized = 0;
    #define NET_CLOSE closesocket
    #define NET_ERROR SOCKET_ERROR
    #define NET_INVALID INVALID_SOCKET
    #define NET_WOULDBLOCK WSAEWOULDBLOCK
    #define NET_LASTERR WSAGetLastError()
    #ifndef SIO_UDP_CONNRESET
        #define SIO_UDP_CONNRESET _WSAIOW(IOC_VENDOR, 12)
    #endif
#else
    #define EXPORT __attribute__((visibility("default")))
    #include <sys/socket.h>
    #include <netinet/in.h>
    #include <arpa/inet.h>
    #include <netdb.h>
    #include <fcntl.h>
    #include <unistd.h>
    #include <errno.h>
    typedef int vx_socket_t;
    #define NET_CLOSE close
    #define NET_ERROR -1
    #define NET_INVALID -1
    #define NET_WOULDBLOCK EWOULDBLOCK
    #define NET_LASTERR errno
#endif

typedef struct {
    struct sockaddr_in addr;
    socklen_t addr_len;
    int active;
} NetPeer;

// The absolute minimum state required to route packets.
static struct {
    vx_socket_t sock;
    uint64_t session_token;
    uint8_t local_id;
    NetPeer peers[8];
} g_net = {
    .sock = NET_INVALID,
    .session_token = 0,
    .local_id = 0
};

static inline int net_set_nonblocking(int sock) {
#if defined(_WIN32)
    u_long mode = 1;
    return ioctlsocket(sock, FIONBIO, &mode) == 0 ? 0 : -1;
#else
    int flags = fcntl(sock, F_GETFL, 0);
    return (flags < 0) ? -1 : fcntl(sock, F_SETFL, flags | O_NONBLOCK);
#endif
}

// FNV-1a 32-bit Hash (Keep this in C because it's wildly fast)
EXPORT uint32_t vx_net_hash_state(const void* data, size_t length, uint32_t initial_hash) {
    const uint8_t* bytes = (const uint8_t*)data;
    uint32_t hash = (initial_hash == 0) ? 0x811C9DC5 : initial_hash;
    for (size_t i = 0; i < length; ++i) {
        hash ^= bytes[i];
        hash *= 0x01000193;
    }
    return hash;
}

static inline void net_cleanup_platform(void) {
#if defined(_WIN32)
    if (net_wsa_initialized) {
        WSACleanup();
        net_wsa_initialized = 0;
    }
#endif
}

EXPORT void vx_net_shutdown(void) {
    if (g_net.sock != NET_INVALID) {
        NET_CLOSE(g_net.sock);
        g_net.sock = NET_INVALID;
    }
    net_cleanup_platform();
}

static inline int net_init_platform(void) {
#if defined(_WIN32)
    if (!net_wsa_initialized) {
        WSADATA wsa;
        if (WSAStartup(MAKEWORD(2, 2), &wsa) != 0) return -1;
        net_wsa_initialized = 1;
    }
#endif
    return 0;
}

EXPORT int vx_net_host(int port) {
    if (g_net.sock != NET_INVALID) vx_net_shutdown();
    if (net_init_platform() < 0) return -1;

    vx_socket_t sock = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
    if (sock == NET_INVALID) return -1;

    int opt = 1;
#if defined(_WIN32)
    setsockopt(sock, SOL_SOCKET, SO_REUSEADDR, (const char*)&opt, sizeof(opt));
#else
    setsockopt(sock, SOL_SOCKET, SO_REUSEADDR, &opt, sizeof(opt));
#endif

    if (net_set_nonblocking(sock) < 0) {
        NET_CLOSE(sock);
        return -1;
    }

    struct sockaddr_in local = {0};
    local.sin_family = AF_INET;
    local.sin_addr.s_addr = htonl(INADDR_ANY);
    local.sin_port = htons((uint16_t)port);

    if (bind(sock, (struct sockaddr*)&local, sizeof(local)) == NET_ERROR) {
        NET_CLOSE(sock);
        return -1;
    }

#if defined(_WIN32)
    DWORD dwBytesReturned = 0;
    BOOL bNewBehavior = FALSE;
    WSAIoctl(sock, SIO_UDP_CONNRESET, &bNewBehavior, sizeof(bNewBehavior), NULL, 0, &dwBytesReturned, NULL, NULL);
#endif

    g_net.sock = sock;
    return 0;
}

EXPORT int vx_net_connect(uint8_t peer_id, const char* ip, int port) {
    if (g_net.sock == NET_INVALID || !ip || peer_id >= 8) return -1;

    g_net.peers[peer_id].addr.sin_family = AF_INET;
    g_net.peers[peer_id].addr.sin_port = htons((uint16_t)port);

    if (inet_pton(AF_INET, ip, &g_net.peers[peer_id].addr.sin_addr) <= 0) {
        struct hostent* he = gethostbyname(ip);
        if (!he || he->h_addrtype != AF_INET) return -1;
        memcpy(&g_net.peers[peer_id].addr.sin_addr, he->h_addr_list[0], he->h_length);
    }

    g_net.peers[peer_id].addr_len = sizeof(struct sockaddr_in);
    g_net.peers[peer_id].active = 1;
    return 0;
}

EXPORT void vx_net_set_session(uint64_t token) { g_net.session_token = token; }
EXPORT void vx_net_set_player_id(uint8_t id) { g_net.local_id = id; }

// Dumps a packet onto the network for all active peers
EXPORT void vx_net_send(LockstepPacket* pkt) {
    if (g_net.sock == NET_INVALID || !pkt) return;
    for (int i = 0; i < 8; i++) {
        if (i == g_net.local_id || !g_net.peers[i].active) continue;
        sendto(g_net.sock, (const char*)pkt, sizeof(LockstepPacket), 0,
              (struct sockaddr*)&g_net.peers[i].addr, g_net.peers[i].addr_len);
    }
}

// Add targeted routing so the pump can dynamically pack history per opponent
EXPORT void vx_net_send_to(LockstepPacket* pkt, uint8_t target_peer) {
    if (g_net.sock == NET_INVALID || !pkt || target_peer >= 8) return;
    if (!g_net.peers[target_peer].active) return;

    sendto(g_net.sock, (const char*)pkt, sizeof(LockstepPacket), 0,
           (struct sockaddr*)&g_net.peers[target_peer].addr,
           g_net.peers[target_peer].addr_len);
}

// Drains the OS UDP buffer directly into Lua's FFI memory block
EXPORT int vx_net_recv_all(LockstepPacket* out_buffer, int max_count) {
    if (g_net.sock == NET_INVALID || !out_buffer) return 0;

    struct sockaddr_in from;
    socklen_t from_len = sizeof(from);
    int count = 0;

    while (count < max_count) {
        ssize_t recvd = recvfrom(g_net.sock, (char*)&out_buffer[count], sizeof(LockstepPacket), 0, (struct sockaddr*)&from, &from_len);

        // EWOULDBLOCK means the OS buffer is empty. We are done for this frame.
        if (recvd < 0) break;

        // Only accept perfectly sized packets
        if (recvd == sizeof(LockstepPacket)) {
            if (out_buffer[count].session_token != g_net.session_token) continue;

            // UDP Pivot Hack: Learn IPs on the fly
            uint8_t pid = out_buffer[count].player_id;
            if (pid < 8) {
                g_net.peers[pid].addr = from;
                g_net.peers[pid].addr_len = from_len;
                g_net.peers[pid].active = 1;
            }
            count++;
        }
    }
    return count;
}

EXPORT int vx_net_stun_punch(const char* stun_server_ip, int stun_port, char* out_ip, int* out_port) {
    if (g_net.sock == NET_INVALID) return 0;

    struct sockaddr_in stun_addr = {0};
    stun_addr.sin_family = AF_INET;
    stun_addr.sin_port = htons((uint16_t)stun_port);
    inet_pton(AF_INET, stun_server_ip, &stun_addr.sin_addr);

    uint8_t req[20] = {0};
    req[0] = 0x00; req[1] = 0x01;
    req[4] = 0x21; req[5] = 0x12; req[6] = 0xA4; req[7] = 0x42;
    for(int i = 8; i < 20; i++) req[i] = i;

    sendto(g_net.sock, (const char*)req, 20, 0, (struct sockaddr*)&stun_addr, sizeof(stun_addr));

    uint8_t resp[1024];
    struct sockaddr_in from;
    socklen_t from_len = sizeof(from);

    for (int wait = 0; wait < 50; wait++) {
        ssize_t recvd = recvfrom(g_net.sock, (char*)resp, sizeof(resp), 0, (struct sockaddr*)&from, &from_len);
        if (recvd >= 20) {
            uint16_t msg_len = (resp[2] << 8) | resp[3];
            int offset = 20;
            while (offset < 20 + msg_len && offset + 4 <= recvd) {
                uint16_t attr_type = (resp[offset] << 8) | resp[offset+1];
                uint16_t attr_len = (resp[offset+2] << 8) | resp[offset+3];

                if (attr_type == 0x0020) {
                    uint16_t xport = (resp[offset+6] << 8) | resp[offset+7];
                    uint32_t xip = (resp[offset+8] << 24) | (resp[offset+9] << 16) | (resp[offset+10] << 8) | resp[offset+11];

                    *out_port = xport ^ 0x2112;
                    uint32_t real_ip = xip ^ 0x2112A442;
                    snprintf(out_ip, 16, "%d.%d.%d.%d", (real_ip >> 24) & 0xFF, (real_ip >> 16) & 0xFF, (real_ip >> 8) & 0xFF, real_ip & 0xFF);
                    return 1;
                }
                int padded_len = (attr_len + 3) & ~3;
                offset += 4 + padded_len;
            }
        }
#if defined(_WIN32)
        Sleep(10);
#else
        usleep(10000);
#endif
    }
    return 0;
}

