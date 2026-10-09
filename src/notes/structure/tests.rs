use super::*;

fn path(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

#[test]
fn outlines_nested_headings_with_line_ranges() {
    let content = "# A\ntext\n## B\nmore\n# C\nend\n";
    let outline = note_structure(content);
    let summary: Vec<_> = outline
        .headings
        .iter()
        .map(|h| (h.heading.clone(), h.level, h.start_line, h.end_line))
        .collect();
    assert_eq!(
        summary,
        vec![
            (path(&["A"]), 1, 1, 4),
            (path(&["A", "B"]), 2, 3, 4),
            (path(&["C"]), 1, 5, 6),
        ]
    );
}

#[test]
fn heading_titles_drop_markers() {
    let content = "## Title ##\nSetext\n===\n";
    let titles: Vec<_> = note_structure(content).headings.into_iter().map(|h| h.heading).collect();
    assert_eq!(titles, vec![path(&["Title"]), path(&["Setext"])]);
}

#[test]
fn ignores_headings_in_frontmatter_and_code() {
    let content = "---\ntitle: x\n---\n```\n# not\n```\n# Real\n";
    let titles: Vec<_> = note_structure(content).headings.into_iter().map(|h| h.heading).collect();
    assert_eq!(titles, vec![path(&["Real"])]);
    assert_eq!(note_structure(content).headings[0].start_line, 7);
}

#[test]
fn finds_inline_and_standalone_block_ids() {
    let content = "Para one ^first\n\n- item\n\n^list\n\n# H\n\n^ignored\n";
    let blocks = note_structure(content).blocks;
    let ids: Vec<_> = blocks.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(ids, ["first", "list"]);
    assert_eq!(&content[blocks[0].start..blocks[0].end], "Para one");
    assert_eq!(&content[blocks[1].start..blocks[1].end], "- item");
}

#[test]
fn selects_sections_and_blocks() {
    let content = "# A\nalpha\n## B\nbeta\n# C\ngamma ^g\n";
    let a = select_note_range(content, Some(&path(&["A"])), None).unwrap();
    assert_eq!(&content[a], "alpha\n## B\nbeta\n");
    let b = select_note_range(content, Some(&path(&["A", "B"])), None).unwrap();
    assert_eq!(&content[b], "beta\n");
    let g = select_note_range(content, None, Some("g")).unwrap();
    assert_eq!(&content[g], "gamma");
    assert_eq!(select_note_range(content, None, None).unwrap(), 0..content.len());
    assert_eq!(select_note_range(content, Some(&path(&["Z"])), None), Err(TargetError::NotFound));
    assert_eq!(select_note_range(content, Some(&path(&["A"])), Some("g")), Err(TargetError::Both));
    assert_eq!(
        select_note_range("# A\n# A\n", Some(&path(&["A"])), None),
        Err(TargetError::Ambiguous)
    );
}

#[test]
fn reads_tasks() {
    let content = "- [ ] open\n- [x] done\n  - [X] nested\n- not a task\n```\n- [ ] code\n```\n";
    let tasks: Vec<_> = note_tasks(content).into_iter().map(|t| (t.line, t.text, t.completed)).collect();
    assert_eq!(
        tasks,
        vec![
            (1, "open".to_string(), false),
            (2, "done".to_string(), true),
            (3, "nested".to_string(), true),
        ]
    );
    let long = format!("- [ ] {}", "x".repeat(600));
    let task = &note_tasks(&long)[0];
    assert!(task.truncated);
    assert_eq!(task.text.len(), 500);
}

#[test]
fn detects_setext_boundaries() {
    assert!(starts_with_setext_boundary("===\n"));
    assert!(starts_with_setext_boundary("---"));
    assert!(starts_with_setext_boundary("Title\n---\n"));
    assert!(!starts_with_setext_boundary("# Title\n"));
    assert!(!starts_with_setext_boundary("plain text\n"));
}

#[test]
fn crlf_offsets_stay_consistent() {
    let content = "# A\r\nbody\r\n# B\r\n";
    let a = select_note_range(content, Some(&path(&["A"])), None).unwrap();
    assert_eq!(&content[a], "body\r\n");
}
