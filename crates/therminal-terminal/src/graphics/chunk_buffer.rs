//! Chunk buffer for multi-part Kitty graphics APC transmissions.
//!
//! The Kitty graphics protocol chunks large image payloads across multiple
//! APC strings using the `m=1` continuation flag. Each chunk keeps the
//! `(image_id, placement_id)` pair stable so the terminal can reassemble
//! the full base64-encoded payload.
//!
//! [`ChunkBuffer`] owns a per-`(image_id, placement_id)` accumulator. Calls
//! must follow the pattern:
//!
//! ```text
//! accept(key, header, payload, more=true)  // chunk 1..=N-1 (m=1)
//! accept(key, header, payload, more=false) // chunk N       (m=0 or missing)
//! ```
//!
//! A 64 MB hard cap applies **per-entry** (a single in-flight image). When an
//! append would push an entry past the cap, the entry is dropped and
//! [`ChunkError::Overflow`] is returned so the caller can emit an error
//! response to the client.

use std::collections::HashMap;

use super::RawGraphicsCommand;

/// Hard cap on a single entry's accumulated payload, in bytes.
///
/// 64 MB is well beyond any reasonable base64-encoded image; picked as a
/// safety bound against a client that never terminates a chunked transfer.
pub const CHUNK_BUFFER_HARD_CAP: usize = 64 * 1024 * 1024;

/// Per-image key used by [`ChunkBuffer`]. A Kitty graphics transmission is
/// uniquely identified by the pair of `(image_id, placement_id)`; both are
/// optional in the protocol, so we normalise missing values to `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkKey {
    pub image_id: u32,
    pub placement_id: u32,
}

impl ChunkKey {
    /// Build a key from a parsed command. Both ids default to `0` when the
    /// client omits `i=`/`p=`.
    pub fn from_command(cmd: &RawGraphicsCommand) -> Self {
        Self {
            image_id: cmd.image_id.unwrap_or(0),
            placement_id: cmd.placement_id.unwrap_or(0),
        }
    }
}

/// Errors produced by the chunk buffer.
#[derive(Debug, thiserror::Error)]
pub enum ChunkError {
    /// Appending this chunk would exceed [`CHUNK_BUFFER_HARD_CAP`] for the
    /// entry. The entry is dropped before this error is returned.
    #[error(
        "kitty graphics chunk buffer overflow for image {image_id}/placement {placement_id}: \
         attempted {attempted} bytes, cap is {cap}"
    )]
    Overflow {
        image_id: u32,
        placement_id: u32,
        attempted: usize,
        cap: usize,
    },
}

/// Accumulator for multi-chunk graphics transmissions.
#[derive(Debug, Default)]
pub struct ChunkBuffer {
    entries: HashMap<ChunkKey, Entry>,
}

#[derive(Debug)]
struct Entry {
    /// Header fields captured from the first chunk. Later chunks only carry
    /// payload + the `m=` flag, so we keep the initial command as the
    /// authoritative metadata source.
    header: RawGraphicsCommand,
    /// Accumulated raw (base64-encoded) payload bytes.
    payload: Vec<u8>,
}

impl ChunkBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of in-flight entries. Intended for diagnostics / tests.
    pub fn in_flight(&self) -> usize {
        self.entries.len()
    }

    /// Append one chunk. Returns `Ok(Some(completed))` when `more == false`
    /// (the transmission is complete and the caller should consume the
    /// reassembled buffer), `Ok(None)` when more chunks are expected, or
    /// `Err(ChunkError::Overflow)` when the cap would be exceeded.
    pub fn append(
        &mut self,
        key: ChunkKey,
        command: RawGraphicsCommand,
        payload: &[u8],
        more: bool,
    ) -> Result<Option<CompletedChunk>, ChunkError> {
        // Fast path: single-shot transmission (no existing entry, no more).
        if !more && !self.entries.contains_key(&key) {
            return Ok(Some(CompletedChunk {
                key,
                header: command,
                payload: payload.to_vec(),
            }));
        }

        let entry = self.entries.entry(key).or_insert_with(|| Entry {
            header: command.clone(),
            payload: Vec::new(),
        });

        let new_len = entry.payload.len().saturating_add(payload.len());
        if new_len > CHUNK_BUFFER_HARD_CAP {
            // Drop the entry so repeated overflowing writes don't pile up.
            self.entries.remove(&key);
            return Err(ChunkError::Overflow {
                image_id: key.image_id,
                placement_id: key.placement_id,
                attempted: new_len,
                cap: CHUNK_BUFFER_HARD_CAP,
            });
        }

        entry.payload.extend_from_slice(payload);

        if more {
            Ok(None)
        } else {
            let entry = self.entries.remove(&key).expect("just inserted");
            Ok(Some(CompletedChunk {
                key,
                header: entry.header,
                payload: entry.payload,
            }))
        }
    }

    /// Drop any in-flight entry for `key`. Used when the client restarts a
    /// transfer or explicitly aborts.
    pub fn abort(&mut self, key: ChunkKey) {
        self.entries.remove(&key);
    }

    /// Remove every in-flight entry (e.g. pane reset).
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// A fully reassembled chunk set, ready to be turned into a
/// [`crate::terminal::GraphicsEvent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedChunk {
    pub key: ChunkKey,
    pub header: RawGraphicsCommand,
    pub payload: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::{GraphicsAction, GraphicsFormat, GraphicsMedium};

    fn header(image_id: u32) -> RawGraphicsCommand {
        RawGraphicsCommand {
            action: GraphicsAction::Transmit,
            format: GraphicsFormat::Rgba,
            medium: GraphicsMedium::Direct,
            image_id: Some(image_id),
            ..RawGraphicsCommand::empty()
        }
    }

    #[test]
    fn single_chunk_short_circuits() {
        let mut buf = ChunkBuffer::new();
        let key = ChunkKey {
            image_id: 1,
            placement_id: 0,
        };
        let out = buf.append(key, header(1), b"abcd", false).unwrap();
        let done = out.expect("single-chunk transfer completes immediately");
        assert_eq!(done.payload, b"abcd");
        assert_eq!(buf.in_flight(), 0);
    }

    #[test]
    fn multi_chunk_reassembles_in_order() {
        let mut buf = ChunkBuffer::new();
        let key = ChunkKey {
            image_id: 7,
            placement_id: 0,
        };

        assert!(buf.append(key, header(7), b"AAA", true).unwrap().is_none());
        assert!(buf.append(key, header(7), b"BBB", true).unwrap().is_none());
        assert_eq!(buf.in_flight(), 1);

        let done = buf
            .append(key, header(7), b"CCC", false)
            .unwrap()
            .expect("final chunk completes");
        assert_eq!(done.payload, b"AAABBBCCC");
        assert_eq!(buf.in_flight(), 0);
    }

    #[test]
    fn overflow_drops_entry_and_errors() {
        let mut buf = ChunkBuffer::new();
        let key = ChunkKey {
            image_id: 9,
            placement_id: 0,
        };

        // First chunk: 1 byte under the cap.
        let first = vec![0u8; CHUNK_BUFFER_HARD_CAP - 1];
        assert!(buf.append(key, header(9), &first, true).unwrap().is_none());
        assert_eq!(buf.in_flight(), 1);

        // Second chunk: 2 bytes (total > cap) → overflow, entry dropped.
        let err = buf.append(key, header(9), &[0u8, 0u8], true).unwrap_err();
        match err {
            ChunkError::Overflow { image_id, .. } => assert_eq!(image_id, 9),
        }
        assert_eq!(buf.in_flight(), 0, "entry must be dropped on overflow");
    }

    #[test]
    fn abort_removes_entry() {
        let mut buf = ChunkBuffer::new();
        let key = ChunkKey {
            image_id: 3,
            placement_id: 0,
        };
        buf.append(key, header(3), b"x", true).unwrap();
        assert_eq!(buf.in_flight(), 1);
        buf.abort(key);
        assert_eq!(buf.in_flight(), 0);
    }

    #[test]
    fn distinct_keys_do_not_interfere() {
        let mut buf = ChunkBuffer::new();
        let a = ChunkKey {
            image_id: 1,
            placement_id: 0,
        };
        let b = ChunkKey {
            image_id: 2,
            placement_id: 0,
        };
        buf.append(a, header(1), b"AA", true).unwrap();
        buf.append(b, header(2), b"BB", true).unwrap();
        let done_a = buf
            .append(a, header(1), b"aa", false)
            .unwrap()
            .expect("a complete");
        let done_b = buf
            .append(b, header(2), b"bb", false)
            .unwrap()
            .expect("b complete");
        assert_eq!(done_a.payload, b"AAaa");
        assert_eq!(done_b.payload, b"BBbb");
    }
}
