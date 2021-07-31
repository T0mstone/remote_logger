//! # A monitor for `remote_logger`
//!
//! This crate provides [`RemoteMonitor`], a type that listens for arbitrarily many
//! connections from loggers, and provides functionality to poll the log messages.

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

new_key_type! {
	/// A unique ID representing a connection
	pub struct ConnectionId;
}

/// Some data about a connection
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ConnectionMetadata {
	/// The address connected to
	pub addr: SocketAddr,
	/// The name the connection gave itself
	pub name: Option<String>,
	/// Whether [`name`](Self::name) is unique in the connection list
	pub name_unique: bool,
}

impl ConnectionMetadata {
	/// A string that is guaranteed to not be ambiguous given the information in `self`
	///
	/// The format is as follows:
	/// - if the name is unique, `~name` is returned
	/// - if the name is not unique, `addr~name` is returned*
	/// - if there is no name, `addr` is returned*
	///
	/// \* `addr` will be the whole address unless the IP is `127.0.0.1` (i.e. localhost),
	/// in which case it will just be `:port`
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

/// The main type for receiving log messages
pub struct RemoteMonitor {
	streams: DenseSlotMap<ConnectionId, (MessageStream, ConnectionMetadata)>,
	closed_streams: Vec<ConnectionMetadata>,
	message_queue: VecDeque<(ConnectionId, String)>,
	conn_rec: Receiver<(TcpStream, SocketAddr)>,
	_jh: JoinHandle<()>,
}

/// An error while updating connections
#[derive(Debug)]
#[cfg_attr(feature = "thiserror", derive(Error))]
pub enum UpdateConnectionsError {
	/// Failed to set timeout for reading
	#[cfg_attr(
		feature = "thiserror",
		error("failed to set timeout for receiver from addr {0}: {1}")
	)]
	SetTimeout(SocketAddr, io::Error),
	/// Failed to receive the initial message, i.e. the header
	#[cfg_attr(
		feature = "thiserror",
		error("failed to receive header for receiver from addr {0}: {1}")
	)]
	ReceiveHeader(SocketAddr, MessageError<HeaderMessagePart>),
	/// The thread accepting connections exited early
	#[cfg_attr(
		feature = "thiserror",
		error("The connection accepting thread exited early")
	)]
	Disconnected,
}

/// An error reading from a connection
#[derive(Debug)]
#[cfg_attr(feature = "thiserror", derive(Error))]
pub enum ReadFromConnectionError {
	/// Failed to peek a byte
	#[cfg_attr(feature = "thiserror", error("failed to peek: {1}"))]
	Peek(ConnectionId, io::Error),
	/// Failed to receive a message
	#[cfg_attr(feature = "thiserror", error("{1}"))]
	Receive(ConnectionId, MessageError<MessagePart>),
}

impl RemoteMonitor {
	/// Create a new `RemoteMonitor` listening on `addr`
	///
	/// Spawns a thread that listens for new connections and accepts them
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

	/// Create a new `RemoteMonitor` listening on localhost (IPv4) at `port`
	pub fn localhost(port: u16) -> io::Result<Self> {
		Self::new(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
	}

	/// Receive all waiting connections from the accepting thread
	///
	/// If the receiver gets disconnected, but some connections were already received, `Ok` is returned.
	/// Only on subsequent calls (when no new connections are received), `Err(Disconnected)` is returned.
	pub fn receive_connections(&mut self) -> Result<Vec<ConnectionId>, UpdateConnectionsError> {
		let mut res = vec![];
		loop {
			match self.conn_rec.try_recv() {
				Ok((stream, addr)) => {
					stream
						.set_read_timeout(Some(Duration::from_millis(2)))
						.map_err(|e| UpdateConnectionsError::SetTimeout(addr, e))?;

					let (stream, header) = MessageStream::init_receive(stream)
						.map_err(|e| UpdateConnectionsError::ReceiveHeader(addr, e))?;

					let metadata = {
						let name = (!header.name.is_empty()).then(|| header.name);
						let name_unique =
							name.is_some() && self.streams.values().all(|t| t.1.name != name);

						ConnectionMetadata {
							addr,
							name,
							name_unique,
						}
					};

					let id = self.streams.insert((stream, metadata));
					res.push(id);
				}
				Err(TryRecvError::Empty) => break,
				Err(TryRecvError::Disconnected) => {
					if res.is_empty() {
						return Err(UpdateConnectionsError::Disconnected);
					} else {
						break;
					}
				}
			}
		}
		Ok(res)
	}

	/// Returns a list of all connections that were closed after the last call to this function
	pub fn closed_connections(&mut self) -> Vec<ConnectionMetadata> {
		std::mem::take(&mut self.closed_streams)
	}

	fn message_tick(&mut self) -> Result<(), ReadFromConnectionError> {
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
					return Err(ReadFromConnectionError::Peek(id, e))
				}
				Err(TryReceiveMessageError::Receive(err)) => {
					return Err(ReadFromConnectionError::Receive(id, err));
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

	/// Returns information about the specified connection
	pub fn connection_info(&self, id: ConnectionId) -> &ConnectionMetadata {
		&self.streams[id].1
	}

	/// Returns the next message, or `Ok(None)` if there is no new message
	pub fn next_message(
		&mut self,
	) -> Result<Option<(ConnectionId, String)>, ReadFromConnectionError> {
		if self.message_queue.is_empty() {
			self.message_tick()?;
		}
		Ok(self.message_queue.pop_front())
	}

	/// Returns the number of current connections
	pub fn num_connections(&self) -> usize {
		self.streams.len()
	}
}
