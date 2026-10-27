//! The one event queue the daemon waits on: a readable socket, a process's exit, a change to a
//! file or directory, and a signal each fire the token they were registered with. macOS has a
//! kqueue; Linux has an epoll over an inotify instance, pidfds, and a signalfd, which keep the
//! same meaning.

#[cfg(target_os = "linux")]
mod epoll;
#[cfg(target_os = "macos")]
mod kqueue;
#[cfg(test)]
mod tests;

#[cfg(target_os = "linux")]
pub use epoll::Queue;
#[cfg(target_os = "macos")]
pub use kqueue::Queue;

pub type Token = u64;

pub struct Fired {
    pub token: Token,
    /// A watched file was deleted or renamed: what its path names now needs watching again.
    pub gone: bool,
}
