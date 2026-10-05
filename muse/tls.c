#include "muse_internal.h"

#include <string.h>

#include <mbedtls/error.h>

static int port_entropy_poll(void *data, unsigned char *output, size_t len, size_t *olen)
{
    (void)data;
    if (muse_port_random(output, len) != 0) {
        return MBEDTLS_ERR_ENTROPY_SOURCE_FAILED;
    }
    *olen = len;
    return 0;
}

/* Days since 1970-01-01 for a civil date, so two timestamps compare as numbers. */
static int64_t days_from_civil(int y, int m, int d)
{
    y -= m <= 2;
    int64_t era = (y >= 0 ? y : y - 399) / 400;
    int64_t yoe = y - era * 400;
    int64_t doy = (153 * (m + (m > 2 ? -3 : 9)) + 2) / 5 + d - 1;
    int64_t doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    return era * 146097 + doe - 719468;
}

static int64_t x509_unix_time(const mbedtls_x509_time *t)
{
    return days_from_civil(t->year, t->mon, t->day) * 86400
           + (int64_t)t->hour * 3600 + (int64_t)t->min * 60 + t->sec;
}

/* Certificate dates are checked here against the port's clock, not by
 * mbedTLS against the C library's time(). On a real PSP time() returned a
 * moment in 1969 while the console's clock was right, so every certificate
 * looked not yet valid. */
static int check_dates(void *data, mbedtls_x509_crt *crt, int depth, uint32_t *flags)
{
    muse_tls *tls = data;
    (void)depth;
    *flags &= ~(uint32_t)(MBEDTLS_X509_BADCERT_EXPIRED | MBEDTLS_X509_BADCERT_FUTURE);
    if (tls->ignore_cert_dates) {
        return 0;
    }
    int64_t now = muse_port_unix_time();
    if (now < x509_unix_time(&crt->valid_from)) {
        *flags |= MBEDTLS_X509_BADCERT_FUTURE;
    } else if (now > x509_unix_time(&crt->valid_to)) {
        *flags |= MBEDTLS_X509_BADCERT_EXPIRED;
    }
    return 0;
}

static int fail(muse_tls *tls, const char *what, int ret)
{
    char text[128];
    mbedtls_strerror(ret, text, sizeof(text));
    muse_logf(&tls->log, "tls: %s failed: -0x%04x %s", what, (unsigned)-ret, text);
    return MUSE_ETLS;
}

int muse_tls_init(muse_tls *tls, bool ignore_cert_dates, muse_log log)
{
    static const char personalization[] = "musegadget-tls";

    tls->ignore_cert_dates = ignore_cert_dates;
    tls->log = log;
    mbedtls_entropy_init(&tls->entropy);
    mbedtls_ctr_drbg_init(&tls->drbg);
    mbedtls_x509_crt_init(&tls->root);
    mbedtls_ssl_config_init(&tls->conf);

    /* mbedTLS reports a failed source only as a generic entropy error, so
     * probe the port first to name the real cause. */
    unsigned char probe[32];
    if (muse_port_random(probe, sizeof(probe)) != 0) {
        muse_logf(&tls->log, "tls: no random source, refusing to start");
        return MUSE_ETLS;
    }

    int ret = mbedtls_entropy_add_source(&tls->entropy, port_entropy_poll, NULL, 32,
                                         MBEDTLS_ENTROPY_SOURCE_STRONG);
    if (ret != 0) {
        return fail(tls, "entropy source", ret);
    }
    ret = mbedtls_ctr_drbg_seed(&tls->drbg, mbedtls_entropy_func, &tls->entropy,
                                (const unsigned char *)personalization,
                                sizeof(personalization) - 1);
    if (ret != 0) {
        return fail(tls, "rng seed", ret);
    }
    ret = mbedtls_x509_crt_parse_der(&tls->root, muse_digicert_global_root_g2_der,
                                     muse_digicert_global_root_g2_der_len);
    if (ret != 0) {
        return fail(tls, "root certificate parse", ret);
    }
    ret = mbedtls_ssl_config_defaults(&tls->conf, MBEDTLS_SSL_IS_CLIENT,
                                      MBEDTLS_SSL_TRANSPORT_STREAM, MBEDTLS_SSL_PRESET_DEFAULT);
    if (ret != 0) {
        return fail(tls, "config", ret);
    }
    mbedtls_ssl_conf_authmode(&tls->conf, MBEDTLS_SSL_VERIFY_REQUIRED);
    mbedtls_ssl_conf_ca_chain(&tls->conf, &tls->root, NULL);
    mbedtls_ssl_conf_min_version(&tls->conf, MBEDTLS_SSL_MAJOR_VERSION_3,
                                 MBEDTLS_SSL_MINOR_VERSION_3);
    mbedtls_ssl_conf_rng(&tls->conf, mbedtls_ctr_drbg_random, &tls->drbg);
    mbedtls_ssl_conf_verify(&tls->conf, check_dates, tls);
    return MUSE_OK;
}

void muse_tls_free(muse_tls *tls)
{
    mbedtls_ssl_config_free(&tls->conf);
    mbedtls_x509_crt_free(&tls->root);
    mbedtls_ctr_drbg_free(&tls->drbg);
    mbedtls_entropy_free(&tls->entropy);
}

int muse_tls_random(muse_tls *tls, void *buf, size_t len)
{
    unsigned char *p = buf;
    while (len > 0) {
        size_t n = len < MBEDTLS_CTR_DRBG_MAX_REQUEST ? len : MBEDTLS_CTR_DRBG_MAX_REQUEST;
        if (mbedtls_ctr_drbg_random(&tls->drbg, p, n) != 0) {
            return MUSE_ETLS;
        }
        p += n;
        len -= n;
    }
    return MUSE_OK;
}
