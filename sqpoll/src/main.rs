use std::ffi::CString;
use std::io;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};

// https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/include/uapi/linux/io_uring.h#L544-L546
const IORING_OFF_SQ_RING: libc::off_t = 0;
const IORING_OFF_CQ_RING: libc::off_t = 0x8000000;
const IORING_OFF_SQES: libc::off_t = 0x10000000;

// https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/include/uapi/linux/io_uring.h#L276
const IORING_OP_READ: u8 = 22;

// https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/include/uapi/linux/io_uring.h#L173
const IORING_SETUP_SQPOLL: u32 = 1 << 1;

// https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/include/uapi/linux/io_uring.h#L569
const IORING_SQ_NEED_WAKEUP: u32 = 1 << 0;

// https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/include/uapi/linux/io_uring.h#L596
const IORING_ENTER_SQ_WAKEUP: libc::c_uint = 1 << 1;

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
    flags: *const AtomicU32,
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
                flags: ring.add(off.flags as usize) as *const AtomicU32,
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

fn reap_cqes(cq: &CqRing, want: u32) -> Vec<IoUringCqe> {
    let mut cqes = Vec::with_capacity(want as usize);
    let mut head = unsafe { (*cq.head).load(Ordering::Relaxed) };

    while (cqes.len() as u32) < want {
        let tail = unsafe { (*cq.tail).load(Ordering::Acquire) };
        if head == tail {
            std::hint::spin_loop();
            continue;
        }
        while head != tail {
            let cqe = unsafe { ptr::read(cq.cqes.add((head & cq.ring_mask) as usize)) };
            cqes.push(cqe);
            head = head.wrapping_add(1);
        }
        unsafe { (*cq.head).store(head, Ordering::Release) };
    }

    cqes
}

fn main() -> io::Result<()> {
    let mut params = IoUringParams::default();
    params.flags |= IORING_SETUP_SQPOLL;

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

    let path = CString::new("hello.txt").unwrap();
    let file_fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
    if file_fd < 0 {
        return Err(io::Error::last_os_error());
    }

    let offsets: [u64; 3] = [0, 4, 8];
    let mut bufs: Vec<Vec<u8>> = (0..offsets.len()).map(|_| vec![0u8; 1]).collect();

    for i in 0..offsets.len() {
        let sqe = unsafe { sqes_base.add(i) };
        unsafe {
            ptr::write_bytes(sqe, 0, 1);
            (*sqe).opcode = IORING_OP_READ;
            (*sqe).fd = file_fd;
            (*sqe).off = offsets[i];
            (*sqe).addr = bufs[i].as_mut_ptr() as u64;
            (*sqe).len = bufs[i].len() as u32;
            (*sqe).user_data = i as u64;
        }
    }

    let want = offsets.len() as u32;
    let tail = unsafe { (*sq.tail).load(Ordering::Relaxed) };
    for i in 0..want {
        unsafe {
            ptr::write(sq.array.add(((tail + i) & sq.ring_mask) as usize), i);
        }
    }
    unsafe {
        (*sq.tail).store(tail.wrapping_add(want), Ordering::Release);
    }

    let flags = unsafe { (*sq.flags).load(Ordering::Acquire) };
    if flags & IORING_SQ_NEED_WAKEUP != 0 {
        let ret = unsafe { io_uring_enter(ring_fd, 0, 0, IORING_ENTER_SQ_WAKEUP) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
    }

    let cqes = reap_cqes(&cq, want);
    for cqe in &cqes {
        if cqe.res < 0 {
            return Err(io::Error::from_raw_os_error(-cqe.res));
        }
        let i = cqe.user_data as usize;
        let n = cqe.res as usize;
        println!(
            "offset {}: {}",
            offsets[i],
            String::from_utf8_lossy(&bufs[i][..n])
        );
    }

    Ok(())
}
