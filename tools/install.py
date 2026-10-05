#!/usr/bin/env python3
"""Copy Muse to the PSP's Memory Stick.

    tools/install.py [--volume /Volumes/NAME] [--no-eject]

Put the PSP in USB Connection mode first. This copies the built app and its
files to PSP/GAME/Muse on the stick and ejects it. Run it again after every
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
APP_FILES = ["commands.json", "voice.json"]
APP_FOLDERS = {"assets/clip": "clip", "assets/fonts": "fonts", "assets/tts": "tts"}
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
    parser.add_argument("--openai-key-file", help="an OpenAI key for the cloud voices (default: .openai_key)")
    parser.add_argument("--no-eject", action="store_true", help="leave the stick mounted")
    args = parser.parse_args()

    eboot = ROOT / "build" / "EBOOT.PBP"
    if not eboot.exists():
        sys.exit("build/EBOOT.PBP is missing. Build the app first, see the README.")
    if not any((ROOT / "assets" / "clip").glob("*.png")):
        sys.exit("assets/clip has no avatar frames. Run tools/pack_video.py first, see the README.")

    stick = find_stick(args.volume)
    target = stick / "PSP" / "GAME" / "Muse"
    state = target / "state"
    state.mkdir(parents=True, exist_ok=True)

    shutil.copyfile(eboot, target / "EBOOT.PBP")
    for name in APP_FILES:
        shutil.copyfile(ROOT / name, target / name)
    for source, name in APP_FOLDERS.items():
        if (target / name).exists():
            shutil.rmtree(target / name)
        shutil.copytree(ROOT / source, target / name, ignore=shutil.ignore_patterns(".DS_Store", "._*"))

    if (state / "pairing.json").exists():
        print("kept the pairing already on the stick")
    elif all((ROOT / "state" / name).exists() for name in PAIRING):
        for name in PAIRING:
            shutil.copyfile(ROOT / "state" / name, state / name)
        print("copied the pairing to the stick. The stick's copy is the live one from now on.")
    else:
        print("no pairing yet. Muse will say it is not paired. Run tools/pair.py, then this again.")

    token = secret(args.sdk_token_file, ".sdk_token")
    if token:
        (state / "sdk_token.txt").write_text(token)
    key = secret(args.openai_key_file, ".openai_key")
    if key:
        (state / "openai_key.txt").write_text(key)
    print("cloud voices: %s" % ("on" if (state / "openai_key.txt").exists() else "off, the built-in voice will be used"))

    # macOS leaves "._" companions on FAT sticks, and the PSP lists them as corrupted data.
    for junk in target.rglob("._*"):
        junk.unlink()
    size = sum(f.stat().st_size for f in target.rglob("*") if f.is_file())
    print("installed Muse to %s (%.1f MB)" % (target, size / 1e6))

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
