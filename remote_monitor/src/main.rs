use std::collections::HashSet;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

struct RemoteLoggerOutput {
	streams: Arc<Mutex<Vec<(TcpStream, SocketAddr)>>>,
	_jh: JoinHandle<()>,
	size_buf: [u8; 8],
	buf: Vec<u8>,
}

impl RemoteLoggerOutput {
	pub fn new<T: ToSocketAddrs>(addr: T) -> std::io::Result<Self> {
		let listener = TcpListener::bind(addr)?;
		let streams = Arc::new(Mutex::new(Vec::new()));
		let streams_2 = Arc::clone(&streams);
		let _jh = std::thread::spawn(move || loop {
			if let Ok(tup) = listener.accept() {
				println!("new connection from {}", tup.1);
				streams_2.lock().unwrap().push(tup);
			}
		});
		Ok(Self {
			streams,
			_jh,
			size_buf: [0; 8],
			buf: Vec::new(),
		})
	}

	pub fn localhost(port: u16) -> std::io::Result<Self> {
		Self::new(format!("localhost:{}", port))
	}

	pub fn tick(&mut self) {
		let mut streams = self.streams.lock().unwrap();
		let one = streams.len() == 1;

		let mut to_remove = HashSet::new();

		for (stream, addr) in streams.iter_mut() {
			if stream.peek(&mut [0]).unwrap() == 0 {
				println!("connection from {} was closed", addr);
				to_remove.insert(*addr);
				continue;
			}

			stream.read_exact(&mut self.size_buf).unwrap();

			self.buf = vec![0; usize::from_ne_bytes(self.size_buf)];
			stream.read_exact(&mut self.buf).unwrap();

			let s = String::from_utf8_lossy(&self.buf);

			let prefix = if one {
				String::new()
			} else if addr.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST) {
				format!(":{}", addr.port())
			} else {
				format!("{}", addr)
			};

			println!("{}> {}", prefix, s);
		}

		streams.retain(|(_, addr)| !to_remove.contains(addr));
	}
}

// todo: add the proper checks, config, and docs
fn main() {
	let mut output = RemoteLoggerOutput::localhost(50_033).unwrap();

	loop {
		output.tick();
	}
}
