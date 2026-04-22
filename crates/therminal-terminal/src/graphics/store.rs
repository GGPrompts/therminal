//! Decoded-image cache for the Kitty graphics protocol (tn-0htm).
//!
//! The APC parser in [`super`] produces [`crate::terminal::GraphicsEvent`]
//! values carrying raw, *encoded* payload bytes (base64 for `t=d`, a file
//! path for `t=f` / `t=t`) plus the wire-level format flag. This module
//! turns those payloads into a flat `Vec<u8>` of pre-multiplied-opacity
//! RGBA8 pixels, caches them by [`ImageId`], and hands renderer tasks a
//! stable `Arc<DecodedImage>` handle to upload.
//!
//! ## Scope
//!
//! CPU-side only. The store owns the pixel buffer; it does **not** touch
//! wgpu. A placeholder [`TextureId`] field sits on every [`DecodedImage`]
//! so the future GPU upload hook (tn-wdn1) has somewhere to land, but
//! nothing in this module actually uploads.
//!
//! Placement logic (tn-0m3i) is also out of scope. The store answers
//! "what are the pixels for image id N?" — placements decide where to
//! draw a cached image.
//!
//! ## Decode matrix
//!
//! | `f=` flag | Source                               | Handling                |
//! |-----------|--------------------------------------|-------------------------|
//! | `f=24`    | raw RGB (3 bytes / px, row-major)    | pad to RGBA, α = 255    |
//! | `f=32`    | raw RGBA (4 bytes / px, row-major)   | passthrough             |
//! | `f=100`   | PNG (via the `image` crate)          | decode → RGBA8          |
//!
//! When the command carries `o=z` (compression extra) the payload is
//! zlib-inflated *before* the format-specific step runs.
//!
//! ## Transmission media
//!
//! | `t=` flag | Source                                        |
//! |-----------|-----------------------------------------------|
//! | `t=d`     | base64-encoded bytes in the APC payload       |
//! | `t=f`     | absolute file path (payload = utf-8 path)     |
//! | `t=t`     | temp-file path (payload = utf-8 path, removed after read) |
//! | `t=s`     | POSIX shared memory — **not supported here**  |
//!
//! `t=f` reads are sandboxed: the resolved path must live inside an
//! allowlist of directories (the user's therminal data dir and the
//! platform temp dir). Symlinks that resolve to targets outside the
//! allowlist are rejected. See [`FileMediumSandbox`] for the policy
//! implementation and the rationale.
//!
//! ## LRU eviction
//!
//! The store enforces a byte budget (default 128 MB). Inserts that push
//! the total over the budget evict least-recently-used entries until the
//! total fits. Eviction order is tracked on every insert and every
//! [`ImageStore::get`] call.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use flate2::read::ZlibDecoder;

use super::chunk_buffer::CHUNK_BUFFER_HARD_CAP;
use super::{GraphicsFormat, GraphicsMedium, RawGraphicsCommand};

/// Default byte budget for the cache (128 MB). An agent session that
/// spams dozens of screenshots will force eviction rather than balloon
/// process memory without bound.
pub const DEFAULT_BUDGET_BYTES: usize = 128 * 1024 * 1024;

/// Stable identifier used by the store's hash map. The protocol allows
/// either of `image_id` / `placement_id` to be absent; we normalise
/// missing values to `0` the same way [`super::chunk_buffer::ChunkKey`]
/// does. A pure-placement `ImageId` (image_id = 0, placement_id = N) is
/// a legal key even though it's unusual in practice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageId {
    pub image_id: u32,
    pub placement_id: u32,
}

impl ImageId {
    /// Build an `ImageId` from a parsed command, filling in `0` where the
    /// corresponding wire field was absent.
    pub fn from_command(cmd: &RawGraphicsCommand) -> Self {
        Self {
            image_id: cmd.image_id.unwrap_or(0),
            placement_id: cmd.placement_id.unwrap_or(0),
        }
    }

    /// Build directly from optional u32s (e.g. on a `GraphicsTransmit`
    /// event where the id fields are already split out).
    pub fn new(image_id: Option<u32>, placement_id: Option<u32>) -> Self {
        Self {
            image_id: image_id.unwrap_or(0),
            placement_id: placement_id.unwrap_or(0),
        }
    }
}

/// Placeholder for the GPU texture handle the renderer will mint on
/// first draw (tn-wdn1). Kept as a tiny newtype so the downstream
/// renderer task can replace the inner type without every call-site
/// having to change. Not constructed inside this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextureId(pub u64);

/// A decoded image, ready for the GPU upload hook.
///
/// `pixels` is always `width * height * 4` bytes of RGBA8, row-major,
/// top-to-bottom. `gpu_texture` is set lazily by the renderer on first
/// draw and stays valid for the lifetime of the [`DecodedImage`] (i.e.
/// until LRU eviction drops the [`Arc`]).
#[derive(Debug)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub gpu_texture: OnceLock<TextureId>,
}

impl DecodedImage {
    /// Pixel-buffer size in bytes. Used by the store to enforce the
    /// byte budget — the `Arc<DecodedImage>` header and the renderer's
    /// `TextureId` slot are both constant-sized and ignored here.
    pub fn byte_size(&self) -> usize {
        self.pixels.len()
    }
}

/// Errors produced by the store. The protocol layer (`graphics/mod.rs`)
/// translates these into an APC error response — `EINVAL` for malformed
/// input, `ENOENT` for a missing file, `ENOMEM` for decode failures that
/// blow past a limit, `EACCES` for sandbox denials.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The base64 payload was not valid base64.
    #[error("graphics store: invalid base64 payload: {0}")]
    Base64(String),
    /// `o=z` was set but the zlib stream was corrupt / truncated.
    #[error("graphics store: zlib inflate failed: {0}")]
    ZlibInflate(String),
    /// `o=z` decompressed output exceeded the per-payload hard cap. Guards
    /// against a decompression bomb where a tiny compressed payload
    /// expands into gigabytes of plaintext and exhausts memory.
    #[error("graphics store: zlib decompressed output exceeded cap ({cap} bytes)")]
    ZlibOutputTooLarge { cap: usize },
    /// `f=24` / `f=32` payload was not the expected `width * height * N`
    /// bytes.
    #[error(
        "graphics store: raw pixel size mismatch: got {got} bytes, expected {expected} \
         (width={width}, height={height}, bpp={bpp})"
    )]
    RawSize {
        got: usize,
        expected: usize,
        width: u32,
        height: u32,
        bpp: usize,
    },
    /// `f=24` / `f=32` transmit was missing either `s=` or `v=`.
    #[error("graphics store: raw format requires s=width and v=height")]
    RawMissingDimensions,
    /// `f=100` payload was not a valid PNG.
    #[error("graphics store: PNG decode failed: {0}")]
    PngDecode(String),
    /// A declared format is recognised by the parser but not handled here
    /// (e.g. `f=default` without any other hint). Exposed so the call
    /// site can render a precise diagnostic.
    #[error("graphics store: unsupported format")]
    UnsupportedFormat,
    /// `t=s` (POSIX shared memory) is intentionally not implemented.
    #[error("graphics store: transmission medium t=s (shared memory) is not supported")]
    SharedMemoryUnsupported,
    /// `t=f` / `t=t` carried a non-utf8 path.
    #[error("graphics store: path payload is not valid UTF-8")]
    PathNotUtf8,
    /// The file pointed at by `t=f` / `t=t` does not exist or is
    /// unreadable.
    #[error("graphics store: file not found or unreadable: {path}")]
    FileNotFound { path: String },
    /// The file pointed at by `t=f` / `t=t` exceeded the per-payload
    /// hard cap. Prevents an oversized on-disk payload from bypassing
    /// the in-memory chunk buffer cap.
    #[error("graphics store: file {path} exceeds cap ({cap} bytes)")]
    FileTooLarge { path: String, cap: usize },
    /// The file path escaped the `t=f` sandbox.
    #[error(
        "graphics store: file path {path} is outside the allowed sandbox \
         (permitted roots: therminal data dir, platform temp dir)"
    )]
    FileOutsideSandbox { path: String },
    /// Generic I/O failure while reading a `t=f` / `t=t` file.
    #[error("graphics store: i/o error: {0}")]
    Io(String),
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e.to_string())
    }
}

/// Sandbox policy for `t=f` reads.
///
/// We mimic Kitty's posture: only read files that the user's own
/// process already has access to, and err toward a small whitelist of
/// directories rather than trying to enumerate every safe path. The
/// allowlist currently contains:
///
/// 1. The therminal data dir (e.g. `~/.local/share/therminal/` on Linux,
///    `%APPDATA%\therminal\` on Windows). Agents drop screenshots here
///    when they want the terminal to render them.
/// 2. The platform temp dir (`/tmp` on Unix, `%TEMP%` on Windows).
///    `t=t` (the terminal deletes the file after reading) is the common
///    use case here and fits the sandbox naturally.
///
/// Symlink handling: we canonicalise the requested path and then check
/// the canonical form against the allowlist. A symlink planted inside
/// the temp dir that points at `/etc/passwd` will canonicalise out of
/// the sandbox and be rejected.
///
/// The struct is `Default` so the common path — the [`ImageStore`]
/// built by the daemon with no extra roots — works with zero
/// configuration. Tests use [`FileMediumSandbox::with_extra_root`] to
/// point at a `tempfile::tempdir()`.
#[derive(Debug, Clone)]
pub struct FileMediumSandbox {
    /// Extra allowlist entries. In practice only tests populate this.
    extra_roots: Vec<PathBuf>,
    /// `true` in production; tests override via
    /// [`FileMediumSandbox::roots_only`] so they can build a closed
    /// sandbox that excludes `/tmp` (which is where `tempfile::tempdir`
    /// lives — without this tests cannot reliably assert "path outside
    /// sandbox" because the "outside" dir also lives under `/tmp`).
    include_defaults: bool,
}

impl Default for FileMediumSandbox {
    fn default() -> Self {
        Self {
            extra_roots: Vec::new(),
            include_defaults: true,
        }
    }
}

impl FileMediumSandbox {
    /// Construct a sandbox with a single extra allowlist root. The
    /// stable default roots (therminal data dir + platform temp dir)
    /// are always included.
    pub fn with_extra_root(root: impl Into<PathBuf>) -> Self {
        Self {
            extra_roots: vec![root.into()],
            include_defaults: true,
        }
    }

    /// Build a closed sandbox that *only* contains the supplied roots.
    /// Intended for tests that want to assert rejection of paths the
    /// platform temp dir would otherwise allow.
    pub fn roots_only(roots: Vec<PathBuf>) -> Self {
        Self {
            extra_roots: roots,
            include_defaults: false,
        }
    }

    /// Add an additional allowlist root. Returns `self` for chaining.
    pub fn add_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.extra_roots.push(root.into());
        self
    }

    /// Default roots derived from the runtime crate. Kept as a method
    /// (rather than a `const`) because `therminal_runtime::paths::data_dir`
    /// resolves lazily from `dirs`.
    fn default_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::with_capacity(2 + self.extra_roots.len());
        if self.include_defaults {
            roots.push(therminal_runtime::paths::data_dir());
            roots.push(std::env::temp_dir());
        }
        roots.extend(self.extra_roots.iter().cloned());
        roots
    }

    /// Resolve a requested path against the sandbox. On success returns
    /// the canonical path the caller should read from; on failure
    /// returns an error the protocol layer can translate to an APC
    /// response.
    pub fn resolve(&self, requested: &str) -> Result<PathBuf, StoreError> {
        let requested_path = Path::new(requested);
        // Canonicalise so symlinks, `..`, and relative paths all collapse
        // to a single comparison form. Missing files surface here as
        // `FileNotFound` rather than a nonsensical sandbox error.
        let canonical = match std::fs::canonicalize(requested_path) {
            Ok(p) => p,
            Err(_) => {
                return Err(StoreError::FileNotFound {
                    path: requested.to_string(),
                });
            }
        };

        let roots = self.default_roots();
        let mut allowed = false;
        for root in &roots {
            let Ok(canonical_root) = std::fs::canonicalize(root) else {
                continue;
            };
            if canonical.starts_with(&canonical_root) {
                allowed = true;
                break;
            }
        }

        if !allowed {
            return Err(StoreError::FileOutsideSandbox {
                path: requested.to_string(),
            });
        }
        Ok(canonical)
    }
}

/// Parsed transmit command — what the store consumes.
///
/// The caller (the interceptor glue in `graphics/mod.rs`) strips the
/// event shell off [`crate::terminal::GraphicsEvent::GraphicsTransmit`]
/// and hands the store this cleaned-up view. Keeping it as a separate
/// struct lets the store stay decoupled from the event enum — the
/// tests can build one directly without mocking a full APC round-trip.
#[derive(Debug, Clone)]
pub struct TransmitCommand {
    pub image_id: ImageId,
    pub format: GraphicsFormat,
    pub medium: GraphicsMedium,
    pub width_px: Option<u32>,
    pub height_px: Option<u32>,
    pub payload: Vec<u8>,
    /// Set iff the original APC header had `o=z`.
    pub compression: bool,
}

impl TransmitCommand {
    /// Convenience builder used by `graphics/mod.rs` — promotes the
    /// `o=z` flag out of `RawGraphicsCommand.extras` and assembles the
    /// rest of the fields.
    ///
    /// The argument count mirrors the fields split out of the
    /// [`crate::terminal::GraphicsEvent::GraphicsTransmit`] variant
    /// verbatim, so the caller is a mechanical transcription rather
    /// than a candidate for a builder struct.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        image_id: Option<u32>,
        placement_id: Option<u32>,
        format: GraphicsFormat,
        medium: GraphicsMedium,
        width_px: Option<u32>,
        height_px: Option<u32>,
        payload: Vec<u8>,
        command: &RawGraphicsCommand,
    ) -> Self {
        let compression = command.extras.get("o").map(String::as_str) == Some("z");
        Self {
            image_id: ImageId::new(image_id, placement_id),
            format,
            medium,
            width_px,
            height_px,
            payload,
            compression,
        }
    }
}

/// Cache of decoded images keyed by [`ImageId`].
///
/// The cache is intentionally simple: a hash map plus an
/// insertion-ordered list used for LRU eviction. Hit the cache via
/// [`Self::get`] (bumps the entry to most-recently-used) and insert via
/// [`Self::ingest_transmit`] / [`Self::insert`]. Budget enforcement
/// runs on every insert — past the limit, oldest entries are dropped
/// until the total fits.
///
/// The map is `Send + Sync` by way of the `Arc<DecodedImage>` values;
/// the daemon wraps it in a `Mutex` when sharing across tasks.
pub struct ImageStore {
    images: HashMap<ImageId, Arc<DecodedImage>>,
    /// Oldest → newest. `ids_by_recency.last()` is the most-recently
    /// used entry. Pushing / removing happens on every `get` + `insert`
    /// so the vec length always matches `images.len()`.
    ids_by_recency: Vec<ImageId>,
    bytes_used: usize,
    budget_bytes: usize,
    sandbox: FileMediumSandbox,
    /// Per-payload hard cap for `t=f` / `t=t` file reads. Defaults to
    /// [`CHUNK_BUFFER_HARD_CAP`]. Tests can override via
    /// [`Self::with_file_medium_cap`] so they can exercise the
    /// overflow path without writing a 64 MB fixture.
    file_medium_cap: usize,
}

impl std::fmt::Debug for ImageStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageStore")
            .field("entries", &self.images.len())
            .field("bytes_used", &self.bytes_used)
            .field("budget_bytes", &self.budget_bytes)
            .finish()
    }
}

impl Default for ImageStore {
    fn default() -> Self {
        Self::new(DEFAULT_BUDGET_BYTES)
    }
}

impl ImageStore {
    /// Build a store with a custom byte budget. Pass
    /// [`DEFAULT_BUDGET_BYTES`] unless a test is exercising the LRU.
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            images: HashMap::new(),
            ids_by_recency: Vec::new(),
            bytes_used: 0,
            budget_bytes,
            sandbox: FileMediumSandbox::default(),
            file_medium_cap: CHUNK_BUFFER_HARD_CAP,
        }
    }

    /// Replace the default sandbox. Useful for tests that want to
    /// whitelist a `tempfile::tempdir()`.
    pub fn with_sandbox(mut self, sandbox: FileMediumSandbox) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Override the per-payload hard cap for `t=f` / `t=t` reads. Intended
    /// for tests that want to assert rejection of oversized files without
    /// writing a full-sized fixture to disk.
    #[cfg(test)]
    pub fn with_file_medium_cap(mut self, cap: usize) -> Self {
        self.file_medium_cap = cap;
        self
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.images.len()
    }

    /// `true` iff the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    /// Current byte budget.
    pub fn budget_bytes(&self) -> usize {
        self.budget_bytes
    }

    /// Current bytes in use (sum of `pixels.len()` across entries).
    pub fn bytes_used(&self) -> usize {
        self.bytes_used
    }

    /// Look up a cached entry and bump it to most-recently-used.
    ///
    /// Returns an `Arc` clone so the caller can hand the pixels to the
    /// GPU uploader without holding the store lock. The renderer's
    /// `TextureId` field lives on the `DecodedImage` itself — subsequent
    /// `get()` calls see the same slot.
    pub fn get(&mut self, id: ImageId) -> Option<Arc<DecodedImage>> {
        if !self.images.contains_key(&id) {
            return None;
        }
        self.bump_recency(id);
        self.images.get(&id).cloned()
    }

    /// Look up a cached entry **without** updating the LRU ordering.
    /// Used for test assertions that want to probe the store without
    /// perturbing it.
    pub fn peek(&self, id: ImageId) -> Option<Arc<DecodedImage>> {
        self.images.get(&id).cloned()
    }

    /// Insert a decoded image at `id`. If an entry already exists under
    /// the same id it is replaced (and its bytes are subtracted from
    /// `bytes_used` before the new entry is counted). LRU eviction
    /// runs unconditionally after insert.
    pub fn insert(&mut self, id: ImageId, image: DecodedImage) -> Arc<DecodedImage> {
        let new_bytes = image.byte_size();
        if let Some(old) = self.images.remove(&id) {
            self.bytes_used = self.bytes_used.saturating_sub(old.byte_size());
            self.ids_by_recency.retain(|entry| *entry != id);
        }
        let arc = Arc::new(image);
        self.images.insert(id, arc.clone());
        self.ids_by_recency.push(id);
        self.bytes_used += new_bytes;
        self.evict_to_budget();
        arc
    }

    /// Decode a transmit command and insert the resulting
    /// [`DecodedImage`]. This is the primary entry point used by the
    /// interceptor glue in `graphics/mod.rs`.
    pub fn ingest_transmit(
        &mut self,
        cmd: TransmitCommand,
    ) -> Result<Arc<DecodedImage>, StoreError> {
        let image = self.decode_transmit(&cmd)?;
        Ok(self.insert(cmd.image_id, image))
    }

    /// Remove a single entry.
    pub fn delete_by_id(&mut self, id: ImageId) {
        if let Some(old) = self.images.remove(&id) {
            self.bytes_used = self.bytes_used.saturating_sub(old.byte_size());
            self.ids_by_recency.retain(|entry| *entry != id);
        }
    }

    /// Drop every cached entry.
    pub fn delete_all(&mut self) {
        self.images.clear();
        self.ids_by_recency.clear();
        self.bytes_used = 0;
    }

    // -- internal ------------------------------------------------------

    /// Decode a transmit command to a [`DecodedImage`] without touching
    /// the cache. Split out from `ingest_transmit` so callers that only
    /// want the pixels (tests, future renderer experiments) can use
    /// it. Does all the heavy lifting:
    ///
    /// 1. Resolve payload bytes from the transmission medium.
    /// 2. Optionally zlib-inflate (when `o=z`).
    /// 3. Apply the format-specific decode (`f=24` / `f=32` / `f=100`).
    fn decode_transmit(&self, cmd: &TransmitCommand) -> Result<DecodedImage, StoreError> {
        let raw_bytes = self.fetch_medium_bytes(cmd)?;
        let decompressed = if cmd.compression {
            zlib_inflate(&raw_bytes)?
        } else {
            raw_bytes
        };
        decode_format(cmd.format, cmd.width_px, cmd.height_px, decompressed)
    }

    /// Resolve the medium (`t=`) to a byte buffer. For `t=d` this is a
    /// base64 decode. For `t=f` / `t=t` we read the file (with the
    /// `t=t` path deleted after the read succeeds). For `t=s` we refuse.
    fn fetch_medium_bytes(&self, cmd: &TransmitCommand) -> Result<Vec<u8>, StoreError> {
        match cmd.medium {
            GraphicsMedium::Direct => BASE64
                .decode(&cmd.payload)
                .map_err(|e| StoreError::Base64(e.to_string())),
            GraphicsMedium::File | GraphicsMedium::TempFile => {
                let raw_path = std::str::from_utf8(&cmd.payload)
                    .map_err(|_| StoreError::PathNotUtf8)?
                    .trim()
                    .trim_end_matches('\0');
                let canonical = self.sandbox.resolve(raw_path)?;
                let cap = self.file_medium_cap;
                // Read through a `take`-limited reader so an oversized
                // file on disk cannot bypass the in-memory chunk-buffer
                // cap. The limit is cap + 1 so a file that is exactly
                // `cap` bytes succeeds while anything larger trips the
                // overflow guard below.
                let file = std::fs::File::open(&canonical).map_err(|e| match e.kind() {
                    std::io::ErrorKind::NotFound => StoreError::FileNotFound {
                        path: raw_path.to_string(),
                    },
                    _ => StoreError::Io(e.to_string()),
                })?;
                let mut limited = file.take((cap as u64).saturating_add(1));
                let mut bytes = Vec::new();
                limited
                    .read_to_end(&mut bytes)
                    .map_err(|e| match e.kind() {
                        std::io::ErrorKind::NotFound => StoreError::FileNotFound {
                            path: raw_path.to_string(),
                        },
                        _ => StoreError::Io(e.to_string()),
                    })?;
                if bytes.len() > cap {
                    return Err(StoreError::FileTooLarge {
                        path: raw_path.to_string(),
                        cap,
                    });
                }
                if matches!(cmd.medium, GraphicsMedium::TempFile) {
                    // Best-effort cleanup — a failed unlink here is
                    // not fatal, the caller already has the bytes.
                    let _ = std::fs::remove_file(&canonical);
                }
                Ok(bytes)
            }
            GraphicsMedium::SharedMemory => Err(StoreError::SharedMemoryUnsupported),
        }
    }

    /// Evict entries from the front of `ids_by_recency` until the total
    /// fits within `budget_bytes`. Runs after every `insert` call. A
    /// single entry that on its own exceeds the budget is still kept —
    /// the alternative is to reject it outright, which the protocol
    /// layer has no natural way to surface. In that degenerate case the
    /// store will hold exactly one entry and `bytes_used` will exceed
    /// `budget_bytes`.
    fn evict_to_budget(&mut self) {
        while self.bytes_used > self.budget_bytes && self.images.len() > 1 {
            let Some(oldest) = self.ids_by_recency.first().copied() else {
                break;
            };
            self.ids_by_recency.remove(0);
            if let Some(removed) = self.images.remove(&oldest) {
                self.bytes_used = self.bytes_used.saturating_sub(removed.byte_size());
            }
        }
    }

    /// Move `id` to the end of the recency list (most-recently-used).
    /// Caller must confirm `images.contains_key(&id)` first.
    fn bump_recency(&mut self, id: ImageId) {
        if let Some(pos) = self.ids_by_recency.iter().position(|entry| *entry == id) {
            self.ids_by_recency.remove(pos);
        }
        self.ids_by_recency.push(id);
    }
}

/// Inflate a zlib stream. Used when the command carries `o=z`.
///
/// Caps the decompressed output at [`CHUNK_BUFFER_HARD_CAP`] so a small
/// compressed payload cannot balloon into a multi-gigabyte decompression
/// bomb that exhausts process memory. The `take` limit is cap + 1 so a
/// stream that produces exactly `cap` bytes succeeds while anything
/// larger trips the overflow guard below.
fn zlib_inflate(bytes: &[u8]) -> Result<Vec<u8>, StoreError> {
    let mut out = Vec::with_capacity(bytes.len().min(CHUNK_BUFFER_HARD_CAP));
    let decoder = ZlibDecoder::new(bytes);
    let mut limited = decoder.take((CHUNK_BUFFER_HARD_CAP as u64).saturating_add(1));
    limited
        .read_to_end(&mut out)
        .map_err(|e| StoreError::ZlibInflate(e.to_string()))?;
    if out.len() > CHUNK_BUFFER_HARD_CAP {
        return Err(StoreError::ZlibOutputTooLarge {
            cap: CHUNK_BUFFER_HARD_CAP,
        });
    }
    Ok(out)
}

/// Apply the `f=` format rule to an already-decompressed byte buffer.
///
/// Returns a [`DecodedImage`] with a tight RGBA8 buffer and a fresh
/// `OnceLock` for the future texture handle.
fn decode_format(
    format: GraphicsFormat,
    width_px: Option<u32>,
    height_px: Option<u32>,
    bytes: Vec<u8>,
) -> Result<DecodedImage, StoreError> {
    match format {
        GraphicsFormat::Rgb => {
            let (w, h) = require_dimensions(width_px, height_px)?;
            let expected = (w as usize) * (h as usize) * 3;
            if bytes.len() != expected {
                return Err(StoreError::RawSize {
                    got: bytes.len(),
                    expected,
                    width: w,
                    height: h,
                    bpp: 3,
                });
            }
            let mut pixels = Vec::with_capacity((w as usize) * (h as usize) * 4);
            for chunk in bytes.chunks_exact(3) {
                pixels.push(chunk[0]);
                pixels.push(chunk[1]);
                pixels.push(chunk[2]);
                pixels.push(0xff);
            }
            Ok(DecodedImage {
                width: w,
                height: h,
                pixels,
                gpu_texture: OnceLock::new(),
            })
        }
        GraphicsFormat::Rgba | GraphicsFormat::Default => {
            let (w, h) = require_dimensions(width_px, height_px)?;
            let expected = (w as usize) * (h as usize) * 4;
            if bytes.len() != expected {
                return Err(StoreError::RawSize {
                    got: bytes.len(),
                    expected,
                    width: w,
                    height: h,
                    bpp: 4,
                });
            }
            Ok(DecodedImage {
                width: w,
                height: h,
                pixels: bytes,
                gpu_texture: OnceLock::new(),
            })
        }
        GraphicsFormat::Png => decode_png(&bytes),
    }
}

fn require_dimensions(
    width_px: Option<u32>,
    height_px: Option<u32>,
) -> Result<(u32, u32), StoreError> {
    match (width_px, height_px) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Ok((w, h)),
        _ => Err(StoreError::RawMissingDimensions),
    }
}

fn decode_png(bytes: &[u8]) -> Result<DecodedImage, StoreError> {
    let dyn_img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .map_err(|e| StoreError::PngDecode(e.to_string()))?;
    let rgba = dyn_img.to_rgba8();
    let (width, height) = rgba.dimensions();
    Ok(DecodedImage {
        width,
        height,
        pixels: rgba.into_raw(),
        gpu_texture: OnceLock::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Write;

    fn base64_encode(bytes: &[u8]) -> Vec<u8> {
        BASE64.encode(bytes).into_bytes()
    }

    fn cmd_with_extras(extras: HashMap<String, String>) -> RawGraphicsCommand {
        RawGraphicsCommand {
            extras,
            ..RawGraphicsCommand::empty()
        }
    }

    // -- raw decode matrix ----------------------------------------------

    #[test]
    fn f24_rgb_decodes_to_rgba_with_opaque_alpha() {
        // 2x1 image: red, green.
        let raw = [0xff, 0x00, 0x00, 0x00, 0xff, 0x00];
        let payload = base64_encode(&raw);
        let mut store = ImageStore::default();
        let cmd = TransmitCommand {
            image_id: ImageId::new(Some(1), None),
            format: GraphicsFormat::Rgb,
            medium: GraphicsMedium::Direct,
            width_px: Some(2),
            height_px: Some(1),
            payload,
            compression: false,
        };
        let img = store.ingest_transmit(cmd).unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 1);
        assert_eq!(
            img.pixels,
            vec![0xff, 0x00, 0x00, 0xff, 0x00, 0xff, 0x00, 0xff]
        );
        assert_eq!(store.bytes_used(), 8);
    }

    #[test]
    fn f32_rgba_passthrough() {
        let raw = [
            0x11, 0x22, 0x33, 0x44, // px 1
            0x55, 0x66, 0x77, 0x88, // px 2
        ];
        let payload = base64_encode(&raw);
        let mut store = ImageStore::default();
        let cmd = TransmitCommand {
            image_id: ImageId::new(Some(7), None),
            format: GraphicsFormat::Rgba,
            medium: GraphicsMedium::Direct,
            width_px: Some(2),
            height_px: Some(1),
            payload,
            compression: false,
        };
        let img = store.ingest_transmit(cmd).unwrap();
        assert_eq!(img.pixels, raw.to_vec());
    }

    #[test]
    fn f32_size_mismatch_is_typed_error() {
        let raw = vec![0u8; 7]; // not a multiple of 4, fewer bytes than 2*1*4
        let payload = base64_encode(&raw);
        let mut store = ImageStore::default();
        let err = store
            .ingest_transmit(TransmitCommand {
                image_id: ImageId::new(Some(1), None),
                format: GraphicsFormat::Rgba,
                medium: GraphicsMedium::Direct,
                width_px: Some(2),
                height_px: Some(1),
                payload,
                compression: false,
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::RawSize { .. }));
    }

    #[test]
    fn f24_missing_dimensions_errors() {
        let payload = base64_encode(&[0u8; 3]);
        let mut store = ImageStore::default();
        let err = store
            .ingest_transmit(TransmitCommand {
                image_id: ImageId::new(Some(1), None),
                format: GraphicsFormat::Rgb,
                medium: GraphicsMedium::Direct,
                width_px: None,
                height_px: None,
                payload,
                compression: false,
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::RawMissingDimensions));
    }

    // -- PNG ------------------------------------------------------------

    fn make_png(width: u32, height: u32, fill_rgba: [u8; 4]) -> Vec<u8> {
        let mut pixels = Vec::with_capacity((width as usize) * (height as usize) * 4);
        for _ in 0..(width as usize * height as usize) {
            pixels.extend_from_slice(&fill_rgba);
        }
        let buf = image::RgbaImage::from_raw(width, height, pixels).unwrap();
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(buf)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn f100_png_decode_roundtrip() {
        let png = make_png(3, 2, [0x10, 0x20, 0x30, 0xff]);
        let payload = base64_encode(&png);
        let mut store = ImageStore::default();
        let img = store
            .ingest_transmit(TransmitCommand {
                image_id: ImageId::new(Some(42), None),
                format: GraphicsFormat::Png,
                medium: GraphicsMedium::Direct,
                width_px: None,
                height_px: None,
                payload,
                compression: false,
            })
            .unwrap();
        assert_eq!(img.width, 3);
        assert_eq!(img.height, 2);
        assert_eq!(img.pixels.len(), 3 * 2 * 4);
        assert_eq!(&img.pixels[..4], &[0x10, 0x20, 0x30, 0xff]);
    }

    #[test]
    fn malformed_base64_is_typed_error() {
        let mut store = ImageStore::default();
        let err = store
            .ingest_transmit(TransmitCommand {
                image_id: ImageId::new(Some(1), None),
                format: GraphicsFormat::Png,
                medium: GraphicsMedium::Direct,
                width_px: None,
                height_px: None,
                payload: b"this is not base64!!!".to_vec(),
                compression: false,
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::Base64(_)));
    }

    #[test]
    fn malformed_png_is_typed_error() {
        let mut store = ImageStore::default();
        let err = store
            .ingest_transmit(TransmitCommand {
                image_id: ImageId::new(Some(1), None),
                format: GraphicsFormat::Png,
                medium: GraphicsMedium::Direct,
                width_px: None,
                height_px: None,
                payload: base64_encode(b"not a png"),
                compression: false,
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::PngDecode(_)));
    }

    // -- zlib inflate ---------------------------------------------------

    #[test]
    fn zlib_inflate_path_decodes_compressed_rgba() {
        use flate2::Compression;
        use flate2::write::ZlibEncoder;

        let raw = [
            0xde, 0xad, 0xbe, 0xef, //
            0x12, 0x34, 0x56, 0x78, //
        ];
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&raw).unwrap();
        let compressed = encoder.finish().unwrap();
        let payload = base64_encode(&compressed);

        let mut store = ImageStore::default();
        let img = store
            .ingest_transmit(TransmitCommand {
                image_id: ImageId::new(Some(1), None),
                format: GraphicsFormat::Rgba,
                medium: GraphicsMedium::Direct,
                width_px: Some(2),
                height_px: Some(1),
                payload,
                compression: true,
            })
            .unwrap();
        assert_eq!(img.pixels, raw.to_vec());
    }

    #[test]
    fn zlib_malformed_is_typed_error() {
        let payload = base64_encode(b"not zlib");
        let mut store = ImageStore::default();
        let err = store
            .ingest_transmit(TransmitCommand {
                image_id: ImageId::new(Some(1), None),
                format: GraphicsFormat::Rgba,
                medium: GraphicsMedium::Direct,
                width_px: Some(2),
                height_px: Some(1),
                payload,
                compression: true,
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::ZlibInflate(_)));
    }

    #[test]
    fn zlib_output_larger_than_cap_is_typed_error() {
        use flate2::Compression;
        use flate2::write::ZlibEncoder;

        // A highly-compressible payload: (CHUNK_BUFFER_HARD_CAP + 1) zeros
        // shrinks to a tiny compressed stream but would blow past the cap
        // on inflate.  Asserts the bomb guard.
        let raw = vec![0u8; CHUNK_BUFFER_HARD_CAP + 1];
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&raw).unwrap();
        let compressed = encoder.finish().unwrap();
        let payload = base64_encode(&compressed);

        let mut store = ImageStore::default();
        let err = store
            .ingest_transmit(TransmitCommand {
                image_id: ImageId::new(Some(1), None),
                format: GraphicsFormat::Rgba,
                medium: GraphicsMedium::Direct,
                width_px: Some(1),
                height_px: Some(1),
                payload,
                compression: true,
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::ZlibOutputTooLarge { .. }));
    }

    // -- t=f sandbox ----------------------------------------------------

    #[test]
    fn t_f_reads_png_inside_sandbox() {
        let tmp = tempfile::tempdir().unwrap();
        let png = make_png(1, 1, [0xff, 0x00, 0x00, 0xff]);
        let path = tmp.path().join("pixel.png");
        std::fs::write(&path, &png).unwrap();

        let sandbox = FileMediumSandbox::with_extra_root(tmp.path());
        let mut store = ImageStore::default().with_sandbox(sandbox);

        let path_str = path.to_string_lossy().into_owned();
        let img = store
            .ingest_transmit(TransmitCommand {
                image_id: ImageId::new(Some(5), None),
                format: GraphicsFormat::Png,
                medium: GraphicsMedium::File,
                width_px: None,
                height_px: None,
                payload: path_str.into_bytes(),
                compression: false,
            })
            .unwrap();
        assert_eq!(img.width, 1);
        assert_eq!(img.height, 1);
        // File still there (t=f does not delete).
        assert!(path.exists());
    }

    #[test]
    fn t_t_deletes_the_file_after_read() {
        let tmp = tempfile::tempdir().unwrap();
        let png = make_png(1, 1, [0x11, 0x22, 0x33, 0xff]);
        let path = tmp.path().join("temp.png");
        std::fs::write(&path, &png).unwrap();

        let sandbox = FileMediumSandbox::with_extra_root(tmp.path());
        let mut store = ImageStore::default().with_sandbox(sandbox);

        let path_str = path.to_string_lossy().into_owned();
        let _ = store
            .ingest_transmit(TransmitCommand {
                image_id: ImageId::new(Some(6), None),
                format: GraphicsFormat::Png,
                medium: GraphicsMedium::TempFile,
                width_px: None,
                height_px: None,
                payload: path_str.into_bytes(),
                compression: false,
            })
            .unwrap();
        assert!(
            !path.exists(),
            "t=t should remove the file after successful read"
        );
    }

    #[test]
    fn t_f_rejects_paths_outside_sandbox() {
        let tmp = tempfile::tempdir().unwrap();
        // `outside_dir` is NOT added to the sandbox. We build a closed
        // sandbox (no default temp/data roots) so the test's assertion
        // works regardless of where `tempfile::tempdir` lives.
        let outside_dir = tempfile::tempdir().unwrap();
        let png = make_png(1, 1, [0x00, 0x00, 0x00, 0xff]);
        let outside_file = outside_dir.path().join("evil.png");
        std::fs::write(&outside_file, &png).unwrap();

        let sandbox = FileMediumSandbox::roots_only(vec![tmp.path().to_path_buf()]);
        let mut store = ImageStore::default().with_sandbox(sandbox);

        let err = store
            .ingest_transmit(TransmitCommand {
                image_id: ImageId::new(Some(1), None),
                format: GraphicsFormat::Png,
                medium: GraphicsMedium::File,
                width_px: None,
                height_px: None,
                payload: outside_file.to_string_lossy().into_owned().into_bytes(),
                compression: false,
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::FileOutsideSandbox { .. }));
    }

    #[test]
    fn t_f_rejects_oversized_file() {
        // Use a tiny per-instance cap so the test can stay small in CI.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.bin");
        std::fs::write(&path, vec![0u8; 1024]).unwrap();
        let sandbox = FileMediumSandbox::with_extra_root(tmp.path());
        let mut store = ImageStore::default()
            .with_sandbox(sandbox)
            .with_file_medium_cap(256);

        let err = store
            .ingest_transmit(TransmitCommand {
                image_id: ImageId::new(Some(1), None),
                format: GraphicsFormat::Rgba,
                medium: GraphicsMedium::File,
                width_px: Some(1),
                height_px: Some(1),
                payload: path.to_string_lossy().into_owned().into_bytes(),
                compression: false,
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::FileTooLarge { .. }));
    }

    #[test]
    fn t_f_missing_file_is_typed_error() {
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = FileMediumSandbox::with_extra_root(tmp.path());
        let mut store = ImageStore::default().with_sandbox(sandbox);

        let missing = tmp.path().join("does-not-exist.png");
        let err = store
            .ingest_transmit(TransmitCommand {
                image_id: ImageId::new(Some(1), None),
                format: GraphicsFormat::Png,
                medium: GraphicsMedium::File,
                width_px: None,
                height_px: None,
                payload: missing.to_string_lossy().into_owned().into_bytes(),
                compression: false,
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::FileNotFound { .. }));
    }

    #[test]
    fn t_s_is_unsupported() {
        let mut store = ImageStore::default();
        let err = store
            .ingest_transmit(TransmitCommand {
                image_id: ImageId::new(Some(1), None),
                format: GraphicsFormat::Rgba,
                medium: GraphicsMedium::SharedMemory,
                width_px: Some(1),
                height_px: Some(1),
                payload: Vec::new(),
                compression: false,
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::SharedMemoryUnsupported));
    }

    // -- LRU ------------------------------------------------------------

    /// Helper: decode one 10×10 RGBA image with known id.
    fn insert_fixed_image(store: &mut ImageStore, id: u32, byte_fill: u8) -> Arc<DecodedImage> {
        let width = 10u32;
        let height = 10u32;
        let raw = vec![byte_fill; (width as usize) * (height as usize) * 4];
        let payload = base64_encode(&raw);
        store
            .ingest_transmit(TransmitCommand {
                image_id: ImageId::new(Some(id), None),
                format: GraphicsFormat::Rgba,
                medium: GraphicsMedium::Direct,
                width_px: Some(width),
                height_px: Some(height),
                payload,
                compression: false,
            })
            .unwrap()
    }

    #[test]
    fn lru_evicts_oldest_past_budget() {
        // Budget = 1000 bytes. Each fixed image is 10*10*4 = 400 bytes.
        // Three images = 1200 bytes ⇒ must evict one.
        let mut store = ImageStore::new(1000);
        insert_fixed_image(&mut store, 1, 0x11);
        insert_fixed_image(&mut store, 2, 0x22);
        insert_fixed_image(&mut store, 3, 0x33);

        assert_eq!(store.len(), 2, "oldest entry should have been evicted");
        assert!(store.peek(ImageId::new(Some(1), None)).is_none());
        assert!(store.peek(ImageId::new(Some(2), None)).is_some());
        assert!(store.peek(ImageId::new(Some(3), None)).is_some());
    }

    #[test]
    fn lru_get_bumps_recency() {
        let mut store = ImageStore::new(1000);
        insert_fixed_image(&mut store, 1, 0x11);
        insert_fixed_image(&mut store, 2, 0x22);
        // Touch 1 so it becomes MRU.
        let _ = store.get(ImageId::new(Some(1), None));
        // Adding 3 should now evict 2, not 1.
        insert_fixed_image(&mut store, 3, 0x33);

        assert!(store.peek(ImageId::new(Some(1), None)).is_some());
        assert!(store.peek(ImageId::new(Some(2), None)).is_none());
        assert!(store.peek(ImageId::new(Some(3), None)).is_some());
    }

    // -- delete ---------------------------------------------------------

    #[test]
    fn delete_by_id_removes_only_target() {
        let mut store = ImageStore::new(10_000);
        insert_fixed_image(&mut store, 1, 0x11);
        insert_fixed_image(&mut store, 2, 0x22);
        store.delete_by_id(ImageId::new(Some(1), None));
        assert!(store.peek(ImageId::new(Some(1), None)).is_none());
        assert!(store.peek(ImageId::new(Some(2), None)).is_some());
        assert_eq!(store.bytes_used(), 400);
    }

    #[test]
    fn delete_all_clears_store() {
        let mut store = ImageStore::new(10_000);
        insert_fixed_image(&mut store, 1, 0x11);
        insert_fixed_image(&mut store, 2, 0x22);
        store.delete_all();
        assert!(store.is_empty());
        assert_eq!(store.bytes_used(), 0);
    }

    #[test]
    fn insert_replaces_same_id_and_updates_bytes() {
        let mut store = ImageStore::new(10_000);
        insert_fixed_image(&mut store, 1, 0x11);
        let before = store.bytes_used();
        insert_fixed_image(&mut store, 1, 0x22);
        let after = store.bytes_used();
        assert_eq!(before, after, "replacing same id should not leak bytes");
        assert_eq!(store.len(), 1);
    }

    // -- TransmitCommand::from_parts ------------------------------------

    #[test]
    fn from_parts_picks_up_oz_compression_flag() {
        let mut extras = HashMap::new();
        extras.insert("o".to_string(), "z".to_string());
        let cmd = cmd_with_extras(extras);
        let built = TransmitCommand::from_parts(
            Some(3),
            Some(1),
            GraphicsFormat::Rgba,
            GraphicsMedium::Direct,
            Some(10),
            Some(20),
            b"p".to_vec(),
            &cmd,
        );
        assert!(built.compression);
        assert_eq!(built.image_id.image_id, 3);
        assert_eq!(built.image_id.placement_id, 1);
    }

    #[test]
    fn from_parts_without_oz_has_compression_false() {
        let cmd = RawGraphicsCommand::empty();
        let built = TransmitCommand::from_parts(
            Some(3),
            None,
            GraphicsFormat::Rgba,
            GraphicsMedium::Direct,
            Some(1),
            Some(1),
            Vec::new(),
            &cmd,
        );
        assert!(!built.compression);
    }
}
