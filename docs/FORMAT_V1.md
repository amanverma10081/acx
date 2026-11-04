# ACX Archive Format, Version 1

This is the canonical specification of the `.acx` v1 format. The PRD
(`docs/PRD.md` §4) only summarises it. Any change to a field here needs
a version bump (§10) and new golden fixtures.

## 1. Conventions

- All integers are little-endian and unsigned unless marked signed.
- `[a, b)` is a byte range that includes byte `a` and excludes byte `b`.
- Block IDs, file IDs, and counts are `u32`. Byte offsets, lengths, and
  sizes are `u64`.
- A record's **total length** is its 16-byte frame plus its payload.
- Record CRC32C values cover the payload only, not the frame.
- CRC32C is the Castagnoli CRC (reflected polynomial `0x82F63B78`,
  initial value and final XOR `0xFFFFFFFF`). Check value:
  `crc32c("123456789") = 0xE3069283`.
- All arithmetic on values read from an archive is checked. Overflow is
  an invalid archive, never a wrap or a panic.

## 2. File layout

```text
offset 0          Header                 64 bytes
offset 64         Data block record 0
                  ...
                  Data block record N-1
manifest.offset   Manifest record
index.offset      Index record
file_len - 64     Footer                 64 bytes
```

A single-pass writer produces this layout. The header holds no offsets;
the footer locates the manifest and the index. A file without a valid
footer is not an archive.

## 3. Header (64 bytes)

| Offset | Size | Field          | Value                              |
| -----: | ---: | -------------- | ---------------------------------- |
|      0 |    8 | Magic          | ASCII `ACXARCH1`                   |
|      8 |    2 | Major version  | `1`                                |
|     10 |    2 | Minor version  | `0` (readers accept any)           |
|     12 |    4 | Header size    | `64`                               |
|     16 |    4 | Required flags | `0`                                |
|     20 |    4 | Optional flags | `0`                                |
|     24 |   32 | Reserved       | all zero                           |
|     56 |    4 | Header CRC32C  | CRC32C of `[0, 56)`                |
|     60 |    4 | Reserved       | zero                               |

Validation order, each failure ending validation:

1. Magic mismatch → not an ACX archive (invalid archive).
2. Major version ≠ 1 → unsupported version. This is checked before the
   CRC so that a future major version is reported as such, not as
   corruption.
3. CRC mismatch → integrity failure.
4. Header size ≠ 64 → invalid archive.
5. Any required flag bit set → unsupported feature. v1 defines none.
6. Any reserved byte non-zero → invalid archive.

Optional flag bits are preserved on read and otherwise ignored. A minor
version only adds optional records (§5), so a v1.0 reader accepts any
minor version.

## 4. Footer (64 bytes)

| Offset | Size | Field                  | Value                     |
| -----: | ---: | ---------------------- | ------------------------- |
|      0 |    8 | Magic                  | ASCII `ACXEND01`          |
|      8 |    8 | Manifest offset        |                           |
|     16 |    8 | Manifest total length  |                           |
|     24 |    8 | Index offset           |                           |
|     32 |    8 | Index total length     |                           |
|     40 |    8 | Total archive length   | equals the file length    |
|     48 |    4 | Manifest CRC32C        | of the manifest payload   |
|     52 |    4 | Index CRC32C           | of the index payload      |
|     56 |    4 | Footer CRC32C          | CRC32C of `[0, 56)`       |
|     60 |    4 | Reserved               | zero                      |

The footer is read from `[file_len - 64, file_len)`. Validation order:

1. `file_len < 128` → invalid archive (no room for header and footer).
2. Magic mismatch → invalid archive.
3. CRC mismatch → integrity failure.
4. Reserved non-zero → invalid archive.
5. Total archive length ≠ `file_len` → invalid archive. Trailing data
   is not allowed in v1.
6. Manifest and index total lengths are each at least 16 (one frame).
7. `manifest.offset >= 64`.
8. `manifest.offset + manifest.len == index.offset`.
9. `index.offset + index.len == file_len - 64`.

Checks 7–9 also guarantee both records lie inside the file.

## 5. Record framing

Every variable-length record starts with a 16-byte frame:

| Offset | Size | Field          |
| -----: | ---: | -------------- |
|      0 |    4 | Record magic   |
|      4 |    2 | Record type    |
|      6 |    2 | Record flags   |
|      8 |    8 | Payload length |

| Type     | Magic  | Record                               |
| -------- | ------ | ------------------------------------ |
| `0x0001` | `ACXM` | Manifest                             |
| `0x0002` | `ACXB` | Data block                           |
| `0x0003` | `ACXI` | Index                                |
| `0x0004` | `ACXG` | Optional metadata (reserved, unused) |

Record flags: bit 0 = optional. A reader that does not know the record
type may skip the record by its payload length only if bit 0 is set;
otherwise it must fail. Other bits are reserved and must be zero.
Manifest, data block, and index records are required: their flags are
always zero.

The reader checks `16 + payload length` against the total length given
by the footer or index, and enforces its configured maximum record size
before allocating.

## 6. Data block payload

| Size | Field                       |
| ---: | --------------------------- |
|    4 | Block ID                    |
|    2 | Codec ID (§9)               |
|    2 | Block flags (zero in v1)    |
|    8 | Original size               |
|    8 | Compressed size             |
|   32 | BLAKE3 of decoded bytes     |
|    4 | CRC32C of encoded payload   |
|    4 | Reserved (zero)             |
|    N | Encoded payload             |

`N` equals the compressed size. For RAW blocks, compressed size equals
original size. Every block's original size is at most the archive's
block size (writer default 16 MiB, accepted range 64 KiB to 256 MiB);
the last block of a file may be shorter. Readers reject blocks whose
original or compressed size exceeds their configured maximum (default
256 MiB) before allocating.

The fields before the payload are not covered by a CRC. A reader
compares each of them with the block's index entry and treats any
mismatch as corruption of that block. Within the record, the block
fields occupy `[16, 80)` and the payload starts at offset 80.

Decoding a block yields exactly the original size in bytes, and its
BLAKE3 must equal the stored digest; anything else is an integrity
failure. Decoders never allocate more than the original size and never
return partial output.

## 7. Manifest payload

| Size | Field                          |
| ---: | ------------------------------ |
|    4 | Manifest schema version (`1`)  |
|    4 | Entry count                    |
|      | Entries                        |

Entry:

| Size | Field                                                            |
| ---: | ---------------------------------------------------------------- |
|    4 | Entry length in bytes, including this field                      |
|    4 | File ID (`0..count-1`, in manifest order)                        |
|    1 | Entry type: `1` file, `2` directory (`3` reserved for symlink)   |
|    1 | Path encoding: `1` UTF-8                                         |
|    2 | Entry flags (zero)                                               |
|    4 | Unix mode bits (`mode & 0o7777`), or `u32::MAX` if unavailable   |
|    8 | Original file size (`0` for directories)                         |
|    8 | Modified time, signed `i64` Unix ns, `i64::MIN` if unavailable   |
|    2 | Path byte length (`1..=65535`)                                   |
|    2 | Reserved (zero)                                                  |
|    N | UTF-8 relative path                                              |
|   32 | BLAKE3 of the whole file (zero for directories)                  |
|    4 | Reference count (`0` for directories and empty files)            |
|   20 | Per reference: Block ID (4), offset in block (8), length (8)     |

References are concatenated in order to rebuild the file. For each
reference `offset + length <= block original size`, and the sum of
lengths equals the file size. Several references may name the same
Block ID; this is the only dedup mechanism in v1. With fixed-size
blocks every offset is `0` and every reference spans its whole block.

Paths are stored exactly as scanned (never Unicode-normalized), use `/`
as separator, and must not be absolute, have a drive or UNC prefix,
contain empty, `.`, or `..` components, or contain NUL. Paths are unique
within a manifest. Parent directories need not have their own entries.
Extraction validates paths again, independently of manifest parsing.

## 8. Index payload

| Size | Field                         |
| ---: | ----------------------------- |
|    4 | Index schema version (`1`)    |
|    4 | Block count                   |
|   72 | Per block: see below          |

| Size | Field                       |
| ---: | --------------------------- |
|    4 | Block ID                    |
|    2 | Codec ID                    |
|    2 | Flags (zero)                |
|    8 | Record offset               |
|    8 | Record total length         |
|    8 | Original size               |
|    8 | Compressed size             |
|   32 | BLAKE3 of decoded bytes     |

Rules, each checked when the index is read:

- Entry `i` has Block ID `i`; flags are zero; the codec ID is known (§9).
- Original and compressed size are at most the reader's maximum block
  size (default 256 MiB), else a resource-limit error.
- RAW blocks have compressed size equal to original size.
- Record total length is `80 + compressed size` (frame 16, block fields
  64, payload).
- Blocks appear in index order without overlap: the first starts at or
  after offset 64, each starts at or after the end of the previous one,
  and the last ends at or before the manifest offset.

v1 writers place blocks back to back from offset 64 to the manifest.
Gaps are reserved for optional records (record flag bit 0) that a later
minor version may add; a v1.0 reader therefore does not reject gaps when
reading the index. A full verification walks `[64, manifest.offset)`
record by record and fails on any record that is neither a listed block
nor an optional record.

Every Block ID named in the manifest is less than the block count.

## 9. Codec IDs

| ID              | Codec                                  |
| --------------- | -------------------------------------- |
| `0x0000`        | RAW                                    |
| `0x0001`        | Zstandard (one complete frame)         |
| `0x0002`        | LZMA2 (reserved, P1)                   |
| `0x0100–0x7FFF` | Reserved for media codecs / transforms |
| `0x8000–0xFFFF` | Experimental                           |

IDs are never reused. An unknown ID is an unsupported-codec error, never
a fallback to another decoder.

## 10. Evolution

- A major version changes layouts incompatibly.
- A minor version may only add optional records (record flag bit 0).
- Reserved fields are zero in v1; never reinterpret a field silently.
- Golden fixtures for every released format live in
  `crates/acx-container/tests/fixtures/`.
