#include "muse_internal.h"

#include <string.h>

static const struct {
    const char *prefix;
    bool secure;
} schemes[] = {
    {"https://", true},
    {"http://", false},
    {"wss://", true},
    {"ws://", false},
};

int muse_url_parse(const char *url, muse_url *out)
{
    if (url == NULL || out == NULL) {
        return MUSE_EINVAL;
    }
    const char *rest = NULL;
    for (size_t i = 0; i < sizeof(schemes) / sizeof(schemes[0]); i++) {
        size_t n = strlen(schemes[i].prefix);
        if (strncmp(url, schemes[i].prefix, n) == 0) {
            rest = url + n;
            out->secure = schemes[i].secure;
            break;
        }
    }
    if (rest == NULL) {
        return MUSE_EINVAL;
    }

    size_t authority_len = strcspn(rest, "/?");
    const char *colon = memchr(rest, ':', authority_len);
    size_t host_len = colon ? (size_t)(colon - rest) : authority_len;
    if (host_len == 0) {
        return MUSE_EINVAL;
    }
    if (host_len >= sizeof(out->host)) {
        return MUSE_ETOOBIG;
    }
    memcpy(out->host, rest, host_len);
    out->host[host_len] = '\0';

    out->port = out->secure ? 443 : 80;
    if (colon != NULL) {
        const char *digits = colon + 1;
        size_t ndigits = authority_len - host_len - 1;
        if (ndigits == 0 || ndigits > 5) {
            return MUSE_EINVAL;
        }
        uint32_t port = 0;
        for (size_t i = 0; i < ndigits; i++) {
            if (digits[i] < '0' || digits[i] > '9') {
                return MUSE_EINVAL;
            }
            port = port * 10 + (uint32_t)(digits[i] - '0');
        }
        if (port == 0 || port > 65535) {
            return MUSE_EINVAL;
        }
        out->port = (uint16_t)port;
    }

    const char *path = rest + authority_len;
    size_t lead = *path == '/' ? 0 : 1;
    size_t path_len = strlen(path);
    if (lead + path_len >= sizeof(out->path)) {
        return MUSE_ETOOBIG;
    }
    out->path[0] = '/';
    memcpy(out->path + lead, path, path_len + 1);
    return MUSE_OK;
}

int muse_url_encode_component(const char *s, char *out, size_t out_len)
{
    static const char hex[] = "0123456789ABCDEF";
    static const char safe[] = "-_.!~*'()";
    size_t n = 0;
    for (; *s != '\0'; s++) {
        unsigned char c = (unsigned char)*s;
        bool keep = (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') ||
                    strchr(safe, c) != NULL;
        size_t need = keep ? 1 : 3;
        if (out_len < need + 1 || n > out_len - need - 1) {
            return MUSE_ETOOBIG;
        }
        if (keep) {
            out[n++] = (char)c;
        } else {
            out[n++] = '%';
            out[n++] = hex[c >> 4];
            out[n++] = hex[c & 15];
        }
    }
    if (n >= out_len) {
        return MUSE_ETOOBIG;
    }
    out[n] = '\0';
    return (int)n;
}
