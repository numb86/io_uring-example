use std::collections::HashMap;
use std::io;
use std::mem;
use std::os::fd::RawFd;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};

const SQ_ENTRIES: u32 = 256;
const BUFFER_SIZE: usize = 4096;
const PORT: u16 = 3456;

// https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/include/uapi/linux/io_uring.h#L544-L546
const IORING_OFF_SQ_RING: i64 = 0;
const IORING_OFF_CQ_RING: i64 = 0x8000000;
const IORING_OFF_SQES: i64 = 0x10000000;

// https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/include/uapi/linux/io_uring.h#L595
const IORING_ENTER_GETEVENTS: u32 = 1;

// https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/include/uapi/linux/io_uring.h#L623
const IORING_FEAT_SINGLE_MMAP: u32 = 1;

// https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/include/uapi/linux/io_uring.h#L267-L281
const IORING_OP_ACCEPT: u8 = 13;
const IORING_OP_SEND: u8 = 26;
const IORING_OP_RECV: u8 = 27;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SqringOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    flags: u32,
    dropped: u32,
    array: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CqringOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    overflow: u32,
    cqes: u32,
    flags: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IoUringParams {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    resv: [u32; 3],
    sq_off: SqringOffsets,
    cq_off: CqringOffsets,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Sqe {
    opcode: u8,
    flags: u8,
    ioprio: u16,
    fd: i32,
    off: u64,
    addr: u64,
    len: u32,
    op_flags: u32,
    user_data: u64,
    buf_index: u16,
    personality: u16,
    splice_fd_in: i32,
    addr3: u64,
    _pad2: u64,
}

impl Sqe {
    fn zeroed() -> Self {
        unsafe { mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Cqe {
    user_data: u64,
    res: i32,
    flags: u32,
}

struct SqRing {
    head: *const AtomicU32,
    tail: *const AtomicU32,
    ring_mask: u32,
    entries: u32,
    array: *mut u32,
}

struct CqRing {
    head: *const AtomicU32,
    tail: *const AtomicU32,
    ring_mask: u32,
    cqes: *const Cqe,
}

impl SqRing {
    unsafe fn new(ring: *mut u8, off: &SqringOffsets) -> Self {
        unsafe {
            Self {
                head: ring.add(off.head as usize) as *const AtomicU32,
                tail: ring.add(off.tail as usize) as *const AtomicU32,
                ring_mask: *(ring.add(off.ring_mask as usize) as *const u32),
                entries: *(ring.add(off.ring_entries as usize) as *const u32),
                array: ring.add(off.array as usize) as *mut u32,
            }
        }
    }
}

impl CqRing {
    unsafe fn new(ring: *mut u8, off: &CqringOffsets) -> Self {
        unsafe {
            Self {
                head: ring.add(off.head as usize) as *const AtomicU32,
                tail: ring.add(off.tail as usize) as *const AtomicU32,
                ring_mask: *(ring.add(off.ring_mask as usize) as *const u32),
                cqes: ring.add(off.cqes as usize) as *const Cqe,
            }
        }
    }
}

struct Uring {
    ring_fd: RawFd,
    sq: SqRing,
    cq: CqRing,
    sqes: *mut Sqe,
    to_submit: u32,
}

fn os_error() -> io::Error {
    io::Error::last_os_error()
}

unsafe fn mmap_region(len: usize, fd: RawFd, offset: i64) -> io::Result<*mut u8> {
    let p = unsafe {
        libc::mmap(
            ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_POPULATE,
            fd,
            offset,
        )
    };
    if p == libc::MAP_FAILED {
        return Err(os_error());
    }
    Ok(p as *mut u8)
}

impl Uring {
    fn new(entries: u32) -> io::Result<Self> {
        let mut params = IoUringParams::default();
        let ring_fd =
            unsafe { libc::syscall(libc::SYS_io_uring_setup, entries, &mut params as *mut _) };
        if ring_fd < 0 {
            return Err(os_error());
        }
        let ring_fd = ring_fd as RawFd;

        let sq_ring_size = params.sq_off.array as usize + params.sq_entries as usize * 4;
        let cq_ring_size =
            params.cq_off.cqes as usize + params.cq_entries as usize * mem::size_of::<Cqe>();

        let (sq_ptr, cq_ptr) = unsafe {
            if params.features & IORING_FEAT_SINGLE_MMAP != 0 {
                let size = sq_ring_size.max(cq_ring_size);
                let p = mmap_region(size, ring_fd, IORING_OFF_SQ_RING)?;
                (p, p)
            } else {
                let sq = mmap_region(sq_ring_size, ring_fd, IORING_OFF_SQ_RING)?;
                let cq = mmap_region(cq_ring_size, ring_fd, IORING_OFF_CQ_RING)?;
                (sq, cq)
            }
        };

        let sqes = unsafe {
            mmap_region(
                params.sq_entries as usize * mem::size_of::<Sqe>(),
                ring_fd,
                IORING_OFF_SQES,
            )? as *mut Sqe
        };

        unsafe {
            Ok(Self {
                ring_fd,
                sq: SqRing::new(sq_ptr, &params.sq_off),
                cq: CqRing::new(cq_ptr, &params.cq_off),
                sqes,
                to_submit: 0,
            })
        }
    }

    fn enter(&self, to_submit: u32, min_complete: u32, flags: u32) -> io::Result<()> {
        let ret = unsafe {
            libc::syscall(
                libc::SYS_io_uring_enter,
                self.ring_fd,
                to_submit,
                min_complete,
                flags,
                ptr::null::<libc::sigset_t>(),
                0usize,
            )
        };
        if ret < 0 {
            let e = os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                return Ok(());
            }
            return Err(e);
        }
        Ok(())
    }

    fn flush(&mut self, min_complete: u32, flags: u32) -> io::Result<()> {
        self.enter(self.to_submit, min_complete, flags)?;
        self.to_submit = 0;
        Ok(())
    }

    // Write the SQE into the SQE array and append the index pointing to that slot
    // to the SQ ring's array. Nothing is handed to the kernel yet at this point
    // (submission happens in flush / submit_and_wait).
    // Side effect: if the ring is full, flush once to make room.
    fn push(&mut self, sqe: Sqe) -> io::Result<()> {
        loop {
            let head = unsafe { (*self.sq.head).load(Ordering::Acquire) };
            let tail = unsafe { (*self.sq.tail).load(Ordering::Relaxed) };
            if tail.wrapping_sub(head) < self.sq.entries {
                let idx = tail & self.sq.ring_mask;
                unsafe {
                    *self.sqes.add(idx as usize) = sqe;
                    *self.sq.array.add(idx as usize) = idx;
                    (*self.sq.tail).store(tail.wrapping_add(1), Ordering::Release);
                }
                self.to_submit += 1;
                return Ok(());
            }
            self.flush(0, 0)?;
        }
    }

    // Submit the accumulated SQEs and wait until at least one CQE arrives
    fn submit_and_wait(&mut self) -> io::Result<()> {
        self.flush(1, IORING_ENTER_GETEVENTS)
    }

    fn pop(&mut self) -> Option<Cqe> {
        let head = unsafe { (*self.cq.head).load(Ordering::Relaxed) };
        let tail = unsafe { (*self.cq.tail).load(Ordering::Acquire) };
        if head == tail {
            return None;
        }
        let cqe = unsafe { *self.cq.cqes.add((head & self.cq.ring_mask) as usize) };
        unsafe { (*self.cq.head).store(head.wrapping_add(1), Ordering::Release) };
        Some(cqe)
    }
}

// ---------------------------------------------------------------------------
// user_data encoding: upper 32 bits = operation tag (TAG_*), lower 32 bits = fd.
// TAG_* values are defined by this code and are unrelated to opcodes (IORING_OP_*).
// ---------------------------------------------------------------------------

const TAG_ACCEPT: u64 = 0;
const TAG_RECV: u64 = 1;
const TAG_SEND: u64 = 2;

fn encode_user_data(tag: u64, fd: RawFd) -> u64 {
    (tag << 32) | (fd as u32 as u64)
}

fn decode_user_data(user_data: u64) -> (u64, RawFd) {
    (user_data >> 32, user_data as u32 as RawFd)
}

struct Conn {
    buf: Vec<u8>,
    // Number of bytes written into buf by the most recent single IORING_OP_RECV operation (assigned from the CQE's res)
    received: usize,
    // Number of bytes out of `received` that have been sent so far
    sent: usize,
}

impl Conn {
    fn new() -> Self {
        Self {
            buf: vec![0u8; BUFFER_SIZE],
            received: 0,
            sent: 0,
        }
    }
}

fn sqe_accept(listen_fd: RawFd) -> Sqe {
    let mut sqe = Sqe::zeroed();
    sqe.opcode = IORING_OP_ACCEPT;
    sqe.fd = listen_fd;
    sqe.user_data = encode_user_data(TAG_ACCEPT, listen_fd);
    sqe
}

fn sqe_recv(fd: RawFd, conn: &mut Conn) -> Sqe {
    let mut sqe = Sqe::zeroed();
    sqe.opcode = IORING_OP_RECV;
    sqe.fd = fd;
    sqe.addr = conn.buf.as_mut_ptr() as u64;
    sqe.len = conn.buf.len() as u32;
    sqe.user_data = encode_user_data(TAG_RECV, fd);
    sqe
}

fn sqe_send(fd: RawFd, conn: &Conn) -> Sqe {
    let mut sqe = Sqe::zeroed();
    sqe.opcode = IORING_OP_SEND;
    sqe.fd = fd;
    sqe.addr = unsafe { conn.buf.as_ptr().add(conn.sent) } as u64;
    sqe.len = (conn.received - conn.sent) as u32;
    sqe.user_data = encode_user_data(TAG_SEND, fd);
    sqe
}

fn listen_socket(port: u16) -> io::Result<RawFd> {
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(os_error());
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
            return Err(os_error());
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
            return Err(os_error());
        }
        if libc::listen(fd, 128) < 0 {
            return Err(os_error());
        }
        Ok(fd)
    }
}

fn close_conn(conns: &mut HashMap<RawFd, Conn>, fd: RawFd) {
    conns.remove(&fd);
    unsafe { libc::close(fd) };
}

fn main() -> io::Result<()> {
    let listen_fd = listen_socket(PORT)?;
    let mut ring = Uring::new(SQ_ENTRIES)?;
    let mut conns: HashMap<RawFd, Conn> = HashMap::new();

    ring.push(sqe_accept(listen_fd))?;
    println!("listening on 0.0.0.0:{}", PORT);

    loop {
        ring.submit_and_wait()?;

        while let Some(cqe) = ring.pop() {
            let (tag, fd) = decode_user_data(cqe.user_data);
            match tag {
                TAG_ACCEPT => {
                    if cqe.res >= 0 {
                        let conn_fd = cqe.res as RawFd;
                        let mut conn = Conn::new();
                        // The SQE stores the address of buf's heap allocation. conn is
                        // moved into the HashMap right after this, but a move does not
                        // relocate the Vec's heap allocation, so the address stays valid.
                        let sqe = sqe_recv(conn_fd, &mut conn);
                        conns.insert(conn_fd, conn);
                        ring.push(sqe)?;
                    } else {
                        eprintln!("accept failed: {}", io::Error::from_raw_os_error(-cqe.res));
                    }
                    ring.push(sqe_accept(listen_fd))?;
                }
                TAG_RECV => {
                    let conn = conns
                        .get_mut(&fd)
                        .expect("IORING_OP_RECV CQE for unknown fd");
                    if cqe.res <= 0 {
                        // 0 = client disconnected, negative = error. Close the connection either way.
                        close_conn(&mut conns, fd);
                        continue;
                    }
                    conn.received = cqe.res as usize;
                    conn.sent = 0;
                    let sqe = sqe_send(fd, conn);
                    ring.push(sqe)?;
                }
                TAG_SEND => {
                    let conn = conns
                        .get_mut(&fd)
                        .expect("IORING_OP_SEND CQE for unknown fd");
                    if cqe.res < 0 {
                        close_conn(&mut conns, fd);
                        continue;
                    }
                    conn.sent += cqe.res as usize;
                    if conn.sent < conn.received {
                        // Partial send. Send the rest.
                        let sqe = sqe_send(fd, conn);
                        ring.push(sqe)?;
                    } else {
                        // Everything has been sent; queue the next IORING_OP_RECV and go back to waiting for data
                        let sqe = sqe_recv(fd, conn);
                        ring.push(sqe)?;
                    }
                }
                _ => unreachable!(),
            }
        }
    }
}
