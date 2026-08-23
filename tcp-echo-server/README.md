# TCP Echo Servers: io_uring vs epoll

A side-by-side comparison of the same TCP echo server implemented with io_uring and with epoll.

The io_uring version does not use liburing. It calls the `io_uring_setup` / `io_uring_enter` syscalls directly and builds the submission/completion rings by hand with `mmap`.

The two implementations are deliberately structured the same way — a `HashMap<fd, Conn>` for connection state, a recv → send → recv cycle, and the same partial-send handling — so that the remaining differences are exactly the differences between the two models.

## The two models

- **io_uring is a completion model.** The process asks the kernel to perform an operation (accept / recv / send) by submitting an SQE, and later receives a CQE telling it the operation has completed and with what result.
- **epoll is a readiness model.** The process asks the kernel to be notified when a socket becomes ready, and then performs the operation (accept / recv / send) itself.

## Directory layout

- [`io_uring-tcp-echo-server/`](io_uring-tcp-echo-server/) — io_uring version, listens on port 3456
- [`epoll-tcp-echo-server/`](epoll-tcp-echo-server/) — epoll version, listens on port 3457
- [`concurrent_echo_test.sh`](concurrent_echo_test.sh) — test script that opens many connections at once

## Requirements

- Linux. Neither server runs on macOS or other platforms.
- For the io_uring version, kernel 5.6 or later (`IORING_OP_SEND` / `IORING_OP_RECV` were added in 5.6).

## Running

Start either server (or both — they use different ports):

```sh
cd io_uring-tcp-echo-server && cargo run   # port 3456
cd epoll-tcp-echo-server && cargo run      # port 3457
```

Then talk to it manually:

```sh
nc localhost 3456
```

Anything you type is echoed back.

## Test script

```sh
./concurrent_echo_test.sh [PORT] [CLIENTS] [ROUNDS]
```

- `PORT` — port to connect to (default 3456)
- `CLIENTS` — number of concurrent connections (default 20)
- `ROUNDS` — echo round trips per connection (default 5)

The script opens all connections at the same time and verifies that every connection gets correct echoes across multiple round trips while the others stay open, forcing the server to handle many connections at once.

## Simplifications and non-goals

- Single-threaded; one buffer of fixed size (4 KiB) per connection.
- Echoes one `recv`'s worth of data at a time — no line buffering or framing.
- No timeouts and minimal error handling.
- This is not a benchmark. The goal is to understand the two I/O models, not to compare their performance.
