//! # Remote Logger
//!
//! This is a logger for the [`log`] crate that
//! works by sending log messages to one or multiple
//! addresses using a TCP connection.
//!
//! The original use-case this library is made for
//! are TUI applications, which can't use stdout to log.
//!
//! See [`remote_logger_protocol`] for info about the protocol used.
//!
//! ## Crate features
//! - **chrono** (*default*): enables the [`chrono`] dependency, which is used to prepend the current datetime (to millisecond precision)
//! to each log message
//! - **thiserror** (*default*): enables the [`thiserror`] dependency, which makes the error types implement [`Error`](std::error::Error)
//! - **len-u128**: sends lengths as `u128` instead of `u64`
//!
//! ## remote_monitor
//! The messages from this crate have to be received by something.
//!
//! The `remote_monitor` crate contains a listener that implements the same protocol
//! used by this crate, so you probably want to use the binary from that crate
//! in a separate window to receive you log messages.
//!
//! [`chrono`]: https://docs.rs/chrono/latest
//! [`thiserror`]: https://docs.rs/thiserror/latest

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::sync::Mutex;

#[cfg(feature = "thiserror")]
use dep_thiserror as thiserror;
use log::{LevelFilter, Log, Metadata, Record};
use remote_logger_protocol::{Header, HeaderMessagePart, MessageError, MessageStream};
#[cfg(feature = "thiserror")]
use thiserror::Error;

// note: some of this code is taken from https://github.com/borntyping/rust-simple_logger

struct RemoteLoggerFinished {
	streams: Mutex<Vec<MessageStream>>,
	default_level: LevelFilter,
	module_levels: Vec<(String, LevelFilter)>,
	fail_strat: LogFailureStrategy,
}

/// How to handle an error occurring inside [`Log::log`]
pub enum LogFailureStrategy {
	/// Ignore the error and move on
	///
	/// This may entail the log message getting lost
	Ignore,
	/// Panic with a descriptive error message
	Panic,
}

/// The main logger type of this crate
pub struct RemoteLogger {
	name: Option<String>,
	output_addrs: Vec<SocketAddr>,
	default_level: LevelFilter,
	module_levels: Vec<(String, LevelFilter)>,
	fail_strat: LogFailureStrategy,
}

/// An error trying to connect to an output address
#[derive(Debug)]
#[cfg_attr(feature = "thiserror", derive(Error))]
pub enum ConnectError {
	/// The socket connection itself failed
	#[cfg_attr(feature = "thiserror", error("failed to connect to address {0}: {1}"))]
	Connect(SocketAddr, std::io::Error),
	/// The initial header could not be sent
	#[cfg_attr(
		feature = "thiserror",
		error("failed to send header to address {0}: {1}")
	)]
	SendHeader(SocketAddr, HeaderMessagePart, std::io::Error),
}

/// An error while trying to initialize the [`RemoteLogger`]
#[derive(Debug)]
#[cfg_attr(feature = "thiserror", derive(Error))]
pub enum InitLoggerError {
	/// The logger has no output address
	#[cfg_attr(feature = "thiserror", error("no address to output to"))]
	NoOutputAddr,
	/// No output address could be connected to
	#[cfg_attr(
		feature = "thiserror",
		error("failed to connect to output address(es)")
	)]
	Connect(Vec<ConnectError>),
	/// The logger could not be set
	#[cfg_attr(feature = "thiserror", error("failed to set the logger: {0}"))]
	SetLogger(log::SetLoggerError),
}

impl Default for RemoteLogger {
	/// Obtain a new logger set up to output to localhost (IPv4) on port `50033`
	fn default() -> Self {
		Self::new().with_output_localhost(50_033)
	}
}

impl RemoteLogger {
	/// Obtain a new empty logger
	///
	/// For a logger already set up to output to localhost on port `50033`, use [`Self::default`]
	pub fn new() -> Self {
		RemoteLogger {
			name: None,
			output_addrs: Vec::new(),
			default_level: LevelFilter::max(),
			module_levels: Vec::new(),
			fail_strat: LogFailureStrategy::Panic,
		}
	}

	/// Set the name the logger will identify itself with
	///
	/// The logger may have a name, which is transmitted over the connection
	/// before the logging begins.
	pub fn with_name(mut self, name: String) -> Self {
		assert!(!name.is_empty(), "setting an empty name is not allowed");
		self.name = Some(name);
		self
	}

	/// Add an address to the list of output addresses
	pub fn with_output_addr(mut self, addr: SocketAddr) -> Self {
		self.output_addrs.push(addr);
		self
	}

	/// Add multiple addresses to the list of output addresses
	pub fn with_output_addrs(mut self, output_addrs: Vec<SocketAddr>) -> Self {
		self.output_addrs.extend(output_addrs);
		self
	}

	/// Add a localhost address (IPv4) on the specified port to the list of output addresses
	pub fn with_output_localhost(self, port: u16) -> Self {
		self.with_output_addr(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
	}

	/// Add a localhost address (IPv6) on the specified port to the list of output addresses
	pub fn with_output_localhost_v6(self, port: u16) -> Self {
		self.with_output_addr(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port))
	}

	/// Set the default log level
	pub fn with_default_level(mut self, level: LevelFilter) -> Self {
		self.default_level = level;
		self
	}

	/// Override the log level for specific modules
	pub fn with_module_level(mut self, target: &str, level: LevelFilter) -> Self {
		self.module_levels.push((target.to_string(), level));
		self
	}

	/// Override the log level for specific modules, providing multiple at once
	pub fn with_module_levels<I: IntoIterator<Item = (String, LevelFilter)>>(
		mut self,
		levels: I,
	) -> Self {
		self.module_levels.extend(levels);
		self
	}

	/// Set the failure strategy for this logger. For more, see the docs of [`LogFailureStrategy`]
	pub fn with_failure_strategy(mut self, strategy: LogFailureStrategy) -> Self {
		self.fail_strat = strategy;
		self
	}

	fn prepare_init(&mut self) -> Result<LevelFilter, InitLoggerError> {
		if self.output_addrs.is_empty() {
			return Err(InitLoggerError::NoOutputAddr);
		}

		// Sort all module levels from most specific to least specific. The length of the module
		// name is used instead of its actual depth to avoid module name parsing.
		self.module_levels
			.sort_by_key(|(name, _level)| name.len().wrapping_neg());

		let max_level = self
			.module_levels
			.iter()
			.map(|(_name, level)| level)
			.copied()
			.max()
			.map(|lvl| lvl.max(self.default_level))
			.unwrap_or(self.default_level);

		Ok(max_level)
	}

	fn header(&self) -> Header {
		Header {
			name: self.name.clone().unwrap_or_default(),
		}
	}

	/// Register the logger with the [`log`] crate.
	/// This must be called in order for [`log`] functions/macros to use this logger
	///
	/// If nothing goes wrong and connecting to at least one output address succeeds,
	/// this function returns `Ok(errs)` with `errs` being the list of failed connections.
	/// For an alternative that always fails any connection fails, see [`init_all`](Self::init_all)
	pub fn init(mut self) -> Result<Vec<ConnectError>, InitLoggerError> {
		let max_level = self.prepare_init()?;

		let header = self.header();

		let mut errs = Vec::new();
		let mut errs2 = Vec::new();
		let streams = self
			.output_addrs
			.into_iter()
			.filter_map(|addr| {
				TcpStream::connect(addr)
					.map(|s| (addr, s))
					.map_err(|e| errs.push(ConnectError::Connect(addr, e)))
					.ok()
			})
			.filter_map(|(addr, stream)| {
				MessageStream::init_send(stream, header.clone())
					.map_err(|e| errs2.push(ConnectError::SendHeader(addr, e.0, e.1)))
					.ok()
			})
			.collect::<Vec<_>>();

		errs.append(&mut errs2);

		if streams.is_empty() {
			return Err(InitLoggerError::Connect(errs));
		}

		let logger = RemoteLoggerFinished {
			streams: Mutex::new(streams),
			default_level: self.default_level,
			module_levels: self.module_levels,
			fail_strat: self.fail_strat,
		};

		log::set_max_level(max_level);
		log::set_boxed_logger(Box::new(logger)).map_err(InitLoggerError::SetLogger)?;

		Ok(errs)
	}

	/// Like [`init`](Self::init), but fails if any of the connections fails
	pub fn init_all(self) -> Result<(), InitLoggerError> {
		self.init()
			.and_then(|v| v.is_empty().then(|| ()).ok_or(InitLoggerError::Connect(v)))
	}
}

#[cfg(feature = "chrono")]
fn format_message(record: &Record) -> String {
	format!(
		"{} {:<5} [{}] {}",
		chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
		record.level().to_string(),
		record.target(),
		record.args()
	)
}

#[cfg(not(feature = "chrono"))]
fn format_message(record: &Record) -> String {
	format!(
		"{:<5} [{}] {}",
		record.level().to_string(),
		record.target(),
		record.args()
	)
}

impl Log for RemoteLoggerFinished {
	fn enabled(&self, metadata: &Metadata) -> bool {
		&metadata.level()
			<= self
				.module_levels
				.iter()
				// At this point the Vec is already sorted so that we can simply take
				// the first match
				.find(|(name, _level)| metadata.target().starts_with(name))
				.map(|(_name, level)| level)
				.unwrap_or(&self.default_level)
	}

	fn log(&self, record: &Record) {
		let msg = format_message(record).into_bytes();
		let msg = &msg;

		let mut streams = match self.streams.lock() {
			Ok(x) => x,
			Err(e) => match self.fail_strat {
				LogFailureStrategy::Ignore => return,
				LogFailureStrategy::Panic => panic!("failed to obtain lock for streams: {}", e),
			},
		};

		for stream in streams.iter_mut() {
			if let Err(MessageError(part, e)) = stream.write_message(msg) {
				match self.fail_strat {
					LogFailureStrategy::Ignore => (),
					LogFailureStrategy::Panic => match stream.inner().peer_addr() {
						Ok(addr) => {
							panic!("failed to write message {} to {}: {}", part, addr, e);
						}
						Err(_) => {
							panic!("failed to write message {} to a stream: {}", part, e);
						}
					},
				}
			}
		}
	}

	fn flush(&self) {
		let mut streams = match self.streams.lock() {
			Ok(x) => x,
			Err(e) => match self.fail_strat {
				LogFailureStrategy::Ignore => return,
				LogFailureStrategy::Panic => panic!("failed to obtain lock for streams: {}", e),
			},
		};

		for stream in streams.iter_mut() {
			if let Err(e) = stream.inner_mut().flush() {
				match self.fail_strat {
					LogFailureStrategy::Ignore => continue,
					LogFailureStrategy::Panic => panic!("failed to flush a stream: {}", e),
				}
			}
		}
	}
}
