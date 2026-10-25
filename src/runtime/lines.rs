//! Reading a conversation's record as it grows: whole JSON lines from an offset, streamed to the
//! caller, so a long record is never held in memory at once.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use super::{Parser, Runtime, Turn, mentions, token};

/// Reads the whole records a conversation file holds past an offset, remembering where it stopped
/// so the next read starts there. A line without its newline yet is left for the next read. A
/// file that was replaced or shrank is read again from the start offset; one that does not exist
/// yet holds no records.
pub struct Lines {
    path: PathBuf,
    start: u64,
    offset: u64,
    identity: Option<(u64, u64)>,
    /// A whole line since `start` was not a JSON record.
    skipped: bool,
}

impl Lines {
    pub fn new(path: &Path, start: u64) -> Self {
        Self {
            path: path.to_path_buf(),
            start,
            offset: start,
            identity: None,
            skipped: false,
        }
    }

    /// The records added since the last read, read one at a time as the caller takes them.
    pub fn read(&mut self) -> Result<Batch<'_>> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Batch {
                    lines: self,
                    reader: None,
                    restarted: false,
                    line: Vec::new(),
                });
            }
            Err(e) => return Err(e.into()),
        };
        let meta = file.metadata()?;
        let identity = Some((meta.dev(), meta.ino()));
        let restarted =
            (self.identity.is_some() && self.identity != identity) || meta.len() < self.offset;
        if restarted {
            self.offset = self.start.min(meta.len());
            self.skipped = false;
        }
        self.identity = identity;
        file.seek(SeekFrom::Start(self.offset))?;
        Ok(Batch {
            lines: self,
            reader: Some(BufReader::new(file)),
            restarted,
            line: Vec::new(),
        })
    }
}

/// One read of a `Lines`: its records in order, each one taken from the file as it is asked for.
pub struct Batch<'a> {
    lines: &'a mut Lines,
    reader: Option<BufReader<File>>,
    /// Reading started over from the start offset, so earlier records no longer count.
    pub restarted: bool,
    line: Vec<u8>,
}

impl Iterator for Batch<'_> {
    type Item = Result<Value>;

    fn next(&mut self) -> Option<Self::Item> {
        let reader = self.reader.as_mut()?;
        loop {
            self.line.clear();
            let read = match reader.read_until(b'\n', &mut self.line) {
                Ok(read) => read,
                Err(e) => return Some(Err(e.into())),
            };
            let body = self.line.strip_suffix(b"\n")?;
            let at = self.lines.offset;
            self.lines.offset += read as u64;
            match serde_json::from_slice::<Value>(body) {
                Ok(record) => return Some(Ok(record)),
                Err(_) if body.trim_ascii().is_empty() => {}
                Err(_) => {
                    log::warn!(
                        "{}:{at}: not a JSON record, skipped",
                        self.lines.path.display()
                    );
                    self.lines.skipped = true;
                }
            }
        }
    }
}

/// Follows the last turn of one conversation from a cursor, reading only what was added since
/// the previous update.
pub struct Tracker {
    runtime: Runtime,
    lines: Lines,
    parser: Parser,
}

impl Tracker {
    pub fn new(runtime: Runtime, path: &Path, cursor: u64) -> Self {
        Self {
            runtime,
            lines: Lines::new(path, cursor),
            parser: runtime.parser(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.lines.path
    }

    /// The offset after the last record read, where a turn after this one would start.
    pub fn end(&self) -> u64 {
        self.lines.offset
    }

    pub fn update(&mut self) -> Result<Turn> {
        let batch = self.lines.read()?;
        if batch.restarted {
            self.parser = self.runtime.parser();
        }
        for record in batch {
            self.parser.feed(&record?);
        }
        Ok(self.parser.turn())
    }
}

/// Reads the last turn after byte offset `cursor` in one pass, or `None` when a whole line was
/// not a JSON record: that line may be what ended the turn.
pub fn last_turn(runtime: Runtime, transcript: &Path, cursor: u64) -> Result<Option<Turn>> {
    let mut tracker = Tracker::new(runtime, transcript, cursor);
    let turn = tracker.update()?;
    Ok((!tracker.lines.skipped).then_some(turn))
}

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
