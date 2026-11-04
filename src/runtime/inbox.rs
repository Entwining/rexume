//! The directory through which the Pi and omp extensions take a conversation's notices and
//! requests: each arrives as `<id>.json`, and the extension renames it to `<id>.accepted` before
//! passing it to the runtime.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::json;

use super::Sent;

/// The conversation's inbox among a runtime's inboxes in `root`.
pub fn dir(root: &Path, native_id: &str) -> PathBuf {
    root.join(native_id.replace('/', "_"))
}

pub fn send(inbox: &Path, id: &str, text: &str) -> Result<Sent> {
    crate::store::create_private_dir(inbox)?;
    // Written whole under a name the extension ignores, then renamed into view.
    let mut file = tempfile::NamedTempFile::with_prefix_in(".", inbox)?;
    file.write_all(json!({ "text": text }).to_string().as_bytes())?;
    file.persist(inbox.join(format!("{id}.json")))?;
    Ok(Sent::Handed)
}

/// Whether the extension took the message from the inbox: the runtime holds it, but the
/// conversation may not have taken it in yet.
pub fn accepted(inbox: &Path, id: &str) -> bool {
    inbox.join(format!("{id}.accepted")).exists()
}
