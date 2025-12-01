// Standalone test server for ALVR IPC
// Mimics what CEncoder does - creates socket and waits for connections

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
#include <stdint.h>
#include <array>
#include <poll.h>

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

    printf("ALVR Test Server\n");
    printf("Creating socket at %s...\n", socket_path);

    // Remove existing socket
    unlink(socket_path);

    int server_fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (server_fd == -1) {
        perror("socket");
        return 1;
    }

    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    strncpy(addr.sun_path, socket_path, sizeof(addr.sun_path) - 1);

    if (bind(server_fd, (struct sockaddr*)&addr, sizeof(addr)) == -1) {
        perror("bind");
        close(server_fd);
        return 1;
    }

    if (listen(server_fd, 1) == -1) {
        perror("listen");
        close(server_fd);
        return 1;
    }

    printf("Listening on %s\n", socket_path);
    printf("Waiting for client connection...\n");
    printf("(Run test_socket or start a Vulkan app with ALVR layer)\n\n");

    int client_fd = accept(server_fd, NULL, NULL);
    if (client_fd == -1) {
        perror("accept");
        close(server_fd);
        return 1;
    }

    printf("Client connected!\n");

    // Read init packet
    init_packet init;
    ssize_t received = read(client_fd, &init, sizeof(init));
    if (received == sizeof(init)) {
        printf("Received init packet:\n");
        printf("  num_images: %u\n", init.num_images);
        printf("  image size: %ux%u\n",
               init.image_create_info.extent.width,
               init.image_create_info.extent.height);
        printf("  source_pid: %d\n", init.source_pid);
    } else {
        printf("Received %zd bytes (expected %zu)\n", received, sizeof(init));
    }

    // Try to receive file descriptors via SCM_RIGHTS
    printf("\nWaiting for file descriptors (SCM_RIGHTS)...\n");

    struct msghdr msg;
    struct cmsghdr* cmsg;
    union {
        struct cmsghdr cm;
        uint8_t buffer[CMSG_SPACE(sizeof(int) * 6)];
    } control;
    struct iovec iov[1];
    char data[1];
    int fds[6] = {-1, -1, -1, -1, -1, -1};

    memset(&msg, 0, sizeof(msg));
    msg.msg_control = &control;
    msg.msg_controllen = sizeof(control);
    iov[0].iov_base = data;
    iov[0].iov_len = 1;
    msg.msg_iov = iov;
    msg.msg_iovlen = 1;

    // Set timeout for recvmsg
    struct pollfd pfd = {client_fd, POLLIN, 0};
    int poll_ret = poll(&pfd, 1, 5000);  // 5 second timeout

    if (poll_ret > 0) {
        ssize_t ret = recvmsg(client_fd, &msg, 0);
        if (ret > 0) {
            for (cmsg = CMSG_FIRSTHDR(&msg); cmsg != NULL; cmsg = CMSG_NXTHDR(&msg, cmsg)) {
                if (cmsg->cmsg_level == SOL_SOCKET && cmsg->cmsg_type == SCM_RIGHTS) {
                    memcpy(fds, CMSG_DATA(cmsg), sizeof(fds));
                    printf("Received file descriptors: %d %d %d %d %d %d\n",
                           fds[0], fds[1], fds[2], fds[3], fds[4], fds[5]);
                    break;
                }
            }
        }
    } else {
        printf("No file descriptors received (timeout or test client)\n");
    }

    // Read present packets
    printf("\nWaiting for present packets (Ctrl+C to exit)...\n");

    while (1) {
        struct pollfd pfd = {client_fd, POLLIN, 0};
        int ret = poll(&pfd, 1, 1000);

        if (ret < 0) {
            perror("poll");
            break;
        } else if (ret == 0) {
            printf(".");
            fflush(stdout);
            continue;
        }

        present_packet packet;
        ssize_t n = read(client_fd, &packet, sizeof(packet));
        if (n == 0) {
            printf("\nClient disconnected\n");
            break;
        } else if (n < 0) {
            perror("read");
            break;
        } else if (n == sizeof(packet)) {
            printf("\nFrame %u: image=%u, semaphore=%llu\n",
                   packet.frame, packet.image, packet.semaphore_value);
        }
    }

    close(client_fd);
    close(server_fd);
    unlink(socket_path);

    printf("Server shutdown\n");
    return 0;
}
