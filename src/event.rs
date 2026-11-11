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
    /// The watched inode was deleted, renamed, or replaced at its path, so later changes there
    /// need a new watch.
    pub gone: bool,
}
