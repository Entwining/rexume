//! Finding receipts of notices in a conversation's record.

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;

use super::{Lines, Runtime, mentions, token};

/// Finds notices' receipts in one owner record, reading only what was added since the last
/// check; one reader serves every notice sent to that record.
pub struct Receipt {
    runtime: Runtime,
    lines: Lines,
}

impl Receipt {
    pub fn new(runtime: Runtime, transcript: &Path) -> Self {
        Self {
            runtime,
            lines: Lines::new(transcript, 0),
        }
    }

    /// The notices among `pending` whose receipt the records added since the last check show.
    /// A notice's token appears only once it was sent, so a record read before cannot hold it.
    pub fn found(&mut self, pending: &[String]) -> Result<HashSet<String>> {
        let tokens: Vec<(String, &String)> = pending.iter().map(|id| (token(id), id)).collect();
        let mut found = HashSet::new();
        for record in self.lines.read()? {
            let record = record?;
            if !self.runtime.received(&record) {
                continue;
            }
            for (needle, id) in &tokens {
                if mentions(&record, needle) {
                    found.insert((*id).clone());
                }
            }
        }
        Ok(found)
    }
}
