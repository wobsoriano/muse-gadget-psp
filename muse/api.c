/* The Muse device API: leased VM lookup and token rotation. Ported from the
 * Muse Gadget SDK's muse_api.py by way of musebadge/api.py. */
#include "muse_internal.h"

#include <stdlib.h>
#include <string.h>

#include <cjson/cJSON.h>
#include <mbedtls/platform_util.h>

#define FETCH_PATH "/fetch_vms"
#define REFRESH_PATH "/device_token/refresh"

static char *join(const char *a, const char *b)
{
    muse_buf out;
    muse_buf_init(&out, 4096);
    if (muse_buf_append_str(&out, a) != 0 || muse_buf_append_str(&out, b) != 0) {
        muse_buf_free(&out);
        return NULL;
    }
    return muse_buf_take_str(&out);
}

static const char *nonempty_string(const cJSON *object, const char *key)
{
    const cJSON *item = cJSON_GetObjectItemCaseSensitive(object, key);
    return cJSON_IsString(item) && item->valuestring[0] != '\0' ? item->valuestring : NULL;
}

static bool truthy(const cJSON *item)
{
    if (item == NULL || cJSON_IsNull(item) || cJSON_IsFalse(item)) {
        return false;
    }
    if (cJSON_IsString(item)) {
        return item->valuestring[0] != '\0';
    }
    if (cJSON_IsNumber(item)) {
        return item->valuedouble != 0;
    }
    if (cJSON_IsArray(item) || cJSON_IsObject(item)) {
        return item->child != NULL;
    }
    return true;
}

void muse_vm_free(muse_vm *vm)
{
    if (vm->vm_auth_token != NULL) {
        mbedtls_platform_zeroize(vm->vm_auth_token, strlen(vm->vm_auth_token));
    }
    free(vm->vm_id);
    free(vm->vm_name);
    free(vm->vm_auth_token);
    memset(vm, 0, sizeof *vm);
}

static bool usable_vm(const cJSON *entry)
{
    return cJSON_IsObject(entry) &&
           (nonempty_string(entry, "vm_ws_url") != NULL || nonempty_string(entry, "vm_url") != NULL) &&
           nonempty_string(entry, "vm_auth_token") != NULL;
}

static int copy_vm(const cJSON *entry, muse_vm *vm)
{
    const char *name = nonempty_string(entry, "vm_name");
    const char *id = nonempty_string(entry, "vm_id");
    vm->vm_name = muse_strdup(name != NULL ? name : "");
    vm->vm_id = muse_strdup(id != NULL ? id : "");
    vm->vm_auth_token = muse_strdup(nonempty_string(entry, "vm_auth_token"));
    vm->is_default = truthy(cJSON_GetObjectItemCaseSensitive(entry, "default"));
    if (vm->vm_name == NULL || vm->vm_id == NULL || vm->vm_auth_token == NULL) {
        muse_vm_free(vm);
        return MUSE_ENOMEM;
    }
    return 0;
}

/* The default VM, else the first usable one. Returns 1 when there is none. */
static int pick_vm(const cJSON *data, const muse_log *log, muse_vm *vm)
{
    if (!cJSON_IsObject(data)) {
        muse_logf(log, "VM fetch: unexpected response");
        return 1;
    }
    const cJSON *title = cJSON_GetObjectItemCaseSensitive(data, "error_title");
    const cJSON *code = cJSON_GetObjectItemCaseSensitive(data, "backend_error_code");
    if (truthy(title) || truthy(code)) {
        muse_logf(log, "VM fetch error: %s", cJSON_IsString(title) ? title->valuestring : "unknown");
        return 1;
    }
    const cJSON *list = cJSON_GetObjectItemCaseSensitive(data, "vm_list");
    if (!cJSON_IsArray(list)) {
        muse_logf(log, "VM fetch: missing vm_list");
        return 1;
    }
    const cJSON *chosen = NULL;
    const cJSON *entry;
    int count = 0;
    cJSON_ArrayForEach(entry, list) {
        if (!usable_vm(entry)) {
            continue;
        }
        count++;
        bool is_default = truthy(cJSON_GetObjectItemCaseSensitive(entry, "default"));
        if (chosen == NULL || (is_default && !truthy(cJSON_GetObjectItemCaseSensitive(chosen, "default")))) {
            chosen = entry;
        }
    }
    muse_logf(log, "VM fetch: %d VMs", count);
    return chosen == NULL ? 1 : copy_vm(chosen, vm);
}

int muse_api_fetch_default_vm(muse_tls *tls, bool allow_plaintext, const char *api_root,
                              const char *access_token, const char *user_agent, muse_log log,
                              int *status, muse_vm *vm)
{
    *status = 0;
    memset(vm, 0, sizeof *vm);

    char *url = join(api_root, FETCH_PATH);
    char *bearer = join("Bearer ", access_token);
    muse_buf body;
    muse_buf_init(&body, MUSE_MAX_HTTP_BODY);
    int rc = MUSE_ENOMEM;
    if (url != NULL && bearer != NULL) {
        const muse_header headers[] = {
            {"Authorization", bearer},
            {"X-API-Version", "1.0.0"},
            {"User-Agent", user_agent},
        };
        rc = muse_http_request(tls, allow_plaintext, "GET", url, headers,
                               sizeof headers / sizeof headers[0], NULL, 0, MUSE_HTTP_TIMEOUT_MS,
                               status, &body);
    }
    if (rc != 0) {
        muse_logf(&log, "VM fetch failed: error %d", rc);
        *status = 0;
        rc = MUSE_EIO;
    } else if (*status != 200) {
        muse_logf(&log, "VM fetch failed: HTTP %d", *status);
        rc = *status == 401 ? MUSE_EAUTH : MUSE_EHTTP;
    } else {
        cJSON *data = cJSON_ParseWithLength((const char *)body.data, body.len);
        rc = pick_vm(data, &log, vm);
        cJSON_Delete(data);
    }

    if (bearer != NULL) {
        mbedtls_platform_zeroize(bearer, strlen(bearer));
    }
    free(bearer);
    free(url);
    muse_buf_free(&body);
    return rc;
}

static char *refresh_body(const char *device_id, const char *sdk_token)
{
    cJSON *body = cJSON_CreateObject();
    char *text = NULL;
    if (body != NULL && cJSON_AddStringToObject(body, "device_id", device_id) != NULL &&
        (sdk_token == NULL || sdk_token[0] == '\0' ||
         cJSON_AddStringToObject(body, "sdk_token", sdk_token) != NULL)) {
        text = cJSON_PrintUnformatted(body);
    }
    cJSON_Delete(body);
    return text;
}

int muse_api_refresh_token(muse_tls *tls, bool allow_plaintext, const char *api_root,
                           const char *refresh_token, const char *device_id,
                           const char *user_agent, const char *sdk_token, muse_log log,
                           int *status, char **access, char **refresh)
{
    *status = 0;
    *access = NULL;
    *refresh = NULL;

    /* Apps now hand over refresh tokens that already carry the hatch_refresh:
     * prefix; doubling it makes the server reject it. */
    const char *colon = strrchr(refresh_token, ':');
    const char *raw_refresh = colon != NULL ? colon + 1 : refresh_token;

    char *url = join(api_root, REFRESH_PATH);
    char *bearer = join("Bearer hatch_refresh:", raw_refresh);
    char *request = refresh_body(device_id, sdk_token);
    muse_buf body;
    muse_buf_init(&body, MUSE_MAX_HTTP_BODY);
    int rc = MUSE_ENOMEM;
    if (url != NULL && bearer != NULL && request != NULL) {
        const muse_header headers[] = {
            {"Authorization", bearer},
            {"Content-Type", "application/json"},
            {"User-Agent", user_agent},
        };
        rc = muse_http_request(tls, allow_plaintext, "POST", url, headers,
                               sizeof headers / sizeof headers[0], request, strlen(request),
                               MUSE_HTTP_TIMEOUT_MS, status, &body);
    }
    if (rc != 0) {
        muse_logf(&log, "token refresh failed: error %d", rc);
        *status = 0;
        rc = MUSE_EIO;
    } else if (*status != 200) {
        muse_logf(&log, "token refresh failed: HTTP %d", *status);
        rc = *status == 401 ? MUSE_EAUTH : MUSE_EHTTP;
    } else {
        cJSON *data = cJSON_ParseWithLength((const char *)body.data, body.len);
        const cJSON *tokens = data;
        const cJSON *payload = cJSON_GetObjectItemCaseSensitive(data, "payload");
        if (cJSON_IsObject(payload)) {
            tokens = payload;
        }
        const char *new_access = nonempty_string(tokens, "access_token");
        const char *new_refresh = nonempty_string(tokens, "refresh_token");
        if (new_access != NULL && new_refresh != NULL) {
            *access = muse_strdup(new_access);
            *refresh = muse_strdup(new_refresh);
            rc = *access != NULL && *refresh != NULL ? 0 : MUSE_ENOMEM;
            if (rc != 0) {
                free(*access);
                free(*refresh);
                *access = NULL;
                *refresh = NULL;
            }
        } else {
            muse_logf(&log, "token refresh response missing tokens");
            rc = MUSE_EPROTO;
        }
        cJSON_Delete(data);
    }

    if (bearer != NULL) {
        mbedtls_platform_zeroize(bearer, strlen(bearer));
    }
    free(bearer);
    free(url);
    cJSON_free(request);
    if (body.data != NULL) {
        mbedtls_platform_zeroize(body.data, body.cap);
    }
    muse_buf_free(&body);
    return rc;
}
