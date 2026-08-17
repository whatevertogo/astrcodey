"""Frame codec tests (mirrors wire::frame invariants)."""

from __future__ import annotations

import asyncio
import unittest

from s5r.errors import FrameError
from s5r.frames import (
    MAX_FRAME_BYTES,
    MAX_FRAME_HEADER_BYTES,
    StdioTransport,
    encode_frame,
    parse_frame_header,
)


def _reader_transport(data: bytes) -> StdioTransport:
    reader = asyncio.StreamReader()
    reader.feed_data(data)
    reader.feed_eof()
    return StdioTransport(reader, None, None)  # type: ignore[arg-type]


class FrameHeaderTest(unittest.TestCase):
    def test_round_trip(self) -> None:
        payload = b'{"type":"result"}'
        frame = encode_frame(payload)
        header, body = frame.split(b"\n", 1)
        self.assertEqual(parse_frame_header(header), len(payload))
        self.assertEqual(body, payload)

    def test_zero_length_is_valid(self) -> None:
        self.assertEqual(parse_frame_header(b"0"), 0)

    def test_rejects_empty_signed_and_spaced_headers(self) -> None:
        for header in (b"", b"+12", b" 12", b"12 "):
            with self.assertRaises(FrameError, msg=header):
                parse_frame_header(header)

    def test_rejects_oversized_and_overlong_headers(self) -> None:
        with self.assertRaises(FrameError):
            parse_frame_header(str(MAX_FRAME_BYTES + 1).encode())
        with self.assertRaises(FrameError):
            parse_frame_header(b"1" * (MAX_FRAME_HEADER_BYTES + 1))

    def test_encode_rejects_oversized_payload(self) -> None:
        with self.assertRaises(FrameError):
            encode_frame(b"x" * (MAX_FRAME_BYTES + 1))


class FrameReadTest(unittest.IsolatedAsyncioTestCase):
    async def test_read_frame(self) -> None:
        transport = _reader_transport(b"5\nhello2\nhi")
        self.assertEqual(await transport.read_frame(), b"hello")
        self.assertEqual(await transport.read_frame(), b"hi")
        with self.assertRaises(EOFError):
            await transport.read_frame()

    async def test_eof_mid_payload_is_eof(self) -> None:
        transport = _reader_transport(b"5\nhe")
        with self.assertRaises(EOFError):
            await transport.read_frame()

    async def test_rejects_bad_header(self) -> None:
        transport = _reader_transport(b"+5\nhello")
        with self.assertRaises(FrameError):
            await transport.read_frame()

    async def test_rejects_overlong_header(self) -> None:
        transport = _reader_transport(b"1" * (MAX_FRAME_HEADER_BYTES + 1) + b"\n")
        with self.assertRaises(FrameError):
            await transport.read_frame()


if __name__ == "__main__":
    unittest.main()
