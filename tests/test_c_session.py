"""End to end: the C library's service loop on real sockets against the fake Muse.

    uv run --with pytest --with cryptography --with websockets pytest tests/test_c_session.py -v

Set MUSE_SANITIZE=ON to build with AddressSanitizer and UBSan.
"""

import hashlib
import json
import os
import pathlib
import struct
import subprocess
import sys

import pytest

ROOT = pathlib.Path(__file__).resolve().parent.parent
SANITIZE = os.environ.get("MUSE_SANITIZE", "OFF")
BUILD = ROOT / ("build-host-asan" if SANITIZE == "ON" else "build-host")
WAV_BYTES = 100 * 1024


@pytest.fixture(scope="session")
def host_session():
    subprocess.run(["cmake", "-S", str(ROOT / "tests"), "-B", str(BUILD),
                    "-DMUSE_SANITIZE=" + SANITIZE], check=True, capture_output=True)
    built = subprocess.run(["cmake", "--build", str(BUILD)], capture_output=True, text=True)
    assert built.returncode == 0, built.stdout + built.stderr
    return BUILD / "host_session"


def run_session(host_session, fake_args=(), device_args=(), timeout=90):
    server = subprocess.Popen(
        [sys.executable, str(ROOT / "tests" / "fake_muse_psp.py"), *fake_args],
        stdout=subprocess.PIPE, text=True, cwd=ROOT)
    try:
        ports = json.loads(server.stdout.readline())
        device = subprocess.run(
            [str(host_session), str(ports["api"]), str(ports["vm"]), *device_args],
            capture_output=True, text=True, timeout=timeout)
        assert device.returncode == 0, device.stderr + device.stdout
        assert device.stderr == ""          # sanitizer reports land here
        events = json.loads(device.stdout.strip().splitlines()[-1])
        transcript = json.loads(server.stdout.readline())
    finally:
        server.kill()
    return events, transcript


def text_chat(message):
    return [{"message": message, "output_modality": "text", "device_id": "homelink-010203"},
            "musegadget", True]


def check_api(transcript, fetches=1):
    api = transcript["api"]
    assert [(c["method"], c["path"]) for c in api] == (
        [("POST", "/device_token/refresh")] + [("GET", "/fetch_vms")] * fetches)
    assert api[0]["authorization"] == "Bearer hatch_refresh:r1"
    assert api[0]["user_agent"] == "musebadge-test"
    assert json.loads(api[0]["body"]) == {"device_id": "homelink-010203", "sdk_token": "mgst_test"}
    for fetch in api[1:]:
        assert fetch["authorization"] == "Bearer new-access"
        assert fetch["user_agent"] == "musebadge-test"


def check_vm(vm):
    assert vm[0] == {"path": "/v1/noise?vm_id=vm%201", "authorization": "Bearer vmtok"}
    assert vm[1] == {"control": ["POST", "/link-control", False]}
    register = vm[2]["register"]
    assert (register["type"], register["method"]) == ("req", "link.register")
    assert register["params"] == {
        "node_id": "homelink-010203", "display_name": "Test Badge", "platform": "linux",
        "version": "0.1.0", "device_family": "homehub", "model_id": "linux",
        "is_wakeup_supported": False, "commands_v2": {"badge.show_message": {}},
        "metadata": {"network_ssid": "TestNet"},
    }
    assert vm[3] == {"result": {"method": "link.result", "id": "inv-1", "ok": True,
                                "payload": {"shown": "hello"}}}


def check_logs(events):
    logs = [e[1] for e in events if e[0] == "log"]
    assert "device token rotated" in logs and "registered with the Muse" in logs
    assert not [line for line in logs if "hatch" in line.lower()]
    return logs


def named(events):
    return [e for e in events if e[0] != "log"]


def test_session(host_session):
    events, transcript = run_session(host_session)

    check_api(transcript)
    vm = transcript["vm"]
    check_vm(vm)
    assert vm[4] == {"big_result_ok": True, "big_echo_matches": True}
    assert len(vm) == 5
    assert transcript["chats"] == [text_chat("what is up"), text_chat("and now")]
    assert transcript["subscribes"] == 1    # one stream serves every question
    assert transcript["connections"] == 1

    assert named(events) == [
        ["state", "connecting"],
        ["tokens", "new-access", "r2"],
        ["state", "connected"],
        ["invoke", "badge.show_message", 5, None],
        ["ask", "you said: what is up", "you said: what is up"],
        ["ask", "you said: and now", "you said: and now"],
        ["invoke", "echo", 120000, None],
        ["state", "unpaired"],
    ]
    logs = check_logs(events)
    assert "connecting to test" in logs
    assert any(line.startswith("session ended: unpaired after ") for line in logs)


def test_voice_note(host_session):
    events, transcript = run_session(host_session, ["--voice"], ["--voice"])

    check_api(transcript)
    check_vm(transcript["vm"])
    assert transcript["vm"][4] == {"big_result_ok": True, "big_echo_matches": True}
    voice_chat = [{"message": "", "output_modality": "text",
                   "items": [{"type": "file", "mime_type": "audio/wav",
                              "filename": "voice_note.wav"}]}, "musegadget", False]
    assert transcript["chats"] == [text_chat("what is up"), text_chat("and now"), voice_chat]
    assert transcript["subscribes"] == 1

    sent = [e for e in events if e[0] == "voice"]
    assert len(sent) == 1 and sent[0][1] == WAV_BYTES
    header = (b"RIFF" + struct.pack("<I", WAV_BYTES - 8) + b"WAVEfmt "
              + struct.pack("<IHHIIHH", 16, 1, 1, 16000, 32000, 2, 16)
              + b"data" + struct.pack("<I", WAV_BYTES - 44))
    heard = transcript["voice"]
    assert heard["wav_len"] == WAV_BYTES
    assert heard["header_hex"] == header.hex()
    assert heard["sha256"] == sent[0][2]    # byte-exact through base64 and the body chunks
    assert heard["sha256"] != hashlib.sha256(b"").hexdigest()
    body_bytes = len(json.dumps(voice_chat[0], separators=(",", ":"))) + 4 * ((WAV_BYTES + 2) // 3) \
        + len(',"data_base64":""')
    assert heard["body_chunks"] == -(-body_bytes // (16 * 1024))

    assert [e for e in named(events) if e[0] == "ask"] == [
        ["ask", "you said: what is up", "you said: what is up"],
        ["ask", "you said: and now", "you said: and now"],
        ["ask", "you said: a voice note", "you said: a voice note"],
    ]
    assert named(events)[-1] == ["state", "unpaired"]
    check_logs(events)


def test_dead_connection(host_session):
    events, transcript = run_session(host_session, ["--dead"], timeout=140)

    check_api(transcript, fetches=2)
    assert transcript["connections"] == 2
    check_vm(transcript["dead_vm"])
    assert len(transcript["dead_vm"]) == 4      # it went silent after the first invoke
    vm = transcript["vm"]
    check_vm(vm)
    assert vm[4] == {"big_result_ok": True, "big_echo_matches": True}
    # The chat sent into the dead connection was never read.
    assert transcript["chats"] == [text_chat("what is up"), text_chat("and now")]
    assert transcript["subscribes"] == 1

    logs = check_logs(events)
    quiet = logs.index("the connection to the Muse went quiet; reconnecting")
    assert any(line.startswith("session ended: closed after ") for line in logs[quiet:])
    assert "reconnecting in 2s" in logs[quiet:]
    assert logs.count("registered with the Muse") == 2

    assert named(events) == [
        ["state", "connecting"],
        ["tokens", "new-access", "r2"],
        ["state", "connected"],
        ["invoke", "badge.show_message", 5, None],
        ["ask_failed", "session ended"],
        ["state", "connecting"],
        ["state", "connected"],
        ["invoke", "badge.show_message", 5, None],
        ["ask", "you said: what is up", "you said: what is up"],
        ["ask", "you said: and now", "you said: and now"],
        ["invoke", "echo", 120000, None],
        ["state", "unpaired"],
    ]
