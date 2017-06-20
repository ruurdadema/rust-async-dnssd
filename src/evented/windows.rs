//! On windows we cannot get read (or write) events for sockets in the
//! IOCP model; we only can run asynchronous reads!
//!
//! So we need to use select() to poll for read, and run it in a
//! separate thread.  To cancel that select() we'd need another socket
//! to select() for, and there is no socketpair() - we could use a
//! loopback TCP connection, but a firewall might block it.
//!
//! Instead we use a small (1 second) timeout for the select; it is only
//! used to terminate the thread anyway.
//!
//! This of course wastes one thread per fd we want to watch; a bigger
//! solution would reuse the same backend thread over and over, but than
//! we'd have to try the loopback TCP connection to wake it and fall
//! back to a smaller timeout.

use futures::sync::mpsc as futures_mpsc;
use futures::{Async,Sink,Stream};
use futures::sink::Wait;
use libc;
use std::io;
use std::os::raw::{c_int};
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;
use tokio_core::reactor::{Handle,Remote};
use std::cell::UnsafeCell;

use remote::GetRemote;

#[derive(Clone,Copy,PartialEq,Eq,Debug)]
enum PollRequest {
	Poll,
	Close,
}

struct SelectFdRead {
	fd: c_int,
	read_fds: libc::fd_set,
}
impl SelectFdRead {
	pub fn new(fd: c_int) -> Self {
		use std::mem::uninitialized;
		let mut read_fds : libc::fd_set = unsafe { uninitialized() };
		unsafe { libc::FD_ZERO(&mut read_fds) };
		SelectFdRead{
			fd: fd,
			read_fds: read_fds,
		}
	}

	pub fn select(&mut self, timeout: Option<Duration>) -> bool {
		use std::ptr::null_mut;
		let mut timeout = timeout.map(|timeout|
			libc::timeval{
				tv_sec: timeout.as_secs() as i64,
				tv_usec: (timeout.subsec_nanos() / 1000) as i64,
			}
		);
		unsafe {
			libc::FD_SET(self.fd, &mut self.read_fds);
			libc::select(
				self.fd+1,
				&mut self.read_fds,
				null_mut(),
				null_mut(),
				timeout.as_mut().map(|x| x as *mut _).unwrap_or(null_mut()),
			);
			libc::FD_ISSET(self.fd, &mut self.read_fds)
		}
	}
}

struct Inner {
	/// file descriptor to watch read events for
	fd: c_int,
	/// background select thread
	_thread: thread::JoinHandle<()>,
	/// either the select thread is running a Poll request or we manually
	/// sent a response through `send_response`
	pending_request: bool,
	/// send poll or close request to select thread
	send_request: std_mpsc::SyncSender<PollRequest>,
	/// when need_read() is called we use this to trigger a response if
	/// we already know the read event is pending
	send_response: Wait<futures_mpsc::Sender<()>>,
	/// a response means a read event is pending
	recv_response: futures_mpsc::Receiver<()>,
	remote: Remote,
}
impl Inner {
	fn poll_read(&mut self) -> Async<()> {
		debug!("poll read");
		if !self.pending_request {
			let mut read_fds = SelectFdRead::new(self.fd);
			if read_fds.select(Some(Duration::from_millis(0))) {
				debug!("poll read: local ready");
				return Async::Ready(());
			} else {
				debug!("poll read: not ready, start thread");
				self.send_request.send(PollRequest::Poll).expect("select thread terminated");
				self.pending_request = true;
			}
		}

		match self.recv_response.poll().unwrap() {
			Async::Ready(None) => unreachable!(),
			Async::Ready(Some(())) => {
				debug!("poll read: thread ready");
				self.pending_request = false;
				Async::Ready(())
			},
			Async::NotReady => {
				debug!("poll read: thread not ready");
				Async::NotReady
			},
		}
	}

	fn need_read(&mut self) {
		// we need to get Async::NotReady from recv_response.poll
		match self.recv_response.poll().unwrap() {
			Async::Ready(None) => unreachable!(),
			Async::Ready(Some(())) => {
				// was ready. damn...
				assert!(self.pending_request);
				// try again - can't be ready again
				match self.recv_response.poll().unwrap() {
					Async::Ready(None) => unreachable!(),
					Async::Ready(Some(())) => unreachable!(),
					Async::NotReady => (),
				}
				// now send a response - it was ready after all
				self.send_response.send(()).unwrap();
			},
			Async::NotReady => {
				// yay!
				//
				// now we need something to trigger a response
				let mut read_fds = SelectFdRead::new(self.fd);
				self.pending_request = true;
				if read_fds.select(Some(Duration::from_millis(0))) {
					// ready, send a response
					self.send_response.send(()).unwrap();
				} else {
					debug!("poll need read: not ready, start thread");
					self.send_request.send(PollRequest::Poll).expect("select thread terminated");
				}
			},
		}
	}
}

pub struct PollReadFd(UnsafeCell<Inner>);
impl PollReadFd {
	/// does not take overship of fd
	pub fn new(fd: c_int, handle: &Handle) -> io::Result<Self> {
		// buffer one request for "Close"
		let (send_request, recv_request) = std_mpsc::sync_channel(1);
		// buffer one notification
		let (send_response, recv_response) = futures_mpsc::channel(1);
		let outer_send_response = send_response.clone().wait();

		let thread = thread::spawn(move || {
			let mut read_fds = SelectFdRead::new(fd);
			let mut send_response = send_response.wait();
			loop {
				debug!("[select thread] waiting for request");
				match recv_request.recv() {
					Ok(PollRequest::Poll) => (),
					Ok(PollRequest::Close) => return,
					Err(_) => return,
				}
				debug!("[select thread] start polling");

				while !read_fds.select(Some(Duration::from_millis(1000))) {
					match recv_request.try_recv() {
						Ok(PollRequest::Poll) => unreachable!(),
						Ok(PollRequest::Close) => return,
						Err(_) => (), // back to select()
					}
				}

				debug!("[select thread] read event");

				if send_response.send(()).is_err() { return; }
			}
		});

		Ok(PollReadFd(UnsafeCell::new(Inner{
			fd: fd,
			_thread: thread,
			pending_request: false,
			send_request: send_request,
			send_response: outer_send_response,
			recv_response: recv_response,
			remote: handle.remote().clone(),
		})))
	}

	fn inner(&self) -> &mut Inner {
		unsafe { &mut *self.0.get() }
	}

	pub fn poll_read(&self) -> Async<()> {
		self.inner().poll_read()
	}

	pub fn need_read(&self) {
		self.inner().need_read()
	}
}

impl GetRemote for PollReadFd {
	fn remote(&self) -> &Remote {
		&self.inner().remote
	}
}

impl Drop for PollReadFd {
	fn drop(&mut self) {
		let _ = self.inner().send_request.send(PollRequest::Close);
	}
}
