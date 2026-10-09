//! Parsing and validating Obsidian notes: paths, properties, tags, links,
//! outlines and tasks, plus the paged content scan behind `search_notes`.

mod deeplink;
mod parse;
mod path;
mod properties;
mod scan;
mod structure;

pub use deeplink::make_deep_link;
pub use parse::{NoteMetadata, mask_code, parse_frontmatter_and_links};
pub use path::{InvalidNotePath, is_valid_note_path, validate_note_path};
pub use properties::{Frontmatter, PropertyError, read_properties, split_frontmatter, update_properties};
pub use scan::{
    LineMatch, SCAN_NOTE_MAX_CHARS, SCAN_PAGE_CHARS, SCAN_READ_CONCURRENCY, ScanOptions, ScanParameters, scan_notes,
};
pub use structure::{
    NoteBlock, NoteHeading, NoteStructure, NoteTask, TargetError, note_structure, note_tasks, select_note_range,
    starts_with_setext_boundary,
};
