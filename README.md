# Muse on a Sony PSP

A native PSP app that turns a PlayStation Portable into a [Muse gadget](https://gadgets.muse.ai/).
Hold R, talk into the built-in microphone, and Muse answers on screen and out
loud through the speaker.

Everything runs on the PSP. The app holds its own encrypted session with Muse
over Wi-Fi, sends your voice as a voice note, and speaks the reply. It is
written in C, and the Muse client is a port of the
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
- **A Memory Stick** with about 15 MB free.
- **A Muse account, the Muse app on your phone, and an SDK token** from
  [gadgets.muse.ai](https://gadgets.muse.ai/settings/sdk-tokens).
- **A computer** to build the app. I used macOS. Linux should work, and I
  have not tried it.
- **A second device with Bluetooth, for pairing.** You need it once, and it
  has to run a Muse gadget client. A Raspberry Pi or a Linux computer with the SDK's Linux client
  will do. See [Pair it with Muse](#4-pair-it-with-muse) for why.
- **A short video of an avatar** on a plain light background. No artwork ships
  with this repository.
- **An OpenAI API key, if you want the natural voices.** Without one the PSP
  uses its built-in voice.

## Install

### 1. Get the PSP toolchain

Download a [pspdev release](https://github.com/pspdev/pspdev/releases) and
unpack it so that `toolchain/pspdev/bin` exists. On macOS the compiler also
needs three libraries:

```sh
brew install libmpc mpfr gmp
```

Then, in every shell you build from:

```sh
export PSPDEV=$PWD/toolchain/pspdev
export PATH=$PSPDEV/bin:$PATH
```

### 2. Make the avatar

The app plays a short looping clip of a character. Bring your own video of one
on a plain, near-white background, a few seconds long, ending close to how it
starts.

```sh
uv run --with pillow --with numpy tools/pack_video.py path/to/your/video.mp4
uv run --with pillow tools/make_icon.py
```

The first command cuts the character out and packs the frames into
`assets/clip`. The second draws the icon and backdrop the PSP's menu shows.
Git ignores both outputs, so your artwork stays out of the repository.

### 3. Build

```sh
mkdir -p build && cd build
psp-cmake ../app
make
cd ..
```

This produces `build/EBOOT.PBP`.

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

Put your SDK token in a file named `.sdk_token` in this folder. For the cloud
voices, put an OpenAI key in `.openai_key`. Git ignores both.

Put the PSP in **Settings > USB Connection**, then:

```sh
tools/install.py
```

It copies everything to `PSP/GAME/Muse` on the Memory Stick and ejects it. Run
it again after every rebuild. It never overwrites a pairing that is already on
the stick.

## Use it

Start your custom firmware, join Wi-Fi once in the PSP's Network Settings, make
sure the date and time are set, and run **Muse** from the Game menu. The status
goes from Joining Wi-Fi to Connecting to Muse to Connected.

| Button | What it does |
|---|---|
| Hold **R** | Talk. Release to send. Two notes play when it starts and stops listening. |
| **Select** | Choose the voice. Cross picks one and plays a sample. |
| **Circle** | Clear the message |
| **Cross** | Show a sample message |
| **Triangle** | Dance |
| **Start** | Quit |

**Voices.** Alloy, Coral, Nova, Sage, Onyx and Echo come from OpenAI and need a
key. **Built in** is made on the PSP itself by the Pico speech engine. It
sounds synthetic and needs no key or network. The PSP falls back to it when
it cannot reach a cloud voice.

**What Muse can do to the PSP.** Ask Muse from any device:

| Command | What it does |
|---|---|
| `psp.show_message` | Shows a message on the PSP and speaks it |
| `psp.clear` | Clears the message |
| `psp.dance` | Makes the avatar dance |
| `psp.status` | Reads the battery and Wi-Fi |

Name the device when you ask, for example "show hello on my PSP".

## How it works

```
app/           the PSP app: screen, buttons, microphone, speaker, voices
muse/          the Muse gadget client as a C99 library
third_party/   the Pico speech engine
tools/         avatar packing, menu art, pairing, installing
tests/         the client against a fake Muse, and checks you can run by hand
```

- **The Muse client** (`muse/`) speaks the gadget protocol. It fetches your
  Muse's address over HTTPS, opens a WebSocket, runs a Noise XX handshake
  inside it, registers the PSP, and then serves commands and chat. It uses
  mbedTLS and cJSON from the PSP toolchain.
- **A voice turn.** The recording goes to Muse as a WAV voice note. Muse
  answers in text. The app shows it, sends the text to the chosen voice, and
  plays the audio while it is still arriving.
- **Things the PSP made hard.** It has no hardware random source, so room
  noise from the microphone seeds the encryption. Its C library's clock is
  wrong, so the client checks certificate dates against the real-time clock chip.
  Its name lookup could hang forever, so the client does its own DNS.

## Develop

The Muse client also builds for your computer, which is where its tests run:

```sh
git clone https://github.com/facebookincubator/muse-gadget-sdk vendor/muse-gadget-sdk
cmake -S tests -B build-host && cmake --build build-host
uv run --with pytest --with cryptography --with websockets pytest tests/test_c_session.py
tests/psp_build.sh
```

The first test drives the client against a fake Muse, which is built on the
SDK's own Python code. That is what the clone is for. The second proves the
library cross-compiles and links for the PSP.

The app writes two logs next to itself on the Memory Stick, `link_trace.txt`
and `SDL_Log.txt`. Read those first when something misbehaves on the PSP.

## Known limits

- **Tested on a single PSP-3001.** Other models and firmware are unknown.
- **Pairing needs a second device.** There is no way around the missing
  Bluetooth. Any device that can complete Muse's Bluetooth pairing can do it,
  and the pairing is then moved to the PSP by hand.
- **Keys sit in plain text on the Memory Stick.** That includes the pairing
  and the OpenAI key. Anyone holding the stick can read them.
- **The random numbers are weaker than a modern device's.** Microphone noise
  is the best source the PSP offers.
- **TLS 1.2 only.** That is what the toolchain's mbedTLS provides.
- **Long answers are spoken only in part.** About the first 420 characters,
  to fit the PSP's memory. The full text is still shown.
- **Muse sends text, not audio.** The voice is always made separately.

## License

Apache License 2.0. See [LICENSE](LICENSE), and [NOTICE](NOTICE) for the
third-party work this builds on.
