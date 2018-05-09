extern crate futures;
extern crate libc;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::mem::zeroed;
use libc::{fd_set, FD_ZERO, FD_ISSET, FD_SET, FD_CLR, select, timeval};
use futures::{Future, FutureExt};
use futures::executor::Executor;
use futures::io::AsyncWrite;
use futures::io::AsyncWriteExt;
use futures::io::AsyncRead;
use futures::io::AsyncReadExt;
use futures::task::Context;
use futures::Async;
use futures::io::Error;
use futures::executor::block_on;
use futures::task::{Waker, Wake};

use std::thread::sleep;
use std::time::Duration;
use std::os::unix::io::RawFd;
use std::os::unix::io::AsRawFd;

use std::net::ToSocketAddrs;

use std::collections::BTreeMap;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

thread_local! {
	static REACTOR: RefCell<Option<Rc<RefCell<InnerEventLoop>>>> = RefCell::new(None);
}

#[derive(Clone)]
struct EventLoop(Rc<RefCell<InnerEventLoop>>);

impl EventLoop {
	pub fn new() -> Self {
		let inner = InnerEventLoop::new();
		let outer = Rc::new(RefCell::new(inner));

		REACTOR.with(|ev| {
			std::mem::replace(&mut *ev.borrow_mut(), Some(outer.clone()));
		});

		EventLoop(outer)
	}

	pub fn run<F: Future<Item = (), Error = ()>>(self, f: F) {
		self.0.borrow().run(f)
	}
}

struct InnerEventLoop {
	read: RefCell<BTreeMap<RawFd, Waker>>,
	write: RefCell<BTreeMap<RawFd, Waker>>,
}

impl InnerEventLoop {
	pub fn new() -> Self {
		InnerEventLoop {
			read: RefCell::new(BTreeMap::new()),
			write: RefCell::new(BTreeMap::new()),
		}
	}

	fn add_read_interest(&self, fd: RawFd, waker: Waker) {
		println!("adding read interest for {}", fd);
		if !self.read.borrow().contains_key(&fd) {
			self.read.borrow_mut().insert(fd, waker);
		}
	}

	fn add_write_interest(&self, fd: RawFd, waker: Waker) {
		println!("adding write interest for {}", fd);
		if !self.read.borrow().contains_key(&fd) {
			self.write.borrow_mut().insert(fd, waker);
		}
	}

	pub fn run<F: Future<Item = (), Error = ()>>(&self, mut f: F) {
		use futures::task::LocalMap;
    	use futures::executor::LocalPool;

		let pool = LocalPool::new();
		let mut exec = pool.executor();

		let mut map = LocalMap::new();

		struct W;

		impl Wake for W {
			fn wake(arc_self: &Arc<W>) {
				println!("waking")
			}
		}
		let waker = Waker::from(Arc::new(W));
		let mut context = Context::new(&mut map, &waker, &mut exec);

		match f.poll(&mut context) {
			Ok(Async::Ready(_)) => {
				println!("done");
				return;
			}
			Ok(Async::Pending) => {
				println!("future not yet ready");
			}
			Err(_) => {
				panic!("error in future");
			}
		}

		loop {
			println!("select loop start");

			let mut tv: timeval = timeval { 
				tv_sec: 1,
				tv_usec: 0,
			};

			let mut read_fds: fd_set = unsafe { std::mem::zeroed() };
			let mut write_fds: fd_set = unsafe { std::mem::zeroed() };

			unsafe { FD_ZERO(&mut read_fds) };
			unsafe { FD_ZERO(&mut write_fds) };

			let mut nfds = 0;

			for fd in self.read.borrow().keys() {
				println!("added fd {} for read", fd);
				unsafe { FD_SET(*fd, &mut read_fds as *mut fd_set) };
				nfds = std::cmp::max(nfds, fd + 1);
			}

			for fd in self.write.borrow().keys() {
				println!("added fd {} for write", fd);
				unsafe { FD_SET(*fd, &mut write_fds as *mut fd_set) };
				nfds = std::cmp::max(nfds, fd + 1);
			}

			let rv = unsafe {
				select(nfds, &mut read_fds, &mut write_fds, std::ptr::null_mut(), &mut tv)
			};
			if rv == -1 {
				panic!("select()");
			} else if rv == 0 {
				println!("timeout");
			} else {
				println!("data available on {} fds", rv);
			}

			for (fd, waker) in self.read.borrow().iter() {
				let is_set = unsafe { FD_ISSET(*fd, &mut read_fds as *mut fd_set) };
				println!("fd {} set (read)", fd);
				if is_set { 
					waker.wake();
				}
			}

			for (fd, waker) in self.write.borrow().iter() {
				let is_set = unsafe { FD_ISSET(*fd, &mut write_fds as *mut fd_set) };
				println!("fd {} set (write)", fd);
				if is_set { 
					waker.wake();
				}
			}

			match f.poll(&mut context) {
				Ok(Async::Ready(_)) => {
					println!("done");
					return;
				}
				Ok(Async::Pending) => {
					println!("future not yet ready");
				}
				Err(_) => {
					panic!("error in future");
				}
			}
		}
	}
}

#[derive(Debug)]
struct AsyncTcpStream(TcpStream);

impl AsyncTcpStream {
	pub fn connect<A: ToSocketAddrs>(addr: A) -> Result<AsyncTcpStream, std::io::Error> {
		let inner = TcpStream::connect(addr)?;
		inner.set_nonblocking(true)?;
		Ok(AsyncTcpStream(inner))
	}
}

struct NewAsyncTcpStream<A: ToSocketAddrs>(A);

impl<A: ToSocketAddrs> Future for NewAsyncTcpStream<A> {
	type Item = AsyncTcpStream;
	type Error = std::io::Error;

	fn poll(
        &mut self, 
        cx: &mut Context
    ) -> Result<Async<Self::Item>, Self::Error> {
		let inner = TcpStream::connect(&self.0)?;

		inner.set_nonblocking(true)?;

		Ok(Async::Ready(AsyncTcpStream(inner)))
    }
}

impl AsyncRead for AsyncTcpStream {
	fn poll_read(&mut self, cx: &mut Context, buf: &mut [u8]) -> Result<Async<usize>, Error> {
		println!("poll_read() called");

		let fd = self.0.as_raw_fd();
		let waker = cx.waker();

		match self.0.read(buf) {
			Ok(len) =>{
				println!("BUF {:?}", buf);
				Ok(Async::Ready(len))
			},
			Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
				REACTOR.with(|reactor| {
					if let Some(ref reactor) = *reactor.borrow() {
						reactor.borrow().add_read_interest(fd, waker.clone())
					}
				});

				Ok(Async::Pending)
			}
			Err(err) => unimplemented!(),
		}
	}
}

impl AsyncWrite for AsyncTcpStream {
	fn poll_write(
	    &mut self, 
	    cx: &mut Context, 
	    buf: &[u8]
	) -> Result<Async<usize>, Error> {
		println!("poll_write() called");

		let fd = self.0.as_raw_fd();
		let waker = cx.waker();

		match self.0.write(buf) {
			Ok(len) => Ok(Async::Ready(len)),
			Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
				REACTOR.with(|reactor| {
					if let Some(ref reactor) = *reactor.borrow() {
						reactor.borrow().add_write_interest(fd, waker.clone())
					}
				});

				Ok(Async::Pending)
			}
			Err(err) => unimplemented!(),
		}
	}

	fn poll_flush(&mut self, cx: &mut Context) -> Result<Async<()>, Error> {
		println!("poll_flush() called");
		Ok(Async::Ready(()))
	}	

	fn poll_close(&mut self, cx: &mut Context) -> Result<Async<()>, Error> {
		println!("poll_close() called");
		Ok(Async::Ready(()))
	}	
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::thread;

    #[test]
    fn it_works() {
		let stream = AsyncTcpStream::connect("127.0.0.1:9000").unwrap();
		println!("running");
		//let data: Vec<u8> = (0..1000).map(|_| b'a').collect();
		let data = b"hello world\n";
		let mut buf = vec![0; 128];
		let future = stream.write_all(data)
			.and_then(|(stream, _)| {
				stream.read(&mut buf)
			})
			.and_then(|(stream, buf, _)| {
				println!("{:?}", buf);
				Ok(())
			})
			.then(|_| Ok(()));
		let ev_loop = EventLoop::new();
		ev_loop.run(future);
    }
}
