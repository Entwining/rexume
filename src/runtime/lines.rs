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
/// file that was replaced or shrank is read again from the start offset, which the batch reports;
/// one that does not exist yet holds no records.
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

impl Batch<'_> {
    /// The next record with where it starts and the offset after it.
    pub fn next_spanned(&mut self) -> Option<Result<(Value, Span)>> {
        let reader = self.reader.as_mut()?;
        loop {
            self.line.clear();
            let read = match reader.read_until(b'\n', &mut self.line) {
                Ok(read) => read,
                Err(e) => return Some(Err(e.into())),
            };
            let body = self.line.strip_suffix(b"\n")?;
            let start = self.lines.offset;
            self.lines.offset += read as u64;
            match serde_json::from_slice::<Value>(body) {
                Ok(record) => {
                    return Some(Ok((
                        record,
                        Span {
                            start,
                            end: self.lines.offset,
                        },
                    )));
                }
                Err(_) if body.trim_ascii().is_empty() => {}
                Err(_) => {
                    log::warn!(
                        "{}:{start}: not a JSON record, skipped",
                        self.lines.path.display()
                    );
                    self.lines.skipped = true;
                }
            }
        }
    }
}

impl Iterator for Batch<'_> {
    type Item = Result<Value>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_spanned()
            .map(|record| record.map(|(value, _)| value))
    }
}

/// Where a record or a turn lies in its file: the offset it starts at and the one after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: u64,
    pub end: u64,
}

/// A finished turn, in the order the record shows it. Its start offset names it however often the
/// record is read again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sealed {
    pub turn: Turn,
    pub span: Span,
}

/// The turn the record leaves open where it ends, for a runtime whose status can end it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Open {
    pub start: u64,
    /// When its first record was written, in Unix milliseconds, for a runtime that records
    /// nothing when its turn is stopped before any output (Claude Code).
    pub since: Option<i64>,
    /// An answer whose last part may still be coming, with the offset after the part so far.
    pub answer: Option<(Turn, u64)>,
}

/// The turns earlier rounds of a task took. A turn is named by where it starts, and an answered one
/// also by its answer's record ID when it has one, which names it from any offset inside it: a
/// round upgraded from version 1 recorded no start and left the position after the first part of
/// its answer.
#[derive(Default)]
pub struct Taken {
    pub starts: HashSet<u64>,
    pub answers: HashSet<String>,
}

impl Taken {
    fn contains(&self, turn: &Sealed) -> bool {
        self.starts.contains(&turn.span.start)
            || matches!(&turn.turn, Turn::Completed { reference, .. }
                if !reference.is_empty() && self.answers.contains(reference))
    }
}

/// Follows one round through one conversation's record from the task's position: each turn that
/// finishes, in order, and which of them is the round's. A request round's turn is the first to
/// end after the record shows the request received; any other round's is the first that no
/// earlier round took. A record that was replaced or cut is not read again from its start: the
/// turns after the position could no longer be told apart.
pub struct Tracker {
    runtime: Runtime,
    lines: Lines,
    parser: Parser,
    taken: Taken,
    /// The request's token.
    request: Option<String>,
    receipt: Option<u64>,
    /// Each finished turn, and whether an earlier round took it.
    sealed: Vec<(Span, bool)>,
    chosen: Option<Sealed>,
    replaced: bool,
}

impl Tracker {
    pub fn new(
        runtime: Runtime,
        path: &Path,
        position: u64,
        taken: Taken,
        request: Option<&str>,
    ) -> Self {
        Self {
            runtime,
            lines: Lines::new(path, position),
            parser: runtime.parser(),
            taken,
            request: request.map(token),
            receipt: None,
            sealed: Vec::new(),
            chosen: None,
            replaced: false,
        }
    }

    pub fn path(&self) -> &Path {
        &self.lines.path
    }

    /// Reads what was added since the last update.
    pub fn update(&mut self) -> Result<()> {
        if self.replaced {
            return Ok(());
        }
        let mut batch = self.lines.read()?;
        if batch.restarted {
            self.replaced = true;
            return Ok(());
        }
        let mut sealed = Vec::new();
        while let Some(record) = batch.next_spanned() {
            let (record, span) = record?;
            if let Some(token) = &self.request
                && self.receipt.is_none()
                && self.runtime.received(&record)
                && mentions(&record, token)
            {
                self.receipt = Some(span.start);
            }
            self.parser.feed(&record, span, &mut sealed);
            for turn in sealed.drain(..) {
                let taken = self.taken.contains(&turn);
                self.sealed.push((turn.span, taken));
                if self.chosen.is_none()
                    && !taken
                    && after_receipt(self.request.is_some(), self.receipt, turn.span)
                {
                    self.chosen = Some(turn);
                }
            }
        }
        Ok(())
    }

    /// The round's turn, once one finished.
    pub fn chosen(&self) -> Option<&Sealed> {
        self.chosen.as_ref()
    }

    /// The turn left open where the record ends, when it could still be the round's, as it would
    /// end now: with its answer so far, or, stopped before any answer, cancelled at the end of what
    /// was read. Also when it started, for a runtime that records it.
    pub fn open(&self) -> Option<(Sealed, Option<i64>)> {
        self.open_turn().filter(|(open, _)| {
            !self.taken.contains(open)
                && after_receipt(self.request.is_some(), self.receipt, open.span)
        })
    }

    fn open_turn(&self) -> Option<(Sealed, Option<i64>)> {
        let open = self.parser.open()?;
        let (turn, end) = open.answer.unwrap_or((Turn::Cancelled, self.lines.offset));
        let span = Span {
            start: open.start,
            end,
        };
        Some((Sealed { turn, span }, open.since))
    }

    /// Whether the record shows the request received.
    pub fn received(&self) -> bool {
        self.receipt.is_some()
    }

    pub fn replaced(&self) -> bool {
        self.replaced
    }

    /// The finished turns that no round took. Once the record has `ended`, as it has when the
    /// worker's pane closed, an answer its last record leaves open is finished too.
    pub fn unread(&self, ended: bool) -> usize {
        let answered = ended
            && self.open_turn().is_some_and(|(open, _)| {
                matches!(open.turn, Turn::Completed { .. }) && !self.taken.contains(&open)
            });
        self.sealed.iter().filter(|(_, taken)| !taken).count() + usize::from(answered)
    }

    /// The task's position once the round takes the turn at `span`: past every finished turn from
    /// the position on that a round took, in order. A turn taken while still open follows every
    /// finished one.
    pub fn position_after(&self, span: Span) -> u64 {
        let mut position = self.lines.start;
        let pending =
            (!self.sealed.iter().any(|(sealed, _)| *sealed == span)).then_some((span, true));
        for (turn, taken) in self.sealed.iter().chain(pending.as_ref()) {
            if turn.start != span.start && !taken {
                break;
            }
            position = turn.end;
        }
        position
    }
}

/// Whether a turn can be the round's as far as its request goes: a request round's turn ended after
/// the record showed the request received, and any turn can be another round's.
fn after_receipt(request: bool, receipt: Option<u64>, span: Span) -> bool {
    !request || receipt.is_some_and(|at| at < span.end)
}

/// Reads the last turn after byte offset `cursor` in one pass, or `None` when a whole line was
/// not a JSON record: that line may be what ended the turn.
pub fn last_turn(runtime: Runtime, transcript: &Path, cursor: u64) -> Result<Option<Turn>> {
    let mut lines = Lines::new(transcript, cursor);
    let mut parser = runtime.parser();
    let mut batch = lines.read()?;
    let mut sealed = Vec::new();
    while let Some(record) = batch.next_spanned() {
        let (record, span) = record?;
        parser.feed(&record, span, &mut sealed);
        sealed.clear();
    }
    Ok((!lines.skipped).then(|| parser.turn()))
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
