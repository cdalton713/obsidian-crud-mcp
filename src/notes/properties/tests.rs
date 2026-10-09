use serde_json::json;

use super::*;
use crate::notes::{note_structure, note_tasks, select_note_range};

fn map(value: Value) -> Frontmatter {
    match value {
        Value::Object(map) => map,
        _ => panic!("expected an object"),
    }
}

fn update(content: &str, set: Value, remove: &[&str]) -> Result<String, PropertyError> {
    let remove: Vec<String> = remove.iter().map(|s| s.to_string()).collect();
    update_properties(content, &map(set), &remove)
}

#[test]
fn replacing_an_anchor_keeps_an_unrelated_alias_value() {
    let updated = update("---\nstatus: &s todo\nother: *s\n---\nBody", json!({"status": "done"}), &[]).unwrap();
    assert_eq!(Value::Object(read_properties(&updated)), json!({"status": "done", "other": "todo"}));
    assert!(updated.ends_with("---\nBody"));
}

#[test]
fn updates_preserve_unrelated_values_and_the_exact_body() {
    let cases = [
        ("tags: [a, b]\n", "flow list"),
        ("tags:\n- a\n- b\n", "unindented block list"),
        ("title:   \"Spaced\"\n", "extra spacing"),
        ("summary: >\n  a folded value that\n  spans lines\n\n", "folded scalar"),
        ("# leading comment\nother: 1 # trailing\n\n# before\n", "comments"),
    ];
    for (unrelated, label) in cases {
        let before = format!("---\n{unrelated}status: draft\nafter:   [x,y]\n---\nBody\n");
        let updated = update(&before, json!({"status": "done"}), &[]).unwrap();
        let mut expected = read_properties(&before);
        expected.insert("status".into(), json!("done"));
        assert_eq!(read_properties(&updated), expected, "{label}");
        assert!(updated.ends_with("---\nBody\n"), "{label}");
    }
}

#[test]
fn updates_serialize_typed_values() {
    let updated = update(
        "---\nstatus: draft # workflow\ntitle: 'Original'\naliases: [x, y]\n---\n",
        json!({"status": "done", "title": "New", "aliases": ["z", "w"]}),
        &[],
    )
    .unwrap();
    assert_eq!(
        Value::Object(read_properties(&updated)),
        json!({"status": "done", "title": "New", "aliases": ["z", "w"]})
    );
    assert!(!updated.contains("# workflow"));
}

#[test]
fn updates_preserve_crlf_and_bom_and_append_new_keys() {
    assert_eq!(
        update(
            "\u{FEFF}---\r\ntags: [a, b]\r\nstatus: draft\r\n---\r\nBody\r\n",
            json!({"status": "done", "count": 3}),
            &[]
        )
        .unwrap(),
        "\u{FEFF}---\r\ntags: [a, b]\r\nstatus: done\r\ncount: 3\r\n---\r\nBody\r\n"
    );
}

#[test]
fn removal_deletes_selected_keys_and_preserves_others() {
    assert_eq!(
        update("---\n# keep\nnested:\n  k: [1, 2]\n  j: x\nstatus: draft\n---\nBody", json!({}), &["nested"]).unwrap(),
        "---\nstatus: draft\n---\nBody"
    );
    // Untouched lines keep their exact text, spacing included.
    assert_eq!(
        update("---\na:  1\nlast: 2 # z\n---\nBody", json!({}), &["last"]).unwrap(),
        "---\na:  1\n---\nBody"
    );
    assert_eq!(update("---\nonly: x\n---\nBody", json!({}), &["only"]).unwrap(), "Body");
}

#[test]
fn updates_keep_top_level_and_inline_comments() {
    assert_eq!(
        update(
            "---\n# Keep me\ntitle: Original # inline\nstatus: draft\nold: remove\n---\n\n# Body\n",
            json!({"status": "done", "count": 3}),
            &["old"]
        )
        .unwrap(),
        "---\n# Keep me\ntitle: Original # inline\nstatus: done\ncount: 3\n---\n\n# Body\n"
    );
}

#[test]
fn updates_keep_the_formatting_of_untouched_keys() {
    let untouched = "quoted: \"double\"\nsingle: 'single'\nflow: [a, b]\nmap: {k: v}\nblock: |\n  line one\n  line two\n\nnum: 0x1f\n";
    assert_eq!(
        update(&format!("---\n{untouched}status: draft\n---\nBody"), json!({"status": "done"}), &[]).unwrap(),
        format!("---\n{untouched}status: done\n---\nBody")
    );
}

#[test]
fn updates_keep_key_order() {
    assert_eq!(
        update("---\nz: 1\nm: 2\na: 3\n---\n", json!({"m": "two", "b": true}), &[]).unwrap(),
        "---\nz: 1\nm: two\na: 3\nb: true\n---\n"
    );
}

#[test]
fn removal_keeps_neighbouring_comments_and_blank_lines() {
    assert_eq!(
        update(
            "---\n# header\n\na: 1 # about a\n\n# about b\nb: 2\nc: 3 # about c\n# trailing\n---\nBody",
            json!({}),
            &["b"]
        )
        .unwrap(),
        "---\n# header\n\na: 1 # about a\nc: 3 # about c\n# trailing\n---\nBody"
    );
    assert_eq!(
        update("---\n# header\n\nonly: x\n---\nBody", json!({}), &["only"]).unwrap(),
        "---\n# header\n---\nBody"
    );
}

#[test]
fn updates_create_frontmatter_when_missing() {
    assert_eq!(
        update("# Body\n", json!({"status": "done", "tags": ["a"]}), &[]).unwrap(),
        "---\nstatus: done\ntags:\n  - a\n---\n# Body\n"
    );
    assert_eq!(
        update("---\n---\nBody", json!({"status": "done"}), &[]).unwrap(),
        "---\nstatus: done\n---\nBody"
    );
}

#[test]
fn an_unclosed_opening_rule_is_markdown() {
    let content = "---\n# A\n- [ ] task\n";
    let headings: Vec<_> = note_structure(content).headings.into_iter().map(|h| h.heading).collect();
    assert_eq!(headings, vec![vec!["A".to_string()]]);
    let tasks: Vec<_> = note_tasks(content).into_iter().map(|t| (t.line, t.text)).collect();
    assert_eq!(tasks, vec![(3, "task".to_string())]);
    let range = select_note_range(content, Some(&["A".to_string()]), None).unwrap();
    assert_eq!(&content[range.start..], "- [ ] task\n");
    assert!(read_properties(content).is_empty());
    assert_eq!(update(content, json!({"status": "done"}), &[]), Err(PropertyError::NoClosingDelimiter));
}

#[test]
fn rejects_invalid_requests() {
    assert_eq!(update("---\na: 1\n---\n", json!({"a": 2}), &["a"]), Err(PropertyError::SetAndRemove));
    assert_eq!(update("---\na: [\n---\n", json!({"a": 2}), &[]), Err(PropertyError::InvalidYaml));
    assert_eq!(update("---\n- a\n---\n", json!({"a": 2}), &[]), Err(PropertyError::NotAMapping));
    assert_eq!(update("---\na: 1\na: 2\n---\n", json!({"b": 2}), &[]), Err(PropertyError::InvalidYaml));
}

#[test]
fn unchanged_updates_return_the_note_as_is() {
    let content = "---\na:   1\nb: [x]\n---\nBody";
    assert_eq!(update(content, json!({"a": 1.0, "b": ["x"]}), &["missing"]).unwrap(), content);
    assert_eq!(update("Body", json!({}), &["x"]).unwrap(), "Body");
}

#[test]
fn strings_that_need_quotes_are_quoted() {
    for value in ["true", "123", "a: b", "# hash", "", " padded ", "line\nbreak", "null", "[x]", "- item"] {
        let updated = update("---\nk: v\n---\n", json!({ "s": value }), &[]).unwrap();
        assert_eq!(read_properties(&updated).get("s"), Some(&json!(value)), "{value:?} -> {updated}");
    }
}

#[test]
fn reads_properties_leniently() {
    assert_eq!(Value::Object(read_properties("---\ntitle: T\ntags: [a]\n---\nx")), json!({"title": "T", "tags": ["a"]}));
    assert!(read_properties("---\n: [bad\n---\n").is_empty());
    assert!(read_properties("no frontmatter").is_empty());
    assert!(read_properties("---\n# only a comment\n---\n").is_empty());
    assert_eq!(read_properties("---\r\na: 1\r\n---\r\n").get("a"), Some(&json!(1)));
}
