// TCP test server for ALVR Wine→macOS communication
// Compile: clang++ -std=c++17 -o test_tcp_server test_tcp_server.cpp

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <arpa/inet.h>

#define ALVR_TCP_PORT 9944

int main() {
    printf("ALVR TCP Test Server\n");
    printf("====================\n\n");

    int server_fd = socket(AF_INET, SOCK_STREAM, 0);
    if (server_fd < 0) {
        perror("socket");
        return 1;
    }

    // Allow port reuse
    int opt = 1;
    setsockopt(server_fd, SOL_SOCKET, SO_REUSEADDR, &opt, sizeof(opt));

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = INADDR_ANY;
    addr.sin_port = htons(ALVR_TCP_PORT);

    if (bind(server_fd, (struct sockaddr*)&addr, sizeof(addr)) < 0) {
        perror("bind");
        close(server_fd);
        return 1;
    }

    if (listen(server_fd, 1) < 0) {
        perror("listen");
        close(server_fd);
        return 1;
    }

    printf("Listening on port %d...\n", ALVR_TCP_PORT);
    printf("Run the Wine test: test_wine_tcp.exe\n\n");

    struct sockaddr_in client_addr;
    socklen_t client_len = sizeof(client_addr);
    int client_fd = accept(server_fd, (struct sockaddr*)&client_addr, &client_len);
    if (client_fd < 0) {
        perror("accept");
        close(server_fd);
        return 1;
    }

    printf("Client connected from %s:%d\n",
           inet_ntoa(client_addr.sin_addr),
           ntohs(client_addr.sin_port));

    // Read data
    char buffer[1024];
    ssize_t n = recv(client_fd, buffer, sizeof(buffer) - 1, 0);
    if (n > 0) {
        buffer[n] = '\0';
        printf("Received %zd bytes: %s\n", n, buffer);
    } else if (n == 0) {
        printf("Client disconnected\n");
    } else {
        perror("recv");
    }

    printf("\n=== WINE TO MACOS TCP COMMUNICATION WORKS! ===\n");

    close(client_fd);
    close(server_fd);
    return 0;
}
