#!/bin/sh
# Builds assets/avatar/avatar.bin from the Muse Gadget SDK's own avatar.
#
#   git clone https://github.com/facebookincubator/muse-gadget-sdk vendor/muse-gadget-sdk
#   tools/avatar.sh
#
# The avatar is Meta's and is not under the SDK's Apache licence. Git ignores
# both the clone and the output.
set -eu
cd "$(dirname "$0")/.."
sdk=${MUSE_GADGET_SDK:-vendor/muse-gadget-sdk}/esp32
[ -f "$sdk/avatar/muse_pixel.c" ] || { echo "Muse Gadget SDK not found at $sdk" >&2; exit 1; }
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cc -O2 -w -I "$sdk/avatar" -I "$sdk/components/muse" -o "$work/render_avatar" tools/render_avatar.c -lm
mkdir -p assets/avatar
"$work/render_avatar" assets/avatar/avatar.bin
echo "wrote assets/avatar/avatar.bin ($(wc -c < assets/avatar/avatar.bin | tr -d ' ') bytes)"
