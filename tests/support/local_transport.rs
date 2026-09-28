//! Real local transports for the protocol tests; no Herdr server is required.
#![allow(dead_code)] // Each test binary uses a different subset of the helpers.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

pub fn endpoint(label: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let name = format!(
        "hm-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    #[cfg(unix)]
    return PathBuf::from(format!("/tmp/{name}.sock"));
    #[cfg(windows)]
    return PathBuf::from(format!(r"C:\herdr-tests\{name}.sock"));
}

#[cfg(unix)]
pub use std::os::unix::net::{UnixListener as LocalListener, UnixStream as LocalStream};

#[cfg(unix)]
pub struct KeepAlive;

#[cfg(unix)]
pub fn keep_alive(_: &LocalListener) -> KeepAlive {
    KeepAlive
}

#[cfg(windows)]
pub use windows::{LocalListener, LocalStream};

#[cfg(windows)]
pub type KeepAlive = windows::KeepAlive;

#[cfg(windows)]
pub fn keep_alive(listener: &LocalListener) -> KeepAlive {
    windows::keep_alive(listener)
}

#[cfg(windows)]
mod windows {
    use std::future::Future;
    use std::io::{self, Read, Write};
    use std::net::Shutdown;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use tokio::runtime::{Builder, Handle};
    use tokio::sync::oneshot;

    // Keep the reactor running independently while the synchronous fake server
    // accepts new connections and its per-connection handler threads do I/O.
    struct Driver {
        handle: Handle,
        stop: Option<oneshot::Sender<()>>,
        thread: Option<JoinHandle<()>>,
    }

    impl Driver {
        fn start() -> io::Result<Arc<Self>> {
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let (stop_tx, stop_rx) = oneshot::channel();
            let thread =
                thread::spawn(
                    move || match Builder::new_current_thread().enable_all().build() {
                        Ok(runtime) => {
                            ready_tx.send(Ok(runtime.handle().clone())).unwrap();
                            runtime.block_on(async {
                                let _ = stop_rx.await;
                            });
                        }
                        Err(error) => {
                            ready_tx.send(Err(error)).unwrap();
                        }
                    },
                );
            let handle = ready_rx.recv().map_err(io::Error::other)??;
            Ok(Arc::new(Self {
                handle,
                stop: Some(stop_tx),
                thread: Some(thread),
            }))
        }

        fn create(&self, path: &Path, first: bool) -> io::Result<NamedPipeServer> {
            let _entered = self.handle.enter();
            // Match Herdr's GenericNamespaced endpoint conversion explicitly,
            // without sharing the production client's conversion helper.
            let pipe_name = format!(r"\\.\pipe\{}", path.to_string_lossy());
            ServerOptions::new()
                .first_pipe_instance(first)
                .create(pipe_name)
        }

        fn wait<T>(
            &self,
            timeout: Option<Duration>,
            operation: impl Future<Output = io::Result<T>>,
        ) -> io::Result<T> {
            self.handle.block_on(async {
                match timeout {
                    Some(duration) => tokio::time::timeout(duration, operation)
                        .await
                        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?,
                    None => operation.await,
                }
            })
        }
    }

    impl Drop for Driver {
        fn drop(&mut self) {
            if let Some(stop) = self.stop.take() {
                let _ = stop.send(());
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    pub struct KeepAlive {
        _driver: Arc<Driver>,
    }

    pub fn keep_alive(listener: &LocalListener) -> KeepAlive {
        // Mio deliberately retains pending writes when a pipe is dropped.
        // The test must keep driving their completion until the client has
        // consumed the response, even if its server handler already exited.
        KeepAlive {
            _driver: Arc::clone(&listener.driver),
        }
    }

    pub struct LocalListener {
        pending: Mutex<NamedPipeServer>,
        path: PathBuf,
        nonblocking: AtomicBool,
        driver: Arc<Driver>,
    }

    impl LocalListener {
        pub fn bind(path: &Path) -> io::Result<Self> {
            let driver = Driver::start()?;
            let pending = driver.create(path, true)?;
            Ok(Self {
                pending: Mutex::new(pending),
                path: path.to_path_buf(),
                nonblocking: AtomicBool::new(false),
                driver,
            })
        }

        pub fn set_nonblocking(&self, enabled: bool) -> io::Result<()> {
            self.nonblocking.store(enabled, Ordering::Relaxed);
            Ok(())
        }

        pub fn accept(&self) -> io::Result<(LocalStream, ())> {
            let mut pending = self.pending.lock().unwrap();
            let timeout = self
                .nonblocking
                .load(Ordering::Relaxed)
                .then_some(Duration::from_millis(1));
            self.driver
                .wait(timeout, pending.connect())
                .map_err(|error| {
                    if error.kind() == io::ErrorKind::TimedOut {
                        io::Error::from(io::ErrorKind::WouldBlock)
                    } else {
                        error
                    }
                })?;
            // Keep another instance available before dispatching the accepted
            // one; concurrent RPCs must not race a missing listener instance.
            let next = self.driver.create(&self.path, false)?;
            let pipe = std::mem::replace(&mut *pending, next);
            Ok((
                LocalStream {
                    pipe,
                    driver: Arc::clone(&self.driver),
                    read_timeout: Mutex::new(None),
                    write_timeout: Mutex::new(None),
                },
                (),
            ))
        }
    }

    pub struct LocalStream {
        pipe: NamedPipeServer,
        driver: Arc<Driver>,
        read_timeout: Mutex<Option<Duration>>,
        write_timeout: Mutex<Option<Duration>>,
    }

    impl LocalStream {
        pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            *self.read_timeout.lock().unwrap() = timeout;
            Ok(())
        }

        pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            *self.write_timeout.lock().unwrap() = timeout;
            Ok(())
        }

        pub fn shutdown(&self, _: Shutdown) -> io::Result<()> {
            self.pipe.disconnect()
        }
    }

    impl Read for LocalStream {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if buffer.is_empty() {
                return Ok(0);
            }
            self.driver.wait(*self.read_timeout.lock().unwrap(), async {
                loop {
                    self.pipe.readable().await?;
                    match self.pipe.try_read(buffer) {
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                        // Windows reports the peer's clean close as a broken
                        // or disconnected pipe, where Unix read returns EOF.
                        Err(error) if matches!(error.raw_os_error(), Some(109 | 233)) => {
                            return Ok(0);
                        }
                        result => return result,
                    }
                }
            })
        }
    }

    impl Write for LocalStream {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.driver
                .wait(*self.write_timeout.lock().unwrap(), async {
                    loop {
                        self.pipe.writable().await?;
                        match self.pipe.try_write(buffer) {
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                            result => return result,
                        }
                    }
                })
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
