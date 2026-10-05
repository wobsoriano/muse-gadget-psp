/* The library's DNS packets against a real name server.
 *
 *   dns_check <server> <host>...
 */
#include <arpa/inet.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#include "muse_internal.h"

int main(int argc, char **argv)
{
    int failed = 0;
    for (int i = 2; i < argc; i++) {
        uint8_t packet[512], address[4];
        uint16_t id = (uint16_t)(0x4d00 + i);
        size_t n = muse_dns_query(packet, sizeof(packet), argv[i], id);
        struct sockaddr_in server = {.sin_family = AF_INET, .sin_port = htons(53)};
        inet_pton(AF_INET, argv[1], &server.sin_addr);
        int fd = socket(AF_INET, SOCK_DGRAM, 0);
        struct pollfd ready = {fd, POLLIN, 0};
        ssize_t got = -1;
        if (n > 0 && sendto(fd, packet, n, 0, (struct sockaddr *)&server, sizeof(server)) == (ssize_t)n &&
            poll(&ready, 1, 3000) > 0) {
            got = recv(fd, packet, sizeof(packet), 0);
        }
        close(fd);
        int rc = got > 0 ? muse_dns_answer(packet, (size_t)got, id, address) : -1;
        if (rc == 0) {
            printf("%s -> %u.%u.%u.%u (%zd byte answer)\n", argv[i], address[0], address[1],
                   address[2], address[3], got);
        } else {
            printf("%s -> FAILED (query %zu bytes, answer %zd bytes, rc %d)\n", argv[i], n, got, rc);
            failed = 1;
        }
    }
    return failed;
}
