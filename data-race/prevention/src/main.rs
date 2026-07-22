use std::io;
use std::process;
use std::ptr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

// https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/include/uapi/linux/io_uring.h#L544-L546
const IORING_OFF_SQ_RING: libc::off_t = 0;
const IORING_OFF_CQ_RING: libc::off_t = 0x8000000;
const IORING_OFF_SQES: libc::off_t = 0x10000000;

// https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/include/uapi/linux/io_uring.h#L276
const IORING_OP_READ: u8 = 22;

static WORK_COUNT: AtomicU64 = AtomicU64::new(0);

fn do_work() {
    thread::sleep(Duration::from_millis(100));
    WORK_COUNT.fetch_add(1, Ordering::Relaxed);
}

fn start_timer(budget: Duration) {
    thread::spawn(move || {
        thread::sleep(budget);
        let count = WORK_COUNT.load(Ordering::Relaxed);
        println!("time is up: work done = {count}");
        process::exit(0);
    });
}

#[repr(C)]
#[derive(Default)]
struct IoSqringOffsets {
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
#[derive(Default)]
struct IoCqringOffsets {
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
#[derive(Default)]
struct IoUringParams {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    resv: [u32; 3],
    sq_off: IoSqringOffsets,
    cq_off: IoCqringOffsets,
}

#[repr(C)]
struct IoUringSqe {
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
    __pad2: [u64; 2],
}

#[repr(C)]
struct IoUringCqe {
    user_data: u64,
    res: i32,
    flags: u32,
}

unsafe fn io_uring_setup(entries: u32, params: *mut IoUringParams) -> i32 {
    unsafe { libc::syscall(libc::SYS_io_uring_setup, entries, params) as i32 }
}

unsafe fn io_uring_enter(fd: i32, to_submit: u32, min_complete: u32, flags: u32) -> i32 {
    unsafe {
        libc::syscall(
            libc::SYS_io_uring_enter,
            fd,
            to_submit,
            min_complete,
            flags,
            ptr::null::<libc::c_void>(),
            0usize,
        ) as i32
    }
}

struct SqRing {
    head: *const AtomicU32,
    tail: *const AtomicU32,
    ring_mask: u32,
    array: *mut u32,
}

struct CqRing {
    head: *const AtomicU32,
    tail: *const AtomicU32,
    ring_mask: u32,
    cqes: *const IoUringCqe,
}

impl SqRing {
    unsafe fn new(ring: *mut u8, off: &IoSqringOffsets) -> Self {
        unsafe {
            Self {
                head: ring.add(off.head as usize) as *const AtomicU32,
                tail: ring.add(off.tail as usize) as *const AtomicU32,
                ring_mask: *(ring.add(off.ring_mask as usize) as *const u32),
                array: ring.add(off.array as usize) as *mut u32,
            }
        }
    }
}

impl CqRing {
    unsafe fn new(ring: *mut u8, off: &IoCqringOffsets) -> Self {
        unsafe {
            Self {
                head: ring.add(off.head as usize) as *const AtomicU32,
                tail: ring.add(off.tail as usize) as *const AtomicU32,
                ring_mask: *(ring.add(off.ring_mask as usize) as *const u32),
                cqes: ring.add(off.cqes as usize) as *const IoUringCqe,
            }
        }
    }
}

fn mmap_ring(fd: i32, len: usize, offset: libc::off_t) -> io::Result<*mut u8> {
    let ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_POPULATE,
            fd,
            offset,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(ptr as *mut u8)
}

fn submit(
    ring_fd: i32,
    sq: &SqRing,
    sqe_index: u32,
    buf: Vec<u8>,
    in_flight: &mut Option<Vec<u8>>,
) -> io::Result<()> {
    let tail = unsafe { (*sq.tail).load(Ordering::Relaxed) };
    unsafe {
        ptr::write(sq.array.add((tail & sq.ring_mask) as usize), sqe_index);
        (*sq.tail).store(tail.wrapping_add(1), Ordering::Release);
    }

    let ret = unsafe { io_uring_enter(ring_fd, 1, 0, 0) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }

    *in_flight = Some(buf);

    Ok(())
}

fn peek_completion(
    cq: &CqRing,
    in_flight: &mut Option<Vec<u8>>,
) -> io::Result<Option<(IoUringCqe, Vec<u8>)>> {
    let head = unsafe { (*cq.head).load(Ordering::Relaxed) };
    let tail = unsafe { (*cq.tail).load(Ordering::Acquire) };

    if head == tail {
        return Ok(None);
    }

    let cqe = unsafe { ptr::read(cq.cqes.add((head & cq.ring_mask) as usize)) };
    unsafe { (*cq.head).store(head.wrapping_add(1), Ordering::Release) };

    if cqe.res < 0 {
        return Err(io::Error::from_raw_os_error(-cqe.res));
    }

    let buf = in_flight
        .take()
        .expect("completion without in-flight buffer");

    Ok(Some((cqe, buf)))
}

fn main() -> io::Result<()> {
    let mut params = IoUringParams::default();

    let ring_fd = unsafe { io_uring_setup(8, &mut params) };
    if ring_fd < 0 {
        return Err(io::Error::last_os_error());
    }

    let sq_ring_size =
        params.sq_off.array as usize + params.sq_entries as usize * std::mem::size_of::<u32>();
    let cq_ring_size = params.cq_off.cqes as usize
        + params.cq_entries as usize * std::mem::size_of::<IoUringCqe>();
    let sqes_size = params.sq_entries as usize * std::mem::size_of::<IoUringSqe>();

    let sq_ring = mmap_ring(ring_fd, sq_ring_size, IORING_OFF_SQ_RING)?;
    let cq_ring = mmap_ring(ring_fd, cq_ring_size, IORING_OFF_CQ_RING)?;
    let sqes = mmap_ring(ring_fd, sqes_size, IORING_OFF_SQES)?;

    let sq = unsafe { SqRing::new(sq_ring, &params.sq_off) };
    let cq = unsafe { CqRing::new(cq_ring, &params.cq_off) };

    let sqes_base = sqes as *mut IoUringSqe;

    let mut pipe_fds = [0i32; 2];
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let read_fd = pipe_fds[0];
    let write_fd = pipe_fds[1];

    let mut buf = vec![0u8; 256];

    let sqe = unsafe { sqes_base.add(0) };
    unsafe {
        ptr::write_bytes(sqe, 0, 1);
        (*sqe).opcode = IORING_OP_READ;
        (*sqe).fd = read_fd;
        (*sqe).off = u64::MAX;
        (*sqe).addr = buf.as_mut_ptr() as u64;
        (*sqe).len = buf.len() as u32;
        (*sqe).user_data = 0x01;
    }

    thread::spawn(move || {
        thread::sleep(Duration::from_secs(1));
        let data = b"hello from pipe";
        unsafe {
            libc::write(write_fd, data.as_ptr() as *const libc::c_void, data.len());
        }
    });

    start_timer(Duration::from_secs(3));

    let mut in_flight: Option<Vec<u8>> = None;
    submit(ring_fd, &sq, 0, buf, &mut in_flight)?;

    let mut account_name = buf;
    account_name[..5].copy_from_slice(b"alice");
    println!(
        "account_name: {}",
        String::from_utf8_lossy(&account_name[..15])
    );

    println!("starting work...");
    loop {
        do_work();

        if WORK_COUNT.load(Ordering::Relaxed) == 20 {
            println!(
                "account_name: {}",
                String::from_utf8_lossy(&account_name[..15])
            );
        }
        if let Some((cqe, _buf)) = peek_completion(&cq, &mut in_flight)? {
            println!("received message: {} bytes", cqe.res);
        }
    }
}
