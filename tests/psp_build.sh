#!/bin/bash
# Cross-builds the Muse library for the PSP and proves a program linking it
# has no unresolved symbols. The last line printed is PASS or FAIL.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PSPDEV="${PSPDEV:-$ROOT/toolchain/pspdev}"
export PATH="$PSPDEV/bin:$PATH"

BUILD="$ROOT/build-psp"
LIB="$BUILD/libmusegadget.a"
CHECK="$BUILD/linkcheck"
LINK_LIBS="-lmbedtls -lmbedx509 -lmbedcrypto -lcjson -lpspnet_inet -lpspnet_resolver -lpspnet_apctl -lpspnet -lpsprtc -lpspopenpsid"
SDL_LIBS="$("$PSPDEV/psp/bin/sdl2-config" --libs)"
# What psp-gcc links on its own, from `psp-gcc -dumpspecs`.
DEFAULT_LIBS="-lm -lpthreadglue -lpthread -lcglue -lc -lpsputility -lpspsdk -lpspmodinfo -lpspuser -lgcc"
LIB_DIRS="$PSPDEV/psp/lib $PSPDEV/psp/sdk/lib $(dirname "$(psp-gcc -print-libgcc-file-name)")"

fail() {
    echo "$1"
    echo FAIL
    exit 1
}

psp-cmake -S "$ROOT/muse" -B "$BUILD" || fail "psp-cmake configure failed"
cmake --build "$BUILD" || fail "library build failed"
[ -f "$LIB" ] || fail "missing $LIB"

mkdir -p "$CHECK"
provided="$CHECK/provided.txt"
: > "$provided"
psp-nm --defined-only "$LIB" | awk 'NF == 3 { print $3 }' >> "$provided"
for flag in $LINK_LIBS $SDL_LIBS $DEFAULT_LIBS; do
    case "$flag" in
    -l*)
        for dir in $LIB_DIRS; do
            archive="$dir/lib${flag#-l}.a"
            [ -f "$archive" ] && psp-nm --defined-only "$archive" 2>/dev/null | awk 'NF == 3 { print $3 }' >> "$provided"
        done
        ;;
    *.a)
        psp-nm --defined-only "$flag" 2>/dev/null | awk 'NF == 3 { print $3 }' >> "$provided"
        ;;
    esac
done
sort -u "$provided" -o "$provided"
missing="$(psp-nm -u "$LIB" | awk 'NF == 2 && $1 == "U" { print $2 }' | sort -u | comm -23 - "$provided")"
if [ -n "$missing" ]; then
    echo "undefined in libmusegadget.a and provided by no PSP library on the link line:"
    echo "$missing"
    fail "unresolved symbols"
fi
echo "every undefined symbol in libmusegadget.a is provided by a linked PSP library"

cat > "$CHECK/main.c" <<'SOURCE'
#include <SDL2/SDL.h>
#include <string.h>

#include "muse.h"

int main(int argc, char **argv)
{
    (void)argc;
    (void)argv;
    muse_config config;
    memset(&config, 0, sizeof(config));
    muse *m = muse_new(&config);
    if (!m) {
        return 1;
    }
    muse_run_end end = muse_run(m);
    muse_free(m);
    return (int)end;
}
SOURCE

# shellcheck disable=SC2086
psp-gcc -std=c99 -Wall -Wextra -Werror -DPSP \
    -I"$ROOT/muse" -I"$PSPDEV/psp/include" -I"$PSPDEV/psp/sdk/include" \
    "$CHECK/main.c" "$LIB" \
    -L"$PSPDEV/lib" -L"$PSPDEV/psp/lib" -L"$PSPDEV/psp/sdk/lib" -Wl,-zmax-page-size=128 \
    $LINK_LIBS $SDL_LIBS \
    -o "$CHECK/linkcheck.elf" || fail "dummy program did not link"
echo "linked $CHECK/linkcheck.elf"
echo PASS
