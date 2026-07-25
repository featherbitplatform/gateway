//! Response body encode/decode helpers shared by body-mutating plugins
//! (response-rewrite, gzip/brotli compression, loggers that include response
//! bodies).
//!
//! Mirrors APISIX's `content-decode.lua` semantics: upstream bodies are
//! decoded before text filters run, then optionally re-encoded on the way
//! out. HTTP `deflate` is the zlib-wrapped format (RFC 1950), not raw
//! DEFLATE — this module follows that convention.
//!
//! # Body-mutation convention for plugin authors
//!
//! After replacing a response body you MUST:
//! 1. remove the `content-length` header — the server layer recomputes it
//!    from the final body; and
//! 2. remove `content-encoding` (if you left the body decoded) or set it to
//!    [`ContentEncoding::header_value`] of the codec you re-encoded with.
//!
//! Getting either wrong yields truncated or garbled responses at the client:
//! a stale `content-length` truncates or over-reads the stream, and a stale
//! `content-encoding` makes the client "decompress" plain bytes.

use std::io::{Read, Write};

use bytes::Bytes;
use flate2::read::{GzDecoder, ZlibDecoder};
use flate2::write::{GzEncoder, ZlibEncoder};
use flate2::Compression;

/// Buffer size used for brotli's streaming reader/writer, matching the 4 KiB
/// chunks flate2 uses internally.
const BROTLI_BUFFER_SIZE: usize = 4096;

/// Brotli window size (log2). 22 is the codec's common default and what
/// nginx's brotli module ships with.
const BROTLI_LGWIN: u32 = 22;

/// Content-Encoding values the gateway can decode/encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentEncoding {
    Gzip,
    /// HTTP "deflate": zlib-wrapped DEFLATE (RFC 1950).
    Deflate,
    Brotli,
}

impl ContentEncoding {
    /// Parses a `Content-Encoding` header value ("gzip", "deflate", "br"),
    /// case-insensitively and ignoring surrounding whitespace.
    ///
    /// `Ok(None)` means "identity" or an empty value — the body is not
    /// encoded and there is nothing to do. `Err` carries the unsupported
    /// encoding name; the caller decides whether to skip the body untouched
    /// or fail the request.
    pub fn parse(value: &str) -> Result<Option<Self>, String> {
        let normalized = value.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "" | "identity" => Ok(None),
            "gzip" => Ok(Some(Self::Gzip)),
            "deflate" => Ok(Some(Self::Deflate)),
            "br" => Ok(Some(Self::Brotli)),
            other => Err(format!("unsupported content-encoding: {}", other)),
        }
    }

    /// The canonical header value ("gzip", "deflate", "br") — what plugins
    /// should write into `content-encoding` after re-encoding a body.
    pub fn header_value(&self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Deflate => "deflate",
            Self::Brotli => "br",
        }
    }
}

/// Decodes a body compressed with `encoding`. Errors on corrupt or
/// truncated data, with a message naming the codec.
pub fn decode(encoding: &ContentEncoding, body: &Bytes) -> Result<Bytes, String> {
    let mut out = Vec::new();
    match encoding {
        ContentEncoding::Gzip => {
            GzDecoder::new(body.as_ref())
                .read_to_end(&mut out)
                .map_err(|e| format!("gzip decode error: {}", e))?;
        }
        ContentEncoding::Deflate => {
            ZlibDecoder::new(body.as_ref())
                .read_to_end(&mut out)
                .map_err(|e| format!("deflate decode error: {}", e))?;
        }
        ContentEncoding::Brotli => {
            brotli::Decompressor::new(body.as_ref(), BROTLI_BUFFER_SIZE)
                .read_to_end(&mut out)
                .map_err(|e| format!("brotli decode error: {}", e))?;
        }
    }
    Ok(Bytes::from(out))
}

/// Encodes a body with `encoding`. `level` is clamped to the codec's valid
/// range (gzip/deflate 0-9, brotli 0-11), so out-of-range config values
/// degrade to maximum compression instead of failing.
pub fn encode(encoding: &ContentEncoding, body: &Bytes, level: u32) -> Result<Bytes, String> {
    match encoding {
        ContentEncoding::Gzip => {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::new(level.min(9)));
            encoder
                .write_all(body)
                .and_then(|_| encoder.finish())
                .map(Bytes::from)
                .map_err(|e| format!("gzip encode error: {}", e))
        }
        ContentEncoding::Deflate => {
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(level.min(9)));
            encoder
                .write_all(body)
                .and_then(|_| encoder.finish())
                .map(Bytes::from)
                .map_err(|e| format!("deflate encode error: {}", e))
        }
        ContentEncoding::Brotli => {
            let mut out = Vec::new();
            {
                let mut writer = brotli::CompressorWriter::new(
                    &mut out,
                    BROTLI_BUFFER_SIZE,
                    level.min(11),
                    BROTLI_LGWIN,
                );
                writer
                    .write_all(body)
                    .and_then(|_| writer.flush())
                    .map_err(|e| format!("brotli encode error: {}", e))?;
                // Dropping the writer finalizes the brotli stream into `out`.
            }
            Ok(Bytes::from(out))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &[u8] = b"the quick brown fox jumps over the lazy dog, \
                            repeated so compression has something to chew on: \
                            the quick brown fox jumps over the lazy dog";

    #[test]
    fn test_content_codec_round_trip_gzip() {
        let body = Bytes::from_static(SAMPLE);
        let encoded = encode(&ContentEncoding::Gzip, &body, 6).unwrap();
        assert_ne!(encoded, body);
        let decoded = decode(&ContentEncoding::Gzip, &encoded).unwrap();
        assert_eq!(decoded, body);
    }

    #[test]
    fn test_content_codec_round_trip_deflate() {
        let body = Bytes::from_static(SAMPLE);
        let encoded = encode(&ContentEncoding::Deflate, &body, 6).unwrap();
        assert_ne!(encoded, body);
        let decoded = decode(&ContentEncoding::Deflate, &encoded).unwrap();
        assert_eq!(decoded, body);
    }

    #[test]
    fn test_content_codec_round_trip_brotli() {
        let body = Bytes::from_static(SAMPLE);
        let encoded = encode(&ContentEncoding::Brotli, &body, 6).unwrap();
        assert_ne!(encoded, body);
        let decoded = decode(&ContentEncoding::Brotli, &encoded).unwrap();
        assert_eq!(decoded, body);
    }

    #[test]
    fn test_content_codec_round_trip_empty_body() {
        for encoding in [
            ContentEncoding::Gzip,
            ContentEncoding::Deflate,
            ContentEncoding::Brotli,
        ] {
            let encoded = encode(&encoding, &Bytes::new(), 6).unwrap();
            let decoded = decode(&encoding, &encoded).unwrap();
            assert!(decoded.is_empty(), "{:?} empty round trip", encoding);
        }
    }

    #[test]
    fn test_content_codec_decode_corrupt_errors() {
        let garbage = Bytes::from_static(b"\x00\x01definitely not a compressed stream\xff\xfe");
        for encoding in [
            ContentEncoding::Gzip,
            ContentEncoding::Deflate,
            ContentEncoding::Brotli,
        ] {
            let result = decode(&encoding, &garbage);
            assert!(result.is_err(), "{:?} should reject corrupt data", encoding);
        }
    }

    #[test]
    fn test_content_codec_decode_truncated_gzip_errors() {
        let body = Bytes::from_static(SAMPLE);
        let encoded = encode(&ContentEncoding::Gzip, &body, 6).unwrap();
        let truncated = encoded.slice(..encoded.len() / 2);
        assert!(decode(&ContentEncoding::Gzip, &truncated).is_err());
    }

    #[test]
    fn test_content_codec_parse() {
        assert_eq!(
            ContentEncoding::parse("gzip"),
            Ok(Some(ContentEncoding::Gzip))
        );
        assert_eq!(
            ContentEncoding::parse("GZIP"),
            Ok(Some(ContentEncoding::Gzip))
        );
        assert_eq!(
            ContentEncoding::parse(" gzip "),
            Ok(Some(ContentEncoding::Gzip))
        );
        assert_eq!(
            ContentEncoding::parse("br"),
            Ok(Some(ContentEncoding::Brotli))
        );
        assert_eq!(
            ContentEncoding::parse("deflate"),
            Ok(Some(ContentEncoding::Deflate))
        );
        assert_eq!(ContentEncoding::parse("identity"), Ok(None));
        assert_eq!(ContentEncoding::parse("Identity"), Ok(None));
        assert_eq!(ContentEncoding::parse(""), Ok(None));
        assert_eq!(ContentEncoding::parse("   "), Ok(None));
        assert!(ContentEncoding::parse("x-unknown").is_err());
        // Multi-codec values are unsupported, not silently misparsed.
        assert!(ContentEncoding::parse("gzip, br").is_err());
    }

    #[test]
    fn test_content_codec_header_value() {
        assert_eq!(ContentEncoding::Gzip.header_value(), "gzip");
        assert_eq!(ContentEncoding::Deflate.header_value(), "deflate");
        assert_eq!(ContentEncoding::Brotli.header_value(), "br");
    }

    #[test]
    fn test_content_codec_level_clamped() {
        let body = Bytes::from_static(SAMPLE);
        for encoding in [
            ContentEncoding::Gzip,
            ContentEncoding::Deflate,
            ContentEncoding::Brotli,
        ] {
            // Out-of-range level must clamp, not panic or error.
            let encoded = encode(&encoding, &body, 99).unwrap();
            let decoded = decode(&encoding, &encoded).unwrap();
            assert_eq!(decoded, body, "{:?} with clamped level", encoding);
        }
    }
}
