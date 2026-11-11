use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::json;

use super::Sent;

pub fn dir(root: &Path, native_id: &str) -> PathBuf {
    root.join(native_id.replace('/', "_"))
}

pub fn send(inbox: &Path, id: &str, text: &str) -> Result<Sent> {
    crate::store::create_private_dir(inbox)?;
    let mut file = tempfile::NamedTempFile::with_prefix_in(".", inbox)?;
    file.write_all(json!({ "text": text }).to_string().as_bytes())?;
    file.persist(inbox.join(format!("{id}.json")))?;
    Ok(Sent::Handed)
}

pub fn accepted(inbox: &Path, id: &str) -> bool {
    inbox.join(format!("{id}.accepted")).exists()
}
