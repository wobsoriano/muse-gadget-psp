"""End to end: the Rust Muse client on real sockets against the fake Muse.

    uv run --with pytest --with cryptography --with websockets pytest tests/test_rust_session.py -v

The first three tests cover what muse-psp/tests/test_c_session.py covers. The
rest cover what the Rust client does that the C client does not.
"""

import hashlib
import json
import os
import pathlib
import socket
import struct
import subprocess
import sys

import pytest

ROOT = pathlib.Path(__file__).resolve().parent.parent
WAV_BYTES = 100 * 1024
DEVICE = "homelink-010203"
FILE_LINE = "[file:"


@pytest.fixture(scope="session")
def driver():
    built = subprocess.run(["cargo", "build", "--bin", "session"], cwd=ROOT / "host", capture_output=True, text=True)
    assert built.returncode == 0, built.stderr
    return ROOT / "host" / "target" / "debug" / "session"


def run_session(driver, state_dir, fake_args=(), device_args=(), timeout=90, api_port=None, authority=None):
    env = {**os.environ, "MUSE_GADGET_SDK": str(ROOT / "vendor" / "muse-gadget-sdk")}
    if authority:
        fake_args = [*fake_args, "--tls", str(state_dir)]
        device_args = [*device_args, "--tls=" + str(state_dir / authority)]
    server = subprocess.Popen([sys.executable, str(ROOT / "tests" / "fake_muse.py"), *fake_args],
                              stdout=subprocess.PIPE, text=True, cwd=ROOT, env=env)
    try:
        ports = json.loads(server.stdout.readline())
        device = subprocess.run(
            [str(driver), str(api_port or ports["api"]), str(ports["vm"]), str(state_dir), *device_args],
            capture_output=True, text=True, timeout=timeout)
        events = json.loads(device.stdout.strip().splitlines()[-1])
        assert device.returncode == 0, device.stderr + json.dumps(events, indent=1)
        assert device.stderr == ""
        if "--until-unreachable" in device_args:
            server.kill()
            return events, None
        transcript = json.loads(server.stdout.readline())
    finally:
        server.kill()
    return events, transcript


def text_chat(message):
    return [{"message": message, "output_modality": "text", "device_id": DEVICE}, "musegadget", True]


def check_api(transcript, fetches=1):
    api = transcript["api"]
    assert [(c["method"], c["path"]) for c in api] == (
        [("POST", "/device_token/refresh")] + [("GET", "/fetch_vms")] * fetches)
    assert api[0]["authorization"] == "Bearer hatch_refresh:r1"
    assert api[0]["user_agent"] == "musebadge-test"
    assert json.loads(api[0]["body"]) == {"device_id": DEVICE, "sdk_token": "mgst_test"}
    for fetch in api[1:]:
        assert fetch["authorization"] == "Bearer new-access"
        assert fetch["user_agent"] == "musebadge-test"


def check_vm(vm):
    assert vm[0] == {"path": "/v1/noise?vm_id=vm%201", "authorization": "Bearer vmtok"}
    assert vm[1] == {"control": ["POST", "/link-control", False]}
    register = vm[2]["register"]
    assert (register["type"], register["method"]) == ("req", "link.register")
    assert register["params"] == {
        "node_id": DEVICE, "display_name": "Test Badge", "platform": "linux",
        "version": "0.1.0", "device_family": "homehub", "model_id": "linux",
        "is_wakeup_supported": False, "commands_v2": {"badge.show_message": {}},
        "metadata": {"network_ssid": "TestNet"},
    }
    assert vm[3] == {"result": {"method": "link.result", "id": "inv-1", "ok": True,
                                "payload": {"shown": "hello"}}}


def check_logs(events):
    logs = [e[1] for e in events if e[0] == "log"]
    assert "device token rotated" in logs and "registered with Muse" in logs
    assert not [line for line in logs if "hatch" in line.lower() or "access" in line.lower()]
    return logs


def named(events, skip=("log", "settled")):
    return [e for e in events if e[0] not in skip]


def saved_pairing(state_dir):
    assert not (state_dir / "pairing.json.new").exists()
    return json.loads((state_dir / "pairing.json").read_text())


def test_session(driver, tmp_path):
    events, transcript = run_session(driver, tmp_path)

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
        ["invoke", "badge.show_message", 5],
        ["ask", "you said: what is up", "you said: what is up"],
        ["ask", "you said: and now", "you said: and now"],
        ["invoke", "echo", 120000],
        ["state", "unpaired"],
    ]
    # The early callback comes once per reply, with the whole text, before the final one.
    order = [e for e in events if e[0] in ("settled", "ask")]
    assert order == [["settled", "you said: what is up"], ["ask", "you said: what is up", "you said: what is up"],
                     ["settled", "you said: and now"], ["ask", "you said: and now", "you said: and now"]]
    logs = check_logs(events)
    assert "connecting to test" in logs
    assert any(line.startswith("session ended: Unpaired after ") for line in logs)
    saved = saved_pairing(tmp_path)
    assert (saved["access_token"], saved["refresh_token"]) == ("new-access", "r2") and saved["saved_at"] > 0


def test_voice_note(driver, tmp_path):
    events, transcript = run_session(driver, tmp_path, ["--voice"], ["--voice"])

    check_api(transcript)
    check_vm(transcript["vm"])
    assert transcript["vm"][4] == {"big_result_ok": True, "big_echo_matches": True}
    # A voice note carries the device's id, as a typed message does.
    voice_chat = [{"message": "", "output_modality": "text", "device_id": DEVICE,
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
    # What Muse heard arrives without the line naming the recording, and only for the voice note.
    assert [e for e in events if e[0] == "heard"] == [["heard", "a voice note"]]
    assert FILE_LINE not in json.dumps(named(events))
    assert named(events)[-1] == ["state", "unpaired"]
    check_logs(events)


def test_dead_connection(driver, tmp_path):
    events, transcript = run_session(driver, tmp_path, ["--dead"], timeout=140)

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
    quiet = logs.index("the connection to Muse went quiet")
    assert any(line.startswith("session ended: Failed(Quiet) after ") for line in logs[quiet:])
    assert "trying again in 2 s" in logs[quiet:]
    assert logs.count("registered with Muse") == 2

    # A session that drops after working is "connecting" again, never "unreachable".
    assert named(events) == [
        ["state", "connecting"],
        ["tokens", "new-access", "r2"],
        ["state", "connected"],
        ["invoke", "badge.show_message", 5],
        ["ask_failed", "session ended", ""],
        ["state", "connecting"],
        ["state", "connected"],
        ["invoke", "badge.show_message", 5],
        ["ask", "you said: what is up", "you said: what is up"],
        ["ask", "you said: and now", "you said: and now"],
        ["invoke", "echo", 120000],
        ["state", "unpaired"],
    ]


def test_command_on_the_chat_stream(driver, tmp_path):
    events, transcript = run_session(driver, tmp_path, ["--chat-invoke"])

    assert transcript["chat_invoke_result"] == {
        "method": "link.result", "id": "inv-chat", "ok": True, "payload": {"shown": "from the chat stream"}}
    invokes = [e for e in events if e[0] == "invoke"]
    assert invokes == [["invoke", "badge.show_message", 5],
                       ["invoke", "badge.show_message", len("from the chat stream")],
                       ["invoke", "echo", 120000]]
    assert [e for e in events if e[0] == "ask"] == [
        ["ask", "you said: what is up", "you said: what is up"],
        ["ask", "you said: and now", "you said: and now"],
    ]
    assert named(events)[-1] == ["state", "unpaired"]


def test_a_door_that_says_403_is_asked_again_quickly(driver, tmp_path):
    events, transcript = run_session(driver, tmp_path, ["--forbid", "3"])

    knocks = transcript["knocks"]
    assert len(knocks) == 4 and transcript["connections"] == 1
    gaps = [later - earlier for earlier, later in zip(knocks, knocks[1:])]
    # The API said every 150 ms. The C client would have waited 15 seconds.
    assert all(0.14 <= gap < 1.0 for gap in gaps), gaps
    check_api(transcript)       # one VM fetch: the same bearer is offered each time
    assert [e for e in named(events) if e[0] == "state"] == [
        ["state", "connecting"], ["state", "connected"], ["state", "unpaired"]]
    assert len([e for e in events if e[0] == "ask"]) == 2


def test_a_door_that_stays_shut_is_unreachable(driver, tmp_path):
    events, _ = run_session(driver, tmp_path, ["--forbid", "1000"], ["--until-unreachable"])

    logs = [e[1] for e in events if e[0] == "log"]
    # Five more tries as the API allowed, then the long wait the C client always took.
    assert logs.count("connecting to test") == 6
    assert "trying again in 15 s" in logs
    assert [e for e in named(events) if e[0] == "state"] == [
        ["state", "connecting"], ["state", "unreachable", "Refused(403)"]]


def test_no_network_is_unreachable(driver, tmp_path):
    with socket.socket() as unused:
        unused.bind(("127.0.0.1", 0))
        closed_port = unused.getsockname()[1]
    events, _ = run_session(driver, tmp_path, device_args=["--until-unreachable"], api_port=closed_port)

    assert named(events) == [["state", "connecting"], ["state", "unreachable", "Network"]]
    assert not (tmp_path / "pairing.json").exists()


def test_rejected_registration_is_unreachable(driver, tmp_path):
    events, _ = run_session(driver, tmp_path, ["--reject-register"], ["--until-unreachable"])

    assert [e for e in named(events) if e[0] == "state"] == [
        ["state", "connecting"], ["state", "unreachable", "Protocol"]]
    assert 'link.register rejected: "not welcome"' in [e[1] for e in events if e[0] == "log"]


def test_new_tokens_are_not_used_until_saved(driver, tmp_path):
    events, transcript = run_session(driver, tmp_path, device_args=["--save-fails=1"])

    # One refresh only: the old pair died with it, so the new pair is held
    # through the failed save and nothing is asked of the API until it is saved.
    check_api(transcript)
    assert named(events)[:5] == [
        ["state", "connecting"],
        ["save_failed"],
        ["state", "unreachable", "Storage"],
        ["tokens", "new-access", "r2"],
        ["state", "connected"],
    ]
    assert named(events)[-1] == ["state", "unpaired"]
    assert saved_pairing(tmp_path)["refresh_token"] == "r2"


def test_the_whole_session_over_tls(driver, tmp_path):
    """The path the PSP takes: every byte through Secure, certificates checked."""
    events, transcript = run_session(driver, tmp_path, ["--voice"], ["--voice"], authority="ca.der")

    check_api(transcript)
    check_vm(transcript["vm"])
    assert transcript["vm"][4] == {"big_result_ok": True, "big_echo_matches": True}
    assert transcript["voice"]["sha256"] == [e for e in events if e[0] == "voice"][0][2]
    assert [e[1] for e in events if e[0] == "ask"] == [
        "you said: what is up", "you said: and now", "you said: a voice note"]
    assert [e for e in named(events) if e[0] == "state"] == [
        ["state", "connecting"], ["state", "connected"], ["state", "unpaired"]]


def test_a_certificate_from_a_stranger_is_unreachable(driver, tmp_path):
    events, _ = run_session(driver, tmp_path, device_args=["--until-unreachable"], authority="stranger.der")

    assert named(events) == [["state", "connecting"], ["state", "unreachable", "Security"]]
    assert not (tmp_path / "pairing.json").exists()
