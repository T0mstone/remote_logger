use std::collections::VecDeque;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{Receiver, SendError, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

#[cfg(feature = "thiserror")]
use dep_thiserror as thiserror;
use remote_logger_protocol::{
	HeaderMessagePart, MessageError, MessagePart, MessageStream, TryReceiveMessageError,
};
use slotmap::{new_key_type, DenseSlotMap};
#[cfg(feature = "thiserror")]
use thiserror::Error;

// todo: docs

new_key_type! {
	pub struct ConnectionId;
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ConnectionMetadata {
	pub addr: SocketAddr,
	pub name: Option<String>,
	pub name_unique: bool,
}

impl ConnectionMetadata {
	pub fn unique_string(&self) -> String {
		let is_localhost = self.addr.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST);

		match self.name.clone() {
			Some(name) if self.name_unique => format!("~{}", name),
			Some(name) if is_localhost => format!(":{}~{}", self.addr.port(), name),
			Some(name) => format!("{}~{}", self.addr, name),
			None if is_localhost => format!(":{}", self.addr.port()),
			None => self.addr.to_string(),
		}
	}
}

pub struct RemoteMonitor {
	streams: DenseSlotMap<ConnectionId, (MessageStream, ConnectionMetadata)>,
	closed_streams: Vec<ConnectionMetadata>,
	message_queue: VecDeque<(ConnectionId, String)>,
	conn_rec: Receiver<(TcpStream, SocketAddr)>,
	_jh: JoinHandle<()>,
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
		error("failed to receive header for receiver from addr {0}: {1}")
	)]
	ReceiveHeader(SocketAddr, MessageError<HeaderMessagePart>),
	#[cfg_attr(
		feature = "thiserror",
		error("The connection accepting thread exited early")
	)]
	Disconnected,
}

#[derive(Debug)]
#[cfg_attr(feature = "thiserror", derive(Error))]
pub enum ReadFromStreamError {
	#[cfg_attr(feature = "thiserror", error("failed to peek: {1}"))]
	Peek(ConnectionId, io::Error),
	#[cfg_attr(feature = "thiserror", error("{1}"))]
	Receive(ConnectionId, MessageError<MessagePart>),
}

impl RemoteMonitor {
	pub fn new<T: ToSocketAddrs>(addr: T) -> io::Result<Self> {
		let listener = TcpListener::bind(addr)?;

		let (sen, conn_rec) = std::sync::mpsc::channel();
		let _jh = std::thread::spawn(move || loop {
			if let Ok(tup) = listener.accept() {
				let res = sen.send(tup);

				if let Err(SendError(_)) = res {
					// receiver closed, so we can stop this thread
					break;
				}
			}
		});

		Ok(Self {
			streams: DenseSlotMap::with_key(),
			closed_streams: Vec::new(),
			message_queue: VecDeque::new(),
			conn_rec,
			_jh,
		})
	}

	pub fn localhost(port: u16) -> io::Result<Self> {
		Self::new(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
	}

	// todo: docs (mention the maybe unintuitive error behaviour)
	pub fn receive_connections(&mut self) -> Result<Vec<ConnectionMetadata>, UpdateStreamsError> {
		let mut res = vec![];
		loop {
			match self.conn_rec.try_recv() {
				Ok((stream, addr)) => {
					stream
						.set_read_timeout(Some(Duration::from_millis(2)))
						.map_err(|e| UpdateStreamsError::SetTimeout(addr, e))?;

					let (stream, header) = MessageStream::init_receive(stream)
						.map_err(|e| UpdateStreamsError::ReceiveHeader(addr, e))?;

					let metadata = {
						let name = (!header.name.is_empty()).then(|| header.name);
						let name_unique =
							name.is_some() && self.streams.values().any(|t| t.1.name == name);

						ConnectionMetadata {
							addr,
							name,
							name_unique,
						}
					};

					res.push(metadata.clone());

					self.streams.insert((stream, metadata));
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

	pub fn closed_connections(&mut self) -> Vec<ConnectionMetadata> {
		std::mem::take(&mut self.closed_streams)
	}

	fn message_tick(&mut self) -> Result<(), ReadFromStreamError> {
		let mut closed_stream_idxs = Vec::new();
		let mut closed_streams = Vec::new();

		for (id, (stream, _)) in self.streams.iter_mut() {
			match stream.try_receive_message::<Vec<u8>>() {
				Ok(None) => (),
				Ok(Some(vec)) => {
					let s = String::from_utf8_lossy(&vec);
					self.message_queue.push_back((id, s.to_string()));
				}
				Err(TryReceiveMessageError::Closed) => {
					closed_stream_idxs.push(id);
					continue;
				}
				Err(TryReceiveMessageError::Peek(e)) => {
					return Err(ReadFromStreamError::Peek(id, e))
				}
				Err(TryReceiveMessageError::Receive(err)) => {
					return Err(ReadFromStreamError::Receive(id, err));
				}
			}
		}

		// sort from high to low to avoid index shifts when removing
		closed_stream_idxs.sort_by(|l, r| l.cmp(r).reverse());

		for id in closed_stream_idxs.clone() {
			// since the id was obtained just above, we can be sure that the corresponding value is still in the map,
			// so unwrapping here is ok
			let (_, meta) = self.streams.remove(id).unwrap();
			closed_streams.push(meta);
		}

		self.closed_streams.extend(closed_streams);

		Ok(())
	}

	pub fn connection_info(&self, id: ConnectionId) -> &ConnectionMetadata {
		&self.streams[id].1
	}

	pub fn next_message(&mut self) -> Result<Option<(ConnectionId, String)>, ReadFromStreamError> {
		if self.message_queue.is_empty() {
			self.message_tick()?;
		}
		Ok(self.message_queue.pop_front())
	}

	pub fn num_connections(&self) -> usize {
		self.streams.len()
	}
}
