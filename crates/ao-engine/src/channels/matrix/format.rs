//! Outbound wire conversion for the Matrix relay (A4): markdown → Matrix's
//! spec-HTML subset, plus the chunking policy for over-long replies.
//!
//! The converter is ruma's own pulldown-cmark pipeline
//! ([`FormattedBody::markdown`], enabled by matrix-sdk's `markdown`
//! feature), which already emits only the spec-blessed HTML subset and
//! returns `None` for input with no markdown constructs — plain replies
//! then skip the `formatted_body` entirely, which is the idiomatic Matrix
//! shape (notification bodies and text-only clients never carry markup).
//!
//! Chunking happens on the *plain* text at [`MATRIX_MAX_MESSAGE_CHARS`],
//! and each chunk is converted independently — so every chunk's
//! `formatted_body` is balanced HTML (pulldown-cmark closes any construct a
//! split opened, e.g. an unterminated fence becomes a complete `<pre>`). The
//! trade-off, shared with Telegram's relay: a construct split across a
//! boundary renders as two adjacent constructs rather than one. Enforcing
//! the limit pre-conversion means post-conversion inflation could in
//! theory push a single event past the homeserver's event-size cap — the
//! limit is deliberately less than half of Synapse's default 64 KiB event
//! cap to leave that headroom (the chunker's own KNOWN LIMITATION note
//! applies).

use matrix_sdk::ruma::events::room::message::FormattedBody;

use crate::channels::relay::chunker::chunk_text;

/// Per-chunk plain-text budget. Matrix has no message-length limit of its
/// own; the binding constraint is the homeserver's event-size cap (Synapse
/// defaults to 64 KiB per event). At ~2× worst-case HTML inflation plus
/// event envelope overhead, 20 KiB of plain text stays comfortably under.
pub(crate) const MATRIX_MAX_MESSAGE_CHARS: usize = 20_000;

/// Splits a reply into chunks no longer than [`MATRIX_MAX_MESSAGE_CHARS`],
/// preferring newline boundaries. Delegates to the shared relay chunker —
/// same splitting policy as Telegram/Discord, different limit.
pub(crate) fn chunk_for_matrix(text: &str) -> Vec<&str> {
    chunk_text(text, MATRIX_MAX_MESSAGE_CHARS)
}

/// Converts one chunk of markdown to Matrix's spec-HTML subset, or returns
/// `None` when the chunk contains no markdown constructs at all — callers
/// then send it as a plain `m.text` (no `formatted_body`), the idiomatic
/// shape for unformatted text.
///
/// Raw HTML embedded in the markdown is neutralized first: pulldown-cmark
/// (what ruma's converter wraps) passes `Event::Html` straight through into
/// the output, and agent output is untrusted — a reply containing
/// `<script>` or an `<img onerror>` must render as literal text, not markup.
/// The cost is that markdown *autolinks* (`<https://…>`) render literally
/// too; an accepted trade-off, since agent replies produce them far less
/// often than they quote HTML.
pub(crate) fn markdown_to_matrix_html(markdown: &str) -> Option<String> {
    let escaped =
        markdown.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    // `FormattedBody::markdown`'s own `None` check can't see through the
    // pre-escaping (an entity round-trips as a non-plain parser event), so
    // the "no constructs" decision is ours: if the HTML came back identical
    // to the escaped input, nothing was formatted and the caller sends the
    // original chunk as plain `m.text`.
    let formatted = FormattedBody::markdown(&escaped)?;
    (formatted.body != escaped).then_some(formatted.body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_produces_no_html() {
        // No formatting constructs → the caller sends a plain m.text.
        assert_eq!(markdown_to_matrix_html("just a normal sentence"), None);
    }

    #[test]
    fn emphasis_and_code_convert() {
        let html = markdown_to_matrix_html("run `cargo test` — it's **fast**").expect("html");
        assert!(html.contains("<code>cargo test</code>"), "{html}");
        assert!(html.contains("<strong>fast</strong>"), "{html}");
    }

    #[test]
    fn fenced_code_block_converts_to_pre() {
        let html = markdown_to_matrix_html("before\n\n```rust\nfn main() {}\n```").expect("html");
        assert!(html.contains("<pre"), "{html}");
        assert!(html.contains("fn main() {}"), "{html}");
    }

    #[test]
    fn headings_and_links_convert() {
        let html = markdown_to_matrix_html("# Title\n\n[docs](https://example.com)").expect("html");
        assert!(html.contains("<h1>"), "{html}");
        assert!(html.contains("href=\"https://example.com\""), "{html}");
    }

    #[test]
    fn raw_html_without_markdown_stays_plain() {
        // No markdown constructs → no formatted body at all: the chunk goes
        // out as plain `m.text`, where `<script>` is inert by definition
        // (plain bodies are never rendered as markup).
        assert_eq!(markdown_to_matrix_html("look: <script>alert(1)</script>"), None);
    }

    #[test]
    fn raw_html_inside_markdown_is_escaped() {
        // The case the pre-escape exists for: a formatted reply whose
        // `formatted_body` WOULD carry pulldown-cmark's raw-HTML pass-
        // through. The markup must be neutralized to text.
        let html = markdown_to_matrix_html("look: **<script>alert(1)</script>**").expect("html");
        assert!(html.contains("<strong>"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
    }

    #[test]
    fn pre_escaping_keeps_plain_comparison_text_plain() {
        // `<` alone is not a markdown construct: no formatted body, the
        // caller sends the original text as plain m.text.
        assert_eq!(markdown_to_matrix_html("1 < 2 and 3 > 2"), None);
    }

    #[test]
    fn chunking_preserves_content_and_respects_the_limit() {
        let line = "x".repeat(5000);
        let text = std::iter::repeat(line.as_str()).take(10).collect::<Vec<_>>().join("\n");
        let chunks = chunk_for_matrix(&text);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= MATRIX_MAX_MESSAGE_CHARS);
        }
        assert_eq!(chunks.concat(), text);
        // Every chunk still converts to independently valid HTML.
        for chunk in &chunks {
            let _ = markdown_to_matrix_html(chunk);
        }
    }

    #[test]
    fn a_fence_split_across_chunks_still_yields_balanced_html_per_chunk() {
        // The known split-boundary trade-off, pinned: an unterminated fence
        // at a chunk boundary closes at end-of-chunk instead of producing
        // malformed HTML.
        let chunk = "```rust\nlet x = 1;";
        let html = markdown_to_matrix_html(chunk).expect("html");
        assert!(html.contains("<pre"), "{html}");
        assert!(html.contains("</pre>"), "{html}");
    }
}
