use mio::{
	Poll,
	Token,
	Events,
	Interest,
	event::Event,
	net::{
		TcpListener,
		TcpStream
	}
};
use std::{
	fs,
	time,
	io::{
		Read,
		Error,
		Write,
		ErrorKind
	},
	net::{
		IpAddr,
		Ipv4Addr,
		SocketAddr
	},
	path::{
		Path,
		PathBuf
	},
	sync::Arc,
	collections::HashMap
};
use clap::{
	Parser,
	ArgAction
};

const CAPACITY: usize = 128;
const REQUEST_SIZE: usize = 1024;
const SERVER_TOKEN: Token = Token(0);

struct Session {
	client: TcpStream,
	response: Option<Vec<u8>>
}

struct Cache {
	content: Arc<Vec<u8>>,
	modified_timestamp: u64
}

#[derive(Parser)]
#[command(disable_help_flag = true, version, disable_version_flag = true)]
struct Args {
	#[arg(short, action = ArgAction::Help, hide = true)]
	help: Option<bool>,

	#[arg(short, action = ArgAction::Version, help = "Show version")]
	version: Option<bool>,

	#[arg(short, default_value_t = 80, help = "Runs on the specified port")]
	port: u16,

	#[arg(short, default_value = "static", help = "Serve the directory files")]
	base_dir: String
}

fn main() {
	let args = Args::parse();

	if let Err(error) = run_server(args.port, args.base_dir) {
		if permission_denied(&error) {
			eprintln!("Permission denied, you need administrator privileges.");

			return;
		}

		eprintln!("Unknown error: {}", error);
	}
}

fn run_server(port: u16, base_dir: String) -> Result<(), Error> {
	let mut poll = Poll::new()?;

	let mut events = Events::with_capacity(CAPACITY);

	let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), port);

	let mut server = TcpListener::bind(address)?;

	poll.registry()
		.register(&mut server, SERVER_TOKEN, Interest::READABLE)?;

	let mut next_client_id: usize = 1;

	let mut sessions: HashMap<usize, Session> = HashMap::new();

	let mut caches: HashMap<PathBuf, Cache> = HashMap::new();

	println!("Running on: 0.0.0.0:{}", port);

	loop {
		if let Err(error) = poll.poll(&mut events, None) {
			if interrupted(&error) {
				continue;
			}

			return Err(error);
		}

		for event in events.iter() {
			match event.token() {
				SERVER_TOKEN => {
					loop {
						if let Err(error) = accept_client(&server, &poll, &mut next_client_id, &mut sessions) {
							if would_block(&error) {
								break;
							}
						}
					}
				},
				Token(client_id) => {
					if let Err(_) = handle_client(&mut sessions, client_id, &event, &poll, base_dir.clone(), &mut caches) {
						close_client(&mut sessions, client_id, &poll);
					}
				}
			}
		}
	}
}

fn interrupted(error: &Error) -> bool {
	error.kind() == ErrorKind::Interrupted
}

fn accept_client(server: &TcpListener, poll: &Poll, next_client_id: &mut usize, sessions: &mut HashMap<usize, Session>) -> Result<(), Error>{
	let (mut client, _address) = server.accept()?;

	poll.registry()
		.register(&mut client, Token(*next_client_id), Interest::READABLE)?;

	sessions.insert(*next_client_id, Session {
		client: client,
		response: None
	});

	*next_client_id += 1;

	Ok(())
}

fn would_block(error: &Error) -> bool {
	error.kind() == ErrorKind::WouldBlock
}

fn handle_client(sessions: &mut HashMap<usize, Session>, client_id: usize, event: &Event, poll: &Poll, base_dir: String, caches: &mut HashMap<PathBuf, Cache>) -> Result<(), Error> {
	if let Some(session) = sessions.get_mut(&client_id) {
		if event.is_readable() {
			let mut data = vec![0; REQUEST_SIZE];

			let mut bytes_readed = 0;

			loop {
				match session.client.read(&mut data[bytes_readed..]) {
					Ok(0) => {
						break;
					},
					Ok(n) => {
						bytes_readed += n;
					},
					Err(error) => {
						if interrupted(&error) {
							continue;
						}

						if would_block(&error) {
							break;
						}

						return Err(error);
					}
				};
			}

			if bytes_readed == 0 {
				return Err(Error::new(ErrorKind::Other, ""));
			}

			data.truncate(bytes_readed);

			poll.registry()
				.reregister(&mut session.client, Token(client_id), Interest::WRITABLE)?;

			let response = handle_request(data, base_dir, caches);

			session.response = Some(response);

			return Ok(());
		}

		if event.is_writable() {
			if let Some(response) = &session.response {
				session.client.write_all(&response)?;

				session.client.flush()?;
			}

			close_client(sessions, client_id, poll);

			return Ok(())
		}
	}

	Ok(())
}

fn handle_request(data: Vec<u8>, base_dir: String, caches: &mut HashMap<PathBuf, Cache>) -> Vec<u8> {
	let path = match parse_request(data) {
		Ok(path) => path,
		Err(_) => {
			return build_response("400 Bad Request", HashMap::from([("Content-Type", "text/html")]), bad_request_html());
		}
	};

	let full_path = match prevent_directory_transversal(base_dir, path.clone()) {
		Ok(full_path) => full_path,
		Err(error) => {
			if permission_denied(&error) {
				return build_response("403 Forbidden", HashMap::from([("Content-Type", "text/html")]), forbidden_html());
			}

			if not_found(&error) {
				return build_response("404 Not Found", HashMap::from([("Content-Type", "text/html")]), not_found_html())
			}

			return build_response("500 Internal Server Error", HashMap::from([("Content-Type", "text/html")]), internal_server_error_html())
		}
	};

	match get_content_from_cache(caches, full_path.clone()) {
		Ok(body) => {
			if let Some(body) = body {
				return build_response("200 OK", HashMap::new(), body);
			}
		},
		Err(error) => {
			if not_found(&error) {
				return build_response("404 Not Found", HashMap::from([("Content-Type", "text/html")]), not_found_html())
			}

			return build_response("500 Internal Server Error", HashMap::from([("Content-Type", "text/html")]), internal_server_error_html())
		}
	};

	let body = match get_content(full_path.clone(), path) {
		Ok(body) => Arc::new(body),
		Err(error) => {
			if not_found(&error) {
				return build_response("404 Not Found", HashMap::from([("Content-Type", "text/html")]), not_found_html());
			}

			return build_response("500 Internal Server Error", HashMap::from([("Content-Type", "text/html")]), internal_server_error_html());
		}
	};

	match get_modified_timestamp(full_path.clone()) {
		Ok(current_timestamp) => {
			caches.insert(full_path, Cache {
				content: Arc::clone(&body),
				modified_timestamp: current_timestamp
			});
		},
		Err(error) => {
			if not_found(&error) {
				return build_response("404 Not Found", HashMap::from([("Content-Type", "text/html")]), not_found_html());
			}

			return build_response("500 Internal Server Error", HashMap::from([("Content-Type", "text/html")]), internal_server_error_html())
		}
	};

	build_response("200 OK", HashMap::new(), body.to_vec())
}

fn parse_request(data: Vec<u8>) -> Result<String, Error> {
	let request = match String::from_utf8(data) {
		Ok(request) => request,
		Err(_) => {
			return Err(Error::new(ErrorKind::InvalidData, ""));
		}
	};

	let mut lines = request.lines();

	let first_line = match lines.nth(0) {
		Some(first_line) => first_line,
		None => {
			return Err(Error::new(ErrorKind::InvalidData, ""));
		}
	};

	let path = match first_line.split(' ').nth(1) {
		Some(path) => path.to_string(),
		None => {
			return Err(Error::new(ErrorKind::InvalidData, ""));
		}
	};

	Ok(path)
}

fn bad_request_html() -> Vec<u8> {
	html_boilerplate("Bad Request", "<h1>Bad Request</h1>")
}

fn forbidden_html() -> Vec<u8> {
	html_boilerplate("Forbidden", "<h1>Forbidden</h1>")
}

fn not_found_html() -> Vec<u8> {
	html_boilerplate("Not Found", "<h1>Not Found</h1>")
}

fn internal_server_error_html() -> Vec<u8> {
	html_boilerplate("Internal Server Error", "<h1>Internal Server Error</h1>")
}

fn html_boilerplate(title: &str, content: &str) -> Vec<u8> {
	format!("<!DOCTYPE html>\n<html lang=\"en\">\n\t<head>\n\t\t<title>{}</title>\n\n\t\t<meta charset=\"UTF-8\"/>\n\t\t<meta name=\"robots\" content=\"noindex\"/>\n\t\t<meta http-equiv=\"X-UA-Compatible\" content=\"IE=edge\"/>\n\t\t<meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\n\t</head>\n\t<body>\n\t\t<main>\n\t\t\t{}\n\t\t</main>\n\t</body>\n</html>", title, content).as_bytes().to_vec()
}

fn build_response(status: &str, headers: HashMap<&str, &str>, mut body: Vec<u8>) -> Vec<u8> {
	let mut response = Vec::new();

	response.append(&mut "HTTP/1.0 ".as_bytes().to_vec());

	response.append(&mut status.as_bytes().to_vec());

	response.append(&mut  "\r\n".as_bytes().to_vec());

	for header in headers {
		response.append(&mut format!("{}: {}\r\n", header.0, header.1).as_bytes().to_vec());
	}

	response.append(&mut "Server: Jasmin\r\n".as_bytes().to_vec());

	response.append(&mut "\r\n".as_bytes().to_vec());

	response.append(&mut body);

	response
}

fn prevent_directory_transversal(base_dir: String, mut path: String) -> Result<PathBuf, Error> {
	let base_path = Path::new(&base_dir)
		.canonicalize()?;

	path = path.trim_start_matches('/')
		.to_string();

	if path.is_empty() {
		path = String::from("index.html");
	}

	let full_path = base_path.join(path)
		.canonicalize()?;

	if !full_path.starts_with(base_path) {
		return Err(Error::new(ErrorKind::PermissionDenied, ""));
	}

	Ok(full_path)
}

fn permission_denied(error: &Error) -> bool {
	error.kind() == ErrorKind::PermissionDenied
}

fn not_found(error: &Error) -> bool {
	error.kind() == ErrorKind::NotFound
}

fn get_content_from_cache(caches: &mut HashMap<PathBuf, Cache>, full_path: PathBuf) -> Result<Option<Vec<u8>>, Error> {
	let cache = match caches.get_mut(&full_path) {
		Some(cache) => cache,
		None => {
			return Ok(None);
		}
	};

	let current_timestamp = match get_modified_timestamp(full_path.clone()) {
		Ok(current_timestamp) => current_timestamp,
		Err(error) => {
			if not_found(&error) {
				caches.remove(&full_path);
			}

			return Err(error);
		}
	};

	if cache.modified_timestamp != current_timestamp {
		return Ok(None)
	}

	Ok(Some(cache.content.to_vec()))
}

fn get_modified_timestamp(full_path: PathBuf) -> Result<u64, Error> {
	let metadata = fs::metadata(full_path)?;

	let timestamp = match metadata.modified()?.duration_since(time::UNIX_EPOCH) {
		Ok(timestamp) => timestamp.as_secs(),
		Err(_) => {
			return Err(Error::new(ErrorKind::Other, ""));
		}
	};

	Ok(timestamp)
}

fn get_content(full_path: PathBuf, path: String) -> Result<Vec<u8>, Error> {
	if !full_path.exists() {
		return Err(Error::new(ErrorKind::NotFound, ""));
	}

	if full_path.is_file() {
		let content = fs::read(full_path)?;

		return Ok(content);
	}

	let title = format!("Index of: {}", path);

	let entries: Vec<String> = fs::read_dir(full_path)?.filter_map(|entry| {
		let entry = entry.ok()?;

		entry.file_name()
			.into_string()
				.ok()
	}).collect();

	let mut tags = entries.iter()
		.map(|entry| format!("<a href=\"{}\">{}</a>", format!("{}/{}", path, entry), entry))
			.collect::<Vec<_>>()
				.join("\n\n\t\t\t<br/>\n\n\t\t\t");

	if tags.is_empty() {
		tags = String::from("<p>It's empty.</p>");
	}

	let html = html_boilerplate(&title, &tags);

	Ok(html)
}

fn close_client(sessions: &mut HashMap<usize, Session>, client_id: usize, poll: &Poll) {
	let session = sessions.get_mut(&client_id)
		.unwrap();

	let _ = poll.registry()
		.deregister(&mut session.client);

	sessions.remove(&client_id);
}