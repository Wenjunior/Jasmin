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
	collections::HashMap,
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
	}
};

const PORT: u16 = 80;
const CAPACITY: usize = 128;
const REQUEST_SIZE: usize = 1024;
const SERVER_TOKEN: Token = Token(0);

fn accept_client(server: &TcpListener, poll: &Poll, next_client_id: &mut usize, clients: &mut HashMap<usize, TcpStream>) -> Result<(), Error>{
	let (mut client, _address) = server.accept()?;

	poll.registry()
		.register(&mut client, Token(*next_client_id), Interest::READABLE)?;

	clients.insert(*next_client_id, client);

	*next_client_id += 1;

	Ok(())
}

fn handle_client(clients: &mut HashMap<usize, TcpStream>, client_id: usize, event: &Event, poll: &Poll, responses: &mut HashMap<usize, Vec<u8>>) -> Result<(), Error> {
	if let Some(client) = clients.get_mut(&client_id) {
		if event.is_readable() {
			let mut data = vec![0; REQUEST_SIZE];

			let mut bytes_readed = 0;

			loop {
				match client.read(&mut data[bytes_readed..]) {
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
				.reregister(client, Token(client_id), Interest::WRITABLE)?;

			responses.insert(client_id, data);

			return Ok(());
		}

		if event.is_writable() {
			let response = responses.get_mut(&client_id)
				.unwrap();

			client.write_all(response)?;

			client.flush()?;

			close_client(clients, client_id, responses, poll);

			return Ok(())
		}
	}

	Ok(())
}

fn close_client(clients: &mut HashMap<usize, TcpStream>, client_id: usize, responses: &mut HashMap<usize, Vec<u8>>, poll: &Poll) {
	let client = clients.get_mut(&client_id)
		.unwrap();

	let _ = poll.registry()
		.deregister(client);

	clients.remove(&client_id);

	if responses.contains_key(&client_id) {
		responses.remove(&client_id);
	}
}

fn run_server() -> Result<(), Error> {
	let mut poll = Poll::new()?;

	let mut events = Events::with_capacity(CAPACITY);

	let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), PORT);

	let mut server = TcpListener::bind(address)?;

	poll.registry()
		.register(&mut server, SERVER_TOKEN, Interest::READABLE)?;

	let mut next_client_id: usize = 1;

	let mut clients: HashMap<usize, TcpStream> = HashMap::new();

	let mut responses: HashMap<usize, Vec<u8>> = HashMap::new();

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
						if let Err(error) = accept_client(&server, &poll, &mut next_client_id, &mut clients) {
							if would_block(&error) {
								break;
							}
						}
					}
				},
				Token(client_id) => {
					if let Err(_) = handle_client(&mut clients, client_id, &event, &poll, &mut responses) {
						close_client(&mut clients, client_id, &mut responses, &poll);
					}
				}
			}
		}
	}
}

fn interrupted(error: &Error) -> bool {
	error.kind() == ErrorKind::Interrupted
}

fn would_block(error: &Error) -> bool {
	error.kind() == ErrorKind::WouldBlock
}

fn main() {
	if let Err(error) = run_server() {
		eprintln!("{}", error);
	}
}