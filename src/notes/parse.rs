//! Tags and links in Obsidian Markdown.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::properties::{Frontmatter, read_properties, split_frontmatter};

static INLINE_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|\s)#([\p{L}\p{N}_/-][\p{L}\p{M}\p{N}_/-]*)").unwrap());
static ALL_DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\p{Nd}+$").unwrap());
static WIKILINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[\[([^\]|]+)(?:\|[^\]]+)?\]\]").unwrap());
static MARKDOWN_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\(([^)]+\.md)\)").unwrap());
static FENCE_OPEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^ {0,3}(`{3,}|~{3,})").unwrap());

/// What a note says about itself: properties, tags and outgoing links.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NoteMetadata {
    pub frontmatter: Frontmatter,
    /// Tags from the `tags` property and inline `#tags`, without `#`, deduplicated in order.
    pub tags: Vec<String>,
    /// Targets of `[[wikilinks]]` and `[text](note.md)` links, deduplicated in order.
    pub links: Vec<String>,
}

/// Parse a note's properties, tags and links. Malformed frontmatter reads as none.
pub fn parse_frontmatter_and_links(content: &str) -> NoteMetadata {
    let frontmatter = read_properties(content);
    let mut tags: Vec<String> = Vec::new();
    let mut add_tag = |tag: String| {
        if !tag.is_empty() && !tags.contains(&tag) {
            tags.push(tag);
        }
    };

    let property_tags: Vec<String> = match frontmatter.get("tags") {
        Some(Value::Array(values)) => values.iter().filter_map(scalar_tag).collect(),
        Some(Value::String(list)) => list.split(',').map(str::to_owned).collect(),
        _ => Vec::new(),
    };
    for tag in property_tags {
        let tag = tag.trim();
        add_tag(tag.strip_prefix('#').unwrap_or(tag).to_owned());
    }

    // Properties are parsed above; inline tags belong only to the Markdown body.
    let parts = split_frontmatter(content);
    let masked_body = mask_code(parts.body);
    for capture in INLINE_TAG.captures_iter(&masked_body) {
        let tag = &capture[1];
        if !ALL_DIGITS.is_match(tag) {
            add_tag(tag.to_owned());
        }
    }

    // Links count in properties (e.g. `related: "[[Note]]"`) and in the body,
    // but not inside code. Scanned separately so a match never spans the two.
    let mut links: Vec<String> = Vec::new();
    for text in [parts.yaml.unwrap_or(""), masked_body.as_str()] {
        let wikilinks = WIKILINK.captures_iter(text).map(|c| c[1].to_owned());
        let markdown = MARKDOWN_LINK.captures_iter(text).map(|c| c[2].to_owned());
        for link in wikilinks.chain(markdown) {
            if !links.contains(&link) {
                links.push(link);
            }
        }
    }
    NoteMetadata { frontmatter, tags, links }
}

/// A `tags` list entry as text: strings and numbers count, anything else is ignored.
fn scalar_tag(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(match n.as_f64() {
            Some(f) if n.is_f64() && f.fract() == 0.0 => format!("{}", f as i64),
            _ => n.to_string(),
        }),
        _ => None,
    }
}

/// Replace code with dots so nothing inside can start or extend a tag or link.
///
/// Masks fenced code blocks (``` or ~~~, opener at line start with up to three
/// spaces of indent, closer of the same character and at least the same length;
/// an unclosed fence runs to the end) and inline code spans (a backtick run of
/// length n closes at the next run of exactly n within the same paragraph; a
/// span never crosses a blank line; an unmatched run is literal). Line breaks
/// are kept. Indented code blocks, `%%` comments and math are not masked.
pub fn mask_code(content: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let mut paragraph: Vec<&str> = Vec::new();
    let flush = |paragraph: &mut Vec<&str>, out: &mut Vec<String>| {
        if !paragraph.is_empty() {
            let masked = mask_inline_spans(&paragraph.join("\n"));
            out.extend(masked.split('\n').map(str::to_owned));
            paragraph.clear();
        }
    };
    for line in content.split('\n') {
        let open = FENCE_OPEN.captures(line);
        if let Some((char, len)) = fence {
            let closes = open.as_ref().is_some_and(|open| {
                let run = &open[1];
                run.starts_with(char) && run.len() >= len && line.trim() == run
            });
            out.push(dots(line));
            if closes {
                fence = None;
            }
            continue;
        }
        if let Some(open) = open {
            let run = &open[1];
            let char = run.chars().next().unwrap_or('`');
            if char == '~' || !line[open.get(0).map_or(0, |m| m.end())..].contains('`') {
                flush(&mut paragraph, &mut out);
                fence = Some((char, run.len()));
                out.push(dots(line));
                continue;
            }
        }
        if line.trim().is_empty() {
            flush(&mut paragraph, &mut out);
            out.push(line.to_owned());
            continue;
        }
        paragraph.push(line);
    }
    flush(&mut paragraph, &mut out);
    out.join("\n")
}

fn dots(text: &str) -> String {
    ".".repeat(text.chars().count())
}

/// Mask inline code spans. Backtick runs are collected in one pass and each is
/// linked to the next run of the same length, so matching stays linear.
fn mask_inline_spans(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i] == b'`' {
            i += 1;
        }
        runs.push((start, i - start));
    }
    let mut next = vec![None; runs.len()];
    let mut seen: HashMap<usize, usize> = HashMap::new();
    for r in (0..runs.len()).rev() {
        next[r] = seen.get(&runs[r].1).copied();
        seen.insert(runs[r].1, r);
    }
    let mut result = String::with_capacity(text.len());
    let mut from = 0;
    let mut r = 0;
    while r < runs.len() {
        let Some(close) = next[r] else {
            r += 1;
            continue;
        };
        let start = runs[r].0;
        let end = runs[close].0 + runs[close].1;
        result.push_str(&text[from..start]);
        result.extend(text[start..end].chars().map(|c| if c == '\n' { '\n' } else { '.' }));
        from = end;
        r = close + 1;
    }
    result.push_str(&text[from..]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(content: &str) -> Vec<String> {
        parse_frontmatter_and_links(content).tags
    }

    fn links(content: &str) -> Vec<String> {
        parse_frontmatter_and_links(content).links
    }

    #[test]
    fn reads_property_and_inline_tags() {
        assert_eq!(tags("---\ntags: [a, \"#b\", 2024]\n---\nText #c and #d/e"), ["a", "b", "2024", "c", "d/e"]);
        assert_eq!(tags("---\ntags: x, y\n---\n"), ["x", "y"]);
        assert_eq!(tags("#start of note"), ["start"]);
        assert_eq!(tags("#123 is not a tag but #1a is"), ["1a"]);
        assert_eq!(tags("café #résumé"), ["résumé"]);
        assert_eq!(tags("no#tag here"), Vec::<String>::new());
        assert_eq!(tags("#dup #dup"), ["dup"]);
    }

    #[test]
    fn ignores_tags_in_code() {
        assert_eq!(tags("```\n#notatag\n```\n#real"), ["real"]);
        assert_eq!(tags("~~~\n#no\n~~~"), Vec::<String>::new());
        assert_eq!(tags("`#no` but #yes"), ["yes"]);
        assert_eq!(tags("``code ` #no``"), Vec::<String>::new());
        assert_eq!(tags("`unclosed #yes"), ["yes"]);
        assert_eq!(tags("`a\n\n#yes`"), ["yes"]);
        assert_eq!(tags("```\nunclosed fence #no"), Vec::<String>::new());
    }

    #[test]
    fn frontmatter_hashes_are_not_inline_tags() {
        assert_eq!(tags("---\ntitle: \"#notatag\"\n---\nbody"), Vec::<String>::new());
    }

    #[test]
    fn reads_links() {
        assert_eq!(links("See [[A]], [[B|alias]] and [x](c.md) and [[A]]"), ["A", "B", "c.md"]);
        assert_eq!(links("---\nrelated: \"[[P]]\"\n---\n"), ["P"]);
        assert_eq!(links("`[[no]]`\n```\n[[no]]\n```"), Vec::<String>::new());
        assert_eq!(links("[web](https://x.com)"), Vec::<String>::new());
    }

    #[test]
    fn masking_keeps_line_structure() {
        let content = "a `b` c\n```\nx\n```\nd";
        let masked = mask_code(content);
        assert_eq!(masked.lines().count(), content.lines().count());
        assert_eq!(masked, "a ... c\n...\n.\n...\nd");
    }

    #[test]
    fn reads_scalar_properties() {
        let meta =
            parse_frontmatter_and_links("---\ntitle: My Note\ndate: 2026-03-24\nstatus: draft\n---\n\n# Content");
        assert_eq!(meta.frontmatter.get("title"), Some(&Value::from("My Note")));
        // Dates stay strings, as they do in Obsidian's property view.
        assert_eq!(meta.frontmatter.get("date"), Some(&Value::from("2026-03-24")));
        assert_eq!(meta.frontmatter.get("status"), Some(&Value::from("draft")));
    }

    #[test]
    fn reads_block_list_tags() {
        assert_eq!(
            tags("---\ntags:\n  - project\n  - active\n  - important\n---\n\nContent"),
            ["project", "active", "important"]
        );
    }

    #[test]
    fn reads_non_ascii_property_keys() {
        let meta = parse_frontmatter_and_links("---\nämne: unicode\nsenast_ändrad: 2026-09-16\ntitle: plain\n---\n");
        assert_eq!(meta.frontmatter.get("ämne"), Some(&Value::from("unicode")));
        assert_eq!(meta.frontmatter.get("senast_ändrad"), Some(&Value::from("2026-09-16")));
        assert_eq!(meta.frontmatter.get("title"), Some(&Value::from("plain")));
    }

    #[test]
    fn non_ascii_inline_tags_are_whole() {
        assert_eq!(
            tags("Se #lägen och #art/rutin-för-personal här, samt #日本語"),
            ["lägen", "art/rutin-för-personal", "日本語"]
        );
    }

    #[test]
    fn combining_marks_stay_inside_keys_and_tags() {
        let key = "a\u{0308}mne";
        let tag = "la\u{0308}gen";
        let meta = parse_frontmatter_and_links(&format!("---\n{key}: nfd\n---\n\nText #{tag} here"));
        assert_eq!(meta.frontmatter.get(key), Some(&Value::from("nfd")));
        assert_eq!(meta.tags, [tag]);
    }

    #[test]
    fn ignores_tags_in_double_backtick_spans() {
        assert_eq!(tags("Set the `#Kategori` field and ``#Household`` too, but keep #real here"), ["real"]);
    }

    #[test]
    fn ignores_tags_in_fences_with_info_strings() {
        let content = "#before\n```\n#Household\n#inside/nested\n```\n#after\n~~~md\n#tilde\n~~~\n";
        assert_eq!(tags(content), ["before", "after"]);
    }

    #[test]
    fn unclosed_fence_runs_to_the_end() {
        assert_eq!(tags("#kept\n```\n#lost\n#also-lost"), ["kept"]);
    }

    #[test]
    fn unmatched_backtick_run_is_literal() {
        assert_eq!(tags("A stray ` here and #tag stays; a ``span with #hidden`` hides it"), ["tag"]);
    }

    #[test]
    fn stray_backticks_do_not_pair_across_blank_lines() {
        let mut parts = vec!["The user`s request needs follow up. #important".to_owned()];
        parts.extend((0..20).map(|i| format!("## Section {i}\nSome notes here. #tag{i}")));
        parts.push("Circling back, thats it`s done. #wrapup".to_owned());
        let found = tags(&parts.join("\n\n"));
        assert_eq!(found.len(), 22, "{found:?}");
        for tag in ["important", "tag0", "tag19", "wrapup"] {
            assert!(found.iter().any(|t| t == tag), "missing {tag} in {found:?}");
        }
    }

    #[test]
    fn masks_many_unmatched_backtick_runs_in_linear_time() {
        let mut content = String::new();
        let mut n = 1;
        while content.len() < 2_000_000 {
            content.push_str(&"`".repeat(n));
            content.push_str(" x ");
            n += 1;
        }
        content.push_str("#end");
        let start = std::time::Instant::now();
        assert_eq!(tags(&content), ["end"]);
        // Generous for debug builds; a quadratic scan takes minutes here.
        assert!(start.elapsed() < std::time::Duration::from_secs(10), "took {:?}", start.elapsed());
    }

    #[test]
    fn masking_does_not_create_or_extend_tags() {
        assert_eq!(tags("`x`#glued and #tag`y` and `#a`#b"), ["tag"]);
    }

    #[test]
    fn all_digit_inline_tags_are_rejected() {
        let found = tags("See PR #1984 and issue #20; #y1984 and #2026/09 and #x-1 are tags");
        assert_eq!(found, ["y1984", "2026/09", "x-1"]);
    }

    #[test]
    fn non_decimal_number_tags_are_kept() {
        assert_eq!(tags("Chapter #Ⅳ and footnote #² are tags, #42 is not"), ["Ⅳ", "²"]);
    }

    #[test]
    fn property_and_inline_tags_are_deduplicated() {
        assert_eq!(tags("---\ntags: [shared]\n---\n\nAlso #shared inline"), ["shared"]);
    }

    #[test]
    fn markdown_links_must_target_notes() {
        assert_eq!(links("See [link](https://example.com) and [img](photo.png)"), Vec::<String>::new());
        assert_eq!(
            links("See [my link](other-note.md) and [another](folder/note.md)"),
            ["other-note.md", "folder/note.md"]
        );
    }

    #[test]
    fn links_in_code_are_ignored_but_property_links_count() {
        let content = [
            "---",
            "related: \"[[From Property]]\"",
            "---",
            "See [[Real]] and [doc](real.md).",
            "Inline `[[Not Inline]]` and `[x](not-inline.md)`.",
            "```",
            "[[Not Fenced]]",
            "[y](not-fenced.md)",
            "```",
        ]
        .join("\n");
        let mut found = links(&content);
        found.sort();
        assert_eq!(found, ["From Property", "Real", "real.md"]);
    }

    #[test]
    fn plain_text_has_no_metadata() {
        assert_eq!(parse_frontmatter_and_links("Just plain text, no metadata."), NoteMetadata::default());
    }

    #[test]
    fn unclosed_frontmatter_is_not_frontmatter() {
        assert!(parse_frontmatter_and_links("---\ntitle: Broken\nNo closing delimiter").frontmatter.is_empty());
    }
}
