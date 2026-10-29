"""A local stand-in for the Muse API and VM, built on Meta's reference code.

Copied from muse-psp/tests/fake_muse_psp.py and extended for the Rust client.
It uses the Muse Gadget SDK's own Python code for the server's half of the
protocol, so the client is tested against the reference, not against itself.
The SDK is read from vendor/muse-gadget-sdk, or from MUSE_GADGET_SDK.

Serves the device API over plain HTTP and the Noise WebSocket on a second
port, then walks one scripted session: register, invoke, chats, a large
invoke, unpair. Prints one JSON line with the two ports when ready and one
with the transcript when the session ends.

    --voice  expect a third chat carrying a WAV, sent as a request plus body chunks
    --dead   the first VM connection goes silent after its first invoke; the
             script runs on the connection the device opens next
    --forbid N       the VM's front door answers the first N upgrades with 403,
                     and the API says to ask again every 150 ms, up to 5 times
    --chat-invoke    the first chat's reply is followed by a command sent as a
                     client.invoke event on the chat stream
    --reject-register  link.register is answered with an error
    --tls DIR        serve HTTPS and WSS as "localhost", with a certificate made
                     on the spot. Its authority is written to DIR/ca.der, and
                     an authority that signed nothing to DIR/stranger.der
    --call           the device calls on a second connection, with app_id=hatch-web
                     in its address. The call is played through: readiness, the
                     device's talk, a reply held for its turn, then Muse's goodbye.
                     The link session waits for the call in place of chats
    --call-barge     the device presses again while the first reply plays, and
                     hangs up after the second
    --call-reject    the call's media request is answered 403
    --call-dead      the call's session dies just after readiness
"""

import argparse
import asyncio
import base64
import hashlib
import json
import os
import datetime
import pathlib
import re
import ssl
import struct
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
SDK = pathlib.Path(os.environ.get("MUSE_GADGET_SDK") or ROOT / "vendor" / "muse-gadget-sdk")
if not (SDK / "linux" / "src" / "musegadget").is_dir():
    sys.exit("Muse Gadget SDK not found at %s. Clone it there or set MUSE_GADGET_SDK." % SDK)
sys.path.insert(0, str(SDK / "linux" / "src"))

from websockets.asyncio.server import serve  # noqa: E402
from websockets.exceptions import ConnectionClosed  # noqa: E402

from musegadget.link_client import MessageDecoder, encode_message  # noqa: E402
from musegadget.noise import (  # noqa: E402
    ApplicationResponse, BodyChunk, NoiseFrameDecoder, NoiseXXResponder, ServiceFrame,
    encode_noise_frames,
)
from musegadget.noise.transport import decode_request_envelope, encode_response_envelope  # noqa: E402

parser = argparse.ArgumentParser()
parser.add_argument("--voice", action="store_true")
parser.add_argument("--dead", action="store_true")
parser.add_argument("--forbid", type=int, default=0)
parser.add_argument("--chat-invoke", action="store_true")
parser.add_argument("--reject-register", action="store_true")
parser.add_argument("--tls")
parser.add_argument("--call", action="store_true")
parser.add_argument("--call-barge", action="store_true")
parser.add_argument("--call-reject", action="store_true")
parser.add_argument("--call-dead", action="store_true")
args = parser.parse_args()

BIG_TEXT = "badge " * 20000  # 120 KB, so the invoke spans two Noise chunks
EXPECTED_ASKS = 3 if args.voice else 2
DEAD_SECONDS = 70
CALL = args.call or args.call_barge or args.call_reject or args.call_dead
CALL_RATE = 24000
SAMPLES_PER_TENTH_SECOND = CALL_RATE // 10
FIRST_REPLY, SECOND_REPLY = 1111, 2222
transcript = {"api": [], "vm": [], "dead_vm": [], "chats": [], "subscribes": 0,
              "connections": 0, "voice": None, "knocks": [], "chat_invoke_result": None, "call": None,
              "invoke_during_call": None}
done = asyncio.Event()
call_open = asyncio.Event()
call_done = asyncio.Event()


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
            hints = {"retry_after_ms": 150, "max_retry_count": 5} if args.forbid else {}
            status, payload = 200, {**hints, "vm_list": [
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

    async def raw_frame(self):
        """The next whole frame the device sent."""
        while True:
            plain = self.recv_cipher.decrypt_with_ad(b"", await self.ws.recv())
            assembled = self.decoder.decode(plain)
            if assembled is not None:
                return decode_request_envelope(assembled)

    async def next_frame(self):
        """The next frame the script cares about; side requests are answered here."""
        while True:
            frame = await self.raw_frame()
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
                # The stored message of a voice note ends with a line naming the file.
                stored = heard + "\n[file:audio/wav workspace/user/files/voice_note-64.wav]" if body.get("items") else heard
                await self.stream_reply(user_id, "you said: " + heard, stored)
                if args.chat_invoke and len(transcript["chats"]) == 1:
                    await self.send_frame(ServiceFrame.body_chunk(self.subscription, BodyChunk(data=json.dumps({
                        "type": "event", "event": "client.invoke", "ts_ms": 1, "payload": {
                            "command_id": "badge.show_message", "invoke_id": "inv-chat",
                            "params_json": json.dumps({"text": "from the chat stream"}), "timeout_ms": 30000},
                    }).encode() + b"\n")))
        await self.send_frame(ServiceFrame.response(stream_id, ApplicationResponse(
            status=200, body=json.dumps(reply).encode(), end_body=True)))

    async def stream_reply(self, user_id, text, stored):
        def line(seq, name, payload):
            return json.dumps({"type": "event", "seq": seq, "event": name, "payload": payload}).encode() + b"\n"

        # The shapes the real service sends: the reply names no parent, its
        # text chunks name themselves as parent, and "done" carries no text.
        reply_id = "assistant-msg-" + user_id
        half = len(text) // 2
        data = b"".join([
            line(1, "delta.message_start", {"message_id": "earlier", "reply_to_message_id": ""}),
            line(2, "delta.text_append", {"message_id": "earlier", "text": "not for this badge"}),
            line(3, "message.user", {"message_id": user_id, "display_text": stored}),
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
            for message in self.messages.feed(frame.value.data):
                if message.get("id") == "inv-chat":
                    transcript["chat_invoke_result"] = message
                else:
                    return message

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


def ndjson(*events):
    return b"".join(json.dumps(event, separators=(",", ":")).encode() + b"\n" for event in events)


def voice(sample, samples, response_id=None):
    """Muse's voice as the VM sends it, every sample the same."""
    event = {"type": "continuous_audio_video.delta", "audio_delta": {
        "audio_frames": base64.b64encode(struct.pack("<h", sample) * samples).decode(),
        "format": {"type": "", "rate": 24000}, "num_channels": 1, "num_frames": samples}}
    if response_id:
        event["response_id"] = response_id
    return event


KEEPALIVE = voice(0, 960)


def speech(what, item):
    return {"type": "input_audio_buffer.speech_" + what, "item_id": item}


def response(what, response_id, status=None):
    return {"type": "response." + what, "response": {"id": response_id, **({"status": status} if status else {})}}


def turn(submission, item):
    return {"type": "hatch.voice.turn_started", "submission_message_id": submission, "item_id": item}


def settled(submission):
    return {"type": "hatch.voice.turn_settled", "response_message_id": submission}


class Call:
    """The VM's side of one realtime voice call, and a record of what the device did."""

    def __init__(self, vm):
        self.vm = vm
        self.media = None
        self.end_call = None
        self.ready = False
        self.goodbye_at = None
        self.log = {
            "path": vm.ws.request.path, "authorization": vm.ws.request.headers.get("Authorization"),
            "order": [], "media_request": None, "admission": None, "offer": None,
            # One letter per append: z for exact zeros, L for anything else.
            "appends": "", "early_appends": 0, "bad_appends": 0, "after_end_call": 0, "commits": 0,
            "cancels": [], "end_calls": [], "half_close": None, "resets": [], "unexpected": [],
            "sent": {"p1": 0, "p2": 0, "stale": 0}, "error": None,
            # Seconds from the first append to the last, and from Muse's goodbye to end-call.
            "appending": None, "goodbye_to_end_call": None,
        }
        self.first_append_at = None

    def mark(self, milestone):
        self.log["order"].append(milestone)

    async def read(self):
        """Records every frame the device sends, until it closes."""
        try:
            while True:
                frame = await self.vm.raw_frame()
                if frame.kind == "request":
                    self.on_request(frame)
                elif frame.kind == "body_chunk" and frame.stream_id == self.media:
                    for line in frame.value.data.splitlines():
                        self.on_record(json.loads(line))
                    if frame.value.end_body:
                        self.log["half_close"] = len(frame.value.data)
                        self.mark("half_close")
                elif frame.kind == "reset":
                    self.log["resets"].append(frame.stream_id)
                else:
                    self.log["unexpected"].append("%s on stream %d" % (frame.kind, frame.stream_id))
        except ConnectionClosed:
            pass
        except Exception as error:
            self.log["error"] = "reading: %r" % error

    def on_request(self, frame):
        request = frame.value
        headers = {header.key: header.value for header in request.headers}
        if request.path.startswith("/voice/realtime?") and self.media is None:
            self.media = frame.stream_id
            self.log["media_request"] = {"path": request.path, "headers": headers,
                                         "body": request.body.decode(), "end_body": request.end_body}
            self.mark("media_request")
        elif request.path == "/voice/end-call":
            self.end_call = frame.stream_id
            if self.goodbye_at is not None:
                self.log["goodbye_to_end_call"] = time.monotonic() - self.goodbye_at
            self.log["end_calls"].append({"headers": headers, "body": json.loads(request.body),
                                          "end_body": request.end_body})
            self.mark("end_call")
        else:
            self.log["unexpected"].append("request " + request.path)

    def on_record(self, record):
        kind = record.get("type")
        if kind == "input_audio_buffer.append":
            pcm = base64.b64decode(record["audio"], validate=True)
            self.log["bad_appends"] += len(pcm) != 1920
            self.log["early_appends"] += not self.ready
            self.log["after_end_call"] += self.end_call is not None
            if not self.log["appends"]:
                self.mark("append")
                self.first_append_at = time.monotonic()
            self.log["appends"] += "L" if any(pcm) else "z"
            self.log["appending"] = time.monotonic() - self.first_append_at
        elif kind == "input_audio_buffer.commit":
            self.log["commits"] += 1
        elif kind == "response.cancel":
            self.log["cancels"].append(record.get("response_id"))
            self.mark("cancel")
        elif kind == "hatch.voice.call.start" and self.log["admission"] is None:
            self.log["admission"] = record
            self.mark("admission")
        elif kind == "hatch.voice.capability.offer" and self.log["offer"] is None:
            self.log["offer"] = record
            self.mark("offer")
        else:
            self.log["unexpected"].append(record)

    async def until(self, ready, what, seconds=20):
        for _ in range(int(seconds / 0.05)):
            if ready():
                return
            await asyncio.sleep(0.05)
        raise TimeoutError("no " + what)

    def talked(self, times):
        """The device has talked this many times, and gone quiet after the last."""
        return len(re.findall(r"L+z{5}", self.log["appends"])) >= times

    async def send(self, *events):
        """Records down the media stream, in body chunks that cut lines anywhere."""
        data = ndjson(*events)
        for start in range(0, len(data), 4000):
            await self.vm.send_frame(ServiceFrame.body_chunk(self.media, BodyChunk(data=data[start:start + 4000])))

    async def respond(self, stream, status, body=b"", end=False):
        await self.vm.send_frame(ServiceFrame.response(stream, ApplicationResponse(
            status=status, body=body, end_body=end)))

    def reply(self, name, response_id, sample, samples):
        """A reply in records of 0.1 s, counted as sent."""
        self.log["sent"][name] += samples
        tenth = SAMPLES_PER_TENTH_SECOND
        return [voice(sample, min(tenth, samples - start), response_id) for start in range(0, samples, tenth)]

    async def answer_end_call(self):
        """Answers end-call a moment late, so a half-close sent before the answer would show."""
        await self.until(lambda: self.end_call is not None, "end-call", 15)
        await asyncio.sleep(0.3)
        await self.respond(self.end_call, 200, b'{"ok":true,"result":{"ok":true}}', end=True)
        self.mark("end_call_answered")

    async def close_down(self):
        """end-call, then the device's half-close, then the end of the media response."""
        await self.answer_end_call()
        await self.until(lambda: self.log["half_close"] is not None, "half-close", 10)
        await self.vm.send_frame(ServiceFrame.body_chunk(self.media, BodyChunk(data=b"", end_body=True)))
        self.mark("media_ended")

    async def script(self):
        await self.until(lambda: self.log["offer"], "admission and offer")
        if args.call_reject:
            await self.respond(self.media, 403, b'{"error":"not now"}', end=True)
            self.mark("head")
            return await self.answer_end_call()
        await self.respond(self.media, 200)
        self.mark("head")
        # Zeros before readiness, and after it before any real audio, are keepalives.
        await self.send({"type": "session.created"}, KEEPALIVE, KEEPALIVE)
        await asyncio.sleep(0.5)
        self.ready = True
        await self.send({"type": "hatch.voice.session", "call_id": self.log["admission"]["call_id"],
                         "session_id": "s1", "thread_id": "t1"}, KEEPALIVE, KEEPALIVE)
        self.mark("session")
        if args.call_dead:
            await asyncio.sleep(0.1)
            await self.send({"type": "error", "error": {"code": "session_dead", "message": "synthetic"}})
            return await self.close_down()
        await self.until(lambda: self.talked(1), "talk")
        # The start of the reply comes before the turn that confirms it, so it is held.
        await self.send(speech("started", "i1"), speech("stopped", "i1"), response("created", "r1"),
                        *self.reply("p1", "r1", FIRST_REPLY, 4 * SAMPLES_PER_TENTH_SECOND), turn("m1", "i1"))
        if args.call_barge:
            return await self.barge()
        await self.send(*self.reply("p1", "r1", FIRST_REPLY, 15 * SAMPLES_PER_TENTH_SECOND), KEEPALIVE,
                        response("done", "r1"), settled("m1"), {"type": "conversation.item.added", "item": {
                            "type": "session_control", "name": " STOP ", "call_id": "ctl-9", "arguments": "{}"}})
        self.goodbye_at = time.monotonic()
        await self.close_down()

    async def barge(self):
        """The first reply plays in real time until the device interrupts it."""
        for _ in range(100):
            if self.log["cancels"]:
                break
            await self.send(*self.reply("p1", "r1", FIRST_REPLY, SAMPLES_PER_TENTH_SECOND))
            await asyncio.sleep(0.1)
        else:
            raise TimeoutError("no cancel")
        await self.send(*self.reply("stale", "r1", FIRST_REPLY, 3 * SAMPLES_PER_TENTH_SECOND),
                        response("done", "r1", "cancelled"), settled("m1"), speech("started", "i2"))
        await self.until(lambda: self.talked(2), "second talk")
        await self.send(speech("stopped", "i2"), response("created", "r2"),
                        *self.reply("p2", "r2", SECOND_REPLY, 3 * SAMPLES_PER_TENTH_SECOND), turn("m2", "i2"),
                        *self.reply("p2", "r2", SECOND_REPLY, 6 * SAMPLES_PER_TENTH_SECOND), response("done", "r2"),
                        settled("m2"))
        await self.close_down()


async def call_handler(ws):
    vm = Vm(ws)
    call = Call(vm)
    transcript["call"] = call.log
    reader = None
    try:
        await vm.handshake()
        call_open.set()
        reader = asyncio.create_task(call.read())
        await asyncio.wait_for(call.script(), 60)
        await asyncio.wait_for(ws.wait_closed(), 15)
        call.mark("closed")
    except Exception as error:
        call.log["error"] = call.log["error"] or repr(error)
    finally:
        if reader and not reader.done():
            reader.cancel()
        call_done.set()


async def vm_handler(ws):
    if "app_id=hatch-web" in ws.request.path:
        return await call_handler(ws)
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
    if args.reject_register:
        await vm.send_message({"type": "res", "id": register["id"], "ok": False, "error": "not welcome"})
        await ws.wait_closed()
        return
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

    if CALL:
        await asyncio.wait_for(call_open.wait(), 30)
        await vm.send_message({"method": "link.invoke", "id": "inv-call", "command": "badge.show_message",
                               "params": {"text": "on a call"}})
        transcript["invoke_during_call"] = {"result": await vm.next_message(), "call_up": not call_done.is_set()}
        await asyncio.wait_for(call_done.wait(), 90)
    else:
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
    transcript["knocks"].append(time.monotonic())
    if len(transcript["knocks"]) <= args.forbid:
        return connection.respond(403, "not yet\n")
    if request.headers.get("Authorization") != "Bearer vmtok":
        transcript["vm"].append({"rejected": request.headers.get("Authorization")})
        return connection.respond(401, "no\n")
    return None


def make_tls(directory):
    from cryptography import x509
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import ec
    from cryptography.x509.oid import ExtendedKeyUsageOID, NameOID

    directory = pathlib.Path(directory)
    now = datetime.datetime.now(datetime.timezone.utc)

    def certificate(name, key, issuer_name, issuer_key, authority):
        builder = (x509.CertificateBuilder()
                   .subject_name(name).issuer_name(issuer_name).public_key(key.public_key())
                   .serial_number(x509.random_serial_number())
                   .not_valid_before(now - datetime.timedelta(minutes=5))
                   .not_valid_after(now + datetime.timedelta(days=1))
                   .add_extension(x509.BasicConstraints(ca=authority, path_length=None), critical=True))
        if not authority:
            builder = (builder
                       .add_extension(x509.SubjectAlternativeName([x509.DNSName("localhost")]), critical=False)
                       .add_extension(x509.ExtendedKeyUsage([ExtendedKeyUsageOID.SERVER_AUTH]), critical=False))
        return builder.sign(issuer_key, hashes.SHA256())

    def authority(common_name):
        key = ec.generate_private_key(ec.SECP256R1())
        name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, common_name)])
        return key, name, certificate(name, key, name, key, True)

    ca_key, ca_name, ca = authority("fake Muse authority")
    _, _, stranger = authority("an authority that signed nothing")
    key = ec.generate_private_key(ec.SECP256R1())
    leaf = certificate(x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "localhost")]), key, ca_name, ca_key, False)
    (directory / "ca.der").write_bytes(ca.public_bytes(serialization.Encoding.DER))
    (directory / "stranger.der").write_bytes(stranger.public_bytes(serialization.Encoding.DER))
    chain = directory / "server.pem"
    chain.write_bytes(leaf.public_bytes(serialization.Encoding.PEM) + key.private_bytes(
        serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()))
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(chain)
    return context


async def main():
    tls = make_tls(args.tls) if args.tls else None
    api = await asyncio.start_server(api_handler, "127.0.0.1", 0, ssl=tls)
    # A dead peer sends nothing either, so its own keepalive pings are off.
    keepalive = {"ping_interval": None} if args.dead else {}
    vm = await serve(vm_handler, "127.0.0.1", 0, process_request=check_bearer, max_size=None,
                     ssl=tls, **keepalive)
    print(json.dumps({"api": api.sockets[0].getsockname()[1],
                      "vm": vm.sockets[0].getsockname()[1]}), flush=True)
    try:
        await asyncio.wait_for(done.wait(), 150)
    finally:
        print(json.dumps(transcript), flush=True)


asyncio.run(main())
