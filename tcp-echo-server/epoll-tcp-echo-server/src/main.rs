use std::collections::HashMap;
use std::io;
use std::mem;
use std::os::fd::RawFd;

const MAX_EVENTS: usize = 256;
const BUFFER_SIZE: usize = 4096;
const PORT: u16 = 3457;

fn errno() -> io::Error {
    io::Error::last_os_error()
}

struct Epoll {
    epfd: RawFd,
    events: Vec<libc::epoll_event>,
}

impl Epoll {
    fn new() -> io::Result<Self> {
        let epfd = unsafe { libc::epoll_create1(0) };
        if epfd < 0 {
            return Err(errno());
        }
        Ok(Self {
            epfd,
            events: vec![libc::epoll_event { events: 0, u64: 0 }; MAX_EVENTS],
        })
    }

    fn ctl(&self, op: i32, fd: RawFd, interest: u32) -> io::Result<()> {
        let mut ev = libc::epoll_event {
            events: interest,
            u64: fd as u32 as u64,
        };
        let ret = unsafe { libc::epoll_ctl(self.epfd, op, fd, &mut ev) };
        if ret < 0 {
            return Err(errno());
        }
        Ok(())
    }

    fn add(&self, fd: RawFd, interest: u32) -> io::Result<()> {
        self.ctl(libc::EPOLL_CTL_ADD, fd, interest)
    }

    fn modify(&self, fd: RawFd, interest: u32) -> io::Result<()> {
        self.ctl(libc::EPOLL_CTL_MOD, fd, interest)
    }

    fn del(&self, fd: RawFd) -> io::Result<()> {
        let ret =
            unsafe { libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut()) };
        if ret < 0 {
            return Err(errno());
        }
        Ok(())
    }

    fn wait(&mut self) -> io::Result<usize> {
        loop {
            let n = unsafe {
                libc::epoll_wait(self.epfd, self.events.as_mut_ptr(), MAX_EVENTS as i32, -1)
            };
            if n < 0 {
                let e = errno();
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(e);
            }
            return Ok(n as usize);
        }
    }
}

#[derive(Debug, PartialEq)]
enum ConnState {
    // Waiting for EPOLLIN. Once the socket becomes readable, the process calls recv itself.
    WaitReadable,
    // Waiting for EPOLLOUT. Once the socket becomes writable, the process calls send itself.
    WaitWritable,
}

struct Conn {
    buf: Vec<u8>,
    received: usize, // number of bytes received by recv
    sent: usize,     // number of those bytes sent so far
    state: ConnState,
}

impl Conn {
    fn new() -> Self {
        Self {
            buf: vec![0u8; BUFFER_SIZE],
            received: 0,
            sent: 0,
            state: ConnState::WaitReadable,
        }
    }
}

fn listen_socket(port: u16) -> io::Result<RawFd> {
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_NONBLOCK, 0);
        if fd < 0 {
            return Err(errno());
        }
        let one: i32 = 1;
        if libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &one as *const _ as *const libc::c_void,
            mem::size_of::<i32>() as u32,
        ) < 0
        {
            return Err(errno());
        }
        let addr = libc::sockaddr_in {
            sin_family: libc::AF_INET as u16,
            sin_port: port.to_be(),
            sin_addr: libc::in_addr { s_addr: 0 },
            sin_zero: [0; 8],
        };
        if libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            mem::size_of::<libc::sockaddr_in>() as u32,
        ) < 0
        {
            return Err(errno());
        }
        if libc::listen(fd, 128) < 0 {
            return Err(errno());
        }
        Ok(fd)
    }
}

fn close_conn(epoll: &Epoll, conns: &mut HashMap<RawFd, Conn>, fd: RawFd) {
    // close also removes the fd from the interest list, so del is redundant,
    // but we call it to be explicit.
    let _ = epoll.del(fd);
    conns.remove(&fd);
    unsafe { libc::close(fd) };
}

fn is_eagain(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::EAGAIN)
}

fn main() -> io::Result<()> {
    let listen_fd = listen_socket(PORT)?;
    let mut epoll = Epoll::new()?;
    let mut conns: HashMap<RawFd, Conn> = HashMap::new();

    epoll.add(listen_fd, libc::EPOLLIN as u32)?;
    println!("listening on 0.0.0.0:{}", PORT);

    loop {
        let n = epoll.wait()?;

        for i in 0..n {
            let ev = epoll.events[i];
            let fd = ev.u64 as u32 as RawFd;

            if fd == listen_fd {
                loop {
                    let conn_fd = unsafe {
                        libc::accept4(
                            listen_fd,
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                            libc::SOCK_NONBLOCK,
                        )
                    };
                    if conn_fd < 0 {
                        let e = errno();
                        if !is_eagain(&e) {
                            eprintln!("accept failed: {}", e);
                        }
                        break;
                    }
                    conns.insert(conn_fd, Conn::new());
                    epoll.add(conn_fd, libc::EPOLLIN as u32)?;
                }
                continue;
            }

            let conn = conns.get_mut(&fd).expect("event for unknown fd");

            match conn.state {
                ConnState::WaitReadable => {
                    // Notified that the socket is readable, so the process calls recv itself.
                    let ret = unsafe {
                        libc::recv(
                            fd,
                            conn.buf.as_mut_ptr() as *mut libc::c_void,
                            conn.buf.len(),
                            0,
                        )
                    };
                    if ret < 0 {
                        let e = errno();
                        if is_eagain(&e) {
                            // Even after a notification, the socket may not be readable.
                            // Keep the interest as is and go back to waiting.
                            continue;
                        }
                        close_conn(&epoll, &mut conns, fd);
                        continue;
                    }
                    if ret == 0 {
                        close_conn(&epoll, &mut conns, fd);
                        continue;
                    }
                    conn.received = ret as usize;
                    conn.sent = 0;
                    conn.state = ConnState::WaitWritable;
                    // Where the io_uring version would "submit a send". Here we do not
                    // perform the send yet; we only switch our interest to the socket
                    // becoming writable. A change of interest, not a request for an operation.
                    epoll.modify(fd, libc::EPOLLOUT as u32)?;
                }
                ConnState::WaitWritable => {
                    // Notified that the socket is writable, so the process calls send itself.
                    let ret = unsafe {
                        libc::send(
                            fd,
                            conn.buf.as_ptr().add(conn.sent) as *const libc::c_void,
                            conn.received - conn.sent,
                            0,
                        )
                    };
                    if ret < 0 {
                        let e = errno();
                        if is_eagain(&e) {
                            continue;
                        }
                        close_conn(&epoll, &mut conns, fd);
                        continue;
                    }
                    conn.sent += ret as usize;
                    if conn.sent < conn.received {
                        // Partial send. Keep the interest in EPOLLOUT and wait to send the rest.
                        // Where the io_uring version would "re-submit the remainder".
                        continue;
                    }
                    // Everything has been sent; go back to waiting for data
                    conn.state = ConnState::WaitReadable;
                    epoll.modify(fd, libc::EPOLLIN as u32)?;
                }
            }
        }
    }
}
