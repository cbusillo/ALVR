// Test frame sender - simulates Wine ALVR driver sending frames to macOS encoder
// Compile: x86_64-w64-mingw32-gcc -o test_frame_sender.exe test_frame_sender.c -lws2_32

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <winsock2.h>
#include <ws2tcpip.h>

#pragma comment(lib, "ws2_32.lib")

#define ALVR_TCP_PORT 9944
#define TEST_WIDTH 1920
#define TEST_HEIGHT 1080

#pragma pack(push, 1)
typedef struct {
    unsigned int num_images;
    unsigned char device_uuid[16];
    unsigned int width;
    unsigned int height;
    unsigned int format;
    unsigned int mem_index;
    unsigned int source_pid;
} socket_init_packet;

typedef struct {
    unsigned int image_index;
    unsigned int frame_number;
    unsigned long long semaphore_value;
    float pose[3][4];
    unsigned int width;
    unsigned int height;
    unsigned int stride;
    unsigned char is_idr;
    unsigned int data_size;
} socket_frame_packet;
#pragma pack(pop)

int main(int argc, char* argv[]) {
    int num_frames = 10;
    if (argc > 1) {
        num_frames = atoi(argv[1]);
    }

    printf("ALVR Frame Sender Test\n");
    printf("=======================\n");
    printf("Will send %d test frames to localhost:%d\n\n", num_frames, ALVR_TCP_PORT);

    // Initialize Winsock
    WSADATA wsaData;
    int result = WSAStartup(MAKEWORD(2, 2), &wsaData);
    if (result != 0) {
        printf("WSAStartup failed: %d\n", result);
        return 1;
    }

    // Create TCP socket
    SOCKET sock = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (sock == INVALID_SOCKET) {
        printf("socket() failed: %d\n", WSAGetLastError());
        WSACleanup();
        return 1;
    }

    // Disable Nagle's algorithm
    int flag = 1;
    setsockopt(sock, IPPROTO_TCP, TCP_NODELAY, (char*)&flag, sizeof(flag));

    // Connect to macOS encoder
    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_port = htons(ALVR_TCP_PORT);
    addr.sin_addr.s_addr = inet_addr("127.0.0.1");

    printf("Connecting to 127.0.0.1:%d...\n", ALVR_TCP_PORT);
    if (connect(sock, (struct sockaddr*)&addr, sizeof(addr)) == SOCKET_ERROR) {
        printf("connect() failed: %d\n", WSAGetLastError());
        printf("Make sure the macOS encoder is running!\n");
        closesocket(sock);
        WSACleanup();
        return 1;
    }
    printf("Connected!\n\n");

    // Send init packet
    socket_init_packet init = {0};
    init.num_images = 3;
    init.width = TEST_WIDTH;
    init.height = TEST_HEIGHT;
    init.format = 87;  // DXGI_FORMAT_B8G8R8A8_UNORM
    init.source_pid = GetCurrentProcessId();

    printf("Sending init packet: %dx%d\n", init.width, init.height);
    if (send(sock, (char*)&init, sizeof(init), 0) == SOCKET_ERROR) {
        printf("send init failed: %d\n", WSAGetLastError());
        closesocket(sock);
        WSACleanup();
        return 1;
    }

    // Allocate test pixel buffer (gradient pattern)
    unsigned int data_size = TEST_WIDTH * TEST_HEIGHT * 4;  // BGRA
    unsigned char* pixels = (unsigned char*)malloc(data_size);
    if (!pixels) {
        printf("malloc failed\n");
        closesocket(sock);
        WSACleanup();
        return 1;
    }

    // Send frames
    for (int frame = 0; frame < num_frames; frame++) {
        // Create test pattern (shifts each frame for visual verification)
        for (int y = 0; y < TEST_HEIGHT; y++) {
            for (int x = 0; x < TEST_WIDTH; x++) {
                int idx = (y * TEST_WIDTH + x) * 4;
                pixels[idx + 0] = (x + frame * 10) & 0xFF;      // B
                pixels[idx + 1] = (y + frame * 5) & 0xFF;       // G
                pixels[idx + 2] = (frame * 20) & 0xFF;          // R
                pixels[idx + 3] = 255;                           // A
            }
        }

        // Build frame packet
        socket_frame_packet pkt = {0};
        pkt.image_index = frame % 3;
        pkt.frame_number = frame;
        pkt.semaphore_value = frame;
        pkt.width = TEST_WIDTH;
        pkt.height = TEST_HEIGHT;
        pkt.stride = TEST_WIDTH * 4;
        pkt.is_idr = (frame == 0) ? 1 : 0;  // First frame is IDR
        pkt.data_size = data_size;

        // Identity pose
        pkt.pose[0][0] = 1.0f; pkt.pose[1][1] = 1.0f; pkt.pose[2][2] = 1.0f;

        // Send header
        if (send(sock, (char*)&pkt, sizeof(pkt), 0) == SOCKET_ERROR) {
            printf("send frame header failed: %d\n", WSAGetLastError());
            break;
        }

        // Send pixels
        if (send(sock, (char*)pixels, data_size, 0) == SOCKET_ERROR) {
            printf("send pixel data failed: %d\n", WSAGetLastError());
            break;
        }

        printf("Sent frame %d (%u bytes)\n", frame, (unsigned int)(sizeof(pkt) + data_size));
        Sleep(16);  // ~60fps timing
    }

    printf("\nDone! Sent %d frames.\n", num_frames);

    free(pixels);
    closesocket(sock);
    WSACleanup();
    return 0;
}
