use std::io;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

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

fn main() -> io::Result<()> {
    let mut pipe_fds = [0i32; 2];
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let read_fd = pipe_fds[0];
    let write_fd = pipe_fds[1];

    let mut buf = vec![0u8; 256];

    thread::spawn(move || {
        thread::sleep(Duration::from_secs(1));
        let data = b"hello from pipe";
        unsafe {
            libc::write(write_fd, data.as_ptr() as *const libc::c_void, data.len());
        }
    });

    start_timer(Duration::from_secs(3));

    let n = unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    println!("completed: {}", String::from_utf8_lossy(&buf[..n as usize]));

    println!("starting work...");
    loop {
        do_work();
    }
}
