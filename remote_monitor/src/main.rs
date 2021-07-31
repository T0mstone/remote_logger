use remote_monitor::RemoteMonitor;

// todo: error handling, config, and docs
fn main() {
	let mut monitor = RemoteMonitor::localhost(50_033).unwrap();

	loop {
		let opened = monitor.receive_connections().unwrap();
		for meta in opened {
			println!("new connection from {}", meta.unique_string());
		}

		let closed = monitor.closed_connections();
		for meta in closed {
			println!("connection from {} was closed", meta.unique_string());
		}

		let one = monitor.num_connections() == 1;

		while let Some((id, s)) = monitor.next_message().unwrap() {
			let prefix = if one {
				String::new()
			} else {
				monitor.connection_info(id).unique_string()
			};

			println!("{}> {}", prefix, s);
		}
	}
}
