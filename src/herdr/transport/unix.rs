use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub struct LocalStream(UnixStream);

impl LocalStream {
    pub fn connect(path: impl AsRef<Path>, _timeout: Duration) -> io::Result<Self> {
        UnixStream::connect(path).map(Self)
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self.0.set_read_timeout(timeout) {
            // macOS may reject socket options after peer closure while buffered
            // frames and EOF are still readable. Preserve those frames.
            Err(error)
                if error.kind() == io::ErrorKind::InvalidInput && self.0.peer_addr().is_err() =>
            {
                Ok(())
            }
            result => result,
        }
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.set_write_timeout(timeout)
    }

    pub fn shutdown_handle(&self) -> io::Result<ShutdownHandle> {
        self.0
            .try_clone()
            .map(|stream| ShutdownHandle(Arc::new(stream)))
    }
}

impl Read for LocalStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
}

impl Write for LocalStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

#[derive(Clone)]
pub struct ShutdownHandle(Arc<UnixStream>);

impl ShutdownHandle {
    pub fn shutdown(&self) -> io::Result<()> {
        match self.0.shutdown(Shutdown::Both) {
            Err(error) if error.kind() == io::ErrorKind::NotConnected => Ok(()),
            result => result,
        }
    }
}
