// Simple test client for ALVR IPC socket
// Simulates what the Vulkan layer does

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
#include <stdint.h>
#include <array>

// Minimal protocol structs (from protocol.h)
#define VK_UUID_SIZE 16

struct VkExtent3D {
    uint32_t width;
    uint32_t height;
    uint32_t depth;
};

struct VkImageCreateInfo {
    uint32_t sType;
    void* pNext;
    uint32_t flags;
    uint32_t imageType;
    uint32_t format;
    VkExtent3D extent;
    uint32_t mipLevels;
    uint32_t arrayLayers;
    uint32_t samples;
    uint32_t tiling;
    uint32_t usage;
    uint32_t sharingMode;
    uint32_t queueFamilyIndexCount;
    uint32_t* pQueueFamilyIndices;
    uint32_t initialLayout;
};

struct init_packet {
    uint32_t num_images;
    std::array<uint8_t, VK_UUID_SIZE> device_uuid;
    VkImageCreateInfo image_create_info;
    size_t mem_index;
    pid_t source_pid;
};

struct present_packet {
    uint32_t image;
    uint32_t frame;
    uint64_t semaphore_value;
    float pose[3][4];
};

int main() {
    const char* socket_path = "/tmp/alvr-ipc";

    printf("ALVR Socket Test Client\n");
    printf("Connecting to %s...\n", socket_path);

    int sock = socket(AF_UNIX, SOCK_STREAM, 0);
    if (sock == -1) {
        perror("socket");
        return 1;
    }

    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    strncpy(addr.sun_path, socket_path, sizeof(addr.sun_path) - 1);

    if (connect(sock, (struct sockaddr*)&addr, sizeof(addr)) == -1) {
        perror("connect");
        printf("\nSocket not found at %s\n", socket_path);
        printf("The CEncoder server needs to be running first.\n");
        printf("This happens when SteamVR loads the ALVR driver.\n");
        close(sock);
        return 1;
    }

    printf("Connected!\n");

    // Send init packet
    init_packet init = {};
    init.num_images = 3;
    init.image_create_info.extent.width = 1920;
    init.image_create_info.extent.height = 1080;
    init.image_create_info.extent.depth = 1;
    init.source_pid = getpid();

    printf("Sending init packet (pid=%d, %dx%d)...\n",
           init.source_pid,
           init.image_create_info.extent.width,
           init.image_create_info.extent.height);

    ssize_t sent = write(sock, &init, sizeof(init));
    if (sent != sizeof(init)) {
        perror("write init");
        close(sock);
        return 1;
    }
    printf("Init packet sent (%zd bytes)\n", sent);

    // Note: The server expects file descriptors via SCM_RIGHTS here
    // For a simple test, we'll just close
    printf("Test complete - connection works!\n");
    printf("(Full test would require sending GPU memory FDs via SCM_RIGHTS)\n");

    close(sock);
    return 0;
}
