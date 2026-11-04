//! ACX v1 binary format (`docs/FORMAT_V1.md`).
//!
//! No codec or filesystem-walking dependencies: callers decode payloads
//! with `acx-codecs` and then call [`check_decoded`].

mod archive;
mod block;
mod fixed;
mod le;
mod manifest;
mod record;

pub use archive::{ArchiveReader, ArchiveWriter, MAX_METADATA_LEN, read_block_record};
pub use block::{
    BLOCK_HEAD_LEN, BLOCK_RECORD_HEAD_LEN, INDEX_ENTRY_LEN, INDEX_SCHEMA_VERSION, IndexEntry,
    block_record_head, check_decoded, decode_index, encode_index, read_block,
};
pub use fixed::{
    FOOTER_LEN, FOOTER_MAGIC, Footer, HEADER_LEN, HEADER_MAGIC, Header, Span, VERSION_MAJOR,
    VERSION_MINOR,
};
pub use manifest::{
    BlockRef, EntryKind, MANIFEST_SCHEMA_VERSION, ManifestEntry, decode_manifest, encode_manifest,
    validate_path,
};
pub use record::{FLAG_OPTIONAL, FRAME_LEN, Frame, RecordType};
