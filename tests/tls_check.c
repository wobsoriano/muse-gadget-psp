#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "muse.h"

int main(int argc, char **argv)
{
    if (argc < 2 || argc > 3 || (argc == 3 && strcmp(argv[2], "--ignore-dates") != 0)) {
        fprintf(stderr, "usage: tls_check <url> [--ignore-dates]\n");
        return 1;
    }
    int status = 0;
    char *body = NULL;
    int err = muse_https_get(argv[1], "musegadget-psp-tls-check", argc == 3, &status, &body);
    printf("status=%d err=%d\n", status, err);
    if (body != NULL) {
        printf("%.200s\n", body);
    }
    free(body);
    if (err == MUSE_OK) {
        return 0;
    }
    return err == MUSE_ETLS ? 2 : 1;
}
