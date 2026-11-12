//! File opens from Explorer are forwarded to the running player. The OS picks
//! the listener's port. A lock and endpoint record in the user's data directory
//! let later launches find it without reserving a fixed port.

use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, ErrorKind, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

use eframe::egui;

const LOCK_FILE: &str = "player.lock";
const ENDPOINT_FILE: &str = "player.endpoint";
const READY: &[u8] = b"MusicPlayer/2";
const ACK: [u8; 1] = [6];
const MAX_PATH_BYTES: usize = 128 * 1024;
const HANDOFF_TIMEOUT: Duration = Duration::from_millis(900);
const REQUEST_TIMEOUT: Duration = Duration::from_millis(400);

pub enum Startup {
    Primary(Option<FileRequests>),
    Secondary,
}

pub struct FileRequests {
    receiver: Receiver<PathBuf>,
    repaint: Arc<OnceLock<egui::Context>>,
}

impl FileRequests {
    pub fn attach(self, ctx: egui::Context) -> Receiver<PathBuf> {
        let _ = self.repaint.set(ctx.clone());
        // A launch can be queued before eframe finishes creating the window.
        ctx.request_repaint();
        self.receiver
    }
}

struct Endpoint {
    address: SocketAddr,
    client_token: [u8; 16],
    server_token: [u8; 16],
}

impl Endpoint {
    fn read(path: &Path) -> io::Result<Self> {
        let mut record = [0; 34];
        File::open(path)?.read_exact(&mut record)?;
        let port = u16::from_be_bytes([record[0], record[1]]);
        if port == 0 {
            return Err(ErrorKind::InvalidData.into());
        }
        let mut client_token = [0; 16];
        let mut server_token = [0; 16];
        client_token.copy_from_slice(&record[2..18]);
        server_token.copy_from_slice(&record[18..]);
        Ok(Self {
            address: (Ipv4Addr::LOCALHOST, port).into(),
            client_token,
            server_token,
        })
    }

    fn write(&self, path: &Path) -> io::Result<()> {
        let mut record = [0; 34];
        record[..2].copy_from_slice(&self.address.port().to_be_bytes());
        record[2..18].copy_from_slice(&self.client_token);
        record[18..].copy_from_slice(&self.server_token);
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        options.open(path)?.write_all(&record)
    }
}

pub fn acquire(path: Option<&Path>) -> Startup {
    #[cfg(windows)]
    let directory =
        std::env::var_os("LOCALAPPDATA").map(|path| PathBuf::from(path).join("MusicPlayer"));
    #[cfg(not(windows))]
    let directory = eframe::storage_dir("MusicPlayer");

    match directory {
        Some(directory) if directory.is_absolute() => acquire_in(&directory.join("instance"), path),
        _ => Startup::Primary(None),
    }
}

fn acquire_in(directory: &Path, path: Option<&Path>) -> Startup {
    let Ok(lock) = open_lock(directory) else {
        return Startup::Primary(None);
    };
    let endpoint_file = directory.join(ENDPOINT_FILE);
    match lock.try_lock() {
        Ok(()) => Startup::Primary(start_listener(lock, &endpoint_file).ok()),
        Err(TryLockError::WouldBlock) if forward(&endpoint_file, path).is_ok() => {
            Startup::Secondary
        }
        // Opening a file stays available if local IPC cannot complete.
        Err(_) => Startup::Primary(None),
    }
}

fn open_lock(directory: &Path) -> io::Result<File> {
    fs::create_dir_all(directory)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(LOCK_FILE))
}

fn start_listener(lock: File, endpoint_file: &Path) -> io::Result<FileRequests> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let mut endpoint = Endpoint {
        address: listener.local_addr()?,
        client_token: [0; 16],
        server_token: [0; 16],
    };
    for token in [&mut endpoint.client_token, &mut endpoint.server_token] {
        getrandom::fill(token).map_err(|error| io::Error::other(error.to_string()))?;
    }
    endpoint.write(endpoint_file)?;

    let (sender, receiver) = mpsc::sync_channel(64);
    let repaint = Arc::new(OnceLock::<egui::Context>::new());
    let ctx = Arc::clone(&repaint);
    thread::Builder::new()
        .name("file-handoff".into())
        .spawn(move || {
            let _lock = lock;
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let _ = receive(stream, &endpoint, &sender, &ctx);
                    }
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })?;

    Ok(FileRequests { receiver, repaint })
}

fn forward(endpoint_file: &Path, path: Option<&Path>) -> io::Result<()> {
    let path = path.map(std::path::absolute).transpose()?;
    let path = match &path {
        Some(path) => path.to_str().ok_or(ErrorKind::InvalidData)?.as_bytes(),
        None => &[],
    };
    if path.len() > MAX_PATH_BYTES {
        return Err(ErrorKind::InvalidInput.into());
    }

    let deadline = Instant::now() + HANDOFF_TIMEOUT;
    let mut stream = loop {
        if let Ok(stream) = connect(endpoint_file, deadline) {
            break stream;
        }
        // The lock owner might still be publishing a new endpoint.
        let remaining = time_left(deadline)?;
        thread::sleep(remaining.min(Duration::from_millis(15)));
    };
    stream.set_write_timeout(Some(time_left(deadline)?))?;
    stream.write_all(&(path.len() as u32).to_be_bytes())?;
    stream.write_all(path)?;
    let mut ack = [0];
    read_before(&mut stream, &mut ack, deadline)?;
    if ack != ACK {
        return Err(ErrorKind::InvalidData.into());
    }
    Ok(())
}

fn connect(endpoint_file: &Path, deadline: Instant) -> io::Result<TcpStream> {
    let endpoint = Endpoint::read(endpoint_file)?;
    let deadline = deadline.min(Instant::now() + REQUEST_TIMEOUT);
    let mut stream = TcpStream::connect_timeout(&endpoint.address, time_left(deadline)?)?;
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(time_left(deadline)?))?;
    stream.write_all(&endpoint.client_token)?;
    let mut ready = [0; READY.len()];
    read_before(&mut stream, &mut ready, deadline)?;
    if ready != READY {
        return Err(ErrorKind::InvalidData.into());
    }
    let mut token = [0; 16];
    read_before(&mut stream, &mut token, deadline)?;
    if token != endpoint.server_token {
        return Err(ErrorKind::PermissionDenied.into());
    }
    Ok(stream)
}

fn receive(
    mut stream: TcpStream,
    endpoint: &Endpoint,
    sender: &SyncSender<PathBuf>,
    repaint: &OnceLock<egui::Context>,
) -> io::Result<()> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    stream.set_nodelay(true)?;
    let mut token = [0; 16];
    read_before(&mut stream, &mut token, deadline)?;
    if token != endpoint.client_token {
        return Err(ErrorKind::PermissionDenied.into());
    }
    stream.set_write_timeout(Some(time_left(deadline)?))?;
    stream.write_all(READY)?;
    // The client sends its file path only after the listener proves it owns
    // the endpoint record. This proof is independent of the client's token.
    stream.write_all(&endpoint.server_token)?;

    let mut length = [0; 4];
    read_before(&mut stream, &mut length, deadline)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_PATH_BYTES {
        return Err(ErrorKind::InvalidData.into());
    }
    if length != 0 {
        let mut bytes = vec![0; length];
        read_before(&mut stream, &mut bytes, deadline)?;
        let path = PathBuf::from(String::from_utf8(bytes).map_err(|_| ErrorKind::InvalidData)?);
        if !path.is_absolute() {
            return Err(ErrorKind::InvalidData.into());
        }
        sender.try_send(path).map_err(io::Error::other)?;
        if let Some(ctx) = repaint.get() {
            ctx.request_repaint();
        }
    }
    stream.set_write_timeout(Some(time_left(deadline)?))?;
    stream.write_all(&ACK)
}

fn read_before(stream: &mut TcpStream, mut buffer: &mut [u8], deadline: Instant) -> io::Result<()> {
    while !buffer.is_empty() {
        stream.set_read_timeout(Some(time_left(deadline)?))?;
        match stream.read(buffer) {
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(size) => buffer = &mut buffer[size..],
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn time_left(deadline: Instant) -> io::Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(ErrorKind::TimedOut.into())
    } else {
        Ok(remaining)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_launch_delivers_an_exact_path() {
        let directory = tempfile::tempdir().unwrap();
        let Startup::Primary(Some(primary)) = acquire_in(directory.path(), None) else {
            panic!("first launch should own the listener");
        };
        let path = directory.path().join("Café set [01].mp3");
        assert!(matches!(
            acquire_in(directory.path(), Some(&path)),
            Startup::Secondary
        ));
        assert_eq!(
            primary.receiver.recv_timeout(HANDOFF_TIMEOUT).unwrap(),
            path
        );
    }

    #[test]
    fn unrelated_listener_never_receives_the_track_path() {
        let directory = tempfile::tempdir().unwrap();
        let lock = open_lock(directory.path()).unwrap();
        lock.try_lock().unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        Endpoint {
            address: listener.local_addr().unwrap(),
            client_token: [7; 16],
            server_token: [9; 16],
        }
        .write(&directory.path().join(ENDPOINT_FILE))
        .unwrap();
        let unrelated = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            drop(listener);
            stream.set_read_timeout(Some(HANDOFF_TIMEOUT)).unwrap();
            let mut credentials = [0; 16];
            stream.read_exact(&mut credentials).unwrap();
            stream.write_all(READY).unwrap();
            // A plausible protocol marker and the bytes the client sent do not
            // prove this listener owns the endpoint record.
            stream.write_all(&credentials).unwrap();
            let mut remainder = Vec::new();
            if let Err(error) = stream.read_to_end(&mut remainder) {
                assert_eq!(error.kind(), ErrorKind::ConnectionReset);
            }
            assert!(remainder.is_empty());
        });

        let path = directory.path().join("private title.mp3");
        assert!(matches!(
            acquire_in(directory.path(), Some(&path)),
            Startup::Primary(None)
        ));
        unrelated.join().unwrap();
    }

    #[test]
    fn a_stalled_peer_times_out_and_the_next_open_is_delivered() {
        let directory = tempfile::tempdir().unwrap();
        let Startup::Primary(Some(primary)) = acquire_in(directory.path(), None) else {
            panic!("first launch should own the listener");
        };
        let _stalled = connect(
            &directory.path().join(ENDPOINT_FILE),
            Instant::now() + HANDOFF_TIMEOUT,
        )
        .unwrap();
        let path = directory.path().join("next.mp3");
        assert!(matches!(
            acquire_in(directory.path(), Some(&path)),
            Startup::Secondary
        ));
        assert_eq!(
            primary.receiver.recv_timeout(HANDOFF_TIMEOUT).unwrap(),
            path
        );
    }
}
