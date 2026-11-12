//! Colour tokens. Categorical slots are assigned in fixed order by input
//! file, never cycled; light and dark steps are separately chosen values.
//!
//! Every coloured SVG element carries both a presentation attribute with the
//! light value (for renderers without CSS variables, e.g. librsvg) and a
//! class whose CSS rule uses the variable (browsers prefer the CSS rule, so
//! dark mode works there).

use std::sync::LazyLock;

pub const MAX_SERIES: usize = 8;

/// (token, light, dark)
const TOKENS: &[(&str, &str, &str)] = &[
    ("surface", "#fcfcfb", "#1a1a19"),
    ("page", "#f9f9f7", "#0d0d0d"),
    ("ink-1", "#0b0b0b", "#f0efec"),
    ("ink-2", "#52514e", "#c3c2b7"),
    ("ink-muted", "#898781", "#898781"),
    ("grid", "#e1e0d9", "#2c2c2a"),
    ("axis", "#c3c2b7", "#383835"),
    ("ring", "rgba(11,11,11,0.10)", "rgba(255,255,255,0.10)"),
    ("good", "#006300", "#0ca30c"),
    ("bad", "#d03b3b", "#e66767"),
    ("s1", "#2a78d6", "#3987e5"),
    ("s2", "#eb6834", "#d95926"),
    ("s3", "#1baf7a", "#199e70"),
    ("s4", "#eda100", "#c98500"),
    ("s5", "#e87ba4", "#d55181"),
    ("s6", "#008300", "#008300"),
    ("s7", "#6250d6", "#9085e9"),
    ("s8", "#e34948", "#e66767"),
];

fn light(token: &str) -> &'static str {
    TOKENS.iter().find(|t| t.0 == token).map(|t| t.1).expect("unknown colour token")
}

/// `class="f-<token>" fill="<light>"`
pub fn fill(token: &str) -> String {
    format!(r#"class="f-{token}" fill="{}""#, light(token))
}

/// `class="s-<token>" stroke="<light>"`
pub fn stroke(token: &str) -> String {
    format!(r#"class="s-{token}" stroke="{}""#, light(token))
}

/// Token name for a series slot. Callers must keep `slot < MAX_SERIES`
/// (`compare` rejects more inputs); hues are never recycled.
pub fn series_token(slot: usize) -> String {
    assert!(slot < MAX_SERIES, "at most {MAX_SERIES} series are supported");
    format!("s{}", slot + 1)
}

pub fn series_var(slot: usize) -> String {
    format!("var(--{})", series_token(slot))
}

/// CSS shared by every SVG and the HTML page. Scoped under `.ck`.
pub static STYLE: LazyLock<String> = LazyLock::new(|| {
    let vars = |dark: bool| -> String {
        TOKENS.iter().map(|(n, l, d)| format!("--{n}:{};", if dark { d } else { l })).collect()
    };
    let classes: String = TOKENS
        .iter()
        .map(|(n, _, _)| format!(".ck .f-{n}{{fill:var(--{n})}}.ck .s-{n}{{stroke:var(--{n})}}"))
        .collect();
    format!(
        ".ck{{color-scheme:light;{}}}@media (prefers-color-scheme: dark){{.ck{{color-scheme:dark;{}}}}}{classes}{}",
        vars(false),
        vars(true),
        r#"
.ck text { font-family: system-ui, -apple-system, "Segoe UI", sans-serif; font-variant-numeric: tabular-nums; }
.ck .hit { fill: transparent; }
.ck .hit:hover + .dot { opacity: 1; }
.ck .dot { opacity: 0; pointer-events: none; }
.ck .bar:hover { filter: brightness(1.12); }
"#
    )
});

/// Class list plus light-value fallbacks for an element that needs extra
/// classes and/or both fill and stroke tokens.
pub fn paint(extra_class: &str, fill_token: Option<&str>, stroke_token: Option<&str>) -> String {
    let mut classes = vec![extra_class.to_string()];
    let mut attrs = String::new();
    if let Some(t) = fill_token {
        classes.push(format!("f-{t}"));
        attrs.push_str(&format!(r#" fill="{}""#, light(t)));
    }
    if let Some(t) = stroke_token {
        classes.push(format!("s-{t}"));
        attrs.push_str(&format!(r#" stroke="{}""#, light(t)));
    }
    format!(r#"class="{}"{attrs}"#, classes.join(" ").trim())
}
