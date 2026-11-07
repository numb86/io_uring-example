use std::ffi::CString;
use std::io;
use std::mem;
use std::os::fd::RawFd;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};

const SQ_ENTRIES: u32 = 8;

// https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/include/uapi/linux/io_uring.h#L544-L546
const IORING_OFF_SQ_RING: i64 = 0;
const IORING_OFF_CQ_RING: i64 = 0x8000000;
const IORING_OFF_SQES: i64 = 0x10000000;

// https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/include/uapi/linux/io_uring.h#L595
const IORING_ENTER_GETEVENTS: u32 = 1;

// https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/include/uapi/linux/io_uring.h#L623
const IORING_FEAT_SINGLE_MMAP: u32 = 1;

// https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/include/uapi/linux/io_uring.h#L276-L277
const IORING_OP_READ: u8 = 22;
const IORING_OP_WRITE: u8 = 23;

// https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/include/uapi/linux/io_uring.h#L144-L145
// https://github.com/torvalds/linux/blob/028ef9c96e96197026887c0f092424679298aae8/include/uapi/linux/io_uring.h#L158-L161
const IOSQE_IO_LINK: u8 = 1 << 2;
const IOSQE_IO_HARDLINK: u8 = 1 << 3;

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

const FILE_PATH: &str = "hello.txt";
const INITIAL_CONTENT: &[u8] = b"foo";
const WRITE_DATA: &[u8] = b"bar";

fn reset_file() -> io::Result<()> {
    std::fs::write(FILE_PATH, INITIAL_CONTENT)
}

fn write_sqe(fd: RawFd, data: &[u8], flags: u8) -> Sqe {
    let mut sqe = Sqe::zeroed();
    sqe.opcode = IORING_OP_WRITE;
    sqe.flags = flags;
    sqe.fd = fd;
    sqe.off = 0;
    sqe.addr = data.as_ptr() as u64;
    sqe.len = data.len() as u32;
    sqe
}

fn read_sqe(fd: RawFd, buf: &mut [u8], flags: u8) -> Sqe {
    let mut sqe = Sqe::zeroed();
    sqe.opcode = IORING_OP_READ;
    sqe.flags = flags;
    sqe.fd = fd;
    sqe.off = 0;
    sqe.addr = buf.as_mut_ptr() as u64;
    sqe.len = buf.len() as u32;
    sqe
}

// Submit the two SQEs in a single io_uring_enter and wait until both have
// completed. Returns [res of sqes[0], res of sqes[1]]. Each SQE's array index is
// stored in its user_data, and each CQE is matched back to its SQE through that
// index. Nothing here depends on when either operation runs or completes.
fn submit_and_wait_all(ring: &mut Uring, mut sqes: [Sqe; 2]) -> io::Result<[i32; 2]> {
    for (i, sqe) in sqes.iter_mut().enumerate() {
        sqe.user_data = i as u64;
        ring.push(*sqe)?;
    }

    let mut results = [None; 2];
    while results.iter().any(|r| r.is_none()) {
        ring.submit_and_wait()?;
        while let Some(cqe) = ring.pop() {
            results[cqe.user_data as usize] = Some(cqe.res);
        }
    }

    Ok(results.map(|r| r.unwrap()))
}

fn errno_name(res: i32) -> String {
    match -res {
        libc::EBADF => "EBADF".to_string(),
        libc::ECANCELED => "ECANCELED".to_string(),
        errno => format!("errno {}", errno),
    }
}

// Report a case whose SQEs are [write WRITE_DATA, read]: shows what the read got.
fn report_write_then_read(flags: &str, write_res: i32, read_res: i32, buf: &[u8]) {
    let label = format!(
        "SQEs [write {:?}, read], {}",
        String::from_utf8_lossy(WRITE_DATA),
        flags
    );
    if write_res < 0 {
        println!(
            "{} -> the write failed with {}",
            label,
            errno_name(write_res)
        );
        return;
    }
    if read_res < 0 {
        println!("{} -> the read failed with {}", label, errno_name(read_res));
        return;
    }
    println!(
        "{} -> the read got {:?}",
        label,
        String::from_utf8_lossy(&buf[..read_res as usize])
    );
}

// Describe the outcome of the read that comes after another SQE: whether it ran,
// and what it got if it did. ECANCELED means it was cancelled without running.
fn describe_read(read_res: i32, buf: &[u8]) -> String {
    if read_res == -libc::ECANCELED {
        return "did not run (ECANCELED)".to_string();
    }
    if read_res < 0 {
        return format!("failed with {}", errno_name(read_res));
    }
    format!(
        "ran and got {:?}",
        String::from_utf8_lossy(&buf[..read_res as usize])
    )
}

// Report a case whose SQEs are [failing read, read]: shows what the failing read
// got and the outcome of the read.
fn report_failing_read_then_read(flags: &str, failing_read_res: i32, read_res: i32, buf: &[u8]) {
    println!(
        "SQEs [failing read, read], {} -> the failing read got {}; the read {}",
        flags,
        errno_name(failing_read_res),
        describe_read(read_res, buf)
    );
}

// Report a case whose SQEs are [read of asked.len() bytes, read]: shows what that
// read got, and the outcome of the read.
fn report_sized_read_then_read(
    flags: &str,
    asked: &[u8],
    sized_read_res: i32,
    read_res: i32,
    buf: &[u8],
) {
    let got = if sized_read_res < 0 {
        errno_name(sized_read_res)
    } else {
        format!(
            "{:?}",
            String::from_utf8_lossy(&asked[..sized_read_res as usize])
        )
    };
    println!(
        "SQEs [read of {} bytes, read], {} -> the read of {} bytes got {}; the read {}",
        asked.len(),
        flags,
        asked.len(),
        got,
        describe_read(read_res, buf)
    );
}

fn main() -> io::Result<()> {
    let mut ring = Uring::new(SQ_ENTRIES)?;

    let path = CString::new(FILE_PATH).unwrap();
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        return Err(os_error());
    }

    // [write, read], no flags: nothing orders the two operations.
    reset_file()?;
    let mut buf = vec![0u8; 256];
    let [write_res, read_res] = submit_and_wait_all(
        &mut ring,
        [write_sqe(fd, WRITE_DATA, 0), read_sqe(fd, &mut buf, 0)],
    )?;
    report_write_then_read("no flags", write_res, read_res, &buf);

    // [write, read], IOSQE_IO_LINK on the write: the read does not start until the
    // write has completed.
    reset_file()?;
    let mut buf = vec![0u8; 256];
    let [write_res, read_res] = submit_and_wait_all(
        &mut ring,
        [
            write_sqe(fd, WRITE_DATA, IOSQE_IO_LINK),
            read_sqe(fd, &mut buf, 0),
        ],
    )?;
    report_write_then_read("IOSQE_IO_LINK", write_res, read_res, &buf);

    // [write, read], IOSQE_IO_HARDLINK on the write: same as IOSQE_IO_LINK.
    reset_file()?;
    let mut buf = vec![0u8; 256];
    let [write_res, read_res] = submit_and_wait_all(
        &mut ring,
        [
            write_sqe(fd, WRITE_DATA, IOSQE_IO_HARDLINK),
            read_sqe(fd, &mut buf, 0),
        ],
    )?;
    report_write_then_read("IOSQE_IO_HARDLINK", write_res, read_res, &buf);

    // [failing read, read], no flags: fd -1 is never valid, so the failing read gets
    // EBADF. The read is unaffected.
    reset_file()?;
    let mut unused = vec![0u8; 256];
    let mut buf = vec![0u8; 256];
    let [failing_read_res, read_res] = submit_and_wait_all(
        &mut ring,
        [read_sqe(-1, &mut unused, 0), read_sqe(fd, &mut buf, 0)],
    )?;
    report_failing_read_then_read("no flags", failing_read_res, read_res, &buf);

    // [failing read, read], IOSQE_IO_LINK on the failing read: the failure cancels
    // the read, which completes with ECANCELED without running.
    reset_file()?;
    let mut unused = vec![0u8; 256];
    let mut buf = vec![0u8; 256];
    let [failing_read_res, read_res] = submit_and_wait_all(
        &mut ring,
        [
            read_sqe(-1, &mut unused, IOSQE_IO_LINK),
            read_sqe(fd, &mut buf, 0),
        ],
    )?;
    report_failing_read_then_read("IOSQE_IO_LINK", failing_read_res, read_res, &buf);

    // [failing read, read], IOSQE_IO_HARDLINK on the failing read: the read still
    // runs, after the failing read has completed.
    reset_file()?;
    let mut unused = vec![0u8; 256];
    let mut buf = vec![0u8; 256];
    let [failing_read_res, read_res] = submit_and_wait_all(
        &mut ring,
        [
            read_sqe(-1, &mut unused, IOSQE_IO_HARDLINK),
            read_sqe(fd, &mut buf, 0),
        ],
    )?;
    report_failing_read_then_read("IOSQE_IO_HARDLINK", failing_read_res, read_res, &buf);

    // [read of 256 bytes, read], IOSQE_IO_LINK on the read of 256 bytes: its res is 3,
    // a positive number, but 3 is not the 256 it asked for. For a link that counts as
    // a failure, so the read is cancelled.
    reset_file()?;
    let mut asked = vec![0u8; 256];
    let mut buf = vec![0u8; 256];
    let [sized_read_res, read_res] = submit_and_wait_all(
        &mut ring,
        [
            read_sqe(fd, &mut asked, IOSQE_IO_LINK),
            read_sqe(fd, &mut buf, 0),
        ],
    )?;
    report_sized_read_then_read("IOSQE_IO_LINK", &asked, sized_read_res, read_res, &buf);

    // [read of 3 bytes, read], IOSQE_IO_LINK on the read of 3 bytes: it gets exactly
    // the 3 bytes it asked for, so the link holds and the read runs. What counts is
    // not how small the res is but whether it equals the length asked for.
    reset_file()?;
    let mut asked = vec![0u8; 3];
    let mut buf = vec![0u8; 256];
    let [sized_read_res, read_res] = submit_and_wait_all(
        &mut ring,
        [
            read_sqe(fd, &mut asked, IOSQE_IO_LINK),
            read_sqe(fd, &mut buf, 0),
        ],
    )?;
    report_sized_read_then_read("IOSQE_IO_LINK", &asked, sized_read_res, read_res, &buf);

    Ok(())
}
