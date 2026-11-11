use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use super::{Parser, Runtime, Turn, mentions, token};

pub struct Lines {
    path: PathBuf,
    start: u64,
    offset: u64,
    identity: Option<(u64, u64)>,
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

pub struct Batch<'a> {
    lines: &'a mut Lines,
    reader: Option<BufReader<File>>,
    pub restarted: bool,
    line: Vec<u8>,
}

impl Batch<'_> {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sealed {
    pub turn: Turn,
    pub span: Span,
    pub took: Vec<String>,
}

impl Sealed {
    /// A cancelled turn may be a prompt another hook rejected, which took nothing in.
    pub fn received(&self, token: &str) -> bool {
        self.turn != Turn::Cancelled && self.took.iter().any(|t| t == token)
    }
}

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

pub struct Tracker {
    runtime: Runtime,
    lines: Lines,
    parser: Parser,
    taken: Taken,
    request: Option<String>,
    /// The request is `delivered` already; one this record shows no receipt of was received in the
    /// record that version 8 replaced.
    delivered: bool,
    receipt: Option<u64>,
    untaken: Option<Sealed>,
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
        request: Option<&(String, bool)>,
    ) -> Self {
        Self {
            runtime,
            lines: Lines::new(path, position),
            parser: runtime.parser(),
            taken,
            request: request.map(|(id, _)| token(id)),
            delivered: request.is_some_and(|&(_, delivered)| delivered),
            receipt: None,
            untaken: None,
            sealed: Vec::new(),
            chosen: None,
            replaced: false,
        }
    }

    pub fn path(&self) -> &Path {
        &self.lines.path
    }

    pub fn update(&mut self) -> Result<()> {
        if self.replaced {
            return Ok(());
        }
        let mut batch = self.lines.read()?;
        if batch.restarted {
            // The position and the taken turns' starts are offsets into the record before it was
            // replaced or cut.
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
                let took = self.request.as_ref().is_some_and(|t| turn.took.contains(t));
                if let Some(token) = &self.request
                    && self.receipt.is_none()
                    && turn.received(token)
                {
                    self.receipt = Some(turn.span.start);
                }
                let taken = self.taken.contains(&turn);
                self.sealed.push((turn.span, taken));
                if self.chosen.is_some() || taken {
                    continue;
                }
                if took || after_receipt(self.request.is_some(), self.receipt, turn.span) {
                    self.chosen = Some(turn);
                } else if self.delivered && self.untaken.is_none() {
                    self.untaken = Some(turn);
                }
            }
        }
        if self.delivered && self.receipt.is_none() {
            self.receipt = Some(self.lines.start);
            if self.chosen.is_none() {
                self.chosen = self.untaken.take();
            }
        }
        Ok(())
    }

    pub fn chosen(&self) -> Option<&Sealed> {
        self.chosen.as_ref()
    }

    pub fn received(&self) -> bool {
        self.receipt.is_some()
    }

    pub fn replaced(&self) -> bool {
        self.replaced
    }

    pub fn unread(&self) -> usize {
        self.sealed.iter().filter(|(_, taken)| !taken).count()
    }

    pub fn position_after(&self, span: Span) -> u64 {
        let mut position = self.lines.start;
        for (turn, taken) in &self.sealed {
            if turn.start != span.start && !taken {
                break;
            }
            position = turn.end;
        }
        position
    }
}

fn after_receipt(request: bool, receipt: Option<u64>, span: Span) -> bool {
    !request || receipt.is_some_and(|at| at < span.end)
}

/// `None` when a whole line after `cursor` was not JSON: that line may be what ended the turn.
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

pub fn last_record<T>(
    path: &Path,
    needle: &str,
    pick: impl Fn(&Value) -> Option<T>,
) -> Result<Option<T>> {
    const BLOCK: u64 = 64 * 1024;
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let needle = needle.as_bytes();
    let mut pos = file.metadata()?.len();
    let mut carry = Vec::new();
    let mut unfinished = true;
    while pos > 0 {
        let start = pos.saturating_sub(BLOCK);
        let mut chunk = vec![0; usize::try_from(pos - start)?];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut chunk)?;
        chunk.append(&mut carry);
        let mut lines: Vec<&[u8]> = chunk.split(|b| *b == b'\n').collect();
        if unfinished {
            unfinished = lines.len() == 1;
            lines.pop();
        }
        if start > 0 && !lines.is_empty() {
            carry = lines.remove(0).to_vec();
        }
        for line in lines.iter().rev() {
            if !line.windows(needle.len()).any(|w| w == needle) {
                continue;
            }
            if let Some(found) = serde_json::from_slice(line).ok().and_then(|r| pick(&r)) {
                return Ok(Some(found));
            }
        }
        pos = start;
    }
    Ok(None)
}
