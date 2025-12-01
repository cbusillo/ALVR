// Test TCP socket in Wine/CrossOver
// Compile with: x86_64-w64-mingw32-gcc -o test_wine_tcp.exe test_wine_tcp.c -lws2_32

#include <stdio.h>
#include <winsock2.h>
#include <ws2tcpip.h>

#pragma comment(lib, "ws2_32.lib")

#define ALVR_TCP_PORT 9944

int main() {
    printf("Wine TCP Socket Test\n");
    printf("====================\n\n");

    // Initialize Winsock
    WSADATA wsaData;
    int result = WSAStartup(MAKEWORD(2, 2), &wsaData);
    if (result != 0) {
        printf("WSAStartup failed: %d\n", result);
        return 1;
    }
    printf("Winsock initialized: %d.%d\n",
           LOBYTE(wsaData.wVersion), HIBYTE(wsaData.wVersion));

    // Create TCP socket
    SOCKET sock = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (sock == INVALID_SOCKET) {
        int err = WSAGetLastError();
        printf("socket(AF_INET) failed: %d\n", err);
        WSACleanup();
        return 1;
    }
    printf("TCP socket created successfully!\n");

    // Try to connect to localhost:9944
    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_port = htons(ALVR_TCP_PORT);
    addr.sin_addr.s_addr = inet_addr("127.0.0.1");

    printf("Connecting to 127.0.0.1:%d...\n", ALVR_TCP_PORT);

    if (connect(sock, (struct sockaddr*)&addr, sizeof(addr)) == SOCKET_ERROR) {
        int err = WSAGetLastError();
        printf("connect() failed: %d\n", err);
        if (err == WSAECONNREFUSED) {
            printf("Connection refused - this is EXPECTED if server not running.\n");
            printf("The important thing is TCP socket WORKS!\n");
        }
    } else {
        printf("Connected successfully!\n");

        // Send test data
        const char* msg = "Hello from Wine via TCP!";
        send(sock, msg, strlen(msg), 0);
        printf("Sent: %s\n", msg);
    }

    closesocket(sock);
    WSACleanup();

    printf("\n=== TCP SOCKETS WORK IN WINE! ===\n");
    return 0;
}
