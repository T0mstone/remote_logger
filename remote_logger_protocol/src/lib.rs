//! # The protocol for `remote_logger`
//!
//! This crate specifies a thin communication layer
//! built on top of a [`TcpStream`], which is
//! used by the `remote_logger` and `remote_monitor` crates
//!
//! ## The protocol
//!
//! The basis of the protocol is that a binary message
//! is sent by first sending its length(1), then the message bytes.
//!
//! (1): The length is either treated as `u64` or `u128`, depending on whether the `len-u128` crate feature is enabled
//!
//! The first message sent by a logger has to be a [`Header`], all subsequent messages are counted as
//! log messages.
//!
//! ## Where to start
//! The main type to use is [`MessageStream`].
//!
//! ## Crate features
//! - **thiserror** (*default*): enables the [`thiserror`] dependency, which makes the error types implement [`Error`](std::error::Error)
//! - **len-u128**: sends lengths as `u128` instead of `u64`
//!
//! [`thiserror`]: https://docs.rs/thiserror/latest

use std::fmt::{Debug, Display};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;
use std::{fmt, io};

#[cfg(feature = "thiserror")]
use thiserror::Error;

use crate::private::Sealed;

#[cfg(not(feature = "len-u128"))]
type LengthType = u64;
#[cfg(feature = "len-u128")]
type LengthType = u128;

const LEN_LEN: usize = std::mem::size_of::<LengthType>();

/// A helper type to send messages more easily and without having to import the [`MessageType`] trait
#[derive(Debug)]
pub struct MessageStream {
	inner: TcpStream,
}

impl MessageStream {
	/// Create a `MessageStream` from a [`TcpStream`] and send the [`Header`]
	pub fn init_send(
		mut inner: TcpStream,
		header: Header,
	) -> Result<Self, MessageError<HeaderMessagePart>> {
		header.write_to(&mut inner)?;
		Ok(Self { inner })
	}

	/// Create a `MessageStream` from a [`TcpStream`] and receive a [`Header`]
	pub fn init_receive(
		inner: TcpStream,
	) -> Result<(Self, Header), MessageError<HeaderMessagePart>> {
		let mut res = Self { inner };
		let hdr = res.receive_message::<Header>()?;
		Ok((res, hdr))
	}

	/// Write a message to the stream
	pub fn write_message<M: MessageType>(
		&mut self,
		msg: &M,
	) -> Result<(), MessageError<M::MessagePart>> {
		msg.write_to(&mut self.inner)
	}

	/// Set the timeout for reading a message. Forwards to [`TcpStream::set_read_timeout`]
	pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
		self.inner.set_read_timeout(dur)
	}

	/// Get immutable access to the underlying [`TcpStream`]
	pub fn inner(&self) -> &TcpStream {
		&self.inner
	}

	/// Get mutable access to the underlying [`TcpStream`]
	pub fn inner_mut(&mut self) -> &mut TcpStream {
		&mut self.inner
	}

	/// Receive a message (blocking until the message is fully read)
	///
	/// This returns an error if a timeout is encountered. For more info, see [`TcpStream::set_read_timeout`]
	pub fn receive_message<M: MessageType>(&mut self) -> Result<M, MessageError<M::MessagePart>> {
		M::read_from(&mut self.inner)
	}

	/// Try to see if the stream has data available (by peeking for a single byte and detecting a timeout)
	/// and if so, receive an entire message (blocking like [`receive_message`]). Only on timeout, `Ok(None)` is returned
	///
	/// Errors counted as timeout are [`TimedOut`] and [`WouldBlock`].
	///
	/// [`receive_message`]: Self::receive_message
	/// [`TimedOut`]: std::io::ErrorKind::TimedOut
	/// [`WouldBlock`]: std::io::ErrorKind::WouldBlock
	pub fn try_receive_message<M: MessageType>(
		&mut self,
	) -> Result<Option<M>, TryReceiveMessageError<M::MessagePart>> {
		match self.inner.peek(&mut [0]) {
			Ok(0) => {
				// peeking returning 0 bytes means the stream is DONE
				return Err(TryReceiveMessageError::Closed);
			}
			Ok(_) => (),
			Err(e) => {
				return match e.kind() {
					// these indicate that no data is currently available
					// (the error is stated in https://doc.rust-lang.org/std/net/struct.TcpStream.html#method.set_read_timeout
					// as being platform-specific so it may be possible that a platform will return a different error kind
					// on timeout. In that case, this code will have to be updated to include the different error kind)
					io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Ok(None),
					_ => Err(TryReceiveMessageError::Peek(e)),
				};
			}
		}

		self.receive_message::<M>()
			.map(Some)
			.map_err(TryReceiveMessageError::Receive)
	}
}

/// An error while reading or writing part of an implementation of [`MessageType`]
///
/// The first field is supposed to be an enum showing which stage of the read/write process caused the error
#[derive(Debug)]
#[cfg_attr(feature = "thiserror", derive(Error))]
#[cfg_attr(
	feature = "thiserror",
	error("error processing message part <{0}>: {1}")
)]
pub struct MessageError<Part: Debug + Display>(pub Part, pub io::Error);

/// An error returned by [`MessageStream::try_receive_message`]
#[derive(Debug)]
#[cfg_attr(feature = "thiserror", derive(Error))]
pub enum TryReceiveMessageError<Part: Debug + Display> {
	/// Failed to peek for new data
	#[cfg_attr(feature = "thiserror", error("peeking the stream failed: {0}"))]
	Peek(io::Error),
	/// The stream was closed
	#[cfg_attr(
		feature = "thiserror",
		error("peeking the stream returned that it was closed")
	)]
	Closed,
	/// Failed to receive message
	#[cfg_attr(feature = "thiserror", error("{0}"))]
	Receive(MessageError<Part>),
}

/// The trait for a message that adheres to this protocol
pub trait MessageType: Sized + Sealed {
	/// This is supposed to be an enum showing which stage of the read/write process caused the error
	type MessagePart: Debug + Display;

	/// Write a message
	fn write_to<W: Write>(&self, writer: &mut W) -> Result<(), MessageError<Self::MessagePart>>;
	/// Read a message
	fn read_from<R: Read>(reader: &mut R) -> Result<Self, MessageError<Self::MessagePart>>;
}

mod private {
	pub trait Sealed {}
}

/// The [`MessagePart`] for `Vec<u8>`
///
/// [`MessagePart`]: MessageType::MessagePart
#[derive(Debug)]
pub enum MessagePart {
	/// The first part of the message is the length of the body
	///
	/// This is either 8 bytes or 16 bytes long, depending on the `len-u128` crate feature
	Length,
	/// The second part of the message is the body (the bytes to send)
	Body,
}

impl Display for MessagePart {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match self {
			Self::Length => f.pad("length"),
			Self::Body => f.pad("body"),
		}
	}
}

impl Sealed for Vec<u8> {}

/// A binary message is sent by sending the length and then the message.
/// See the [crate docs](crate#the-protocol) for more details
impl MessageType for Vec<u8> {
	type MessagePart = MessagePart;

	fn write_to<W: Write>(&self, writer: &mut W) -> Result<(), MessageError<Self::MessagePart>> {
		let len = self.len() as LengthType;
		let size_buf = len.to_be_bytes();
		writer
			.write_all(&size_buf)
			.map_err(|e| MessageError(MessagePart::Length, e))?;
		writer
			.write_all(self)
			.map_err(|e| MessageError(MessagePart::Body, e))
	}

	fn read_from<R: Read>(reader: &mut R) -> Result<Self, MessageError<Self::MessagePart>> {
		let mut size_buf = [0; LEN_LEN];
		reader
			.read_exact(&mut size_buf)
			.map_err(|e| MessageError(MessagePart::Length, e))?;
		let len = LengthType::from_be_bytes(size_buf) as usize;
		let mut buf = vec![0; len];
		reader
			.read_exact(&mut buf)
			.map_err(|e| MessageError(MessagePart::Body, e))?;
		Ok(buf)
	}
}

// message format: [name len][name body]
/// The header is the first thing sent by a logger
///
/// It contains information regarding the logger itself
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Header {
	/// The name of a logger
	///
	/// The empty string represents a logger without a name
	pub name: String,
}

/// The [`MessagePart`] for [`Header`]
///
/// [`MessagePart`]: MessageType::MessagePart
#[derive(Debug)]
pub enum HeaderMessagePart {
	/// The first (currently only) part of a header
	/// is the name of the logger
	Name(MessagePart),
}

impl Display for HeaderMessagePart {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match self {
			Self::Name(p) => f.pad(&format!("name {}", p)),
		}
	}
}

impl Sealed for Header {}

/// The header is sent by sending all its fields in sequence
///
/// - [`name`](Header::name) is sent as UTF-8 bytes (as [`Vec<u8> as MessageType`](MessageType#impl-MessageType-for-Vec<u8>))
impl MessageType for Header {
	type MessagePart = HeaderMessagePart;

	fn write_to<W: Write>(&self, writer: &mut W) -> Result<(), MessageError<Self::MessagePart>> {
		let name = self.name.clone();

		name.into_bytes()
			.write_to(writer)
			.map_err(|e| MessageError(HeaderMessagePart::Name(e.0), e.1))?;

		Ok(())
	}

	fn read_from<R: Read>(reader: &mut R) -> Result<Self, MessageError<Self::MessagePart>> {
		let name_msg = <Vec<u8>>::read_from(reader)
			.map_err(|e| MessageError(HeaderMessagePart::Name(e.0), e.1))?;

		let name = String::from_utf8_lossy(&name_msg).to_string();

		Ok(Self { name })
	}
}
