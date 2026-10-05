#include "muse_internal.h"

#include <stdlib.h>
#include <string.h>

void muse_buf_init(muse_buf *b, size_t max)
{
    b->data = NULL;
    b->len = 0;
    b->cap = 0;
    b->max = max;
}

int muse_buf_reserve(muse_buf *b, size_t total)
{
    if (total > b->max) {
        return MUSE_ETOOBIG;
    }
    if (total <= b->cap) {
        return MUSE_OK;
    }
    size_t cap = b->cap ? b->cap : 256;
    while (cap < total) {
        cap = cap > b->max / 2 ? b->max : cap * 2;
    }
    if (cap > b->max) {
        cap = b->max;
    }
    uint8_t *data = realloc(b->data, cap);
    if (data == NULL) {
        return MUSE_ENOMEM;
    }
    b->data = data;
    b->cap = cap;
    return MUSE_OK;
}

int muse_buf_append(muse_buf *b, const void *data, size_t len)
{
    if (len == 0) {
        return MUSE_OK;
    }
    if (b->len > b->max || len > b->max - b->len) {
        return MUSE_ETOOBIG;
    }
    int rc = muse_buf_reserve(b, b->len + len);
    if (rc != MUSE_OK) {
        return rc;
    }
    memcpy(b->data + b->len, data, len);
    b->len += len;
    return MUSE_OK;
}

int muse_buf_append_str(muse_buf *b, const char *s)
{
    return muse_buf_append(b, s, strlen(s));
}

void muse_buf_consume(muse_buf *b, size_t n)
{
    if (n >= b->len) {
        b->len = 0;
        return;
    }
    memmove(b->data, b->data + n, b->len - n);
    b->len -= n;
}

void muse_buf_clear(muse_buf *b)
{
    b->len = 0;
}

void muse_buf_free(muse_buf *b)
{
    free(b->data);
    b->data = NULL;
    b->len = 0;
    b->cap = 0;
}

char *muse_buf_take_str(muse_buf *b)
{
    char *s = realloc(b->data, b->len + 1);
    if (s == NULL) {
        muse_buf_free(b);
        return NULL;
    }
    s[b->len] = '\0';
    b->data = NULL;
    b->len = 0;
    b->cap = 0;
    return s;
}

char *muse_strdup(const char *s)
{
    if (s == NULL) {
        return NULL;
    }
    size_t n = strlen(s) + 1;
    char *copy = malloc(n);
    if (copy != NULL) {
        memcpy(copy, s, n);
    }
    return copy;
}
