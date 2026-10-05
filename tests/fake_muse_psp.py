"""A local stand-in for the Muse API and VM, built on Meta's reference code.

It uses the Muse Gadget SDK's own Python code for the server's half of the
protocol, so the C client is tested against the reference, not against itself.
Clone https://github.com/facebookincubator/muse-gadget-sdk into
vendor/muse-gadget-sdk, or point MUSE_GADGET_SDK at a checkout.

Serves the device API over plain HTTP and the Noise WebSocket on a second
port, then walks one scripted session: register, invoke, chats, a large
invoke, unpair. Prints one JSON line with the two ports when ready and one
with the transcript when the session ends.

    --voice  expect a third chat carrying a WAV, sent as a request plus body chunks
    --dead   the first VM connection goes silent after its first invoke; the
             script runs on the connection the device opens next
"""

import argparse
import asyncio
import base64
import hashlib
import json
import os
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SDK = pathlib.Path(os.environ.get("MUSE_GADGET_SDK") or ROOT / "vendor" / "muse-gadget-sdk")
if not (SDK / "linux" / "src" / "musegadget").is_dir():
    sys.exit("Muse Gadget SDK not found at %s. Clone it there or set MUSE_GADGET_SDK." % SDK)
sys.path.insert(0, str(SDK / "linux" / "src"))

from websockets.asyncio.server import serve  # noqa: E402

from musegadget.link_client import MessageDecoder, encode_message  # noqa: E402
from musegadget.noise import (  # noqa: E402
    ApplicationResponse, BodyChunk, NoiseFrameDecoder, NoiseXXResponder, ServiceFrame,
    encode_noise_frames,
)
from musegadget.noise.transport import decode_request_envelope, encode_response_envelope  # noqa: E402

parser = argparse.ArgumentParser()
parser.add_argument("--voice", action="store_true")
parser.add_argument("--dead", action="store_true")
args = parser.parse_args()

BIG_TEXT = "badge " * 20000  # 120 KB, so the invoke spans two Noise chunks
EXPECTED_ASKS = 3 if args.voice else 2
DEAD_SECONDS = 70
transcript = {"api": [], "vm": [], "dead_vm": [], "chats": [], "subscribes": 0,
              "connections": 0, "voice": None}
done = asyncio.Event()


async def api_handler(reader, writer):
    request_line = (await reader.readline()).decode().split()
    headers = {}
    while (line := await reader.readline()) not in (b"\r\n", b""):
        name, _, value = line.decode().partition(":")
        headers[name.strip().lower()] = value.strip()
    body = await reader.readexactly(int(headers.get("content-length", 0)))
    method, path = request_line[0], request_line[1]
    transcript["api"].append({
        "method": method, "path": path, "authorization": headers.get("authorization"),
        "user_agent": headers.get("user-agent"), "body": body.decode(),
    })
    status, payload = 404, {}
    if (method, path) == ("POST", "/device_token/refresh"):
        status, payload = 200, {"payload": {"access_token": "new-access", "refresh_token": "r2"}}
    elif (method, path) == ("GET", "/fetch_vms"):
        if headers.get("authorization") == "Bearer new-access":
            status, payload = 200, {"vm_list": [
                {"vm_ws_url": "wss://ignored", "vm_auth_token": "skip", "vm_name": "other", "vm_id": "vm0"},
                {"vm_ws_url": "wss://ignored", "vm_auth_token": "vmtok", "vm_name": "test",
                 "vm_id": "vm 1", "default": True},
            ]}
        else:
            status = 401
    data = json.dumps(payload).encode()
    # Chunked, because that is what a real edge may send.
    writer.write(b"HTTP/1.1 %d X\r\nTransfer-Encoding: chunked\r\n\r\n" % status)
    writer.write(b"%x\r\n" % len(data) + data + b"\r\n0\r\n\r\n")
    await writer.drain()
    writer.close()


class Vm:
    def __init__(self, ws):
        self.ws = ws
        self.decoder = NoiseFrameDecoder()
        self.messages = MessageDecoder()
        self.stream_id = 0
        self.subscription = 0
        self.uploads = {}  # stream id -> [request, body so far, body chunk count]

    async def handshake(self):
        responder = NoiseXXResponder()
        responder.initialize()
        await self.ws.send(responder.read_message1_and_write_message2(await self.ws.recv()))
        responder.read_message3(await self.ws.recv())
        self.send_cipher, self.recv_cipher = responder.split()

    async def next_frame(self):
        """The next frame the script cares about; side requests are answered here."""
        while True:
            plain = self.recv_cipher.decrypt_with_ad(b"", await self.ws.recv())
            assembled = self.decoder.decode(plain)
            if assembled is None:
                continue
            frame = decode_request_envelope(assembled)
            if frame.kind == "request" and frame.value.path != "/link-control":
                if frame.value.end_body:
                    await self.answer(frame.stream_id, frame.value, frame.value.body, 0)
                else:
                    self.uploads[frame.stream_id] = [frame.value, bytearray(frame.value.body), 0]
                continue
            if frame.kind == "body_chunk" and frame.stream_id in self.uploads:
                upload = self.uploads[frame.stream_id]
                upload[1] += frame.value.data
                upload[2] += 1
                if frame.value.end_body:
                    del self.uploads[frame.stream_id]
                    await self.answer(frame.stream_id, upload[0], bytes(upload[1]), upload[2])
                continue
            return frame

    async def answer(self, stream_id, request, raw_body, body_chunks):
        app_id = {h.key: h.value for h in request.headers}.get("x-app-id")
        path, _, query = request.path.partition("?")
        reply = {"ok": False}
        if (request.verb, path) == ("POST", "/chat/subscribe"):
            transcript["subscribes"] += 1
            self.subscription = stream_id
            await self.send_frame(ServiceFrame.response(stream_id, ApplicationResponse(
                status=200, body=b'{"type":"ack"}\n')))
            return
        if (request.verb, path) == ("POST", "/chat/stream"):
            body = json.loads(raw_body)
            heard = body["message"]
            for item in body.get("items", []):
                wav = base64.b64decode(item.pop("data_base64"), validate=True)
                transcript["voice"] = {
                    "wav_len": len(wav), "header_hex": wav[:44].hex(),
                    "sha256": hashlib.sha256(wav).hexdigest(), "body_chunks": body_chunks,
                }
                # Stands in for the transcript the real service makes of the audio.
                heard = "a voice note"
            transcript["chats"].append([body, app_id, request.end_body])
            user_id = "u%d" % len(transcript["chats"])
            reply = {"ok": True, "result": {"message_id": user_id}}
            if self.subscription:
                # Events race the ack on the real service, so they go first here.
                await self.stream_reply(user_id, "you said: " + heard)
        await self.send_frame(ServiceFrame.response(stream_id, ApplicationResponse(
            status=200, body=json.dumps(reply).encode(), end_body=True)))

    async def stream_reply(self, user_id, text):
        def line(seq, name, payload):
            return json.dumps({"type": "event", "seq": seq, "event": name, "payload": payload}).encode() + b"\n"

        # The shapes the real service sends: the reply names no parent, its
        # text chunks name themselves as parent, and "done" carries no text.
        reply_id = "assistant-msg-" + user_id
        half = len(text) // 2
        data = b"".join([
            line(1, "delta.message_start", {"message_id": "earlier", "reply_to_message_id": ""}),
            line(2, "delta.text_append", {"message_id": "earlier", "text": "not for this badge"}),
            line(3, "message.user", {"message_id": user_id, "display_text": "..."}),
            line(4, "agent.status", {"activity_code": "working"}),
            line(5, "delta.message_start", {"message_id": reply_id, "reply_to_message_id": ""}),
            line(6, "delta.text_append", {"message_id": reply_id, "parent_message_id": reply_id,
                                          "text": text[:half]}),
            line(7, "delta.text_append", {"message_id": reply_id, "parent_message_id": reply_id,
                                          "text": text[half:]}),
            line(8, "agent.status", {"activity_code": "online"}),
            line(9, "delta.message_done", {"message_id": reply_id, "reply_to_message_id": "",
                                           "status": "completed"}),
        ])
        # Split mid-line, so the badge has to reassemble lines across chunks.
        for start in range(0, len(data), 97):
            await self.send_frame(ServiceFrame.body_chunk(
                self.subscription, BodyChunk(data=data[start:start + 97])))

    async def next_message(self):
        while True:
            frame = await self.next_frame()
            messages = self.messages.feed(frame.value.data)
            if messages:
                return messages[0]

    async def send_frame(self, frame):
        for chunk in encode_noise_frames(encode_response_envelope(frame)):
            await self.ws.send(self.send_cipher.encrypt_with_ad(b"", chunk))

    async def send_message(self, message):
        await self.send_frame(ServiceFrame.body_chunk(
            self.stream_id, BodyChunk(data=encode_message(message))))

    async def wait_for_asks(self):
        """Poll the device until every chat turn it means to run has finished.

        The library's public API sends nothing but chats and invoke results,
        so the device reports its progress as the result of a test command.
        Chats are side requests, answered while this waits for each result.
        """
        polls = 0
        while True:
            polls += 1
            await self.send_message({"method": "link.invoke", "id": "poll-%d" % polls,
                                     "command": "test.progress", "params": {}})
            result = await self.next_message()
            if result.get("payload", {}).get("asks_done", 0) >= EXPECTED_ASKS:
                return
            await asyncio.sleep(0.3)


async def vm_handler(ws):
    transcript["connections"] += 1
    dead = args.dead and transcript["connections"] == 1
    log = transcript["dead_vm"] if dead else transcript["vm"]
    log.append({"path": ws.request.path, "authorization": ws.request.headers.get("Authorization")})
    vm = Vm(ws)
    await vm.handshake()
    request = await vm.next_frame()
    vm.stream_id = request.stream_id
    log.append({"control": [request.value.verb, request.value.path, request.value.end_body]})
    await vm.send_frame(ServiceFrame.response(vm.stream_id, ApplicationResponse(status=200)))
    register = await vm.next_message()
    log.append({"register": register})
    await vm.send_message({"type": "res", "id": register["id"], "ok": True})

    await vm.send_message({"method": "link.invoke", "id": "inv-1", "command": "badge.show_message",
                           "params": {"text": "hello"}, "timeout_ms": 5000})
    log.append({"result": await vm.next_message()})

    if dead:
        # A peer that still accepts bytes but never answers: nothing is read,
        # so the device's pings draw no pong and nothing closes the socket.
        ws.transport.pause_reading()
        await asyncio.sleep(DEAD_SECONDS)
        return

    await vm.wait_for_asks()

    await vm.send_message({"method": "link.invoke", "id": "inv-2", "command": "echo",
                           "params": {"text": BIG_TEXT}})
    big = await vm.next_message()
    log.append({"big_result_ok": big.get("ok"), "big_echo_matches": big["payload"]["text"] == BIG_TEXT})

    await ws.ping()
    await vm.send_message({"type": "evt", "event": "link.unpaired"})
    done.set()
    await ws.wait_closed()


def check_bearer(connection, request):
    if request.headers.get("Authorization") != "Bearer vmtok":
        transcript["vm"].append({"rejected": request.headers.get("Authorization")})
        return connection.respond(401, "no\n")
    return None


async def main():
    api = await asyncio.start_server(api_handler, "127.0.0.1", 0)
    # A dead peer sends nothing either, so its own keepalive pings are off.
    keepalive = {"ping_interval": None} if args.dead else {}
    vm = await serve(vm_handler, "127.0.0.1", 0, process_request=check_bearer, max_size=None,
                     **keepalive)
    print(json.dumps({"api": api.sockets[0].getsockname()[1],
                      "vm": vm.sockets[0].getsockname()[1]}), flush=True)
    try:
        await asyncio.wait_for(done.wait(), 150)
    finally:
        print(json.dumps(transcript), flush=True)


asyncio.run(main())
