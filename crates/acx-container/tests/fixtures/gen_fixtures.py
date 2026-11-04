"""Writes the v1 golden fixtures straight from docs/FORMAT_V1.md.

Independent of the Rust code (own CRC32C, own layout) so the fixtures
check the spec, not the encoder. Rerun only for a new format version;
released fixtures must never change.
"""

import struct
from pathlib import Path


def crc32c(data: bytes) -> int:
    crc = 0xFFFFFFFF
    for byte in data:
        crc ^= byte
        for _ in range(8):
            crc = (crc >> 1) ^ (0x82F63B78 if crc & 1 else 0)
    return crc ^ 0xFFFFFFFF


assert crc32c(b"123456789") == 0xE3069283


def seal(body: bytes) -> bytes:
    assert len(body) == 56
    return body + struct.pack("<II", crc32c(body), 0)


# Header: magic, major 1, minor 0, size 64, required 0, optional 0, 32 reserved.
header = seal(b"ACXARCH1" + struct.pack("<HHIII", 1, 0, 64, 0, 0) + bytes(32))

# Footer for a 176-byte archive with no blocks: manifest [64, 88), index [88, 112).
footer = seal(
    b"ACXEND01"
    + struct.pack("<QQQQQII", 64, 24, 88, 24, 176, 0x11223344, 0x55667788)
)

# RAW block 0 holding b"hello" at offset 64. The digest is a stand-in
# (bytes 0..31): Python has no BLAKE3, and these fixtures test layout only.
payload = b"hello"
digest = bytes(range(32))
block_fields = (
    struct.pack("<IHHQQ", 0, 0x0000, 0, len(payload), len(payload))
    + digest
    + struct.pack("<II", crc32c(payload), 0)
)
assert len(block_fields) == 64
block = b"ACXB" + struct.pack("<HHQ", 0x0002, 0, len(block_fields) + len(payload))
block += block_fields + payload

# Index payload (frame excluded) describing that block.
index = struct.pack("<II", 1, 1) + struct.pack(
    "<IHHQQQQ", 0, 0x0000, 0, 64, len(block), len(payload), len(payload)
) + digest
assert len(index) == 8 + 72


# Manifest payload: file "hello.txt" (5 bytes in block 0), then directory "docs".
def entry(file_id, kind, mode, size, mtime, path, digest, refs):
    p = path.encode()
    head = struct.pack("<IBBHIQqHH", file_id, kind, 1, 0, mode, size, mtime, len(p), 0)
    tail = digest + struct.pack("<I", len(refs))
    tail += b"".join(struct.pack("<IQQ", *r) for r in refs)
    body = head + p + tail
    return struct.pack("<I", 4 + len(body)) + body


manifest = struct.pack("<II", 1, 2)
manifest += entry(0, 1, 0o644, 5, 1_700_000_000_123_456_789, "hello.txt", digest, [(0, 0, 5)])
manifest += entry(1, 2, 0xFFFFFFFF, 0, -(2**63), "docs", bytes(32), [])

out = Path(__file__).parent
for name, data in [
    ("manifest_v1.bin", manifest),
    ("header_v1.bin", header),
    ("footer_v1.bin", footer),
    ("block_raw_v1.bin", block),
    ("index_v1.bin", index),
]:
    (out / name).write_bytes(data)
    print(name, data.hex())
