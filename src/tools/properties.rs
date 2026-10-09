//! `update_note_properties`: set or remove frontmatter keys.

use std::sync::Arc;

use serde_json::json;

use super::ToolContext;
use super::params::UpdateNotePropertiesParams;
use crate::mcp::ToolError;
use crate::notes::{make_deep_link, update_properties};
use crate::util::now_ms;
use crate::vault::is_path_writable;

pub async fn update_note_properties(
    ctx: Arc<ToolContext>,
    args: UpdateNotePropertiesParams,
) -> Result<String, ToolError> {
    let UpdateNotePropertiesParams { path, set, remove } = args;
    if !is_path_writable(&path, ctx.write_folders.as_deref()) {
        return Ok(json!({ "error": "Write access denied: path is outside the writable folders." }).to_string());
    }
    let Some(existing) = ctx.vault.read_note(&path).await? else {
        return Ok(json!({ "error": format!("Note not found: {path}") }).to_string());
    };
    let updated = match update_properties(&existing, &set, &remove) {
        Ok(updated) => updated,
        Err(error) => return Ok(json!({ "error": error.to_string() }).to_string()),
    };
    let url = make_deep_link(&ctx.vault_name, &path);
    if updated == existing {
        return Ok(json!({ "status": "unchanged", "path": path, "url": url }).to_string());
    }
    if !ctx.vault.write_note(&path, &updated).await? {
        return Ok(json!({ "error": format!("Failed to write note: {path}") }).to_string());
    }
    ctx.index.update(&path, &updated, Some(now_ms() as f64));
    ctx.changed();
    Ok(json!({ "status": "updated", "path": path, "url": url }).to_string())
}
