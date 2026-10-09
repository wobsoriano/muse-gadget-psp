# Muse on a Sony PSP

A native PSP app that turns a PlayStation Portable into a [Muse gadget](https://gadgets.muse.ai/).
Hold R, talk into the built-in microphone, and Muse answers out loud through
the speaker while its avatar listens, thinks and speaks on screen.

Everything runs on the PSP. The app holds its own encrypted session with Muse
over Wi-Fi, sends your voice as a voice note, and speaks the reply. It is
written in Rust, and the Muse client follows the protocol of the
[Muse Gadget SDK](https://github.com/facebookincubator/muse-gadget-sdk).

**[Watch the demo](https://x.com/wobsoriano/status/2106779263857013060)**

> This is a hobby project. It is not affiliated with, endorsed by, or
> supported by Sony, Meta, or OpenAI. PSP and PlayStation are trademarks of
> Sony Interactive Entertainment. Muse is a product of Meta. You use this at
> your own risk, including the custom firmware it needs.

## What you need

- **A PSP-3000.** I built and tested it on one PSP-3001. It needs the
  built-in microphone, which the PSP-1000 and PSP-2000 do not have.
- **Custom firmware.** System software 6.61 with [ARK](https://github.com/PSP-Arkfive/ARK-5).
  ARK also lets the PSP join a WPA2 Wi-Fi network, which the stock firmware
  cannot.
- **A Memory Stick** with about 5 MB free.
- **A Muse account, the Muse app on your phone, and an SDK token** from
  [gadgets.muse.ai](https://gadgets.muse.ai/settings/sdk-tokens).
- **A computer** to build the app. I used macOS. Linux should work, and I
  have not tried it.
- **A second device with Bluetooth, for pairing.** You need it once, and it
  has to run a Muse gadget client. A Raspberry Pi or a Linux computer with the
  SDK's Linux client will do. See [Pair it with Muse](#4-pair-it-with-muse)
  for why.
- **An OpenAI API key, for the voice.** Without one the PSP still hears you
  and Muse still answers, but the PSP stays silent.

## Install

### 1. Get the tools

Install Rust with [rustup](https://rustup.rs/), then the PSP build tool:

```sh
cargo install cargo-psp
```

The project pins a nightly toolchain in `rust-toolchain.toml`, and rustup
fetches it the first time you build. You also need a C compiler for one step,
and [uv](https://docs.astral.sh/uv/) if you want to run the tests.

### 2. Make the avatar

The app plays the avatar that ships with the Muse Gadget SDK. That avatar is
Meta's and the SDK's licence does not cover it, so it is not in this
repository. You build it from your own copy of the SDK:

```sh
git clone https://github.com/facebookincubator/muse-gadget-sdk vendor/muse-gadget-sdk
tools/avatar.sh
```

This runs the SDK's own renderer on your computer and writes
`assets/avatar/avatar.bin`, one short clip for each mood. Git ignores both
the clone and the output.

### 3. Build

```sh
cargo psp --release
```

This produces `target/mipsel-sony-psp/release/EBOOT.PBP`. It also prints a
wall of warnings. They come from the PSP library in `vendor/psp`, and you can
ignore them.

### 4. Pair it with Muse

Muse pairs a gadget over Bluetooth, and the PSP has no Bluetooth. So a device
that has Bluetooth does that conversation once. What comes out of it is small,
an identity and a pair of tokens, and you hand those to the PSP. After that
the PSP connects to Muse by itself and renews its own credentials.

Any Muse gadget client that has finished pairing will do. The
[Linux client](https://github.com/facebookincubator/muse-gadget-sdk/tree/main/linux)
in the Muse Gadget SDK is the simplest:

1. Install it on a Raspberry Pi or a Linux computer with Bluetooth, and pair
   it from the Muse app, following its README.
2. Stop it, so it is no longer using the pairing:
   `sudo systemctl disable --now musegadget`
3. Copy `identity.json` and `pairing.json` out of `/var/lib/musegadget` to
   your computer.
4. Hand them to this repository:

```sh
tools/pair.py path/to/the/folder
```

The tool saves the pairing in `state/`, which git ignores. Treat it like a
password.

Two rules matter here. Only one device can use an identity at a time, and
every renewal replaces the tokens. So once the PSP has the pairing, leave the
client it came from switched off, or remove it.

This works on paper because the two clients use the same files and fields. I
have not run it end to end with the Linux client, so expect to adjust a step.

### 5. Copy it to the PSP

Put your SDK token in a file named `.sdk_token` in this folder, and an OpenAI
key in `.openai_key`. Git ignores both.

Put the PSP in **Settings > USB Connection**, then:

```sh
tools/install.py
```

It copies the app and the avatar to `PSP/GAME/Muse` on the Memory Stick and
ejects it. Run it again after every rebuild. It never overwrites a pairing
that is already on the stick.

## Use it

Start your custom firmware, join Wi-Fi once in the PSP's Network Settings, make
sure the date and time are set, and run **Muse** from the Game menu.

Hold **R** to talk and release to send. Press **HOME** to quit.

The corner of the screen shows whether the PSP can reach Muse, and the avatar
shows what it is doing.

| You see | It means |
|---|---|
| An amber light and "connecting" | Joining Wi-Fi or reaching Muse |
| A green light and "online" | Ready. Hold R to talk. |
| A red light and "offline" | Muse has been out of reach for 45 seconds, or the PSP is not paired. The app keeps trying. |
| The avatar in blue | Listening while you hold R |
| The avatar in pink, with thought dots | Your question is on its way, or Muse is answering |
| The avatar in green, with sound waves | Speaking the answer |
| Hearts for a few seconds, no sound | Muse answered and the PSP could not speak it |

**What Muse can do to the PSP.** Ask Muse from any device:

| Command | What it does |
|---|---|
| `psp.say` | Speaks a message through the PSP |
| `psp.dance` | Makes the avatar dance |
| `psp.status` | Reads the battery and Wi-Fi |

Name the device when you ask, for example "say hello on my PSP".

## How it works

```
src/      the PSP app: screen, avatar, microphone, speaker, Wi-Fi, storage
link/     the Muse client and the speech request, with no PSP code in them
host/     programs that run link/ on a computer
tools/    the avatar, pairing, installing
tests/    the client against a fake Muse
vendor/   the PSP library, with two small additions
```

- **The Muse client** (`link/`) fetches your Muse's address over HTTPS, opens
  a WebSocket, runs a Noise XX handshake inside it, registers the PSP, and
  then serves commands and questions. It uses rustls for TLS 1.3 and has no
  threads of its own. The app calls it in a loop.
- **A voice turn.** The app cuts the recording to a third of its size, evens
  out its loudness, and sends it to Muse as a WAV voice note. Muse answers in
  text. The app sends that text to OpenAI and plays the audio while it is
  still arriving.
- **Five threads share one processor.** They are the screen, the microphone,
  the connection, the speech download and the speaker. They pass work to
  each other through atomics, because a lock between threads of different
  priority can hang a PSP.
- **Things the PSP made hard.** It has no hardware random source, so room
  noise from the microphone feeds the encryption. Its own name lookup could
  hang forever, so the app does its own DNS. Its working folder belongs to
  one thread, so the app opens every file by its full path.

## Develop

`link/` builds for your computer, which is where its tests run:

```sh
cd link && cargo test && cd ..
uv run --with pytest --with cryptography --with websockets pytest tests/test_rust_session.py
```

The first runs the pure parts, including a Noise handshake checked byte for
byte against the SDK's Python. The second drives the client against a fake
Muse built on the SDK's own code, which is what the clone in step 2 is for.

Two programs in `host/` run the PSP's network code against the real services
from a computer. `host-check` makes one HTTPS request, and `speech` speaks a
sentence and saves the audio.

The app writes `log.txt` next to itself on the Memory Stick, with a time on
every line. Read that first when something misbehaves on the PSP.

## Known limits

- **Tested on a single PSP-3001.** Other models and firmware are unknown.
- **Pairing needs a second device.** There is no way around the missing
  Bluetooth.
- **Keys sit in plain text on the Memory Stick.** That includes the pairing
  and the OpenAI key. Anyone holding the stick can read them.
- **The random numbers are weaker than a modern device's.** Microphone noise
  is the best source the PSP offers.
- **No voice without OpenAI.** There is no offline voice and no text on
  screen. When the PSP cannot speak an answer, all you get is the hearts.
- **Uploads are slow and sometimes stall.** A two second question takes
  several seconds to reach Muse on my PSP. If one gets lost on the way,
  the app asks it again once by itself, which takes longer still.
- **Long answers get cut short.** The PSP speaks about the first 420
  characters.
- **Muse sends text, not audio.** OpenAI makes the voice.

## License

Apache License 2.0. See [LICENSE](LICENSE), and [NOTICE](NOTICE) for the
third-party work this builds on.
