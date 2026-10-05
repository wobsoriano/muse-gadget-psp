#include "muse_internal.h"

#include <stdarg.h>
#include <stdio.h>

void muse_logf(const muse_log *log, const char *fmt, ...)
{
    if (log == NULL || log->fn == NULL) {
        return;
    }
    char line[512];
    va_list args;
    va_start(args, fmt);
    vsnprintf(line, sizeof(line), fmt, args);
    va_end(args);
    log->fn(log->user, line);
}
