#!/usr/bin/env python3
"""Copy Muse to the PSP's Memory Stick.

    tools/install.py [--volume /Volumes/NAME] [--no-eject]

Put the PSP in USB Connection mode first. This copies the built app and the
avatar to PSP/GAME/Muse on the stick and ejects it. Run it again after every
rebuild.

This copies the pairing only once. From then on the PSP renews its own tokens
and saves them on the stick, so the copy on the stick is the live one and the
copy in state/ here goes stale. Overwriting the stick's pairing with a stale
one would unpair the PSP, so this leaves an existing pairing alone.
"""
import argparse
import pathlib
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
EBOOT = ROOT / "target" / "mipsel-sony-psp" / "release" / "EBOOT.PBP"
AVATAR = ROOT / "assets" / "avatar" / "avatar.bin"
PAIRING = ["identity.json", "pairing.json"]


def find_stick(given):
    if given:
        return pathlib.Path(given)
    roots = [pathlib.Path("/Volumes"), pathlib.Path("/media"), pathlib.Path("/run/media")]
    found = [v for root in roots if root.is_dir() for v in root.glob("*") if (v / "PSP" / "GAME").is_dir()]
    found += [v for root in roots[1:] if root.is_dir() for v in root.glob("*/*") if (v / "PSP" / "GAME").is_dir()]
    if len(found) != 1:
        sys.exit("expected one Memory Stick with a PSP/GAME folder, found %d. "
                 "Put the PSP in USB Connection mode, or pass --volume." % len(found))
    return found[0]


def secret(path_option, default_name):
    path = pathlib.Path(path_option) if path_option else ROOT / default_name
    return path.read_text().strip() if path.exists() else None


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--volume", help="the Memory Stick (default: the one mounted)")
    parser.add_argument("--sdk-token-file", help="your Muse SDK token (default: .sdk_token)")
    parser.add_argument("--openai-key-file", help="an OpenAI key for the voice (default: .openai_key)")
    parser.add_argument("--no-eject", action="store_true", help="leave the stick mounted")
    args = parser.parse_args()

    if not EBOOT.exists():
        sys.exit("%s is missing. Build the app first, see the README." % EBOOT.relative_to(ROOT))
    if not AVATAR.exists():
        sys.exit("%s is missing. Run tools/avatar.sh first, see the README." % AVATAR.relative_to(ROOT))

    stick = find_stick(args.volume)
    target = stick / "PSP" / "GAME" / "Muse"
    state = target / "state"
    state.mkdir(parents=True, exist_ok=True)

    shutil.copyfile(EBOOT, target / "EBOOT.PBP")
    shutil.copyfile(AVATAR, target / "avatar.bin")

    if (state / "pairing.json").exists():
        print("kept the pairing already on the stick")
    elif all((ROOT / "state" / name).exists() for name in PAIRING):
        for name in PAIRING:
            shutil.copyfile(ROOT / "state" / name, state / name)
        print("copied the pairing to the stick. The stick's copy is the live one from now on.")
    else:
        print("no pairing yet. Muse will show as offline. Run tools/pair.py, then this again.")

    token = secret(args.sdk_token_file, ".sdk_token")
    if token:
        (state / "sdk_token.txt").write_text(token)
    key = secret(args.openai_key_file, ".openai_key")
    if key:
        (state / "openai_key.txt").write_text(key)
    print("voice: %s" % ("on" if (state / "openai_key.txt").exists() else "off, replies will not be spoken"))

    # macOS leaves "._" companions on FAT sticks, and the PSP lists them as corrupted data.
    for junk in target.rglob("._*"):
        junk.unlink()
    print("installed Muse to %s" % target)

    if args.no_eject:
        return
    subprocess.run(["sync"], check=False)
    if sys.platform == "darwin":
        done = subprocess.run(["diskutil", "eject", str(stick)], capture_output=True, text=True)
        print("ejected. Leave USB mode on the PSP." if done.returncode == 0
              else "could not eject %s. Eject it yourself before leaving USB mode.\n%s" % (stick, done.stderr.strip()))
    else:
        print("Eject %s before leaving USB mode on the PSP." % stick)


if __name__ == "__main__":
    main()
