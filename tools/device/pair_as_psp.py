# Runs on the badge, started by tools/pair.py.
#
# It runs the badge's own Muse app against a separate state folder, so the
# Muse app sees a second, new gadget named PSP. tools/pair.py then saves that
# pairing for the PSP and removes it from the badge. The badge's own pairing
# in /state/muse is not touched.
import os

import musebadge.main as app

SOURCE, TARGET = "/state/muse", "/state/muse-psp"
try:
    os.mkdir(TARGET)
except OSError:
    pass
# The SDK token and the Wi-Fi the badge already knows carry over.
for name in ("sdk_token.json", "wifi.json"):
    try:
        with open(SOURCE + "/" + name) as src:
            data = src.read()
        with open(TARGET + "/" + name, "w") as dst:
            dst.write(data)
    except OSError:
        pass

app.STATE_DIR = TARGET
app.DISPLAY_NAME = "PSP"
app.main()
