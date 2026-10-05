/* Client side of the Muse Noise session: the Noise_XX_25519_AESGCM_SHA256
 * initiator, chunked transport frames and the service envelopes. Ported from
 * the Muse Gadget SDK's noise package by way of an earlier Python port. */
#include "muse_internal.h"

#include <stdlib.h>
#include <string.h>

#include <mbedtls/ecp.h>
#include <mbedtls/gcm.h>
#include <mbedtls/md.h>
#include <mbedtls/platform_util.h>
#include <mbedtls/sha256.h>

#define DH_LEN 32u
#define TAG_LEN 16u
#define MIN_MSG2_LEN (DH_LEN + (DH_LEN + TAG_LEN) + TAG_LEN)
#define MAX_HANDSHAKE_MSG 65535u
#define MAX_SAFE_NONCE ((UINT64_C(1) << 53) - 1u)
#define RESET_CANCELLED 1
#define MAX_REQUEST_BYTES ((size_t)MUSE_MAX_TOTAL_CHUNKS * MUSE_MAX_CHUNK_PAYLOAD)

static const char PROTOCOL_NAME[] = "Noise_XX_25519_AESGCM_SHA256";

static const uint8_t LOW_ORDER_POINTS[7][DH_LEN] = {
    {0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
     0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00},
    {0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
     0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00},
    {0xe0, 0xeb, 0x7a, 0x7c, 0x3b, 0x41, 0xb8, 0xae, 0x16, 0x56, 0xe3, 0xfa, 0xf1, 0x9f, 0xc4, 0x6a,
     0xda, 0x09, 0x8d, 0xeb, 0x9c, 0x32, 0xb1, 0xfd, 0x86, 0x62, 0x05, 0x16, 0x5f, 0x49, 0xb8, 0x00},
    {0x5f, 0x9c, 0x95, 0xbc, 0xa3, 0x50, 0x8c, 0x24, 0xb1, 0xd0, 0xb1, 0x55, 0x9c, 0x83, 0xef, 0x5b,
     0x04, 0x44, 0x5c, 0xc4, 0x58, 0x1c, 0x8e, 0x86, 0xd8, 0x22, 0x4e, 0xdd, 0xd0, 0x9f, 0x11, 0x57},
    {0xec, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
     0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f},
    {0xed, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
     0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f},
    {0xee, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
     0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f},
};

typedef struct cipher {
    mbedtls_gcm_context gcm;
    bool keyed;      /* an unkeyed cipher passes bytes through, as early in the handshake */
    bool poisoned;
    uint64_t nonce;
} cipher;

typedef struct piece {
    uint8_t *data;
    size_t len;
    bool present;
} piece;

typedef struct assembly {
    bool used;
    int64_t chunk_id;
    uint32_t total;
    uint32_t have;
    size_t bytes;
    uint32_t started_ms;
    piece *pieces;
} assembly;

typedef enum handshake_step {
    STEP_FAILED = -1,
    STEP_WRITE_MSG1 = 0,
    STEP_READ_MSG2 = 1,
    STEP_WRITE_MSG3 = 2,
    STEP_SPLIT = 3,
    STEP_TRANSPORT = 4
} handshake_step;

struct muse_noise {
    muse_tls *rng;
    handshake_step step;
    uint8_t e_priv[DH_LEN], e_pub[DH_LEN];
    uint8_t s_priv[DH_LEN], s_pub[DH_LEN];
    uint8_t re[DH_LEN];
    uint8_t h[32];
    uint8_t ck[32];
    cipher handshake;
    cipher send;
    cipher recv;
    int64_t next_stream_id;
    assembly pending[MUSE_MAX_PENDING_ASSEMBLIES];
};

/* --- primitives ------------------------------------------------------------ */

static int rng_cb(void *ctx, unsigned char *buf, size_t len)
{
    return muse_tls_random((muse_tls *)ctx, buf, len);
}

static int hmac_sha256(const uint8_t key[32], const uint8_t *data, size_t len, uint8_t out[32])
{
    const mbedtls_md_info_t *info = mbedtls_md_info_from_type(MBEDTLS_MD_SHA256);
    if (info == NULL || mbedtls_md_hmac(info, key, 32, data, len, out) != 0) {
        return MUSE_EIO;
    }
    return 0;
}

static int hkdf2(const uint8_t ck[32], const uint8_t *ikm, size_t ikm_len, uint8_t out1[32],
                 uint8_t out2[32])
{
    uint8_t temp[32];
    uint8_t block[33];
    static const uint8_t one = 0x01;
    int rc = hmac_sha256(ck, ikm, ikm_len, temp);
    if (rc == 0) {
        rc = hmac_sha256(temp, &one, 1, out1);
    }
    if (rc == 0) {
        memcpy(block, out1, 32);
        block[32] = 0x02;
        rc = hmac_sha256(temp, block, sizeof block, out2);
    }
    mbedtls_platform_zeroize(temp, sizeof temp);
    mbedtls_platform_zeroize(block, sizeof block);
    return rc;
}

static int x25519(muse_noise *n, const uint8_t priv[DH_LEN], const uint8_t *peer, uint8_t out[DH_LEN])
{
    mbedtls_ecp_group grp;
    mbedtls_ecp_point p, r;
    mbedtls_mpi d;
    size_t olen = 0;
    int ret;

    mbedtls_ecp_group_init(&grp);
    mbedtls_ecp_point_init(&p);
    mbedtls_ecp_point_init(&r);
    mbedtls_mpi_init(&d);

    MBEDTLS_MPI_CHK(mbedtls_ecp_group_load(&grp, MBEDTLS_ECP_DP_CURVE25519));
    MBEDTLS_MPI_CHK(mbedtls_mpi_read_binary_le(&d, priv, DH_LEN));
    if (peer != NULL) {
        MBEDTLS_MPI_CHK(mbedtls_ecp_point_read_binary(&grp, &p, peer, DH_LEN));
    } else {
        MBEDTLS_MPI_CHK(mbedtls_ecp_copy(&p, &grp.G));
    }
    MBEDTLS_MPI_CHK(mbedtls_ecp_mul(&grp, &r, &d, &p, rng_cb, n->rng));
    MBEDTLS_MPI_CHK(mbedtls_ecp_point_write_binary(&grp, &r, MBEDTLS_ECP_PF_UNCOMPRESSED, &olen,
                                                   out, DH_LEN));
    if (olen != DH_LEN) {
        ret = MBEDTLS_ERR_ECP_BAD_INPUT_DATA;
    }

cleanup:
    mbedtls_mpi_free(&d);
    mbedtls_ecp_point_free(&r);
    mbedtls_ecp_point_free(&p);
    mbedtls_ecp_group_free(&grp);
    return ret == 0 ? 0 : MUSE_EPROTO;
}

static int generate_keypair(muse_noise *n, uint8_t priv[DH_LEN], uint8_t pub[DH_LEN])
{
    if (muse_tls_random(n->rng, priv, DH_LEN) != 0) {
        return MUSE_EIO;
    }
    /* RFC 7748 clamping; mbedTLS refuses an unclamped Curve25519 scalar. */
    priv[0] &= 248u;
    priv[31] &= 127u;
    priv[31] |= 64u;
    return x25519(n, priv, NULL, pub);
}

static int dh(muse_noise *n, const uint8_t priv[DH_LEN], const uint8_t pub[DH_LEN],
              uint8_t shared[DH_LEN])
{
    for (size_t i = 0; i < sizeof LOW_ORDER_POINTS / sizeof LOW_ORDER_POINTS[0]; i++) {
        if (memcmp(pub, LOW_ORDER_POINTS[i], DH_LEN) == 0) {
            return MUSE_EPROTO;
        }
    }
    if (x25519(n, priv, pub, shared) != 0) {
        return MUSE_EPROTO;
    }
    uint8_t acc = 0;
    for (size_t i = 0; i < DH_LEN; i++) {
        acc |= shared[i];
    }
    return acc == 0 ? MUSE_EPROTO : 0;
}

/* --- CipherState ----------------------------------------------------------- */

static void cipher_init(cipher *c)
{
    mbedtls_gcm_init(&c->gcm);
    c->keyed = false;
    c->poisoned = false;
    c->nonce = 0;
}

static void cipher_free(cipher *c)
{
    mbedtls_gcm_free(&c->gcm);
    c->keyed = false;
}

static int cipher_set_key(cipher *c, const uint8_t key[32])
{
    cipher_free(c);
    cipher_init(c);
    if (mbedtls_gcm_setkey(&c->gcm, MBEDTLS_CIPHER_ID_AES, key, 256) != 0) {
        c->poisoned = true;
        return MUSE_EIO;
    }
    c->keyed = true;
    return 0;
}

static int cipher_next_iv(cipher *c, uint8_t iv[12])
{
    if (c->poisoned) {
        return MUSE_EPROTO;
    }
    if (c->nonce >= MAX_SAFE_NONCE) {
        c->poisoned = true;
        return MUSE_EPROTO;
    }
    memset(iv, 0, 4);
    for (unsigned i = 0; i < 8; i++) {
        iv[4 + i] = (uint8_t)(c->nonce >> (56u - 8u * i));
    }
    c->nonce++;
    return 0;
}

static size_t cipher_overhead(const cipher *c)
{
    return c->keyed ? TAG_LEN : 0u;
}

/* out holds len + cipher_overhead(c) bytes. */
static int cipher_encrypt(cipher *c, const uint8_t *ad, size_t ad_len, const uint8_t *in, size_t len,
                          uint8_t *out)
{
    if (!c->keyed) {
        if (len > 0) {
            memcpy(out, in, len);
        }
        return 0;
    }
    uint8_t iv[12];
    int rc = cipher_next_iv(c, iv);
    if (rc != 0) {
        return rc;
    }
    if (mbedtls_gcm_crypt_and_tag(&c->gcm, MBEDTLS_GCM_ENCRYPT, len, iv, sizeof iv, ad, ad_len, in,
                                  out, TAG_LEN, out + len) != 0) {
        c->poisoned = true;
        return MUSE_EIO;
    }
    return 0;
}

/* out holds len - cipher_overhead(c) bytes; *out_len receives that count. */
static int cipher_decrypt(cipher *c, const uint8_t *ad, size_t ad_len, const uint8_t *in, size_t len,
                          uint8_t *out, size_t *out_len)
{
    if (!c->keyed) {
        if (len > 0) {
            memcpy(out, in, len);
        }
        *out_len = len;
        return 0;
    }
    uint8_t iv[12];
    int rc = cipher_next_iv(c, iv);
    if (rc != 0) {
        return rc;
    }
    if (len < TAG_LEN ||
        mbedtls_gcm_auth_decrypt(&c->gcm, len - TAG_LEN, iv, sizeof iv, ad, ad_len,
                                 in + (len - TAG_LEN), TAG_LEN, in, out) != 0) {
        c->poisoned = true;
        return MUSE_EPROTO;
    }
    *out_len = len - TAG_LEN;
    return 0;
}

/* --- SymmetricState -------------------------------------------------------- */

static int mix_hash(muse_noise *n, const uint8_t *data, size_t len)
{
    mbedtls_sha256_context ctx;
    mbedtls_sha256_init(&ctx);
    int ret = mbedtls_sha256_starts_ret(&ctx, 0);
    if (ret == 0) {
        ret = mbedtls_sha256_update_ret(&ctx, n->h, sizeof n->h);
    }
    if (ret == 0 && len > 0) {
        ret = mbedtls_sha256_update_ret(&ctx, data, len);
    }
    if (ret == 0) {
        ret = mbedtls_sha256_finish_ret(&ctx, n->h);
    }
    mbedtls_sha256_free(&ctx);
    return ret == 0 ? 0 : MUSE_EIO;
}

static int mix_key(muse_noise *n, const uint8_t *ikm, size_t len)
{
    uint8_t temp_k[32];
    int rc = hkdf2(n->ck, ikm, len, n->ck, temp_k);
    if (rc == 0) {
        rc = cipher_set_key(&n->handshake, temp_k);
    }
    mbedtls_platform_zeroize(temp_k, sizeof temp_k);
    return rc;
}

static int encrypt_and_hash(muse_noise *n, const uint8_t *in, size_t len, uint8_t *out)
{
    size_t out_len = len + cipher_overhead(&n->handshake);
    int rc = cipher_encrypt(&n->handshake, n->h, sizeof n->h, in, len, out);
    return rc ? rc : mix_hash(n, out, out_len);
}

static int decrypt_and_hash(muse_noise *n, const uint8_t *in, size_t len, uint8_t *out,
                            size_t *out_len)
{
    int rc = cipher_decrypt(&n->handshake, n->h, sizeof n->h, in, len, out, out_len);
    return rc ? rc : mix_hash(n, in, len);
}

static int mix_dh(muse_noise *n, const uint8_t priv[DH_LEN], const uint8_t pub[DH_LEN])
{
    uint8_t shared[DH_LEN];
    int rc = dh(n, priv, pub, shared);
    if (rc == 0) {
        rc = mix_key(n, shared, sizeof shared);
    }
    mbedtls_platform_zeroize(shared, sizeof shared);
    return rc;
}

/* --- handshake ------------------------------------------------------------- */

static void wipe_handshake(muse_noise *n)
{
    mbedtls_platform_zeroize(n->e_priv, sizeof n->e_priv);
    mbedtls_platform_zeroize(n->e_pub, sizeof n->e_pub);
    mbedtls_platform_zeroize(n->s_priv, sizeof n->s_priv);
    mbedtls_platform_zeroize(n->s_pub, sizeof n->s_pub);
    mbedtls_platform_zeroize(n->re, sizeof n->re);
    mbedtls_platform_zeroize(n->h, sizeof n->h);
    mbedtls_platform_zeroize(n->ck, sizeof n->ck);
    cipher_free(&n->handshake);
    cipher_init(&n->handshake);
}

/* Like the Python _expect: a step that fails leaves the handshake unusable. */
static bool enter_step(muse_noise *n, handshake_step step)
{
    if (n->step != step) {
        return false;
    }
    n->step = STEP_FAILED;
    return true;
}

int muse_noise_new(muse_tls *rng, muse_noise **out)
{
    muse_noise *n = calloc(1, sizeof *n);
    if (n == NULL) {
        return MUSE_ENOMEM;
    }
    n->rng = rng;
    n->step = STEP_WRITE_MSG1;
    n->next_stream_id = 1;
    cipher_init(&n->handshake);
    cipher_init(&n->send);
    cipher_init(&n->recv);

    memset(n->h, 0, sizeof n->h);
    memcpy(n->h, PROTOCOL_NAME, sizeof PROTOCOL_NAME - 1);
    memcpy(n->ck, n->h, sizeof n->ck);

    int rc = mix_hash(n, NULL, 0);
    if (rc == 0) {
        rc = generate_keypair(n, n->e_priv, n->e_pub);
    }
    if (rc == 0) {
        rc = generate_keypair(n, n->s_priv, n->s_pub);
    }
    if (rc != 0) {
        muse_noise_free(n);
        return rc;
    }
    *out = n;
    return 0;
}

static void assembly_drop(assembly *a)
{
    if (a->pieces != NULL) {
        for (uint32_t i = 0; i < a->total; i++) {
            free(a->pieces[i].data);
        }
        free(a->pieces);
    }
    memset(a, 0, sizeof *a);
}

void muse_noise_free(muse_noise *n)
{
    if (n == NULL) {
        return;
    }
    wipe_handshake(n);
    cipher_free(&n->handshake);
    cipher_free(&n->send);
    cipher_free(&n->recv);
    for (size_t i = 0; i < MUSE_MAX_PENDING_ASSEMBLIES; i++) {
        assembly_drop(&n->pending[i]);
    }
    mbedtls_platform_zeroize(n, sizeof *n);
    free(n);
}

int muse_noise_write_message1(muse_noise *n, uint8_t out[32])
{
    if (!enter_step(n, STEP_WRITE_MSG1)) {
        return MUSE_EPROTO;
    }
    int rc = mix_hash(n, n->e_pub, DH_LEN);
    if (rc == 0) {
        rc = encrypt_and_hash(n, NULL, 0, NULL);
    }
    if (rc != 0) {
        return rc;
    }
    memcpy(out, n->e_pub, DH_LEN);
    n->step = STEP_READ_MSG2;
    return 0;
}

int muse_noise_read_message2(muse_noise *n, const uint8_t *msg, size_t len)
{
    if (!enter_step(n, STEP_READ_MSG2)) {
        return MUSE_EPROTO;
    }
    if (len < MIN_MSG2_LEN || len > MAX_HANDSHAKE_MSG) {
        return MUSE_EPROTO;
    }
    const size_t static_end = DH_LEN + DH_LEN + TAG_LEN;
    uint8_t rs[DH_LEN];
    size_t rs_len = 0;
    size_t payload_len = 0;
    uint8_t *payload = malloc(len - static_end);
    if (payload == NULL) {
        return MUSE_ENOMEM;
    }

    memcpy(n->re, msg, DH_LEN);
    int rc = mix_hash(n, n->re, DH_LEN);
    if (rc == 0) {
        rc = mix_dh(n, n->e_priv, n->re);
    }
    if (rc == 0) {
        rc = decrypt_and_hash(n, msg + DH_LEN, DH_LEN + TAG_LEN, rs, &rs_len);
    }
    if (rc == 0) {
        rc = mix_dh(n, n->e_priv, rs);
    }
    if (rc == 0) {
        rc = decrypt_and_hash(n, msg + static_end, len - static_end, payload, &payload_len);
    }
    mbedtls_platform_zeroize(rs, sizeof rs);
    free(payload);
    if (rc != 0) {
        return rc;
    }
    n->step = STEP_WRITE_MSG3;
    return 0;
}

int muse_noise_write_message3(muse_noise *n, muse_buf *out)
{
    if (!enter_step(n, STEP_WRITE_MSG3)) {
        return MUSE_EPROTO;
    }
    uint8_t msg[(DH_LEN + TAG_LEN) + TAG_LEN];
    int rc = encrypt_and_hash(n, n->s_pub, DH_LEN, msg);
    if (rc == 0) {
        rc = mix_dh(n, n->s_priv, n->re);
    }
    if (rc == 0) {
        /* The bearer already authenticated us at the upgrade, so the
         * payload is empty and only its tag goes out. */
        rc = encrypt_and_hash(n, NULL, 0, msg + DH_LEN + TAG_LEN);
    }
    if (rc == 0) {
        rc = muse_buf_append(out, msg, sizeof msg);
    }
    if (rc != 0) {
        return rc;
    }
    n->step = STEP_SPLIT;
    return 0;
}

int muse_noise_split(muse_noise *n)
{
    if (!enter_step(n, STEP_SPLIT)) {
        return MUSE_EPROTO;
    }
    uint8_t k1[32], k2[32];
    int rc = hkdf2(n->ck, NULL, 0, k1, k2);
    if (rc == 0) {
        rc = cipher_set_key(&n->send, k1);
    }
    if (rc == 0) {
        rc = cipher_set_key(&n->recv, k2);
    }
    mbedtls_platform_zeroize(k1, sizeof k1);
    mbedtls_platform_zeroize(k2, sizeof k2);
    wipe_handshake(n);
    if (rc != 0) {
        return rc;
    }
    n->step = STEP_TRANSPORT;
    return 0;
}

/* --- transport: sending ---------------------------------------------------- */

static int random_int64(muse_noise *n, int64_t *out)
{
    uint8_t raw[8];
    if (muse_tls_random(n->rng, raw, sizeof raw) != 0) {
        return MUSE_EIO;
    }
    uint64_t value = 0;
    for (size_t i = 0; i < sizeof raw; i++) {
        value = (value << 8) | raw[i];
    }
    *out = (int64_t)value;
    return 0;
}

/* encode_noise_frames plus the seal: each chunk takes the next nonce. */
static int seal_chunks(muse_noise *n, const uint8_t *payload, size_t len, muse_noise_sink sink,
                       void *ctx)
{
    size_t total = (len + MUSE_MAX_CHUNK_PAYLOAD - 1u) / MUSE_MAX_CHUNK_PAYLOAD;
    if (total == 0) {
        total = 1;
    }
    if (total > MUSE_MAX_TOTAL_CHUNKS) {
        return MUSE_ETOOBIG;
    }
    int64_t chunk_id;
    int rc = random_int64(n, &chunk_id);
    if (rc != 0) {
        return rc;
    }

    muse_buf plain;
    muse_buf_init(&plain, MUSE_MAX_CHUNK_PAYLOAD + 64u);
    uint8_t *sealed = malloc(MUSE_MAX_CHUNK_PAYLOAD + 64u + TAG_LEN);
    if (sealed == NULL) {
        return MUSE_ENOMEM;
    }

    for (size_t index = 0; index < total && rc == 0; index++) {
        size_t start = index * MUSE_MAX_CHUNK_PAYLOAD;
        size_t chunk_len = len - start < MUSE_MAX_CHUNK_PAYLOAD ? len - start : MUSE_MAX_CHUNK_PAYLOAD;
        muse_buf_clear(&plain);
        if (chunk_id != 0) {
            rc = muse_pb_put_int64(&plain, 1, chunk_id);
        }
        if (rc == 0 && index != 0) {
            rc = muse_pb_put_uint(&plain, 2, index);
        }
        if (rc == 0) {
            rc = muse_pb_put_uint(&plain, 3, total);
        }
        if (rc == 0 && chunk_len > 0) {
            rc = muse_pb_put_bytes(&plain, 4, payload + start, chunk_len);
        }
        if (rc == 0) {
            rc = cipher_encrypt(&n->send, NULL, 0, plain.data, plain.len, sealed);
        }
        if (rc == 0) {
            rc = sink(ctx, sealed, plain.len + TAG_LEN);
        }
    }

    free(sealed);
    muse_buf_free(&plain);
    return rc;
}

/* _encode_service_request: field 2 wraps [stream_id][field: message].
 * ServiceRequest.service is SERVICE_DAEMON (0), which proto3 omits. */
static int send_service_frame(muse_noise *n, int64_t stream_id, uint32_t field, const muse_buf *message,
                              muse_noise_sink sink, void *ctx)
{
    if (n->step != STEP_TRANSPORT) {
        return MUSE_EPROTO;
    }
    muse_buf frame, request;
    muse_buf_init(&frame, MAX_REQUEST_BYTES);
    muse_buf_init(&request, MAX_REQUEST_BYTES);
    int rc = 0;
    if (stream_id != 0) {
        rc = muse_pb_put_int64(&frame, 1, stream_id);
    }
    if (rc == 0) {
        rc = muse_pb_put_bytes(&frame, field, message->data, message->len);
    }
    if (rc == 0) {
        rc = muse_pb_put_bytes(&request, 2, frame.data, frame.len);
    }
    muse_buf_free(&frame);
    if (rc == 0) {
        rc = seal_chunks(n, request.data, request.len, sink, ctx);
    }
    muse_buf_free(&request);
    return rc;
}

int muse_noise_send_request(muse_noise *n, const char *verb, const char *path,
                            const muse_header *headers, size_t nheaders, const void *body,
                            size_t body_len, bool end_body, int64_t *stream_id,
                            muse_noise_sink sink, void *ctx)
{
    muse_buf message, header;
    muse_buf_init(&message, MAX_REQUEST_BYTES);
    muse_buf_init(&header, MUSE_MAX_HTTP_HEAD);
    int rc = muse_pb_put_str(&message, 1, verb);
    if (rc == 0) {
        rc = muse_pb_put_str(&message, 2, path);
    }
    for (size_t i = 0; i < nheaders && rc == 0; i++) {
        muse_buf_clear(&header);
        rc = muse_pb_put_str(&header, 1, headers[i].name);
        if (rc == 0) {
            rc = muse_pb_put_str(&header, 2, headers[i].value);
        }
        if (rc == 0) {
            rc = muse_pb_put_bytes(&message, 3, header.data, header.len);
        }
    }
    if (rc == 0 && body_len > 0) {
        rc = muse_pb_put_bytes(&message, 4, body, body_len);
    }
    if (rc == 0 && end_body) {
        rc = muse_pb_put_bool(&message, 5, true);
    }
    muse_buf_free(&header);
    if (rc == 0) {
        int64_t id = n->next_stream_id++;
        *stream_id = id;
        rc = send_service_frame(n, id, 2, &message, sink, ctx);
    }
    muse_buf_free(&message);
    return rc;
}

int muse_noise_send_body_chunk(muse_noise *n, int64_t stream_id, const void *data, size_t len,
                               bool end_body, muse_noise_sink sink, void *ctx)
{
    muse_buf message;
    muse_buf_init(&message, MAX_REQUEST_BYTES);
    int rc = 0;
    if (len > 0) {
        rc = muse_pb_put_bytes(&message, 1, data, len);
    }
    if (rc == 0 && end_body) {
        rc = muse_pb_put_bool(&message, 2, true);
    }
    if (rc == 0) {
        rc = send_service_frame(n, stream_id, 4, &message, sink, ctx);
    }
    muse_buf_free(&message);
    return rc;
}

int muse_noise_send_reset(muse_noise *n, int64_t stream_id, const char *reason,
                          muse_noise_sink sink, void *ctx)
{
    muse_buf message;
    muse_buf_init(&message, 1024);
    int rc = muse_pb_put_int64(&message, 1, RESET_CANCELLED);
    if (rc == 0 && reason != NULL && reason[0] != '\0') {
        rc = muse_pb_put_str(&message, 2, reason);
    }
    if (rc == 0) {
        rc = send_service_frame(n, stream_id, 5, &message, sink, ctx);
    }
    muse_buf_free(&message);
    return rc;
}

/* --- transport: receiving -------------------------------------------------- */

typedef struct pb_field {
    uint32_t number;
    int wire;            /* MUSE_WIRE_VARINT or MUSE_WIRE_DELIMITED */
    bool present;
    uint64_t varint;
    const uint8_t *data;
    size_t len;
} pb_field;

/* _decode_fields: the last occurrence of a field wins, unknown fields are
 * skipped, a known field with the wrong wire type is an error. */
static int decode_fields(const uint8_t *data, size_t len, pb_field *fields, size_t nfields)
{
    muse_pb_reader r = {data, len, 0};
    while (r.off < r.len) {
        uint32_t number;
        int wire;
        if (muse_pb_read_key(&r, &number, &wire) != 0) {
            return MUSE_EPROTO;
        }
        pb_field *f = NULL;
        for (size_t i = 0; i < nfields; i++) {
            if (fields[i].number == number) {
                f = &fields[i];
                break;
            }
        }
        int rc;
        if (f == NULL) {
            rc = muse_pb_skip(&r, wire);
        } else if (f->wire != wire) {
            rc = MUSE_EPROTO;
        } else if (wire == MUSE_WIRE_VARINT) {
            rc = muse_pb_read_varint(&r, &f->varint);
        } else {
            rc = muse_pb_read_delimited(&r, &f->data, &f->len);
        }
        if (rc != 0) {
            return MUSE_EPROTO;
        }
        if (f != NULL) {
            f->present = true;
        }
    }
    return 0;
}

static void expire_assemblies(muse_noise *n, uint32_t now)
{
    for (size_t i = 0; i < MUSE_MAX_PENDING_ASSEMBLIES; i++) {
        assembly *a = &n->pending[i];
        if (a->used && (uint32_t)(now - a->started_ms) > MUSE_ASSEMBLY_TTL_MS) {
            assembly_drop(a);
        }
    }
}

static int assembly_open(muse_noise *n, int64_t chunk_id, uint32_t total, uint32_t now, assembly **out)
{
    assembly *slot = NULL;
    for (size_t i = 0; i < MUSE_MAX_PENDING_ASSEMBLIES; i++) {
        assembly *a = &n->pending[i];
        if (a->used && a->chunk_id == chunk_id) {
            *out = a;
            return 0;
        }
        if (!a->used && slot == NULL) {
            slot = a;
        }
    }
    if (slot == NULL) {
        return MUSE_EPROTO;
    }
    slot->pieces = calloc(total, sizeof *slot->pieces);
    if (slot->pieces == NULL) {
        return MUSE_ENOMEM;
    }
    slot->used = true;
    slot->chunk_id = chunk_id;
    slot->total = total;
    slot->started_ms = now;
    *out = slot;
    return 0;
}

/* NoiseFrameDecoder.decode on one decrypted transport frame. */
static int assemble(muse_noise *n, const uint8_t *plain, size_t len, muse_buf *out, bool *complete)
{
    pb_field f[4] = {
        {1, MUSE_WIRE_VARINT, false, 0, NULL, 0},
        {2, MUSE_WIRE_VARINT, false, 0, NULL, 0},
        {3, MUSE_WIRE_VARINT, false, 1, NULL, 0},
        {4, MUSE_WIRE_DELIMITED, false, 0, NULL, 0},
    };
    if (decode_fields(plain, len, f, 4) != 0) {
        return MUSE_EPROTO;
    }
    int64_t chunk_id = (int64_t)f[0].varint;
    uint64_t index = f[1].varint;
    uint64_t total = f[2].varint;
    const uint8_t *payload = f[3].data;
    size_t payload_len = f[3].len;

    if (total < 1 || total > MUSE_MAX_TOTAL_CHUNKS || index >= total ||
        payload_len > MUSE_MAX_CHUNK_PAYLOAD) {
        return MUSE_EPROTO;
    }
    if (total == 1) {
        muse_buf_clear(out);
        int rc = payload_len > 0 ? muse_buf_append(out, payload, payload_len) : 0;
        *complete = rc == 0;
        return rc;
    }

    uint32_t now = muse_port_now_ms();
    expire_assemblies(n, now);
    assembly *a;
    int rc = assembly_open(n, chunk_id, (uint32_t)total, now, &a);
    if (rc != 0) {
        return rc;
    }
    piece *p = &a->pieces[index < a->total ? index : 0];
    if (a->total != total || p->present || a->bytes + payload_len > MUSE_MAX_ASSEMBLY_BYTES) {
        assembly_drop(a);
        return MUSE_EPROTO;
    }
    if (payload_len > 0) {
        p->data = malloc(payload_len);
        if (p->data == NULL) {
            assembly_drop(a);
            return MUSE_ENOMEM;
        }
        memcpy(p->data, payload, payload_len);
    }
    p->len = payload_len;
    p->present = true;
    a->bytes += payload_len;
    a->have++;
    if (a->have < a->total) {
        return 0;
    }

    muse_buf_clear(out);
    for (uint32_t i = 0; i < a->total && rc == 0; i++) {
        if (a->pieces[i].len > 0) {
            rc = muse_buf_append(out, a->pieces[i].data, a->pieces[i].len);
        }
    }
    assembly_drop(a);
    *complete = rc == 0;
    return rc;
}

int muse_noise_receive(muse_noise *n, const uint8_t *ciphertext, size_t len, muse_buf *out,
                       bool *complete)
{
    *complete = false;
    if (n->step != STEP_TRANSPORT || len > MUSE_MAX_WS_MESSAGE) {
        return MUSE_EPROTO;
    }
    uint8_t *plain = malloc(len > 0 ? len : 1);
    if (plain == NULL) {
        return MUSE_ENOMEM;
    }
    size_t plain_len = 0;
    int rc = cipher_decrypt(&n->recv, NULL, 0, ciphertext, len, plain, &plain_len);
    if (rc == 0) {
        rc = assemble(n, plain, plain_len, out, complete);
    }
    free(plain);
    return rc;
}

static int decode_int32(uint64_t raw, int *out)
{
    int64_t value = (int64_t)raw;
    if (value < INT32_MIN || value > INT32_MAX) {
        return MUSE_EPROTO;
    }
    *out = (int)value;
    return 0;
}

static void copy_reason(char dst[128], const uint8_t *src, size_t len)
{
    size_t n = len < 127 ? len : 127;
    for (size_t i = 0; i < n; i++) {
        dst[i] = src[i] < 0x20 || src[i] == 0x7F ? '?' : (char)src[i];
    }
    dst[n] = '\0';
}

int muse_noise_decode_frame(const uint8_t *data, size_t len, muse_frame *frame)
{
    memset(frame, 0, sizeof *frame);

    pb_field outer[1] = {{1, MUSE_WIRE_DELIMITED, false, 0, NULL, 0}};
    if (decode_fields(data, len, outer, 1) != 0 || outer[0].len == 0) {
        return MUSE_EPROTO;
    }
    pb_field f[5] = {
        {1, MUSE_WIRE_VARINT, false, 0, NULL, 0},
        {2, MUSE_WIRE_DELIMITED, false, 0, NULL, 0},
        {3, MUSE_WIRE_DELIMITED, false, 0, NULL, 0},
        {4, MUSE_WIRE_DELIMITED, false, 0, NULL, 0},
        {5, MUSE_WIRE_DELIMITED, false, 0, NULL, 0},
    };
    if (decode_fields(outer[0].data, outer[0].len, f, 5) != 0) {
        return MUSE_EPROTO;
    }
    frame->stream_id = (int64_t)f[0].varint;
    if (f[1].present) {
        return MUSE_EPROTO;   /* a request frame never comes from the server */
    }
    if (f[2].present) {
        pb_field r[3] = {
            {1, MUSE_WIRE_VARINT, false, 0, NULL, 0},
            {4, MUSE_WIRE_VARINT, false, 0, NULL, 0},
            {3, MUSE_WIRE_DELIMITED, false, 0, NULL, 0},
        };
        if (decode_fields(f[2].data, f[2].len, r, 3) != 0 ||
            decode_int32(r[0].varint, &frame->status) != 0) {
            return MUSE_EPROTO;
        }
        frame->kind = MUSE_FRAME_RESPONSE;
        frame->end_body = r[1].varint != 0;
        frame->data = r[2].data;
        frame->data_len = r[2].len;
        return 0;
    }
    if (f[3].present) {
        pb_field c[2] = {
            {2, MUSE_WIRE_VARINT, false, 0, NULL, 0},
            {1, MUSE_WIRE_DELIMITED, false, 0, NULL, 0},
        };
        if (decode_fields(f[3].data, f[3].len, c, 2) != 0) {
            return MUSE_EPROTO;
        }
        frame->kind = MUSE_FRAME_BODY_CHUNK;
        frame->end_body = c[0].varint != 0;
        frame->data = c[1].data;
        frame->data_len = c[1].len;
        return 0;
    }
    if (f[4].present) {
        pb_field r[2] = {
            {1, MUSE_WIRE_VARINT, false, 0, NULL, 0},
            {2, MUSE_WIRE_DELIMITED, false, 0, NULL, 0},
        };
        if (decode_fields(f[4].data, f[4].len, r, 2) != 0) {
            return MUSE_EPROTO;
        }
        frame->kind = MUSE_FRAME_RESET;
        copy_reason(frame->reason, r[1].data, r[1].len);
        return 0;
    }
    return 1;
}
