// Test if Wine/CrossOver supports AF_UNIX sockets
// Compile with: x86_64-w64-mingw32-gcc -o test_wine_socket.exe test_wine_socket.c -lws2_32

#include <stdio.h>
#include <winsock2.h>
#include <afunix.h>

#pragma comment(lib, "ws2_32.lib")

int main() {
    printf("Wine AF_UNIX Socket Test\n");
    printf("========================\n\n");

    // Initialize Winsock
    WSADATA wsaData;
    int result = WSAStartup(MAKEWORD(2, 2), &wsaData);
    if (result != 0) {
        printf("WSAStartup failed: %d\n", result);
        return 1;
    }
    printf("Winsock initialized: %d.%d\n",
           LOBYTE(wsaData.wVersion), HIBYTE(wsaData.wVersion));

    // Try to create AF_UNIX socket
    SOCKET sock = socket(AF_UNIX, SOCK_STREAM, 0);
    if (sock == INVALID_SOCKET) {
        int err = WSAGetLastError();
        printf("socket(AF_UNIX) failed: %d\n", err);
        if (err == WSAEAFNOSUPPORT) {
            printf("AF_UNIX not supported - need different approach!\n");
        }
        WSACleanup();
        return 1;
    }
    printf("AF_UNIX socket created successfully!\n");

    // Try to connect to /tmp/alvr-ipc
    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    strcpy(addr.sun_path, "/tmp/alvr-ipc");

    printf("Connecting to %s...\n", addr.sun_path);

    if (connect(sock, (struct sockaddr*)&addr, sizeof(addr)) == SOCKET_ERROR) {
        int err = WSAGetLastError();
        printf("connect() failed: %d\n", err);
        if (err == WSAECONNREFUSED) {
            printf("Connection refused - this is EXPECTED if server not running.\n");
            printf("The important thing is AF_UNIX socket creation WORKED!\n");
        }
    } else {
        printf("Connected successfully!\n");

        // Send test data
        const char* msg = "Hello from Wine!";
        send(sock, msg, strlen(msg), 0);
        printf("Sent: %s\n", msg);
    }

    closesocket(sock);
    WSACleanup();

    printf("\n=== AF_UNIX SUPPORTED IN WINE! ===\n");
    return 0;
}
