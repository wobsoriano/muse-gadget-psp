/* Protobuf wire helpers, ported from the Muse Gadget SDK's proto.py. */
#include "muse_internal.h"

#include <string.h>

#define MAX_FIELD_NUMBER ((1u << 29) - 1u)
#define FIRST_RESERVED_FIELD_NUMBER 19000u
#define LAST_RESERVED_FIELD_NUMBER 19999u

static bool valid_field_number(uint64_t field)
{
    return field != 0 && field <= MAX_FIELD_NUMBER &&
           !(field >= FIRST_RESERVED_FIELD_NUMBER && field <= LAST_RESERVED_FIELD_NUMBER);
}

static bool valid_wire(int wire)
{
    return wire == MUSE_WIRE_VARINT || wire == MUSE_WIRE_FIXED64 || wire == MUSE_WIRE_DELIMITED ||
           wire == MUSE_WIRE_FIXED32;
}

int muse_pb_put_varint(muse_buf *b, uint64_t v)
{
    uint8_t out[10];
    size_t n = 0;
    do {
        uint8_t byte = (uint8_t)(v & 0x7Fu);
        v >>= 7;
        out[n++] = v ? (uint8_t)(byte | 0x80u) : byte;
    } while (v);
    return muse_buf_append(b, out, n);
}

int muse_pb_put_key(muse_buf *b, uint32_t field, int wire)
{
    if (!valid_field_number(field) || !valid_wire(wire)) {
        return MUSE_EINVAL;
    }
    return muse_pb_put_varint(b, ((uint64_t)field << 3) | (uint64_t)wire);
}

int muse_pb_put_uint(muse_buf *b, uint32_t field, uint64_t v)
{
    int rc = muse_pb_put_key(b, field, MUSE_WIRE_VARINT);
    return rc ? rc : muse_pb_put_varint(b, v);
}

int muse_pb_put_int64(muse_buf *b, uint32_t field, int64_t v)
{
    return muse_pb_put_uint(b, field, (uint64_t)v);
}

int muse_pb_put_bool(muse_buf *b, uint32_t field, bool v)
{
    return muse_pb_put_uint(b, field, v ? 1u : 0u);
}

int muse_pb_put_bytes(muse_buf *b, uint32_t field, const void *data, size_t len)
{
    int rc = muse_pb_put_key(b, field, MUSE_WIRE_DELIMITED);
    if (rc == 0) {
        rc = muse_pb_put_varint(b, len);
    }
    if (rc == 0 && len > 0) {
        rc = muse_buf_append(b, data, len);
    }
    return rc;
}

int muse_pb_put_str(muse_buf *b, uint32_t field, const char *s)
{
    return muse_pb_put_bytes(b, field, s, strlen(s));
}

int muse_pb_read_varint(muse_pb_reader *r, uint64_t *v)
{
    uint64_t value = 0;
    for (unsigned i = 0; i < 10; i++) {
        if (r->off >= r->len) {
            return MUSE_EPROTO;
        }
        uint8_t byte = r->data[r->off++];
        if (i == 9 && (byte & 0xFEu) != 0) {
            return MUSE_EPROTO;
        }
        value |= (uint64_t)(byte & 0x7Fu) << (7u * i);
        if ((byte & 0x80u) == 0) {
            *v = value;
            return 0;
        }
    }
    return MUSE_EPROTO;
}

int muse_pb_read_key(muse_pb_reader *r, uint32_t *field, int *wire)
{
    uint64_t key;
    if (muse_pb_read_varint(r, &key) != 0) {
        return MUSE_EPROTO;
    }
    uint64_t number = key >> 3;
    int type = (int)(key & 0x07u);
    if (!valid_field_number(number) || !valid_wire(type)) {
        return MUSE_EPROTO;
    }
    *field = (uint32_t)number;
    *wire = type;
    return 0;
}

int muse_pb_read_delimited(muse_pb_reader *r, const uint8_t **data, size_t *len)
{
    uint64_t length;
    if (muse_pb_read_varint(r, &length) != 0 || length > r->len - r->off) {
        return MUSE_EPROTO;
    }
    *data = r->data + r->off;
    *len = (size_t)length;
    r->off += (size_t)length;
    return 0;
}

static int skip_fixed(muse_pb_reader *r, size_t width)
{
    if (width > r->len - r->off) {
        return MUSE_EPROTO;
    }
    r->off += width;
    return 0;
}

int muse_pb_skip(muse_pb_reader *r, int wire)
{
    uint64_t ignored;
    const uint8_t *data;
    size_t len;
    switch (wire) {
    case MUSE_WIRE_VARINT:
        return muse_pb_read_varint(r, &ignored);
    case MUSE_WIRE_FIXED64:
        return skip_fixed(r, 8);
    case MUSE_WIRE_DELIMITED:
        return muse_pb_read_delimited(r, &data, &len);
    case MUSE_WIRE_FIXED32:
        return skip_fixed(r, 4);
    default:
        return MUSE_EPROTO;
    }
}
