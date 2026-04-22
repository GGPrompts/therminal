//! Kitty graphics protocol parser (APC layer).
//!
//! Handles the wire format
//!
//! ```text
//! ESC _ G <key>=<value>(,<key>=<value>)* ; <base64-payload> ESC \
//! ```
//!
//! and turns it into structured [`crate::terminal::GraphicsEvent`] values
//! consumed by the renderer. This module is intentionally protocol-only:
//! it does **not** base64-decode the payload, nor does it touch PNG / RGBA
//! bytes. The decoder (tn-0htm) lives elsewhere and is fed the raw payload
//! plus the [`GraphicsFormat`] flag.
//!
//! See <https://sw.kovidgoyal.net/kitty/graphics-protocol/> for the full
//! protocol reference.
//!
//! ## Module layout
//!
//! - [`KittyGraphicsParser`] — byte-by-byte APC sink. Accumulates the payload
//!   into an internal buffer, then, on `intercept_apc_end`, parses the
//!   header and feeds the payload through the [`chunk_buffer::ChunkBuffer`]
//!   when `m=1`. Produces a [`ParseOutput`] per completed APC string.
//! - [`chunk_buffer`] — per-`(image_id, placement_id)` accumulator with a
//!   64 MB hard cap.
//! - [`parse_header`] — free function that turns a comma-separated
//!   `k=v` list into a [`RawGraphicsCommand`].
//! - [`format_response`] — builds the APC envelope for the protocol response
//!   (`OK`, `ENOENT`, etc.) used by the `q=` flag.

pub mod chunk_buffer;

use std::collections::HashMap;

pub use chunk_buffer::{CHUNK_BUFFER_HARD_CAP, ChunkBuffer, ChunkError, ChunkKey, CompletedChunk};

use crate::terminal::GraphicsEvent;

/// APC introducer byte used by the Kitty graphics protocol. Valid APC
/// payloads we care about always start with `'G'` — we drop any other
/// APC string on the floor so we don't accidentally misinterpret
/// unrelated APCs (e.g. iTerm2 or future extensions).
pub const KITTY_APC_PREFIX: u8 = b'G';

/// Parsed `a=` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicsAction {
    /// `a=t` — transmit only.
    Transmit,
    /// `a=T` — transmit-and-display.
    TransmitAndDisplay,
    /// `a=p` — display a previously transmitted image.
    Put,
    /// `a=d` — delete.
    Delete,
    /// `a=q` — capability query.
    Query,
    /// `a=f` — frame data (animation). Accepted but treated as a transmit
    /// variant by the current event stream; animation support lives outside
    /// of this parser.
    Frame,
}

impl GraphicsAction {
    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            b't' => Some(Self::Transmit),
            b'T' => Some(Self::TransmitAndDisplay),
            b'p' => Some(Self::Put),
            b'd' => Some(Self::Delete),
            b'q' => Some(Self::Query),
            b'f' => Some(Self::Frame),
            _ => None,
        }
    }
}

/// Parsed `f=` field (payload format).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicsFormat {
    /// `f=24` — 24-bit RGB.
    Rgb,
    /// `f=32` — 32-bit RGBA.
    Rgba,
    /// `f=100` — PNG.
    Png,
    /// `f=` missing: protocol default is `32`.
    Default,
}

impl GraphicsFormat {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "24" => Some(Self::Rgb),
            "32" => Some(Self::Rgba),
            "100" => Some(Self::Png),
            _ => None,
        }
    }
}

/// Parsed `t=` field (transmission medium).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicsMedium {
    /// `t=d` — direct (base64 in payload). This is the default.
    Direct,
    /// `t=f` — file path.
    File,
    /// `t=t` — temp file (terminal deletes it).
    TempFile,
    /// `t=s` — POSIX shared memory.
    SharedMemory,
}

impl GraphicsMedium {
    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            b'd' => Some(Self::Direct),
            b'f' => Some(Self::File),
            b't' => Some(Self::TempFile),
            b's' => Some(Self::SharedMemory),
            _ => None,
        }
    }
}

/// Parsed `q=` field (quiet level).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QuietLevel {
    /// `q=0` (default) — reply with status for every command.
    #[default]
    Normal,
    /// `q=1` — reply only on error.
    ErrorsOnly,
    /// `q=2` — never reply.
    Silent,
}

impl QuietLevel {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "0" => Some(Self::Normal),
            "1" => Some(Self::ErrorsOnly),
            "2" => Some(Self::Silent),
            _ => None,
        }
    }
}

/// Delete scope derived from `a=d,d=<scope>` combinations.
///
/// The protocol defines a large set of delete forms keyed by `d=`. The
/// parser keeps the original key=value pairs on [`RawGraphicsCommand`] for
/// callers that need to distinguish every variant; [`DeleteScope`] is a
/// coarse-grained view that covers the common cases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteScope {
    /// `a=d` with no id / no selector — delete **all** images.
    All,
    /// `a=d,i=<id>[,p=<pid>]` — delete a specific image/placement.
    ById {
        image_id: Option<u32>,
        placement_id: Option<u32>,
    },
}

/// The full parsed key=value map for a single APC command.
///
/// Fields the parser recognises are promoted to typed members; everything
/// else stays in [`Self::extras`]. The raw command is carried on every
/// [`GraphicsEvent`] so downstream callers can reach back to any field the
/// parser does not surface yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawGraphicsCommand {
    pub action: GraphicsAction,
    pub format: GraphicsFormat,
    pub medium: GraphicsMedium,
    pub image_id: Option<u32>,
    pub placement_id: Option<u32>,
    pub rows: Option<u32>,
    pub cols: Option<u32>,
    pub width_px: Option<u32>,
    pub height_px: Option<u32>,
    pub z_index: Option<i32>,
    pub more_chunks: bool,
    pub quiet: QuietLevel,
    /// Raw key=value pairs that the parser does not promote.
    pub extras: HashMap<String, String>,
}

impl RawGraphicsCommand {
    /// Empty command used as a placeholder (e.g. in tests). All fields are
    /// set to their protocol defaults; `action` is [`GraphicsAction::Query`]
    /// so you get a no-op reply if you accidentally use the default.
    pub fn empty() -> Self {
        Self {
            action: GraphicsAction::Query,
            format: GraphicsFormat::Default,
            medium: GraphicsMedium::Direct,
            image_id: None,
            placement_id: None,
            rows: None,
            cols: None,
            width_px: None,
            height_px: None,
            z_index: None,
            more_chunks: false,
            quiet: QuietLevel::Normal,
            extras: HashMap::new(),
        }
    }
}

/// Parse errors for a single APC command header.
#[derive(Debug, thiserror::Error)]
pub enum GraphicsParseError {
    /// The APC body is empty or missing the `G` introducer.
    #[error("kitty graphics: missing or invalid prefix")]
    MissingPrefix,
    /// A `k=v` pair was malformed (missing `=`, empty key, etc).
    #[error("kitty graphics: malformed key=value pair {pair:?}")]
    MalformedPair { pair: String },
    /// A known key carried a value the parser could not interpret.
    #[error("kitty graphics: invalid value {value:?} for key {key:?}")]
    InvalidValue { key: String, value: String },
    /// `a=` was missing or carried an unknown action byte.
    #[error("kitty graphics: missing or unknown action")]
    UnknownAction,
    /// Chunk buffer rejected a continuation (overflow).
    #[error(transparent)]
    Chunk(#[from] ChunkError),
}

/// Parse the header portion of a Kitty graphics APC body.
///
/// `body` is the bytes **after** the leading `G` prefix, up to but not
/// including the `;` that separates header from payload. When the APC
/// string is pure-header (e.g. `a=q`), pass the full tail.
pub fn parse_header(body: &[u8]) -> Result<RawGraphicsCommand, GraphicsParseError> {
    let s = std::str::from_utf8(body).map_err(|_| GraphicsParseError::MalformedPair {
        pair: format!("{:?}", body),
    })?;

    let mut cmd = RawGraphicsCommand::empty();
    let mut action_seen = false;

    // The protocol defines "empty header" as valid only after a transmit has
    // already bound an id — in practice agents always set at least `a=`. We
    // accept a completely empty header and leave `action_seen` false so the
    // caller returns `UnknownAction`.

    if s.is_empty() {
        return Err(GraphicsParseError::UnknownAction);
    }

    for pair in s.split(',') {
        if pair.is_empty() {
            // Silently skip empty segments (e.g. trailing comma).
            continue;
        }
        let (key, value) = match pair.split_once('=') {
            Some(kv) => kv,
            None => {
                return Err(GraphicsParseError::MalformedPair {
                    pair: pair.to_string(),
                });
            }
        };

        if key.is_empty() {
            return Err(GraphicsParseError::MalformedPair {
                pair: pair.to_string(),
            });
        }

        match key {
            "a" => {
                let b = value.as_bytes().first().copied().unwrap_or(0);
                cmd.action = GraphicsAction::from_byte(b).ok_or_else(|| {
                    GraphicsParseError::InvalidValue {
                        key: key.to_string(),
                        value: value.to_string(),
                    }
                })?;
                action_seen = true;
            }
            "f" => {
                cmd.format = GraphicsFormat::from_str(value).ok_or_else(|| {
                    GraphicsParseError::InvalidValue {
                        key: key.to_string(),
                        value: value.to_string(),
                    }
                })?;
            }
            "t" => {
                let b = value.as_bytes().first().copied().unwrap_or(0);
                cmd.medium = GraphicsMedium::from_byte(b).ok_or_else(|| {
                    GraphicsParseError::InvalidValue {
                        key: key.to_string(),
                        value: value.to_string(),
                    }
                })?;
            }
            "i" => {
                cmd.image_id =
                    Some(
                        value
                            .parse::<u32>()
                            .map_err(|_| GraphicsParseError::InvalidValue {
                                key: key.to_string(),
                                value: value.to_string(),
                            })?,
                    );
            }
            "p" => {
                cmd.placement_id =
                    Some(
                        value
                            .parse::<u32>()
                            .map_err(|_| GraphicsParseError::InvalidValue {
                                key: key.to_string(),
                                value: value.to_string(),
                            })?,
                    );
            }
            "r" => {
                cmd.rows =
                    Some(
                        value
                            .parse::<u32>()
                            .map_err(|_| GraphicsParseError::InvalidValue {
                                key: key.to_string(),
                                value: value.to_string(),
                            })?,
                    );
            }
            "c" => {
                cmd.cols =
                    Some(
                        value
                            .parse::<u32>()
                            .map_err(|_| GraphicsParseError::InvalidValue {
                                key: key.to_string(),
                                value: value.to_string(),
                            })?,
                    );
            }
            "s" => {
                cmd.width_px =
                    Some(
                        value
                            .parse::<u32>()
                            .map_err(|_| GraphicsParseError::InvalidValue {
                                key: key.to_string(),
                                value: value.to_string(),
                            })?,
                    );
            }
            "v" => {
                cmd.height_px =
                    Some(
                        value
                            .parse::<u32>()
                            .map_err(|_| GraphicsParseError::InvalidValue {
                                key: key.to_string(),
                                value: value.to_string(),
                            })?,
                    );
            }
            "z" => {
                cmd.z_index =
                    Some(
                        value
                            .parse::<i32>()
                            .map_err(|_| GraphicsParseError::InvalidValue {
                                key: key.to_string(),
                                value: value.to_string(),
                            })?,
                    );
            }
            "m" => {
                cmd.more_chunks = matches!(value, "1");
            }
            "q" => {
                cmd.quiet = QuietLevel::from_str(value).ok_or_else(|| {
                    GraphicsParseError::InvalidValue {
                        key: key.to_string(),
                        value: value.to_string(),
                    }
                })?;
            }
            _ => {
                cmd.extras.insert(key.to_string(), value.to_string());
            }
        }
    }

    if !action_seen {
        return Err(GraphicsParseError::UnknownAction);
    }

    Ok(cmd)
}

/// A single APC string's parse result.
#[derive(Debug)]
pub struct ParseOutput {
    /// An event produced by this APC string, if the transmission is
    /// complete. `None` means "chunk buffered, waiting for more".
    pub event: Option<GraphicsEvent>,
    /// Bytes that should be written back through the PTY as an APC response.
    /// Empty when suppressed by `q=1` (no error) or `q=2` (silent).
    pub response: Vec<u8>,
    /// `true` iff the APC body started with the Kitty `G` prefix — i.e.
    /// this was a graphics command we own, even if it produced no event
    /// (mid-chunk) and no response (quiet levels). Callers use this to
    /// decide whether to consume the APC from the vte parser.
    pub consumed: bool,
}

impl ParseOutput {
    fn empty() -> Self {
        Self {
            event: None,
            response: Vec::new(),
            consumed: false,
        }
    }
}

/// Stateful APC byte sink for the Kitty graphics protocol.
///
/// One parser lives on each `TherminalInterceptor`. The interceptor feeds
/// bytes via [`Self::push_byte`] for the full APC body (the leading `G`
/// plus header, `;`, payload) and then calls [`Self::finalize`] on `ST`.
/// [`Self::finalize`] returns a [`ParseOutput`] describing the event to
/// emit (if any) and the bytes to write back to the PTY.
#[derive(Debug, Default)]
pub struct KittyGraphicsParser {
    buf: Vec<u8>,
    chunk_buffer: ChunkBuffer,
}

impl KittyGraphicsParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one byte of the APC body. The caller must not include the APC
    /// introducer (`ESC _`) or terminator (`ESC \`) — those are the
    /// interceptor's responsibility.
    pub fn push_byte(&mut self, byte: u8) {
        self.buf.push(byte);
    }

    /// Finalize the current APC string and return a [`ParseOutput`].
    ///
    /// Always leaves the internal buffer empty afterwards so the next APC
    /// string starts clean. Malformed inputs produce a zeroed-out output
    /// (no event, no response) so a bad client cannot steer the terminal
    /// off a cliff.
    pub fn finalize(&mut self) -> ParseOutput {
        let body = std::mem::take(&mut self.buf);
        self.finalize_body(&body)
    }

    fn finalize_body(&mut self, body: &[u8]) -> ParseOutput {
        // Validate Kitty prefix.
        let tail = match body.first() {
            Some(&b) if b == KITTY_APC_PREFIX => &body[1..],
            _ => return ParseOutput::empty(),
        };

        // Split header / payload at the first ';'. Absent ';' means
        // "header-only command" (e.g. a=q, a=p).
        let (header_bytes, payload_bytes) = match tail.iter().position(|&b| b == b';') {
            Some(i) => (&tail[..i], &tail[i + 1..]),
            None => (tail, &[][..]),
        };

        let command = match parse_header(header_bytes) {
            Ok(c) => c,
            Err(err) => {
                return ParseOutput {
                    event: None,
                    response: response_for_parse_error(&err, None, QuietLevel::Normal),
                    // Still ours: the prefix matched, so consume the APC to
                    // avoid "unhandled APC" logs at the vte layer.
                    consumed: true,
                };
            }
        };

        let quiet = command.quiet;

        let mut out = match command.action {
            GraphicsAction::Transmit
            | GraphicsAction::TransmitAndDisplay
            | GraphicsAction::Frame => self.handle_transmit(command, payload_bytes, quiet),
            GraphicsAction::Put => self.handle_display(command, quiet),
            GraphicsAction::Delete => self.handle_delete(command, quiet),
            GraphicsAction::Query => self.handle_query(command, quiet),
        };
        // Any valid-prefix APC we handled is ours to consume, regardless of
        // whether it produced an event (mid-chunks) or a response (q=1/q=2).
        out.consumed = true;
        out
    }

    fn handle_transmit(
        &mut self,
        command: RawGraphicsCommand,
        payload: &[u8],
        quiet: QuietLevel,
    ) -> ParseOutput {
        let key = ChunkKey::from_command(&command);
        let more = command.more_chunks;

        match self
            .chunk_buffer
            .append(key, command.clone(), payload, more)
        {
            Ok(Some(done)) => {
                let display = matches!(command.action, GraphicsAction::TransmitAndDisplay);
                let event = GraphicsEvent::GraphicsTransmit {
                    image_id: done.header.image_id,
                    placement_id: done.header.placement_id,
                    format: done.header.format,
                    medium: done.header.medium,
                    width_px: done.header.width_px,
                    height_px: done.header.height_px,
                    payload: done.payload,
                    display,
                    command: done.header,
                };
                ParseOutput {
                    event: Some(event),
                    response: response_ok(command.image_id, quiet),
                    consumed: false,
                }
            }
            Ok(None) => ParseOutput {
                // Still accumulating — no event yet.
                event: None,
                // Don't send interim OK responses; real terminals only ack
                // when the full transfer completes.
                response: Vec::new(),
                consumed: false,
            },
            Err(err) => ParseOutput {
                event: None,
                response: response_for_parse_error(
                    &GraphicsParseError::Chunk(err),
                    command.image_id,
                    quiet,
                ),
                consumed: false,
            },
        }
    }

    fn handle_display(&mut self, command: RawGraphicsCommand, quiet: QuietLevel) -> ParseOutput {
        let event = GraphicsEvent::GraphicsDisplay {
            image_id: command.image_id,
            placement_id: command.placement_id,
            rows: command.rows,
            cols: command.cols,
            z_index: command.z_index,
            command: command.clone(),
        };
        ParseOutput {
            event: Some(event),
            response: response_ok(command.image_id, quiet),
            consumed: false,
        }
    }

    fn handle_delete(&mut self, command: RawGraphicsCommand, quiet: QuietLevel) -> ParseOutput {
        let scope = if command.image_id.is_some() || command.placement_id.is_some() {
            DeleteScope::ById {
                image_id: command.image_id,
                placement_id: command.placement_id,
            }
        } else {
            DeleteScope::All
        };
        // If the client was deleting a single image that happened to be in
        // flight, drop the chunk buffer entry too so we don't leak memory.
        if let DeleteScope::ById { .. } = scope {
            self.chunk_buffer.abort(ChunkKey::from_command(&command));
        } else {
            self.chunk_buffer.clear();
        }

        let event = GraphicsEvent::GraphicsDelete {
            scope,
            command: command.clone(),
        };
        ParseOutput {
            event: Some(event),
            response: response_ok(command.image_id, quiet),
            consumed: false,
        }
    }

    fn handle_query(&mut self, command: RawGraphicsCommand, quiet: QuietLevel) -> ParseOutput {
        let event = GraphicsEvent::GraphicsQuery {
            image_id: command.image_id,
            command: command.clone(),
        };
        ParseOutput {
            event: Some(event),
            response: response_ok(command.image_id, quiet),
            consumed: false,
        }
    }

    /// Number of in-flight multi-chunk entries. Diagnostics / tests.
    pub fn in_flight(&self) -> usize {
        self.chunk_buffer.in_flight()
    }
}

/// Build the APC envelope bytes: `ESC _ G <header> ; <message> ESC \`.
///
/// Used for protocol responses. `header` is the key=value header (e.g.
/// `"i=1"`) and `message` is the status string (`"OK"`, `"ENOENT"`, ...).
pub fn format_response(header: &str, message: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(header.len() + message.len() + 6);
    out.extend_from_slice(b"\x1b_G");
    out.extend_from_slice(header.as_bytes());
    out.push(b';');
    out.extend_from_slice(message.as_bytes());
    out.extend_from_slice(b"\x1b\\");
    out
}

/// Build an `OK` response, suppressed by the quiet level.
fn response_ok(image_id: Option<u32>, quiet: QuietLevel) -> Vec<u8> {
    if matches!(quiet, QuietLevel::Silent | QuietLevel::ErrorsOnly) {
        return Vec::new();
    }
    let header = match image_id {
        Some(id) => format!("i={}", id),
        None => String::new(),
    };
    format_response(&header, "OK")
}

/// Build an error response from a parse error, respecting the quiet level.
///
/// `image_id` is optional — the caller passes `None` if the parse failed
/// before we could even extract the `i=` field.
fn response_for_parse_error(
    err: &GraphicsParseError,
    image_id: Option<u32>,
    quiet: QuietLevel,
) -> Vec<u8> {
    if matches!(quiet, QuietLevel::Silent) {
        return Vec::new();
    }
    let code = match err {
        GraphicsParseError::MissingPrefix
        | GraphicsParseError::MalformedPair { .. }
        | GraphicsParseError::InvalidValue { .. }
        | GraphicsParseError::UnknownAction => "EINVAL",
        GraphicsParseError::Chunk(_) => "ENOMEM",
    };
    let header = match image_id {
        Some(id) => format!("i={}", id),
        None => String::new(),
    };
    let msg = format!("{}:{}", code, err);
    format_response(&header, &msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::GraphicsEvent;

    fn drive(parser: &mut KittyGraphicsParser, s: &str) -> ParseOutput {
        for b in s.as_bytes() {
            parser.push_byte(*b);
        }
        parser.finalize()
    }

    // -- header parsing --------------------------------------------------------

    #[test]
    fn parse_header_transmit_display() {
        let cmd = parse_header(b"a=T,f=100,i=42,s=640,v=480").unwrap();
        assert_eq!(cmd.action, GraphicsAction::TransmitAndDisplay);
        assert_eq!(cmd.format, GraphicsFormat::Png);
        assert_eq!(cmd.image_id, Some(42));
        assert_eq!(cmd.width_px, Some(640));
        assert_eq!(cmd.height_px, Some(480));
    }

    #[test]
    fn parse_header_malformed_returns_err() {
        // Missing = in a pair.
        assert!(parse_header(b"a=t,badpair").is_err());
    }

    #[test]
    fn parse_header_unknown_action_errors() {
        assert!(parse_header(b"a=x").is_err());
    }

    #[test]
    fn parse_header_missing_action_errors() {
        assert!(parse_header(b"i=1").is_err());
    }

    #[test]
    fn parse_header_unknown_key_goes_to_extras() {
        let cmd = parse_header(b"a=t,Z=hello").unwrap();
        assert_eq!(cmd.extras.get("Z"), Some(&"hello".to_string()));
    }

    #[test]
    fn parse_header_quiet_levels() {
        assert_eq!(parse_header(b"a=t,q=0").unwrap().quiet, QuietLevel::Normal);
        assert_eq!(
            parse_header(b"a=t,q=1").unwrap().quiet,
            QuietLevel::ErrorsOnly
        );
        assert_eq!(parse_header(b"a=t,q=2").unwrap().quiet, QuietLevel::Silent);
    }

    // -- single/multi chunk transmit ------------------------------------------

    #[test]
    fn single_chunk_transmit_emits_event_and_ok_response() {
        let mut p = KittyGraphicsParser::new();
        let out = drive(&mut p, "Ga=t,f=100,i=1;ZmFrZQ=="); // base64("fake")
        let event = out.event.expect("event expected");
        match event {
            GraphicsEvent::GraphicsTransmit {
                image_id,
                payload,
                display,
                ..
            } => {
                assert_eq!(image_id, Some(1));
                assert!(!display);
                assert_eq!(payload, b"ZmFrZQ==");
            }
            other => panic!("unexpected event {:?}", other),
        }
        assert!(!out.response.is_empty(), "q=0 ⇒ response expected");
        assert!(out.response.windows(2).any(|w| w == b"OK"));
    }

    #[test]
    fn multi_chunk_transmit_emits_event_on_final_chunk() {
        let mut p = KittyGraphicsParser::new();

        let out1 = drive(&mut p, "Ga=t,f=100,i=9,m=1;AAAA");
        assert!(out1.event.is_none(), "no event until m=0");
        assert!(
            out1.response.is_empty(),
            "no interim response for mid-chunks"
        );

        let out2 = drive(&mut p, "Ga=t,i=9,m=1;BBBB");
        assert!(out2.event.is_none());
        assert!(out2.response.is_empty());

        let out3 = drive(&mut p, "Ga=t,i=9,m=0;CCCC");
        let event = out3.event.expect("final chunk should emit");
        match event {
            GraphicsEvent::GraphicsTransmit { payload, .. } => {
                assert_eq!(payload, b"AAAABBBBCCCC");
            }
            other => panic!("unexpected event {:?}", other),
        }
        assert!(!out3.response.is_empty());
        assert_eq!(p.in_flight(), 0);
    }

    #[test]
    fn transmit_and_display_sets_display_flag() {
        let mut p = KittyGraphicsParser::new();
        let out = drive(&mut p, "Ga=T,f=100,i=2;ZmFrZQ==");
        match out.event.unwrap() {
            GraphicsEvent::GraphicsTransmit { display, .. } => assert!(display),
            other => panic!("unexpected {:?}", other),
        }
    }

    // -- display / delete / query --------------------------------------------

    #[test]
    fn display_command_emits_event() {
        let mut p = KittyGraphicsParser::new();
        let out = drive(&mut p, "Ga=p,i=5,r=10,c=20,z=3");
        match out.event.unwrap() {
            GraphicsEvent::GraphicsDisplay {
                image_id,
                rows,
                cols,
                z_index,
                ..
            } => {
                assert_eq!(image_id, Some(5));
                assert_eq!(rows, Some(10));
                assert_eq!(cols, Some(20));
                assert_eq!(z_index, Some(3));
            }
            other => panic!("unexpected {:?}", other),
        }
    }

    #[test]
    fn delete_by_id_emits_event() {
        let mut p = KittyGraphicsParser::new();
        let out = drive(&mut p, "Ga=d,i=7");
        match out.event.unwrap() {
            GraphicsEvent::GraphicsDelete { scope, .. } => match scope {
                DeleteScope::ById { image_id, .. } => assert_eq!(image_id, Some(7)),
                other => panic!("expected ById, got {:?}", other),
            },
            other => panic!("unexpected {:?}", other),
        }
    }

    #[test]
    fn delete_all_emits_event() {
        let mut p = KittyGraphicsParser::new();
        let out = drive(&mut p, "Ga=d");
        match out.event.unwrap() {
            GraphicsEvent::GraphicsDelete { scope, .. } => assert_eq!(scope, DeleteScope::All),
            other => panic!("unexpected {:?}", other),
        }
    }

    #[test]
    fn feature_query_emits_event_and_ok() {
        let mut p = KittyGraphicsParser::new();
        let out = drive(&mut p, "Ga=q,i=1");
        match &out.event {
            Some(GraphicsEvent::GraphicsQuery {
                image_id: Some(1), ..
            }) => {}
            other => panic!("unexpected {:?}", other),
        }
        // The feature query probe `\x1b_Gi=1,a=q;\x1b\\` must reply `OK`.
        let expected = format_response("i=1", "OK");
        assert_eq!(out.response, expected);
    }

    #[test]
    fn feature_query_canonical_probe() {
        // Exact canonical probe from the Kitty docs.
        let mut p = KittyGraphicsParser::new();
        let out = drive(&mut p, "Gi=1,a=q;");
        assert!(matches!(
            out.event,
            Some(GraphicsEvent::GraphicsQuery { .. })
        ));
        assert_eq!(out.response, format_response("i=1", "OK"));
    }

    // -- quiet-level gating --------------------------------------------------

    #[test]
    fn quiet_level_0_emits_response() {
        let mut p = KittyGraphicsParser::new();
        let out = drive(&mut p, "Ga=t,f=100,i=1,q=0;ZmFrZQ==");
        assert!(!out.response.is_empty());
    }

    #[test]
    fn quiet_level_2_suppresses_response() {
        let mut p = KittyGraphicsParser::new();
        let out = drive(&mut p, "Ga=t,f=100,i=1,q=2;ZmFrZQ==");
        assert!(out.response.is_empty());
        assert!(out.event.is_some(), "event still emitted");
    }

    #[test]
    fn quiet_level_1_suppresses_ok_but_allows_errors() {
        let mut p = KittyGraphicsParser::new();
        // q=1 with a well-formed command ⇒ no response.
        let out_ok = drive(&mut p, "Ga=q,i=1,q=1");
        assert!(out_ok.response.is_empty());
        // q=1 with a malformed command ⇒ error response.
        let out_err = drive(&mut p, "Ga=bogus,q=1");
        assert!(!out_err.response.is_empty());
    }

    // -- malformed inputs ----------------------------------------------------

    #[test]
    fn malformed_apc_produces_no_event() {
        let mut p = KittyGraphicsParser::new();
        // Wrong prefix — not a Kitty graphics command at all.
        let out = drive(&mut p, "NotKittyAtAll");
        assert!(out.event.is_none());
        assert!(out.response.is_empty());
    }

    #[test]
    fn malformed_header_does_not_panic() {
        let mut p = KittyGraphicsParser::new();
        // Empty body after the G prefix.
        let out = drive(&mut p, "G");
        assert!(out.event.is_none());
    }

    // -- overflow ------------------------------------------------------------

    #[test]
    fn chunk_buffer_overflow_drops_and_errors() {
        let mut p = KittyGraphicsParser::new();

        // Prime with a large first chunk just under the cap.
        let big = vec![b'A'; CHUNK_BUFFER_HARD_CAP - 1];
        let mut head = b"Ga=t,i=99,m=1;".to_vec();
        head.extend_from_slice(&big);
        let out1 = {
            for b in &head {
                p.push_byte(*b);
            }
            p.finalize()
        };
        assert!(out1.event.is_none());
        assert_eq!(p.in_flight(), 1);

        // Second chunk tips us past the cap — must drop + error.
        let out2 = drive(&mut p, "Ga=t,i=99,m=1;BB");
        assert!(out2.event.is_none());
        assert!(!out2.response.is_empty(), "overflow ⇒ error response");
        assert_eq!(p.in_flight(), 0, "entry dropped on overflow");
    }

    // -- response formatter --------------------------------------------------

    #[test]
    fn format_response_wraps_in_apc_envelope() {
        let r = format_response("i=1", "OK");
        assert!(r.starts_with(b"\x1b_G"));
        assert!(r.ends_with(b"\x1b\\"));
        let middle = &r[3..r.len() - 2];
        assert_eq!(middle, b"i=1;OK");
    }

    #[test]
    fn parse_body_exposes_raw_command_on_event() {
        let mut p = KittyGraphicsParser::new();
        let out = drive(&mut p, "Ga=p,i=42,p=1,r=10,c=20,z=5");
        match out.event.unwrap() {
            GraphicsEvent::GraphicsDisplay { command, .. } => {
                assert_eq!(command.placement_id, Some(1));
                assert_eq!(command.z_index, Some(5));
            }
            other => panic!("unexpected {:?}", other),
        }
    }
}
