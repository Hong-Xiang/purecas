//! Deterministic, read-only HTML directory listings.
//!
//! Labels are HTML-escaped with `askama_escape` (the escaping routine
//! extracted from the `askama` template engine); `href` path segments are
//! percent-encoded from the raw Unix directory-entry bytes with
//! `percent_encoding`, so non-UTF-8 names round-trip correctly without ever
//! being (re)interpreted as extra path separators.

use askama_escape::escape_html;
use percent_encoding::{percent_encode, AsciiSet, CONTROLS};

/// One entry rendered into a listing.
pub struct Entry {
    pub raw_name: Vec<u8>,
    pub is_dir: bool,
}

/// Bytes that must be percent-encoded in an `href` path segment beyond the
/// non-ASCII bytes `percent_encode` already always encodes: everything
/// that is meaningful in an HTML attribute value, a URL, or a path
/// separator, so the raw entry-name bytes never need separate HTML
/// escaping once encoded.
const HREF_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'\'')
    .add(b'`')
    .add(b'<')
    .add(b'>')
    .add(b'&')
    .add(b'#')
    .add(b'?')
    .add(b'/')
    .add(b'%');

/// Render a full listing document. `entries` is sorted by display name
/// (the lossy-UTF-8 decoding of the raw bytes), with the raw bytes as a
/// deterministic tie-break for names that share a display form.
pub fn render(mut entries: Vec<Entry>, show_parent_link: bool) -> String {
    entries.sort_by(|a, b| {
        let da = String::from_utf8_lossy(&a.raw_name);
        let db = String::from_utf8_lossy(&b.raw_name);
        da.cmp(&db).then_with(|| a.raw_name.cmp(&b.raw_name))
    });

    let mut out = String::new();
    out.push_str("<!DOCTYPE html>\n<html>\n<head><meta charset=\"utf-8\"></head>\n<body>\n<ul>\n");

    if show_parent_link {
        out.push_str("<li><a href=\"../\">../</a></li>\n");
    }

    for entry in &entries {
        let label = String::from_utf8_lossy(&entry.raw_name);
        let href = percent_encode(&entry.raw_name, HREF_SEGMENT).to_string();
        let suffix = if entry.is_dir { "/" } else { "" };

        out.push_str("<li><a href=\"");
        out.push_str(&href);
        out.push_str(suffix);
        out.push_str("\">");
        escape_html(&mut out, &label).expect("writing to a String is infallible");
        out.push_str(suffix);
        out.push_str("</a></li>\n");
    }

    out.push_str("</ul>\n</body>\n</html>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(name: &[u8], is_dir: bool) -> Entry {
        Entry {
            raw_name: name.to_vec(),
            is_dir,
        }
    }

    #[test]
    fn sorted_by_display_name() {
        let html = render(vec![e(b"banana", false), e(b"apple", false)], false);
        let apple_at = html.find("apple").unwrap();
        let banana_at = html.find("banana").unwrap();
        assert!(apple_at < banana_at);
    }

    #[test]
    fn escapes_html_in_labels() {
        let html = render(vec![e(b"<script>&\"'.txt", false)], false);
        assert!(!html.contains("<script>"));
        assert!(html.contains("&#60;script&#62;&#38;&#34;&#39;.txt"));
    }

    #[test]
    fn percent_encodes_href_segments() {
        let html = render(vec![e(b"a b&c.txt", false)], false);
        assert!(html.contains("href=\"a%20b%26c.txt\""));
    }

    #[test]
    fn directory_entries_get_trailing_slash() {
        let html = render(vec![e(b"sub", true)], false);
        assert!(html.contains("href=\"sub/\">sub/</a>"));
    }

    #[test]
    fn parent_link_only_when_requested() {
        assert!(render(vec![], true).contains("href=\"../\""));
        assert!(!render(vec![], false).contains("href=\"../\""));
    }

    #[test]
    fn non_utf8_name_round_trips_lossily_and_safely() {
        let raw_name = vec![b'a', 0xFF, b'b'];
        let html = render(vec![e(&raw_name, false)], false);
        assert!(html.contains("href=\"a%FFb\""));
    }
}
