//! A small, strict `bytes` range grammar and normalizer, written by hand
//! rather than delegated to a maintained crate.
//!
//! Two maintained candidates were evaluated and rejected for this exact
//! contract: `headers::Range::satisfiable_ranges` does not clamp, filter,
//! or classify malformed-vs-unsatisfiable input the way RFC 9110 requires,
//! and `http-range-header` rejects overlapping ranges (instead of
//! coalescing them) and rejects a suffix range longer than the
//! representation (instead of clamping it to the whole representation).
//! Everything else in this server (`ETag`/date parsing, `Cache-Control`,
//! MIME sniffing, path decoding) still uses a maintained crate; this
//! module is the one deliberately narrow exception, and it is limited to
//! parsing and set normalization — no header rendering, no I/O.

use http::HeaderMap;
use std::str::FromStr;

/// Reject more than this many raw range-specs before normalization.
pub const MAX_RAW_RANGES: usize = 64;
/// Reject more than this many ranges after sorting and coalescing.
pub const MAX_COALESCED_RANGES: usize = 16;

/// The `Range` field line(s) were present but did not parse as a
/// syntactically valid `bytes` ranges-specifier. Maps to `400`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MalformedRange;

/// One syntactically valid, not-yet-clamped `byte-range-spec`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawByteRange {
    /// `first-last`, inclusive, with `first <= last`.
    Closed { first: u64, last: u64 },
    /// `first-`: from `first` through the end of the representation.
    Open { first: u64 },
    /// `-length`: the last `length` bytes of the representation.
    Suffix { length: u64 },
}

/// What reading every `Range` field line (there may be more than one)
/// produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeHeader {
    /// No `Range` field line at all.
    Absent,
    /// A `Range` field line was present but named a unit other than
    /// `bytes`. Per RFC 9110 §14.2, an unrecognized unit is ignored: the
    /// caller serves the full representation rather than rejecting the
    /// request.
    UnsupportedUnit,
    /// One or more syntactically valid `bytes` range-specs, combined in
    /// field order across every `Range` field line.
    Bytes(Vec<RawByteRange>),
}

/// Read every `Range` field line, in field order, and parse them as one
/// combined `bytes` ranges-specifier. RFC 9110 §5.3 treats multiple field
/// lines with the same name as equivalent to one field line with their
/// values joined by commas; that join happens here, before grammar
/// parsing, using the header lines exactly as `HeaderMap` preserves them
/// (in insertion order).
pub fn read_range_header(headers: &HeaderMap) -> Result<RangeHeader, MalformedRange> {
    let mut values = headers.get_all(http::header::RANGE).iter();
    let Some(first) = values.next() else {
        return Ok(RangeHeader::Absent);
    };

    let mut combined_spec = String::new();
    for (i, value) in std::iter::once(first).chain(values).enumerate() {
        let text = value.to_str().map_err(|_| MalformedRange)?;
        let (unit, spec) = text.split_once('=').ok_or(MalformedRange)?;
        if !unit.trim().eq_ignore_ascii_case("bytes") {
            if i == 0 {
                return Ok(RangeHeader::UnsupportedUnit);
            }
            // A later field line switching units mid-combination can
            // never be a valid single ranges-specifier.
            return Err(MalformedRange);
        }
        if i > 0 {
            combined_spec.push(',');
        }
        combined_spec.push_str(spec);
    }

    parse_byte_range_set(&combined_spec).map(RangeHeader::Bytes)
}

fn parse_byte_range_set(spec: &str) -> Result<Vec<RawByteRange>, MalformedRange> {
    spec.split(',').map(|one| parse_one(one.trim())).collect()
}

fn parse_one(spec: &str) -> Result<RawByteRange, MalformedRange> {
    let (first_text, last_text) = spec.split_once('-').ok_or(MalformedRange)?;
    if first_text.is_empty() {
        let length = parse_u64(last_text)?;
        return Ok(RawByteRange::Suffix { length });
    }
    let first = parse_u64(first_text)?;
    if last_text.is_empty() {
        return Ok(RawByteRange::Open { first });
    }
    let last = parse_u64(last_text)?;
    if first > last {
        return Err(MalformedRange);
    }
    Ok(RawByteRange::Closed { first, last })
}

/// Strict, checked `u64` decimal parsing: no sign, no whitespace, no
/// leading `+`, and overflow is rejected rather than wrapping.
fn parse_u64(text: &str) -> Result<u64, MalformedRange> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(MalformedRange);
    }
    u64::from_str(text).map_err(|_| MalformedRange)
}

/// One clamped, satisfiable, inclusive byte range within a representation
/// of some known length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

impl ByteRange {
    /// The number of bytes this range selects. Never zero: `start <= end`
    /// is an invariant of every constructor in this module.
    pub fn byte_len(&self) -> u64 {
        self.end - self.start + 1
    }
}

/// The result of normalizing a syntactically valid raw range set against
/// a representation of length `len`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Select {
    /// Every raw range was outside the representation (including any
    /// request against an empty representation), the raw set was too
    /// large before normalization, or the coalesced set was too large
    /// afterward. Maps to `416`.
    Unsatisfiable,
    /// One or more sorted, coalesced, satisfiable ranges, clamped to
    /// `len`. Never empty and never longer than
    /// [`MAX_COALESCED_RANGES`].
    Ranges(Vec<ByteRange>),
}

/// Normalize `raw` against a representation of length `len`: clamp each
/// range to the representation, drop unsatisfiable members, then sort and
/// coalesce overlapping or adjacent ranges.
pub fn select(raw: &[RawByteRange], len: u64) -> Select {
    if raw.is_empty() || raw.len() > MAX_RAW_RANGES {
        return Select::Unsatisfiable;
    }

    let mut satisfiable: Vec<ByteRange> =
        raw.iter().copied().filter_map(|r| clamp(r, len)).collect();
    if satisfiable.is_empty() {
        return Select::Unsatisfiable;
    }

    satisfiable.sort_by_key(|r| r.start);
    let coalesced = coalesce(satisfiable);
    if coalesced.len() > MAX_COALESCED_RANGES {
        return Select::Unsatisfiable;
    }

    Select::Ranges(coalesced)
}

fn clamp(raw: RawByteRange, len: u64) -> Option<ByteRange> {
    match raw {
        RawByteRange::Closed { first, last } => (first < len).then(|| ByteRange {
            start: first,
            end: last.min(len - 1),
        }),
        RawByteRange::Open { first } => (first < len).then(|| ByteRange {
            start: first,
            end: len - 1,
        }),
        RawByteRange::Suffix { length } => {
            if length == 0 || len == 0 {
                return None;
            }
            let n = length.min(len);
            Some(ByteRange {
                start: len - n,
                end: len - 1,
            })
        }
    }
}

/// Merge ranges that overlap or touch (`end + 1 == next.start`) into a
/// single range. `sorted` must already be sorted by `start`.
fn coalesce(sorted: Vec<ByteRange>) -> Vec<ByteRange> {
    let mut out: Vec<ByteRange> = Vec::with_capacity(sorted.len());
    for r in sorted {
        match out.last_mut() {
            Some(prev) if r.start <= prev.end.saturating_add(1) => {
                prev.end = prev.end.max(r.end);
            }
            _ => out.push(r),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn headers_with_ranges(values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append(http::header::RANGE, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn absent_when_no_range_header() {
        assert_eq!(
            read_range_header(&HeaderMap::new()),
            Ok(RangeHeader::Absent)
        );
    }

    #[test]
    fn unsupported_unit_is_reported_distinctly() {
        assert_eq!(
            read_range_header(&headers_with_ranges(&["items=0-2"])),
            Ok(RangeHeader::UnsupportedUnit)
        );
    }

    #[test]
    fn unit_match_is_case_insensitive() {
        assert_eq!(
            read_range_header(&headers_with_ranges(&["BYTES=0-2"])),
            Ok(RangeHeader::Bytes(vec![RawByteRange::Closed {
                first: 0,
                last: 2
            }]))
        );
    }

    #[test]
    fn parses_closed_open_and_suffix_forms() {
        assert_eq!(
            read_range_header(&headers_with_ranges(&["bytes=0-99,100-,-50"])),
            Ok(RangeHeader::Bytes(vec![
                RawByteRange::Closed { first: 0, last: 99 },
                RawByteRange::Open { first: 100 },
                RawByteRange::Suffix { length: 50 },
            ]))
        );
    }

    #[test]
    fn duplicate_range_field_lines_combine_in_field_order() {
        assert_eq!(
            read_range_header(&headers_with_ranges(&["bytes=0-3", "bytes=10-13"])),
            Ok(RangeHeader::Bytes(vec![
                RawByteRange::Closed { first: 0, last: 3 },
                RawByteRange::Closed {
                    first: 10,
                    last: 13
                },
            ]))
        );
    }

    #[test]
    fn reversed_range_is_malformed() {
        assert_eq!(
            read_range_header(&headers_with_ranges(&["bytes=10-5"])),
            Err(MalformedRange)
        );
    }

    #[test]
    fn non_digit_range_is_malformed() {
        assert_eq!(
            read_range_header(&headers_with_ranges(&["bytes=a-b"])),
            Err(MalformedRange)
        );
    }

    #[test]
    fn overflowing_u64_is_malformed() {
        assert_eq!(
            read_range_header(&headers_with_ranges(&["bytes=99999999999999999999-"])),
            Err(MalformedRange)
        );
    }

    #[test]
    fn empty_spec_is_malformed() {
        assert_eq!(
            read_range_header(&headers_with_ranges(&["bytes="])),
            Err(MalformedRange)
        );
        assert_eq!(
            read_range_header(&headers_with_ranges(&["bytes=0-5,"])),
            Err(MalformedRange)
        );
    }

    #[test]
    fn missing_unit_separator_is_malformed() {
        assert_eq!(
            read_range_header(&headers_with_ranges(&["bytes"])),
            Err(MalformedRange)
        );
    }

    #[test]
    fn select_closed_range_within_bounds() {
        let raw = [RawByteRange::Closed { first: 0, last: 9 }];
        assert_eq!(
            select(&raw, 100),
            Select::Ranges(vec![ByteRange { start: 0, end: 9 }])
        );
    }

    #[test]
    fn select_clamps_closed_range_end_to_length() {
        let raw = [RawByteRange::Closed {
            first: 90,
            last: 999,
        }];
        assert_eq!(
            select(&raw, 100),
            Select::Ranges(vec![ByteRange { start: 90, end: 99 }])
        );
    }

    #[test]
    fn select_open_range_runs_to_end() {
        let raw = [RawByteRange::Open { first: 95 }];
        assert_eq!(
            select(&raw, 100),
            Select::Ranges(vec![ByteRange { start: 95, end: 99 }])
        );
    }

    #[test]
    fn select_suffix_range() {
        let raw = [RawByteRange::Suffix { length: 10 }];
        assert_eq!(
            select(&raw, 100),
            Select::Ranges(vec![ByteRange { start: 90, end: 99 }])
        );
    }

    #[test]
    fn select_oversized_suffix_is_the_entire_representation() {
        let raw = [RawByteRange::Suffix { length: 1_000_000 }];
        assert_eq!(
            select(&raw, 100),
            Select::Ranges(vec![ByteRange { start: 0, end: 99 }])
        );
    }

    #[test]
    fn select_zero_length_suffix_is_unsatisfiable() {
        let raw = [RawByteRange::Suffix { length: 0 }];
        assert_eq!(select(&raw, 100), Select::Unsatisfiable);
    }

    #[test]
    fn select_empty_representation_is_always_unsatisfiable() {
        assert_eq!(
            select(&[RawByteRange::Closed { first: 0, last: 0 }], 0),
            Select::Unsatisfiable
        );
        assert_eq!(
            select(&[RawByteRange::Open { first: 0 }], 0),
            Select::Unsatisfiable
        );
        assert_eq!(
            select(&[RawByteRange::Suffix { length: 10 }], 0),
            Select::Unsatisfiable
        );
    }

    #[test]
    fn select_drops_unsatisfiable_members_and_keeps_satisfiable_ones() {
        let raw = [
            RawByteRange::Closed {
                first: 1000,
                last: 2000,
            }, // out of bounds
            RawByteRange::Closed { first: 0, last: 9 },
        ];
        assert_eq!(
            select(&raw, 100),
            Select::Ranges(vec![ByteRange { start: 0, end: 9 }])
        );
    }

    #[test]
    fn select_all_unsatisfiable_members_is_unsatisfiable() {
        let raw = [RawByteRange::Closed {
            first: 1000,
            last: 2000,
        }];
        assert_eq!(select(&raw, 100), Select::Unsatisfiable);
    }

    #[test]
    fn select_sorts_and_coalesces_overlapping_ranges() {
        let raw = [
            RawByteRange::Closed {
                first: 50,
                last: 59,
            },
            RawByteRange::Closed { first: 0, last: 9 },
            RawByteRange::Closed { first: 5, last: 15 },
        ];
        assert_eq!(
            select(&raw, 100),
            Select::Ranges(vec![
                ByteRange { start: 0, end: 15 },
                ByteRange { start: 50, end: 59 },
            ])
        );
    }

    #[test]
    fn select_coalesces_adjacent_ranges() {
        let raw = [
            RawByteRange::Closed { first: 0, last: 9 },
            RawByteRange::Closed {
                first: 10,
                last: 19,
            },
        ];
        assert_eq!(
            select(&raw, 100),
            Select::Ranges(vec![ByteRange { start: 0, end: 19 }])
        );
    }

    #[test]
    fn select_rejects_more_than_64_raw_ranges() {
        let raw: Vec<RawByteRange> = (0..65)
            .map(|i| RawByteRange::Closed {
                first: i * 2,
                last: i * 2,
            })
            .collect();
        assert_eq!(select(&raw, 1000), Select::Unsatisfiable);
    }

    #[test]
    fn select_rejects_more_than_16_coalesced_ranges() {
        // 17 disjoint single-byte ranges, none adjacent, so none coalesce.
        let raw: Vec<RawByteRange> = (0..17)
            .map(|i| RawByteRange::Closed {
                first: i * 2,
                last: i * 2,
            })
            .collect();
        assert_eq!(select(&raw, 1000), Select::Unsatisfiable);
    }
}
