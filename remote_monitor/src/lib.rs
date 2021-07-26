use std::collections::{HashSet, VecDeque};
use std::io;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{Receiver, SendError, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

#[cfg(feature = "thiserror")]
use thiserror::Error;

#[cfg(not(feature = "len-u128"))]
type LengthType = u64;
#[cfg(feature = "len-u128")]
type LengthType = u128;

const LEN_LEN: usize = std::mem::size_of::<LengthType>();

// todo: docs

pub struct RemoteMonitor {
	streams: Vec<(TcpStream, SocketAddr)>,
	closed_streams: Vec<SocketAddr>,
	message_queue: VecDeque<(SocketAddr, String)>,
	conn_rec: Receiver<(TcpStream, SocketAddr)>,
	_jh: JoinHandle<()>,
	size_buf: [u8; LEN_LEN],
	buf: Vec<u8>,
}

#[derive(Debug)]
#[cfg_attr(feature = "thiserror", derive(Error))]
pub enum UpdateStreamsError {
	#[cfg_attr(
		feature = "thiserror",
		error("failed to set timeout for receiver from addr {0}: {1}")
	)]
	SetTimeout(SocketAddr, io::Error),
	#[cfg_attr(
		feature = "thiserror",
		error("The connection accepting thread exited early")
	)]
	Disconnected,
}

#[derive(Debug)]
#[cfg_attr(feature = "thiserror", derive(Error))]
pub enum ReadFromStreamError {
	#[cfg_attr(feature = "thiserror", error("failed to peek at addr {0}: {1}"))]
	Peek(SocketAddr, io::Error),
	#[cfg_attr(
		feature = "thiserror",
		error("failed to read message length from addr {0}: {1}")
	)]
	ReadLength(SocketAddr, io::Error),
	#[cfg_attr(
		feature = "thiserror",
		error("failed to read message from addr {0}: {1}")
	)]
	ReadMessage(SocketAddr, io::Error),
}

impl RemoteMonitor {
	pub fn new<T: ToSocketAddrs>(addr: T) -> io::Result<Self> {
		let listener = TcpListener::bind(addr)?;

		let (sen, conn_rec) = std::sync::mpsc::channel();
		let _jh = std::thread::spawn(move || loop {
			if let Ok(tup) = listener.accept() {
				let res = sen.send(tup);

				if let Err(SendError { .. }) = res {
					// receiver closed, so we can stop this thread
					break;
				}
			}
		});

		Ok(Self {
			streams: Vec::new(),
			closed_streams: Vec::new(),
			message_queue: VecDeque::new(),
			conn_rec,
			_jh,
			size_buf: [0; LEN_LEN],
			buf: Vec::new(),
		})
	}

	pub fn localhost(port: u16) -> io::Result<Self> {
		Self::new(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
	}

	// todo: docs (mention the maybe unintuitive error behaviour)
	pub fn receive_connections(&mut self) -> Result<Vec<SocketAddr>, UpdateStreamsError> {
		let mut res = vec![];
		loop {
			match self.conn_rec.try_recv() {
				Ok(tup) => {
					tup.0
						.set_read_timeout(Some(Duration::from_millis(2)))
						.map_err(|e| UpdateStreamsError::SetTimeout(tup.1, e))?;
					res.push(tup.1);
					self.streams.push(tup);
				}
				Err(TryRecvError::Empty) => break,
				Err(TryRecvError::Disconnected) => {
					if res.is_empty() {
						return Err(UpdateStreamsError::Disconnected);
					} else {
						break;
					}
				}
			}
		}
		Ok(res)
	}

	pub fn closed_connections(&mut self) -> Vec<SocketAddr> {
		std::mem::take(&mut self.closed_streams)
	}

	fn message_tick(&mut self) -> Result<(), ReadFromStreamError> {
		let mut closed_streams = HashSet::new();

		for (stream, addr) in self.streams.iter_mut() {
			let n_peek = match stream.peek(&mut [0]) {
				Ok(x) => x,
				Err(e) => match e.kind() {
					// these indicate that no data is currently available
					// (the error is stated in https://doc.rust-lang.org/std/net/struct.TcpStream.html#method.set_read_timeout
					// as being platform-specific so it may be possible that a platform will return a different error kind
					// on timeout. In that case, this code will have to be updated to include the different error kind)
					io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => continue,
					_ => return Err(ReadFromStreamError::Peek(*addr, e)),
				},
			};
			if n_peek == 0 {
				closed_streams.insert(*addr);
				continue;
			}

			stream
				.read_exact(&mut self.size_buf)
				.map_err(|e| ReadFromStreamError::ReadLength(*addr, e))?;
			let length = LengthType::from_be_bytes(self.size_buf) as usize;

			self.buf = vec![0; length];
			stream
				.read_exact(&mut self.buf)
				.map_err(|e| ReadFromStreamError::ReadMessage(*addr, e))?;

			let s = String::from_utf8_lossy(&self.buf);

			self.message_queue.push_back((*addr, s.to_string()));
		}

		self.streams
			.retain(|(_, addr)| !closed_streams.contains(addr));
		self.closed_streams.extend(closed_streams);

		Ok(())
	}

	pub fn next_message(&mut self) -> Result<Option<(SocketAddr, String)>, ReadFromStreamError> {
		if self.message_queue.is_empty() {
			self.message_tick()?;
		}
		Ok(self.message_queue.pop_front())
	}

	pub fn num_connections(&self) -> usize {
		self.streams.len()
	}
}
