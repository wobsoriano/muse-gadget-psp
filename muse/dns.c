/* A DNS question for one IPv4 address and the reading of its answer. Pure:
 * the port sends and receives the bytes. */
#include "muse_internal.h"

#include <string.h>

size_t muse_dns_query(uint8_t *packet, size_t cap, const char *host, uint16_t id)
{
    static const uint8_t tail[5] = {0, 0, 1, 0, 1}; /* end of name, type A, class IN */
    size_t n = 12;
    if (cap < n) {
        return 0;
    }
    memset(packet, 0, n);
    packet[0] = (uint8_t)(id >> 8);
    packet[1] = (uint8_t)(id & 0xFF);
    packet[2] = 1; /* recursion wanted */
    packet[5] = 1; /* one question */
    for (const char *label = host; *label != '\0';) {
        size_t length = strcspn(label, ".");
        if (length == 0 || length > 63 || n + length + 1 + sizeof(tail) > cap) {
            return 0;
        }
        packet[n++] = (uint8_t)length;
        memcpy(packet + n, label, length);
        n += length;
        label += length + (label[length] == '.' ? 1u : 0u);
    }
    if (n == 12) {
        return 0;
    }
    memcpy(packet + n, tail, sizeof(tail));
    return n + sizeof(tail);
}

int muse_dns_answer(const uint8_t *packet, size_t got, uint16_t id, uint8_t address[4])
{
    if (got < 12 || packet[0] != (uint8_t)(id >> 8) || packet[1] != (uint8_t)(id & 0xFF) ||
        (packet[2] & 0x80) == 0 || (packet[3] & 15) != 0) {
        return MUSE_EPROTO;
    }
    size_t questions = (size_t)(packet[4] << 8 | packet[5]);
    size_t answers = (size_t)(packet[6] << 8 | packet[7]);
    size_t at = 12;
    for (size_t record = 0; record < questions + answers; record++) {
        /* A name is a run of labels ending in a zero, or in a two byte pointer. */
        while (at < got && packet[at] != 0 && (packet[at] & 0xC0) != 0xC0) {
            at += (size_t)packet[at] + 1;
        }
        if (at >= got) {
            return MUSE_EPROTO;
        }
        at += packet[at] == 0 ? 1u : 2u;
        if (record < questions) {
            at += 4;
            continue;
        }
        if (at + 10 > got) {
            return MUSE_EPROTO;
        }
        unsigned type = (unsigned)(packet[at] << 8 | packet[at + 1]);
        size_t length = (size_t)(packet[at + 8] << 8 | packet[at + 9]);
        at += 10;
        if (at + length > got) {
            return MUSE_EPROTO;
        }
        if (type == 1 && length == 4) {
            memcpy(address, packet + at, 4);
            return MUSE_OK;
        }
        at += length;
    }
    return MUSE_EPROTO;
}
