"""Writes link/src/noise/vectors.rs: known answers from the Muse Gadget SDK's
Python Noise code, for the Rust unit tests.

    MUSE_GADGET_SDK=vendor/muse-gadget-sdk uv run --with cryptography python tests/noise_vectors.py
"""

import os
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SDK = pathlib.Path(os.environ.get("MUSE_GADGET_SDK") or ROOT / "vendor" / "muse-gadget-sdk")
sys.path.insert(0, str(SDK / "linux" / "src"))

from cryptography.hazmat.primitives import serialization  # noqa: E402
from cryptography.hazmat.primitives.asymmetric import x25519  # noqa: E402

from musegadget.noise import (  # noqa: E402
    ApplicationResponse, BodyChunk, NoiseXXInitiator, NoiseXXResponder, ServiceFrame,
    encode_noise_frames, noise_xx,
)
from musegadget.noise.envelope import Header, Reset, ResetCode  # noqa: E402
from musegadget.noise.transport import NoiseTransport, decode_request_envelope, encode_response_envelope  # noqa: E402

# Initiator ephemeral, responder ephemeral, responder static, initiator static:
# the order the handshake asks for them.
PRIVATE_KEYS = [bytes([seed + i for i in range(32)]) for seed in (0x10, 0x40, 0x70, 0xA0)]
queue = list(PRIVATE_KEYS)


def fixed_key_pair():
    private = x25519.X25519PrivateKey.from_private_bytes(queue.pop(0))
    public = private.public_key().public_bytes(
        encoding=serialization.Encoding.Raw, format=serialization.PublicFormat.Raw)
    return noise_xx._X25519KeyPair(private_key=private, public_key_bytes=public)


noise_xx._generate_x25519_key_pair = fixed_key_pair

initiator, responder = NoiseXXInitiator(), NoiseXXResponder()
initiator.initialize()
responder.initialize()
msg1 = initiator.write_message1()
msg2 = responder.read_message1_and_write_message2(msg1)
initiator.read_message2(msg2)
msg3 = initiator.write_message3()
responder.read_message3(msg3)
client_send, client_recv = initiator.split()
server_send, server_recv = responder.split()

# What the SDK's own client seals for a request and a body chunk.
# Streams are numbered from 1, and each frame takes the next nonce.
transport = NoiseTransport(client_send, client_recv)
CHUNK_ID = 0x0123456789ABCDEF
noise_frames = sys.modules["musegadget.noise.framing"]
noise_frames._random_int64 = lambda: CHUNK_ID
headers = [Header("x-request-id", "r-1"), Header("x-app-id", "musegadget")]
started = transport._send_application_request(
    "POST", "/chat/stream", b'{"message":"hi"}', "daemon", headers, False)
sealed_request = started.frames
stream_id = started.stream_id
sealed_body = transport.encrypt_body_chunk(stream_id, b"tail", end_body=True)
assert stream_id == 1 and len(sealed_request) == len(sealed_body) == 1
for sealed in (sealed_request, sealed_body):
    decode_request_envelope(noise_frames.NoiseFrameDecoder().decode(server_recv.decrypt_with_ad(b"", sealed[0])))


def from_server(frame, chunk_id=None):
    return [server_send.encrypt_with_ad(b"", chunk)
            for chunk in encode_noise_frames(encode_response_envelope(frame), chunk_id)]


big = bytes(i % 251 for i in range(150000))
server_frames = {
    "RESPONSE": from_server(ServiceFrame.response(7, ApplicationResponse(status=200, body=b'{"ok":true}', end_body=True))),
    "BODY": from_server(ServiceFrame.body_chunk(7, BodyChunk(data=b"line\n"))),
    "RESET": from_server(ServiceFrame.reset(9, Reset(code=ResetCode.CANCELLED, reason="gone"))),
    "BIG": from_server(ServiceFrame.body_chunk(3, BodyChunk(data=big, end_body=True)), 0x7766554433221100),
}
assert len(server_frames["BIG"]) == 3


def const(name, data):
    return 'pub const %s: &str = "%s";\n' % (name, bytes(data).hex())


out = "// Written by tests/noise_vectors.py from the Muse Gadget SDK's Python. Do not edit.\n\n"
out += const("INITIATOR_EPHEMERAL", PRIVATE_KEYS[0]) + const("INITIATOR_STATIC", PRIVATE_KEYS[3])
out += const("MESSAGE_1", msg1) + const("MESSAGE_2", msg2) + const("MESSAGE_3", msg3)
out += "pub const CHUNK_ID: i64 = 0x%X;\n" % CHUNK_ID
out += const("SEALED_REQUEST", sealed_request[0]) + const("SEALED_BODY", sealed_body[0])
for name in ("RESPONSE", "BODY", "RESET"):
    out += const("FROM_SERVER_" + name, server_frames[name][0])
out += "pub const FROM_SERVER_BIG: [&str; 3] = [\n%s];\n" % "".join(
    '    "%s",\n' % chunk.hex() for chunk in server_frames["BIG"])
target = ROOT / "link" / "src" / "noise" / "vectors.rs"
target.parent.mkdir(exist_ok=True)
target.write_text(out)
print("wrote %s (%d bytes)" % (target, len(out)))
