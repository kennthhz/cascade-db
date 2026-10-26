//! T1.0 spike: prove the async substrate glommio 0.9 works on this host before
//! any boot code is written on top of it.
//!
//! Two questions the plan demands answers for (plan §4 T1.0):
//! 1. Can we build a one-CPU-pinned executor (`Placement::Fixed`) — the
//!    worker-thread = pinned-core primitive behind spec §2?
//! 2. Can we adopt a `std::net::TcpListener`-accepted fd into a glommio
//!    `TcpStream` — the §5.1 housekeeper-accepts/worker-adopts handoff?
//!
//! If either fails on this kernel, R3 (glommio) must be revisited before T1.1.

use std::{
    io::Write,
    os::unix::io::{FromRawFd, IntoRawFd},
};

use futures_lite::AsyncReadExt;

/// CPUs the calling thread may run on, straight from the kernel.
fn current_affinity() -> Vec<u32> {
    unsafe {
        let mut mask: libc::cpu_set_t = std::mem::zeroed();
        let got = libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut mask);
        assert_eq!(got, 0, "sched_getaffinity failed");
        (0..(std::mem::size_of::<libc::cpu_set_t>() * 8))
            .filter(|&cpu| libc::CPU_ISSET(cpu, &mask))
            .map(|cpu| cpu as u32)
            .collect()
    }
}

#[test]
fn hello_executor_pinned() {
    let online = glommio::CpuSet::online().expect("read online CPUs");
    let target = (&online).into_iter().next().expect("at least one CPU").cpu;

    let ex = glommio::LocalExecutorBuilder::new(glommio::Placement::Fixed(target))
        .name("spike-hello")
        .make()
        .expect("executor builds");

    let seen: Vec<u32> = ex.run(async {
        // pid 0 = calling thread; the executor thread must be pinned to `target`.
        current_affinity()
    });

    assert_eq!(
        seen,
        vec![target as u32],
        "executor thread must run on core {target} only"
    );
}

#[test]
fn fd_adoption_smoke() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let mut client = std::net::TcpStream::connect(addr).expect("connect");
    let (server, _) = listener.accept().expect("accept");
    client.write_all(b"ping").expect("client write");

    let ex = glommio::LocalExecutorBuilder::new(glommio::Placement::Unbound)
        .name("spike-adopt")
        .make()
        .expect("executor builds");

    let fd = server.into_raw_fd();
    let buf: [u8; 4] = ex.run(async move {
        // SAFETY: `fd` is exclusively owned (from `into_raw_fd`) and not yet
        // adopted by any executor — the T1.5 handoff discipline.
        let mut stream = unsafe { glommio::net::TcpStream::from_raw_fd(fd) };
        let mut buf = [0u8; 4];
        stream
            .read_exact(&mut buf)
            .await
            .expect("read from adopted fd");
        buf
    });

    assert_eq!(
        &buf, b"ping",
        "bytes written by the std client must arrive on the adopted fd"
    );
}
