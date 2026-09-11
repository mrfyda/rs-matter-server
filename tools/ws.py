#!/usr/bin/env python3
"""Drive a running rs-matter-server from a shell.

Hardware testing needs a client that can send one command and show exactly
what came back and what the server pushed afterwards. Home Assistant is the
wrong instrument for that — it hides the wire — and a general WebSocket tool
does not know the protocol's envelope. This speaks it: the `server_info`
greeting, the `{message_id, command, args}` request, the `{message_id, result}`
or `{message_id, error_code, details}` reply, and the `{event, data}` stream
that follows `start_listening`.

No dependencies, on purpose. It is meant to be copied onto whatever machine is
next to the device — a Pi, a laptop, the container host — and run against a
stock Python.

Usage:

    # Ask one thing and exit.
    tools/ws.py server_info
    tools/ws.py get_node node_id=1

    # Watch the event stream until Ctrl-C.
    tools/ws.py --listen

    # Listen first, then issue a command, so the events it causes are seen.
    # This is the shape almost every hardware check wants.
    tools/ws.py --listen interview_node node_id=1
    tools/ws.py --listen --for 60 device_command node_id=1 endpoint_id=1 \
        cluster_id=6 command_name=toggle

    # Count events by name instead of printing them, for a command that
    # produces hundreds.
    tools/ws.py --listen --count interview_node node_id=1

Arguments are `key=value`. The value is parsed as JSON when it parses, and
taken as a string when it does not, so `node_id=1` is a number, `force=true` is
a boolean, `payload={"level":128}` is an object, and `command_name=toggle` is a
string. `--arg-json` takes a whole args object at once for anything awkward.

Every line is prefixed with milliseconds since the connection opened, which is
how latency claims in PARITY.md were measured.
"""

import argparse
import base64
import json
import os
import socket
import struct
import sys
import time
from urllib.parse import urlparse

DEFAULT_URL = "ws://127.0.0.1:5580/ws"

# Opcodes this client understands. Anything else is a protocol error the
# server should not be producing.
OP_CONTINUATION = 0x0
OP_TEXT = 0x1
OP_BINARY = 0x2
OP_CLOSE = 0x8
OP_PING = 0x9
OP_PONG = 0xA


class WebSocket:
    """The minimum client-side RFC 6455 needed to hold this conversation.

    Text frames only in the useful direction: the protocol is JSON, so a
    binary frame would be a bug worth seeing rather than something to decode.
    """

    def __init__(self, url, timeout):
        parsed = urlparse(url)
        if parsed.scheme != "ws":
            raise SystemExit(
                f"{url}: only ws:// is supported (the server does not terminate TLS)"
            )
        host = parsed.hostname or "127.0.0.1"
        port = parsed.port or 80
        path = parsed.path or "/"

        self.sock = socket.create_connection((host, port), timeout=timeout)
        self.buffer = b""
        self._handshake(host, port, path)

    def _handshake(self, host, port, path):
        key = base64.b64encode(os.urandom(16)).decode()
        request = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n"
            "\r\n"
        )
        self.sock.sendall(request.encode())

        # Read exactly the response headers; anything after them is the first
        # frame and must stay in the buffer.
        while b"\r\n\r\n" not in self.buffer:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise SystemExit("the server closed the connection during the handshake")
            self.buffer += chunk
        headers, self.buffer = self.buffer.split(b"\r\n\r\n", 1)
        status = headers.split(b"\r\n", 1)[0].decode(errors="replace")
        if "101" not in status:
            raise SystemExit(f"the server refused the upgrade: {status}")

    def set_timeout(self, timeout):
        """`None` blocks forever, which is what an open-ended listen wants."""
        self.sock.settimeout(timeout)

    def _recv_exactly(self, count):
        while len(self.buffer) < count:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError("the server closed the connection")
            self.buffer += chunk
        taken, self.buffer = self.buffer[:count], self.buffer[count:]
        return taken

    def send_text(self, text):
        payload = text.encode()
        header = bytearray([0x80 | OP_TEXT])
        # A client must mask; the mask itself is not secrecy, it is a proxy
        # cache-poisoning defence the protocol requires.
        length = len(payload)
        if length < 126:
            header.append(0x80 | length)
        elif length < 1 << 16:
            header.append(0x80 | 126)
            header += struct.pack("!H", length)
        else:
            header.append(0x80 | 127)
            header += struct.pack("!Q", length)
        mask = os.urandom(4)
        masked = bytes(byte ^ mask[i % 4] for i, byte in enumerate(payload))
        self.sock.sendall(bytes(header) + mask + masked)

    def recv_text(self):
        """The next text message, or None once the server closes."""
        message = b""
        while True:
            first, second = self._recv_exactly(2)
            final = bool(first & 0x80)
            opcode = first & 0x0F
            length = second & 0x7F
            if length == 126:
                (length,) = struct.unpack("!H", self._recv_exactly(2))
            elif length == 127:
                (length,) = struct.unpack("!Q", self._recv_exactly(8))
            # A server frame is never masked; if one is, the length just read
            # is wrong and nothing after it can be trusted.
            if second & 0x80:
                raise ConnectionError("the server sent a masked frame")
            payload = self._recv_exactly(length)

            if opcode == OP_CLOSE:
                return None
            if opcode == OP_PING:
                self._send_control(OP_PONG, payload)
                continue
            if opcode == OP_PONG:
                continue
            if opcode == OP_BINARY:
                raise ConnectionError("the server sent a binary frame; the protocol is JSON")

            message += payload
            if final:
                return message.decode(errors="replace")

    def _send_control(self, opcode, payload):
        mask = os.urandom(4)
        masked = bytes(byte ^ mask[i % 4] for i, byte in enumerate(payload))
        self.sock.sendall(bytes([0x80 | opcode, 0x80 | len(payload)]) + mask + masked)

    def close(self):
        try:
            self._send_control(OP_CLOSE, b"")
        except OSError:
            pass
        self.sock.close()


def parse_value(text):
    """`key=value` values are JSON where they can be, strings otherwise."""
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return text


def parse_args_pairs(pairs, arg_json):
    args = {}
    if arg_json:
        args = json.loads(arg_json)
        if not isinstance(args, dict):
            raise SystemExit("--arg-json must be a JSON object")
    for pair in pairs:
        if "=" not in pair:
            raise SystemExit(f"{pair}: arguments are key=value")
        key, _, value = pair.partition("=")
        args[key] = parse_value(value)
    return args


class Printer:
    """Output, timestamped from the moment the connection opened."""

    def __init__(self, started, counting):
        self.started = started
        self.counting = counting
        self.counts = {}

    def elapsed_ms(self):
        return (time.monotonic() - self.started) * 1000

    def line(self, marker, text):
        print(f"[{self.elapsed_ms():9.1f}ms] {marker} {text}", flush=True)

    def event(self, name, data):
        if self.counting:
            self.counts[name] = self.counts.get(name, 0) + 1
            return
        self.line("<<", f"{name} {json.dumps(data, separators=(',', ':'))}")

    def report_counts(self):
        if not self.counting or not self.counts:
            return
        total = sum(self.counts.values())
        print(f"\n{total} event{'' if total == 1 else 's'}:", flush=True)
        for name, count in sorted(self.counts.items(), key=lambda item: -item[1]):
            print(f"  {count:6}  {name}", flush=True)


def main():
    parser = argparse.ArgumentParser(
        description="Send one command to rs-matter-server and watch what follows.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__.split("Usage:", 1)[1] if "Usage:" in __doc__ else None,
    )
    parser.add_argument("--url", default=os.environ.get("RS_MATTER_WS", DEFAULT_URL),
                        help=f"server WebSocket URL (default {DEFAULT_URL}, or $RS_MATTER_WS)")
    parser.add_argument("--listen", action="store_true",
                        help="issue start_listening before the command, and stream events after it")
    parser.add_argument("--for", dest="follow_for", type=float, default=None, metavar="SECONDS",
                        help="how long to keep streaming after the response "
                             "(default: forever with --listen, otherwise exit at once)")
    parser.add_argument("--count", action="store_true",
                        help="tally events by name instead of printing each one")
    parser.add_argument("--arg-json", metavar="JSON",
                        help="the whole args object, for anything key=value cannot express")
    parser.add_argument("--timeout", type=float, default=30.0,
                        help="socket timeout in seconds (default 30)")
    parser.add_argument("command", nargs="?", help="the command to send")
    parser.add_argument("pairs", nargs="*", metavar="key=value", help="command arguments")
    options = parser.parse_args()

    if not options.command and not options.listen:
        parser.error("give a command, --listen, or both")

    args = parse_args_pairs(options.pairs, options.arg_json)
    started = time.monotonic()
    printer = Printer(started, options.count)

    websocket = WebSocket(options.url, options.timeout)
    message_id = 0
    pending = None
    sent_listen = False
    response_at = None

    def send(command, command_args):
        nonlocal message_id
        message_id += 1
        this_id = str(message_id)
        websocket.send_text(json.dumps({
            "message_id": this_id,
            "command": command,
            "args": command_args,
        }))
        printer.line(">>", f"{command} {json.dumps(command_args, separators=(',', ':'))}")
        return this_id

    try:
        while True:
            # Once the command has answered, decide whether to keep reading.
            if pending is None and response_at is not None:
                if options.follow_for is not None:
                    if time.monotonic() - response_at >= options.follow_for:
                        break
                elif not options.listen:
                    break

            # Waiting for a reply is bounded by --timeout and a silence there
            # is a failure. Streaming afterwards is bounded by --for, or by
            # nothing at all, and running out of it is simply the end.
            if pending is not None or response_at is None:
                websocket.set_timeout(options.timeout)
                streaming = False
            elif options.follow_for is not None:
                remaining = options.follow_for - (time.monotonic() - response_at)
                if remaining <= 0:
                    break
                websocket.set_timeout(remaining)
                streaming = True
            else:
                websocket.set_timeout(None)
                streaming = True

            try:
                raw = websocket.recv_text()
            except (socket.timeout, TimeoutError):
                if streaming:
                    break
                printer.line("!!", f"nothing received for {options.timeout}s")
                break
            except ConnectionError as error:
                printer.line("!!", str(error))
                break
            if raw is None:
                printer.line("!!", "the server closed the connection")
                break

            try:
                frame = json.loads(raw)
            except json.JSONDecodeError:
                printer.line("??", raw[:200])
                continue

            # The unsolicited greeting: no message_id, no event name.
            if isinstance(frame, dict) and "schema_version" in frame and "event" not in frame:
                printer.line("--", f"server_info {json.dumps(frame, separators=(',', ':'))}")
                if options.listen and not sent_listen:
                    sent_listen = True
                    pending = send("start_listening", {})
                elif options.command and pending is None and response_at is None:
                    pending = send(options.command, args)
                continue

            if isinstance(frame, dict) and "event" in frame:
                printer.event(frame.get("event", "?"), frame.get("data"))
                continue

            if isinstance(frame, dict) and frame.get("message_id") == pending:
                pending = None
                if "error_code" in frame:
                    printer.line("!!", f"error {frame['error_code']}: {frame.get('details')}")
                    # A failed start_listening leaves nothing worth doing.
                    if not options.command:
                        break
                else:
                    result = frame.get("result")
                    printer.line("<<", json.dumps(result, indent=2, sort_keys=True)
                                 if not options.count else f"result ({type(result).__name__})")
                # start_listening answered; now the command it was setting up for.
                if sent_listen and options.command and response_at is None and pending is None \
                        and frame.get("message_id") == "1":
                    pending = send(options.command, args)
                    continue
                response_at = time.monotonic()
                continue

            printer.line("??", raw[:200])
    except KeyboardInterrupt:
        print(flush=True)
    finally:
        printer.report_counts()
        websocket.close()


if __name__ == "__main__":
    sys.exit(main())
