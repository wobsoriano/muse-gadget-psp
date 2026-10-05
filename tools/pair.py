#!/usr/bin/env python3
"""Give the PSP a Muse pairing. A one-time setup step.

    tools/pair.py FOLDER

Muse pairs a gadget over Bluetooth LE, and the PSP has no Bluetooth. So a
device that has it does the Bluetooth conversation once. What comes out of it
is small, an identity and a pair of tokens. This takes those from FOLDER,
checks them, and saves them in state/ for tools/install.py to copy to the
Memory Stick. From then on the PSP connects to Muse by itself and renews its
own tokens.

FOLDER must hold the two files a Muse gadget client keeps after pairing:

    identity.json   {"mac": "..."}
    pairing.json    access_token, refresh_token, api_url_v2, noise_host,
                    access_token_saved_at

The Linux client in the Muse Gadget SDK writes exactly these, in
/var/lib/musegadget or wherever MUSEGADGET_STATE_DIR points. See the README.

Only one device can use an identity at a time, and every renewal replaces the
tokens. So once the PSP has the pairing, stop and remove the client it came
from. Treat these files like a password.
"""
import argparse
import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
STATE = ROOT / "state"
PAIRING_FIELDS = ("access_token", "refresh_token", "api_url_v2")


def load(folder, name):
    path = folder / name
    try:
        data = json.loads(path.read_text())
    except OSError:
        sys.exit("cannot read %s" % path)
    except ValueError:
        sys.exit("%s is not valid JSON" % path)
    if not isinstance(data, dict):
        sys.exit("%s does not hold a JSON object" % path)
    return data


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("folder", help="where identity.json and pairing.json are")
    parser.add_argument("--force", action="store_true", help="replace a pairing already in state/")
    args = parser.parse_args()

    folder = pathlib.Path(args.folder).expanduser()
    identity = load(folder, "identity.json")
    pairing = load(folder, "pairing.json")

    mac = identity.get("mac")
    if not isinstance(mac, str) or not re.fullmatch(r"(?:[0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}", mac):
        sys.exit("identity.json has no usable \"mac\"")
    missing = [name for name in PAIRING_FIELDS if not isinstance(pairing.get(name), str) or not pairing[name]]
    if missing:
        sys.exit("pairing.json is missing: %s. Has that device finished pairing?" % ", ".join(missing))

    if (STATE / "pairing.json").exists() and not args.force:
        sys.exit("state/ already holds a pairing. Pass --force to replace it.")
    STATE.mkdir(exist_ok=True)
    for name, data in (("identity.json", {"mac": mac}), ("pairing.json", pairing)):
        path = STATE / name
        path.write_text(json.dumps(data))
        path.chmod(0o600)

    print("Saved the pairing for homelink-%s in %s." % (mac.replace(":", "")[-6:].lower(), STATE))
    print("Now stop the client it came from, so the PSP is the only one using it.")
    print("Next: tools/install.py")


if __name__ == "__main__":
    main()
