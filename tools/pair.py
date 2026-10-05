#!/usr/bin/env python3
"""Pair the PSP with Muse. A one-time setup step.

    tools/pair.py            start pairing and wait
    tools/pair.py collect    save the finished pairing into state/

Muse pairs a gadget over Bluetooth LE, and the PSP has no Bluetooth. So the
Bluetooth conversation is done once by a device that has it, and the result,
an identity and a pair of tokens, is saved for the PSP to use from then on.
After that the PSP connects to Muse by itself and renews its own tokens.

This tool uses a Pimoroni Tufty 2350 badge running muse-gadget-tufty
(https://github.com/wobsoriano/muse-gadget-tufty) as that device. Point it at
your checkout of that repository with --badge-repo or MUSE_TUFTY_REPO. It
looks beside this repository by default.

The pairing lands in state/, which git ignores. tools/install.py copies it to
the Memory Stick. Treat those files like a password.
"""
import argparse
import json
import os
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
STATE = ROOT / "state"
REMOTE = "/state/muse-psp"

READ = (
    "import json\n"
    "out = {}\n"
    "for name in ('identity.json', 'pairing.json'):\n"
    "    try:\n"
    "        out[name] = json.loads(open('%s/' + name).read())\n"
    "    except (OSError, ValueError):\n"
    "        out[name] = None\n"
    "print('PAIRSTATE ' + json.dumps(out))\n" % REMOTE
)
REMOVE = (
    "import os\n"
    "for name in os.listdir('%s'):\n"
    "    os.remove('%s/' + name)\n"
    "os.rmdir('%s')\n"
    "print('removed')\n" % (REMOTE, REMOTE, REMOTE)
)


def badge_tool(given):
    candidates = [given, os.environ.get("MUSE_TUFTY_REPO"),
                  ROOT.parent / "muse-gadget-tufty", ROOT.parent / "muse-tufty"]
    for repo in candidates:
        if repo and (pathlib.Path(repo) / "tools" / "badge.py").exists():
            return pathlib.Path(repo) / "tools" / "badge.py"
    sys.exit("cannot find muse-gadget-tufty. Clone it and pass --badge-repo PATH.")


def collect(tool):
    result = subprocess.run([sys.executable, str(tool), "exec", READ], capture_output=True, text=True)
    found = [line for line in result.stdout.splitlines() if line.startswith("PAIRSTATE ")]
    if not found:
        sys.exit("could not read the badge. Is it plugged in?")
    state = json.loads(found[0][len("PAIRSTATE "):])
    if not state["identity.json"] or not state["pairing.json"]:
        sys.exit("the badge has not finished pairing yet")
    STATE.mkdir(exist_ok=True)
    for name, data in state.items():
        path = STATE / name
        path.write_text(json.dumps(data))
        path.chmod(0o600)
    # One identity holds one session, so the badge must not keep a copy.
    subprocess.run([sys.executable, str(tool), "exec", REMOVE], capture_output=True, text=True)
    mac = state["identity.json"]["mac"].replace(":", "")
    print("paired as homelink-%s. Saved to %s and removed from the badge." % (mac[-6:], STATE))
    print("Next: tools/install.py")


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("action", nargs="?", choices=["collect"])
    parser.add_argument("--badge-repo", help="path to a muse-gadget-tufty checkout")
    args = parser.parse_args()
    tool = badge_tool(args.badge_repo)
    if args.action == "collect":
        collect(tool)
        return
    print("Starting pairing on the badge. In the Muse app, add a device and pick the one named PSP.")
    print("When the app says it is done, press Ctrl-C here, then run: tools/pair.py collect")
    subprocess.run([sys.executable, str(tool), "run", str(ROOT / "tools" / "device" / "pair_as_psp.py")])


if __name__ == "__main__":
    main()
