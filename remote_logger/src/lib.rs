use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::sync::Mutex;

use log::{LevelFilter, Log, Metadata, Record};

// note: some of this code is taken from https://github.com/borntyping/rust-simple_logger

// todo: add docs

struct RemoteLoggerFinished {
	streams: Mutex<Vec<TcpStream>>,
	default_level: LevelFilter,
	module_levels: Vec<(String, LevelFilter)>,
}

pub struct RemoteLogger {
	output_addrs: Vec<SocketAddr>,
	default_level: LevelFilter,
	module_levels: Vec<(String, LevelFilter)>,
}

#[derive(Debug)]
pub enum InitLoggerError {
	NoOutputAddr,
	Connect(Vec<(SocketAddr, std::io::Error)>),
	SetLogger(log::SetLoggerError),
}

impl Default for RemoteLogger {
	fn default() -> Self {
		Self::new().with_output_localhost(50_033)
	}
}

impl RemoteLogger {
	pub fn new() -> Self {
		RemoteLogger {
			output_addrs: Vec::new(),
			default_level: LevelFilter::max(),
			module_levels: Vec::new(),
		}
	}

	pub fn with_output_addr(mut self, addr: SocketAddr) -> Self {
		self.output_addrs.push(addr);
		self
	}

	pub fn with_output_addrs(mut self, output_addrs: Vec<SocketAddr>) -> Self {
		self.output_addrs = output_addrs;
		self
	}

	pub fn with_output_localhost(self, port: u16) -> Self {
		self.with_output_addr(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
	}

	pub fn with_output_localhost_v6(self, port: u16) -> Self {
		self.with_output_addr(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port))
	}

	pub fn with_default_level(mut self, level: LevelFilter) -> Self {
		self.default_level = level;
		self
	}

	pub fn with_module_level(mut self, target: &str, level: LevelFilter) -> Self {
		self.module_levels.push((target.to_string(), level));
		self
	}

	pub fn with_module_levels<I: IntoIterator<Item = (String, LevelFilter)>>(
		mut self,
		levels: I,
	) -> Self {
		self.module_levels.extend(levels);
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

	pub fn init(mut self) -> Result<(), InitLoggerError> {
		let max_level = self.prepare_init()?;

		let mut errs = Vec::new();
		let streams = self
			.output_addrs
			.into_iter()
			.filter_map(|addr| {
				TcpStream::connect(addr)
					.map_err(|e| errs.push((addr, e)))
					.ok()
			})
			.collect::<Vec<_>>();

		if streams.is_empty() {
			return Err(InitLoggerError::Connect(errs));
		}

		let logger = RemoteLoggerFinished {
			streams: Mutex::new(streams),
			default_level: self.default_level,
			module_levels: self.module_levels,
		};

		log::set_max_level(max_level);
		log::set_boxed_logger(Box::new(logger)).map_err(InitLoggerError::SetLogger)?;

		Ok(())
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

	// todo: is it ok to unwrap here? -> maybe add a config option
	fn log(&self, record: &Record) {
		let msg = format_message(record);
		let msg = msg.as_bytes();
		let len_bytes = msg.len().to_ne_bytes();

		let mut streams = self.streams.lock().unwrap();

		for stream in streams.iter_mut() {
			stream.write_all(&len_bytes).unwrap();
			stream.write_all(msg).unwrap();
		}
	}

	fn flush(&self) {}
}
