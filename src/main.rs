use nix::sys::epoll::*;
use std::{collections::HashMap, io};
use std::{collections::VecDeque, io::prelude::*};
use std::{
    net::ToSocketAddrs,
    os::unix::io::{AsRawFd, RawFd},
};
use std::{
    net::{TcpListener, TcpStream},
    sync::Arc,
};

#[derive(Debug)]
struct ReadingInput {
    data: String,
    is_complete: bool,
}

impl ReadingInput {
    fn from_str(s: &str) -> io::Result<(Self, &str)> {
        let mut ret = Self {
            data: String::new(),
            is_complete: false,
        };
        let rest = ret.push_str(s);
        Ok((ret, rest))
    }

    fn push_str<'a, 'b>(&'a mut self, s: &'b str) -> &'b str {
        match s.find("\r\n") {
            Some(i) => {
                self.data.push_str(&s[..i]);
                self.is_complete = true;
                &s[(i + 2)..]
            }
            None => {
                self.data.push_str(s);
                self.is_complete = false;
                ""
            }
        }
    }

    fn is_complete(&self) -> bool {
        self.is_complete
    }
}

#[derive(Debug)]
struct User {
    stream: TcpStream,
    name: Option<String>,
    queue: VecDeque<Arc<String>>,
    buf: Option<ReadingInput>,
}

impl User {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            name: None,
            queue: VecDeque::new(),
            buf: None,
        }
    }

    fn push_message(&mut self, message: Arc<String>) {
        self.queue.push_back(message)
    }

    fn pop_message(&mut self) -> Option<Arc<String>> {
        self.queue.pop_front()
    }

    fn name(&self) -> &str {
        self.name.as_deref().unwrap_or("<anonymous>")
    }

    fn raw_fd(&self) -> i32 {
        self.stream.as_raw_fd()
    }

    fn read_cb(&mut self) -> io::Result<Option<String>> {
        let mut buf = [0u8; 4096];
        let s = match self.stream.read(&mut buf) {
            Ok(size) => std::str::from_utf8(&buf[0..size]).expect("ignoring error"),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => "",
            Err(e) => {
                return Err(e);
            }
        };
        let data = self.new_input(s)?;
        if self.name.is_none() {
            self.name = data;
            Ok(None)
        } else {
            Ok(data)
        }
    }

    fn new_input(&mut self, s: &str) -> io::Result<Option<String>> {
        if let Some(mut input) = self.buf.take() {
            input.data.push_str(s);
            if input.is_complete() {
                Ok(Some(input.data))
            } else {
                self.buf = Some(input);
                Ok(None)
            }
        } else {
            let (input, rest) = ReadingInput::from_str(s)?;
            assert_eq!(rest, "");
            if input.is_complete() {
                Ok(Some(input.data))
            } else {
                self.buf = Some(input);
                Ok(None)
            }
        }
    }

    fn write_cb(&mut self) -> io::Result<()> {
        while let Some(msg) = self.pop_message() {
            match self.stream.write(msg.as_bytes()) {
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => eprintln!("could not write message to {:?}, {}", self.name, e),
            };
        }

        Ok(())
    }

    fn close_cb(&mut self) -> io::Result<()> {
        self.stream.shutdown(std::net::Shutdown::Both)?;
        Ok(())
    }
}

#[derive(Debug)]
struct IdGenerator(u64);

impl IdGenerator {
    fn new() -> Self {
        Self(NEW_CONNECTION)
    }

    fn next(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

fn listener_read_event(key: u64) -> EpollEvent {
    EpollEvent::new(EpollFlags::EPOLLRDHUP | EpollFlags::EPOLLIN, key)
}

fn listener_read_write_event(key: u64) -> EpollEvent {
    EpollEvent::new(
        EpollFlags::EPOLLRDHUP | EpollFlags::EPOLLIN | EpollFlags::EPOLLOUT,
        key,
    )
}

#[derive(Debug)]
struct Server {
    epoll_fd: RawFd,
    id_gen: IdGenerator,
    users: HashMap<u64, User>,
}

const NEW_CONNECTION: u64 = 100;

impl Server {
    fn new() -> Self {
        Self {
            epoll_fd: epoll_create().expect("can create epoll queue"),
            id_gen: IdGenerator::new(),
            users: HashMap::new(),
        }
    }

    fn add_user(&mut self, key: u64, user: User) {
        self.users.insert(key, user);
    }

    fn add_interest(&self, fd: RawFd, event: &mut EpollEvent) -> io::Result<()> {
        epoll_ctl(self.epoll_fd, EpollOp::EpollCtlAdd, fd, event)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }

    fn watch(&self, user: &User, event: &mut EpollEvent) -> io::Result<()> {
        epoll_ctl(self.epoll_fd, EpollOp::EpollCtlAdd, user.raw_fd(), event)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }

    fn change_event(&self, user: &User, event: &mut EpollEvent) -> io::Result<()> {
        epoll_ctl(self.epoll_fd, EpollOp::EpollCtlMod, user.raw_fd(), event)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }

    fn unwatch(&self, user: &User) -> io::Result<()> {
        epoll_ctl(self.epoll_fd, EpollOp::EpollCtlDel, user.raw_fd(), None)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    }

    fn handle_new_connection(&mut self, listener: &TcpListener) -> io::Result<()> {
        match listener.accept() {
            Ok((stream, addr)) => {
                stream.set_nonblocking(true)?;
                println!("new client: {}", addr);
                let key = self.id_gen.next();
                let mut user = User::new(stream);
                user.push_message(Arc::new("Enter your name: ".into()));
                self.watch(&user, &mut listener_read_write_event(key))?;
                self.add_user(key, user);
            }
            Err(e) => eprintln!("couldn't accept: {}", e),
        };
        Ok(())
    }

    fn handle_user_readable(&mut self, user: &mut User) -> io::Result<()> {
        let ret = user.read_cb()?;
        if let Some(message) = ret {
            let msg = Arc::new(format!("{}: {}\r\n", user.name(), message));
            // mutability hack
            for (_, u) in &mut self.users {
                u.push_message(msg.clone());
            }
            for (k, u) in &self.users {
                self.change_event(&*u, &mut listener_read_write_event(*k))?;
            }
        }
        Ok(())
    }

    fn handle_user_writable(&mut self, user: &mut User, key: u64) -> io::Result<()> {
        user.write_cb()?;
        if user.queue.is_empty() {
            self.change_event(&user, &mut listener_read_event(key))?;
        }
        Ok(())
    }

    fn handle_user_closed(&mut self, user: &mut User) -> io::Result<()> {
        user.close_cb()?;
        self.unwatch(&user)?;
        Ok(())
    }

    fn handle_user_event(&mut self, ev: EpollEvent) -> io::Result<()> {
        let key = ev.data();
        let events = ev.events();

        if let Some(mut user) = self.users.remove(&key) {
            if events.contains(EpollFlags::EPOLLIN) {
                self.handle_user_readable(&mut user)?;
            }

            if events.contains(EpollFlags::EPOLLOUT) {
                self.handle_user_writable(&mut user, key)?;
            }

            if events.contains(EpollFlags::EPOLLRDHUP) {
                self.handle_user_closed(&mut user)?;
            } else {
                self.add_user(key, user)
            }
        }
        Ok(())
    }

    fn run<A: ToSocketAddrs>(&mut self, addr: A) -> io::Result<()> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        self.add_interest(
            listener.as_raw_fd(),
            &mut listener_read_event(NEW_CONNECTION),
        )?;

        let mut events = vec![EpollEvent::empty(); 1024];

        loop {
            println!("clients connected: {}", self.users.len());

            let res = match epoll_wait(self.epoll_fd, &mut events, -1) {
                Ok(v) => v,
                Err(e) => panic!("error during epoll wait: {}", e),
            };

            for ev in &events[0..res] {
                match ev.data() {
                    NEW_CONNECTION => self.handle_new_connection(&listener)?,
                    _ => self.handle_user_event(*ev)?,
                }
            }
        }
    }
}

fn main() -> io::Result<()> {
    let mut server = Server::new();

    server.run("localhost:8000")
}
