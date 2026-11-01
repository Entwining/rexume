//! Finding receipts of notices in a conversation's record.

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;

use super::{Lines, Parser, Runtime, mentions, token};

/// Finds notices' receipts in one owner record, reading only what was added since the last
/// check; one reader serves every notice sent to that record.
pub struct Receipt {
    runtime: Runtime,
    lines: Lines,
    parser: Parser,
}

impl Receipt {
    pub fn new(runtime: Runtime, transcript: &Path) -> Self {
        Self {
            runtime,
            lines: Lines::new(transcript, 0),
            parser: runtime.parser(),
        }
    }

    /// The notices among `pending` whose receipt the records added since the last check show.
    /// A notice's token appears only once it was sent, so a record read before cannot hold it;
    /// a turn that took it in before the check may end after it, so the parser keeps its state.
    pub fn found(&mut self, pending: &[String]) -> Result<HashSet<String>> {
        let tokens: Vec<(String, &String)> = pending.iter().map(|id| (token(id), id)).collect();
        let mut found = HashSet::new();
        let mut sealed = Vec::new();
        let mut batch = self.lines.read()?;
        if batch.restarted {
            self.parser = self.runtime.parser();
        }
        while let Some(record) = batch.next_spanned() {
            let (record, span) = record?;
            let received = self.runtime.received(&record);
            self.parser.feed(&record, span, &mut sealed);
            for (needle, id) in &tokens {
                if (received && mentions(&record, needle))
                    || sealed.iter().any(|turn| turn.received(needle))
                {
                    found.insert((*id).clone());
                }
            }
            sealed.clear();
        }
        Ok(found)
    }
}
