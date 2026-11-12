//! Compare result files: per-metric medians and spread, change relative to a
//! baseline, and self-contained SVG/HTML output (no JS, no external assets).

pub mod compare;
pub mod html;
pub mod stats;
pub mod svg;
mod theme;

pub use compare::{Comparison, MetricRow, Series};

/// Escape text for XML/HTML content and attribute values.
pub(crate) fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}
