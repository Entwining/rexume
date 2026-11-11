use std::fs::File;
use std::io::{PipeReader, Read, Seek};
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use nix::sys::signal::{SigSet, SigmaskHow, Signal, killpg};
use nix::unistd::Pid;

use crate::event::{Queue, Token};

const STOPS: [Signal; 3] = [Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP];
const OUTPUT: Token = 1;
const STOP: Token = 100;
const EXIT: Token = 1000;
const STDERR_LIMIT: u64 = 4 << 10;
/// No path or listed entry comes near it.
const RECORD_LIMIT: usize = 64 << 10;

type EachRecord<'a> = dyn FnMut(&[u8]) -> Result<()> + 'a;

#[derive(Debug, thiserror::Error)]
#[error("the snapshot was stopped by {0}")]
pub struct Stopped(pub Signal);

/// Git and its filters inherit the block, since `Command` keeps the signal mask: the group's
/// `SIGKILL` is what ends them.
pub struct Blocked(SigSet);

impl Blocked {
    pub fn new() -> Result<Self> {
        let stops: SigSet = STOPS.into_iter().collect();
        Ok(Self(stops.thread_swap_mask(SigmaskHow::SIG_BLOCK)?))
    }
}

impl Drop for Blocked {
    fn drop(&mut self) {
        let _ = self.0.thread_set_mask();
    }
}

pub(super) struct Git {
    queue: Queue,
    budget: Duration,
    until: Instant,
}

impl Git {
    pub fn new(budget: Duration) -> Result<Self> {
        #[cfg_attr(
            target_os = "macos",
            expect(unused_mut, reason = "kqueue's methods take &self")
        )]
        let mut queue = Queue::new()?;
        for signal in STOPS {
            queue.signal(signal, STOP + signal as Token)?;
        }
        Ok(Self {
            queue,
            budget,
            until: Instant::now() + budget,
        })
    }

    pub fn left(&self) -> Result<Duration> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            let seconds = self.budget.as_secs_f64();
            bail!("the snapshot took longer than {seconds} seconds");
        }
        Ok(left)
    }

    /// Standard output goes to a file, since a pipe nobody reads until the command exits would stop
    /// one that prints more than the pipe holds. With `filtered`, standard error goes nowhere, as a
    /// filter may write a file's content to it.
    pub fn run(
        &mut self,
        mut command: Command,
        args: &[&str],
        stdin: Stdio,
        filtered: bool,
    ) -> Result<Ran> {
        let stdout = tempfile::tempfile()?;
        let stderr = (!filtered).then(tempfile::tempfile).transpose()?;
        command.args(args).stdin(stdin).stdout(stdout.try_clone()?);
        command.stderr(match &stderr {
            Some(file) => Stdio::from(file.try_clone()?),
            None => Stdio::null(),
        });
        let mut group = self.spawn(vec![command])?;
        let status = self.wait(&mut group, None)?[0];
        let mut stdout = stdout;
        stdout.rewind()?;
        Ran::new(args[0], status, Some(stdout), stderr)
    }

    pub fn stream(
        &mut self,
        mut command: Command,
        args: &[&str],
        each: &mut EachRecord<'_>,
    ) -> Result<Ran> {
        let (reader, writer) = std::io::pipe()?;
        let stderr = tempfile::tempfile()?;
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(writer)
            .stderr(stderr.try_clone()?);
        let mut group = self.spawn(vec![command])?;
        let status = self.wait(&mut group, Some((reader, each)))?[0];
        Ran::new(args[0], status, None, Some(stderr))
    }

    pub fn pipe(
        &mut self,
        (mut first, first_args): (Command, &[&str]),
        stdin: Stdio,
        (mut second, second_args): (Command, &[&str]),
    ) -> Result<()> {
        let (reader, writer) = std::io::pipe()?;
        let stderr = tempfile::tempfile()?;
        first
            .args(first_args)
            .stdin(stdin)
            .stdout(writer)
            .stderr(stderr.try_clone()?);
        second
            .args(second_args)
            .stdin(reader)
            .stdout(Stdio::null())
            .stderr(stderr.try_clone()?);
        let mut group = self.spawn(vec![first, second])?;
        let statuses = self.wait(&mut group, None)?;
        for (status, name) in statuses.into_iter().zip([first_args[0], second_args[0]]) {
            Ran::new(name, status, None, Some(stderr.try_clone()?))?.check()?;
        }
        Ok(())
    }

    /// Each command is dropped once started, so that this process's ends of the pipes it holds
    /// close.
    fn spawn(&mut self, commands: Vec<Command>) -> Result<Group> {
        self.left()?;
        let mut group = Group {
            id: None,
            children: Vec::new(),
        };
        for mut command in commands {
            let name = command.get_program().to_string_lossy().into_owned();
            let leader = group.id.map_or(0, Pid::as_raw);
            let child = command
                .process_group(leader)
                .spawn()
                .with_context(|| format!("failed to start `{name}`"))?;
            group
                .id
                .get_or_insert(Pid::from_raw(child.id().cast_signed()));
            group.children.push(child);
        }
        Ok(group)
    }

    fn wait(
        &mut self,
        group: &mut Group,
        mut output: Option<(PipeReader, &mut EachRecord<'_>)>,
    ) -> Result<Vec<ExitStatus>> {
        let mut running = 0;
        for (at, child) in group.children.iter().enumerate() {
            // Linux's queue merges equal tokens within one wait, so a shared exit token would
            // undercount exited children.
            running += usize::from(self.queue.exit(i64::from(child.id()), EXIT + at as Token)?);
        }
        if running == 0 {
            group.kill();
        }
        if let Some((reader, _)) = &output {
            self.queue.read(reader.as_fd(), OUTPUT)?;
        }
        let mut pending = Vec::new();
        while running > 0 || output.is_some() {
            let left = self.left()?;
            for fired in self.queue.wait(Some(left))? {
                match fired.token {
                    OUTPUT => {
                        if let Some((reader, each)) = &mut output
                            && !read(reader, &mut pending, &mut **each)?
                        {
                            // Closing the pipe takes it off the queue.
                            output = None;
                        }
                    }
                    token if token >= EXIT => {
                        running -= 1;
                        if running == 0 {
                            // A process the command left would keep the pipe open.
                            group.kill();
                        }
                    }
                    token if token >= STOP => {
                        let signal = i32::try_from(token - STOP)?;
                        return Err(Stopped(Signal::try_from(signal)?).into());
                    }
                    _ => {}
                }
            }
        }
        group.end()
    }
}

fn read(reader: &mut PipeReader, pending: &mut Vec<u8>, each: &mut EachRecord<'_>) -> Result<bool> {
    let mut chunk = [0; 64 << 10];
    let n = reader.read(&mut chunk)?;
    pending.extend_from_slice(&chunk[..n]);
    let mut done = 0;
    while let Some(end) = pending[done..].iter().position(|&b| b == 0) {
        each(&pending[done..done + end])?;
        done += end + 1;
    }
    pending.drain(..done);
    if pending.len() > RECORD_LIMIT {
        bail!("`git` printed a record longer than {RECORD_LIMIT} bytes");
    }
    Ok(n > 0)
}

struct Group {
    id: Option<Pid>,
    children: Vec<Child>,
}

impl Group {
    fn kill(&self) {
        if let Some(id) = self.id {
            let _ = killpg(id, Signal::SIGKILL);
        }
    }

    /// Kills before reaping and forgets the ID: once the processes are reaped, another group can
    /// take it.
    fn end(&mut self) -> Result<Vec<ExitStatus>> {
        self.kill();
        self.id = None;
        self.children
            .drain(..)
            .map(|mut child| Ok(child.wait()?))
            .collect()
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        let _ = self.end();
    }
}

pub(super) struct Ran {
    command: String,
    pub status: ExitStatus,
    stdout: Option<File>,
    pub stderr: Option<String>,
}

impl Ran {
    fn new(
        command: &str,
        status: ExitStatus,
        stdout: Option<File>,
        stderr: Option<File>,
    ) -> Result<Self> {
        let stderr = stderr
            .map(|mut file| -> Result<String> {
                let mut quoted = Vec::new();
                file.rewind()?;
                file.take(STDERR_LIMIT).read_to_end(&mut quoted)?;
                Ok(String::from_utf8_lossy(&quoted).into_owned())
            })
            .transpose()?;
        Ok(Self {
            command: command.to_owned(),
            status,
            stdout,
            stderr,
        })
    }

    /// Git expanding a sparse index goes on without the entries of a directory whose tree it cannot
    /// read, says so, and exits 0, so that message fails the command too. Its other errors while
    /// expanding one, about blobs Rexume did not copy, cost Git only its cache of trees.
    pub fn check(&self) -> Result<()> {
        let stderr = self.stderr.as_deref().unwrap_or_default().trim();
        let unread = stderr.lines().any(|line| {
            line.strip_prefix("error: Could not read ")
                .is_some_and(|object| object.bytes().all(|b| b.is_ascii_hexdigit()))
        });
        if !self.status.success() || unread {
            bail!("`git {}` exited {}: {stderr}", self.command, self.status);
        }
        Ok(())
    }

    pub fn output(self) -> Result<Vec<u8>> {
        self.check()?;
        let mut output = Vec::new();
        self.stdout
            .context("the output was read as it came")?
            .read_to_end(&mut output)?;
        Ok(output)
    }
}
