use std::net::{IpAddr, Ipv4Addr};

use remote_monitor::RemoteMonitor;

// todo: error handling, config, and docs
fn main() {
	let mut monitor = RemoteMonitor::localhost(50_033).unwrap();

	loop {
		let opened = monitor.receive_connections().unwrap();
		for addr in opened {
			println!("new connection from {}", addr);
		}

		let closed = monitor.closed_connections();
		for addr in closed {
			println!("connection from {} was closed", addr);
		}

		let one = monitor.num_connections() == 1;

		while let Some((addr, s)) = monitor.next_message().unwrap() {
			let prefix = if one {
				String::new()
			} else if addr.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST) {
				format!(":{}", addr.port())
			} else {
				format!("{}", addr)
			};

			println!("{}> {}", prefix, s);
		}
	}
}
