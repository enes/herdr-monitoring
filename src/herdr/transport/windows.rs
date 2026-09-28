//! Windows named-pipe I/O behind the synchronous local transport interface.
//!
//! Tokio owns overlapped I/O and cancellation; the existing worker threads drive
//! a current-thread runtime only while connecting, reading, or writing. No extra
//! transport thread is created for each connection.

use std::cell::Cell;
use std::future::Future;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::Notify;

const ERROR_BROKEN_PIPE: i32 = 109;
const ERROR_PIPE_BUSY: i32 = 231;
const ERROR_PIPE_NOT_CONNECTED: i32 = 233;
const BUSY_RETRY_DELAY: Duration = Duration::from_millis(25);

pub struct LocalStream {
    // Drop the pipe before its I/O driver.
    pipe: NamedPipeClient,
    cancellation: Arc<Cancellation>,
    read_timeout: Cell<Option<Duration>>,
    write_timeout: Cell<Option<Duration>>,
    runtime: Runtime,
}

#[derive(Default)]
struct Cancellation {
    cancelled: AtomicBool,
    wake: Notify,
}

impl Cancellation {
    async fn wait(&self) {
        if !self.cancelled.load(Ordering::Acquire) {
            // There is at most one operation because Read/Write require &mut
            // LocalStream. notify_one retains a permit if shutdown happens
            // between the flag check and registering this waiter.
            self.wake.notified().await;
        }
    }
}

#[derive(Clone)]
pub struct ShutdownHandle {
    cancellation: Arc<Cancellation>,
}

impl ShutdownHandle {
    pub fn shutdown(&self) -> io::Result<()> {
        if !self.cancellation.cancelled.swap(true, Ordering::AcqRel) {
            self.cancellation.wake.notify_one();
        }
        Ok(())
    }
}

impl LocalStream {
    pub fn connect(path: impl AsRef<Path>, timeout: Duration) -> io::Result<Self> {
        let pipe_name = super::windows_pipe_name(path.as_ref())?;
        let runtime = Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()?;
        let pipe = runtime.block_on(async {
            tokio::time::timeout(timeout, async {
                loop {
                    match ClientOptions::new().open(pipe_name.as_os_str()) {
                        Ok(pipe) => return Ok(pipe),
                        Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                            tokio::time::sleep(BUSY_RETRY_DELAY).await;
                        }
                        // Missing endpoints return immediately so the event
                        // worker's reconnect/backoff owns server restarts.
                        Err(error) => return Err(error),
                    }
                }
            })
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
        })?;
        Ok(Self {
            pipe,
            cancellation: Arc::default(),
            read_timeout: Cell::new(None),
            write_timeout: Cell::new(None),
            runtime,
        })
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        set_timeout(&self.read_timeout, timeout)
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        set_timeout(&self.write_timeout, timeout)
    }

    pub fn shutdown_handle(&self) -> io::Result<ShutdownHandle> {
        Ok(ShutdownHandle {
            cancellation: Arc::clone(&self.cancellation),
        })
    }
}

impl Read for LocalStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        match self.runtime.block_on(run_operation(
            &self.cancellation,
            self.read_timeout.get(),
            self.pipe.read(buffer),
        )) {
            // Mio already turns ERROR_BROKEN_PIPE into EOF. An explicitly
            // disconnected pipe has the same byte-stream EOF semantics.
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED)
                ) =>
            {
                Ok(0)
            }
            result => result,
        }
    }
}

impl Write for LocalStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        self.runtime.block_on(run_operation(
            &self.cancellation,
            self.write_timeout.get(),
            self.pipe.write(buffer),
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        // Match Tokio's named-pipe flush semantics. Native FlushFileBuffers
        // waits for the peer to drain the pipe; the JSON protocol does not
        // require that and it would bypass request deadlines.
        Ok(())
    }
}

fn set_timeout(cell: &Cell<Option<Duration>>, timeout: Option<Duration>) -> io::Result<()> {
    if timeout.is_some_and(|value| value.is_zero()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "transport timeout must be greater than zero",
        ));
    }
    cell.set(timeout);
    Ok(())
}

async fn run_operation<T>(
    cancellation: &Cancellation,
    timeout: Option<Duration>,
    operation: impl Future<Output = io::Result<T>>,
) -> io::Result<T> {
    tokio::select! {
        biased;
        _ = cancellation.wait() => Err(io::Error::from(io::ErrorKind::ConnectionAborted)),
        result = async {
            match timeout {
                Some(timeout) => tokio::time::timeout(timeout, operation)
                    .await
                    .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?,
                None => operation.await,
            }
        } => result,
    }
}

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::sync::atomic::AtomicU64;

    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    use super::*;

    #[test]
    fn shutdown_before_wait_cancels_current_and_future_operations() {
        let cancellation = Arc::new(Cancellation::default());
        let handle = ShutdownHandle {
            cancellation: Arc::clone(&cancellation),
        };
        handle.shutdown().unwrap();
        handle.shutdown().unwrap();
        let runtime = Builder::new_current_thread().enable_time().build().unwrap();
        for _ in 0..2 {
            let error = runtime
                .block_on(run_operation(
                    &cancellation,
                    Some(Duration::from_secs(1)),
                    pending::<io::Result<()>>(),
                ))
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        }
    }

    fn connected_pipe() -> (std::path::PathBuf, LocalStream, NamedPipeServer, Runtime) {
        static NEXT_PIPE: AtomicU64 = AtomicU64::new(0);
        let endpoint = std::path::PathBuf::from(format!(
            "herdr-monitor-backend-{}-{}",
            std::process::id(),
            NEXT_PIPE.fetch_add(1, Ordering::Relaxed),
        ));
        let runtime = Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap();
        let server = {
            let _entered = runtime.enter();
            ServerOptions::new()
                .max_instances(1)
                .in_buffer_size(4096)
                .create(super::super::windows_pipe_name(&endpoint).unwrap())
                .unwrap()
        };
        let stream = LocalStream::connect(&endpoint, Duration::from_secs(2)).unwrap();
        runtime.block_on(server.connect()).unwrap();
        (endpoint, stream, server, runtime)
    }

    #[test]
    fn busy_pipe_connection_has_a_deadline() {
        let (endpoint, _stream, _server, _runtime) = connected_pipe();
        let result = LocalStream::connect(endpoint, Duration::from_millis(50));
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::TimedOut));
    }

    #[test]
    fn peer_that_does_not_read_cannot_block_writes_forever() {
        let (_endpoint, mut stream, _server, _runtime) = connected_pipe();
        stream
            .set_write_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        // Tokio may accept the first buffer while the overlapped write is in
        // flight. Subsequent writes must respect the timeout if the peer never
        // drains its small input buffer.
        let block = [0; 64 * 1024];
        for _ in 0..16 {
            match stream.write_all(&block) {
                Err(error) => {
                    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
                    return;
                }
                Ok(()) => continue,
            }
        }
        panic!("writes never applied backpressure to the unread pipe");
    }
}
