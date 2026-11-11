//! The runner of a snapshot's Git commands. Each command, with whatever it starts, is a process
//! group of its own, which ends when the command exits, at the snapshot's deadline, or when a
//! signal stops the command that takes the snapshot, so that nothing a snapshot started outlives it
//! while Rexume runs.

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

/// The signals that stop a command, which a snapshot takes through its queue while they are
/// blocked, so that it ends its Git commands and deletes what it wrote before the command stops.
const STOPS: [Signal; 3] = [Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP];
const OUTPUT: Token = 1;
/// Plus the signal's number.
const STOP: Token = 100;
/// Plus the process's place in its group: the queue reports a token once however many of its
/// events one wait takes, so processes that exit together under one token would count as one.
const EXIT: Token = 1000;
/// How much of a command's standard error a note quotes.
const STDERR_LIMIT: u64 = 4 << 10;
/// The longest record of a command's output Rexume reads: no path or listed entry comes near.
const RECORD_LIMIT: usize = 64 << 10;

/// What each NUL-ended record a command prints is passed to.
type EachRecord<'a> = dyn FnMut(&[u8]) -> Result<()> + 'a;

/// The signal that stopped a snapshot, which stops the command once it has deleted what the
/// snapshot wrote.
#[derive(Debug, thiserror::Error)]
#[error("the snapshot was stopped by {0}")]
pub struct Stopped(pub Signal);

/// Blocks the signals that stop a command while it lives, so that the queue of a snapshot running
/// meanwhile takes them. Git and its filters inherit the block, since `Command` keeps the signal
/// mask, and only the group's `SIGKILL` reaches them.
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

/// The Git commands of one snapshot, which share its deadline and the queue that waits for each.
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

    /// The time left before the deadline, failing once there is none.
    pub fn left(&self) -> Result<Duration> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            let seconds = self.budget.as_secs_f64();
            bail!("the snapshot took longer than {seconds} seconds");
        }
        Ok(left)
    }

    /// Runs `command` with `args` and `stdin` to its exit. Its standard output goes to a file,
    /// since a pipe nobody reads until it exits would stop a command that prints more than the pipe
    /// holds; its standard error goes to a file whose beginning the result keeps, or, when a filter
    /// may write a file's content to it, nowhere.
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

    /// Runs `command` with `args`, passing each NUL-ended record of its standard output to `each`
    /// as it arrives; the command ends as soon as `each` fails.
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

    /// Runs `first` with `first_args` and `stdin`, and `second` with `second_args` reading its
    /// output, as one group, and fails unless both succeed.
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

    /// Starts `commands` in one new process group, the first leading it. Each command is dropped
    /// once started, so that the ends of pipes it holds close in this process.
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

    /// Waits until every process of `group` has exited, reading `output` meanwhile, and returns
    /// their statuses. The group is ended whatever happens, its processes reaped only after, so
    /// that no other group can take its ID meanwhile.
    fn wait(
        &mut self,
        group: &mut Group,
        mut output: Option<(PipeReader, &mut EachRecord<'_>)>,
    ) -> Result<Vec<ExitStatus>> {
        let mut running = 0;
        for (at, child) in group.children.iter().enumerate() {
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

/// Reads what `reader` holds, passing each record it completes to `each`; `false` at its end.
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

/// The processes of one Git command.
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

    /// Kills the group and reaps its processes, after which its ID is no longer signalled: once
    /// they are reaped, another group can take it.
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

/// A Git command that ran to its exit.
pub(super) struct Ran {
    command: String,
    pub status: ExitStatus,
    stdout: Option<File>,
    /// The beginning of its standard error, up to `STDERR_LIMIT`, unless a filter could write to
    /// it.
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

    /// Fails unless the command succeeded. Git expanding a sparse index goes on without the
    /// entries of a directory whose tree it cannot read, says so, and exits 0, so that message
    /// fails the command too. The other errors an isolated command reports while it expands one,
    /// about the blobs Rexume did not copy, cost Git only the cache of its trees.
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

    /// The standard output read whole, or the error the command reported.
    pub fn output(self) -> Result<Vec<u8>> {
        self.check()?;
        let mut output = Vec::new();
        self.stdout
            .context("the output was read as it came")?
            .read_to_end(&mut output)?;
        Ok(output)
    }
}
