use std::{
    cell::RefCell,
    collections::VecDeque,
    future::poll_fn,
    io::{self, Read as _, Write as _},
    net::SocketAddr,
    os::fd::{AsRawFd, RawFd},
    pin::Pin,
    sync::{Arc, Mutex},
    task,
};

thread_local! {
    static EPOLL: RefCell<Option<Arc<Epoll>>> = const { RefCell::new(None) };
}

#[repr(transparent)]
pub struct Event(libc::epoll_event);

impl Event {
    pub fn data(&self) -> *const () {
        self.0.u64 as *const ()
    }
}

pub struct Epoll {
    epoll_fd: RawFd,
}

impl Epoll {
    pub fn new() -> Result<Self, io::Error> {
        let fd = unsafe { libc::epoll_create1(0) };
        if fd == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { epoll_fd: fd })
    }

    pub fn register(&self, fd: RawFd, data: *const ()) -> Result<(), io::Error> {
        let mut event = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLOUT | libc::EPOLLET) as u32,
            u64: data as u64,
        };
        let ret = unsafe { libc::epoll_ctl(self.epoll_fd, libc::EPOLL_CTL_ADD, fd, &mut event) };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn deregister(&self, fd: RawFd) -> Result<(), io::Error> {
        let ret = unsafe {
            libc::epoll_ctl(self.epoll_fd, libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut())
        };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn wait(&self, events: &mut Vec<Event>) -> Result<(), io::Error> {
        events.resize_with(events.capacity().min(100), || {
            Event(libc::epoll_event { events: 0, u64: 0 })
        });
        let events_ptr = events.as_mut_ptr() as *mut libc::epoll_event;
        let ret = unsafe { libc::epoll_wait(self.epoll_fd, events_ptr, events.len() as i32, -1) };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }
        events.truncate(ret as usize);
        Ok(())
    }
}

pub struct PollEvented<E: AsRawFd> {
    inner: E,
    waker: Arc<Mutex<Option<task::Waker>>>,
}

impl<E: AsRawFd> PollEvented<E> {
    pub fn new(inner: E) -> Result<Self, io::Error> {
        let waker = Arc::new(Mutex::new(None));
        EPOLL.with_borrow(|poll| {
            let poll = poll.as_ref().unwrap();
            poll.register(inner.as_raw_fd(), Arc::as_ptr(&waker) as *const ())?;
            Ok::<(), io::Error>(())
        })?;
        Ok(Self { inner, waker })
    }

    pub fn register_waker(&self, waker: task::Waker) {
        let mut guard = self.waker.lock().unwrap();
        *guard = Some(waker);
    }
}

impl<E: AsRawFd> Drop for PollEvented<E> {
    fn drop(&mut self) {
        EPOLL.with_borrow(|poll| {
            let poll = poll.as_ref().unwrap();
            let _ = poll.deregister(self.inner.as_raw_fd()).ok();
        });
    }
}

struct Task {
    future: Mutex<Pin<Box<dyn Future<Output = ()> + Send + Sync + 'static>>>,
    handle: Handle,
}

impl std::task::Wake for Task {
    fn wake(self: Arc<Self>) {
        self.handle.schedule(self.clone());
    }
}

#[derive(Clone)]
struct Handle {
    core: Arc<Mutex<Core>>,
}

impl Handle {
    pub fn schedule(&self, task: Arc<Task>) {
        self.core.lock().unwrap().schedule(task);
    }

    pub fn spawn<F>(&self, fut: F)
    where
        F: Future<Output = ()> + Send + Sync + 'static,
    {
        let task = Arc::new(Task {
            future: Mutex::new(Box::pin(fut)),
            handle: Handle {
                core: self.core.clone(),
            },
        });
        self.core.lock().unwrap().schedule(task);
    }
}

struct Core {
    run_queue: VecDeque<Arc<Task>>,
}

impl Core {
    pub fn schedule(&mut self, task: Arc<Task>) {
        self.run_queue.push_back(task);
    }

    pub fn next_task(&mut self) -> Option<Arc<Task>> {
        self.run_queue.pop_front()
    }
}

struct Runtime {
    core: Arc<Mutex<Core>>,
    epoll: Arc<Epoll>,
}

impl Runtime {
    pub fn new() -> Result<Self, io::Error> {
        let epoll = Arc::new(Epoll::new()?);
        EPOLL.with_borrow_mut(|poll| {
            *poll = Some(epoll.clone());
        });
        Ok(Self {
            core: Arc::new(Mutex::new(Core {
                run_queue: VecDeque::new(),
            })),
            epoll,
        })
    }

    pub fn spawn<F>(&self, fut: F)
    where
        F: Future<Output = ()> + Send + Sync + 'static,
    {
        let task = Arc::new(Task {
            future: Mutex::new(Box::pin(fut)),
            handle: Handle {
                core: self.core.clone(),
            },
        });
        self.core.lock().unwrap().schedule(task);
    }

    pub fn run(self) -> Result<(), io::Error> {
        let mut events = Vec::with_capacity(100);
        loop {
            while let Some(task) = { self.core.lock().unwrap().next_task() } {
                let waker = task::Waker::from(task.clone());
                let mut cx = task::Context::from_waker(&waker);
                let mut future = task.future.lock().unwrap();
                let _ = future.as_mut().poll(&mut cx);
            }

            self.epoll.wait(&mut events)?;
            for event in &events {
                let data = event.data();
                if data.is_null() {
                    continue;
                }
                let waker = unsafe { &*(data as *const Mutex<Option<task::Waker>>) };
                if let Some(waker) = waker.lock().unwrap().take() {
                    waker.wake();
                }
            }

            if self.core.lock().unwrap().run_queue.is_empty() {
                return Ok(());
            }
        }
    }

    pub fn handle(&self) -> Handle {
        Handle {
            core: self.core.clone(),
        }
    }
}

pub struct TcpListener {
    inner: PollEvented<std::net::TcpListener>,
}

impl TcpListener {
    pub fn from_std(inner: std::net::TcpListener) -> Result<Self, io::Error> {
        inner.set_nonblocking(true)?;
        Ok(Self {
            inner: PollEvented::new(inner)?,
        })
    }

    pub fn accept(&self) -> impl Future<Output = io::Result<(TcpStream, SocketAddr)>> {
        std::future::poll_fn(|cx| match self.inner.inner.accept() {
            Ok((stream, addr)) => {
                task::Poll::Ready(TcpStream::from_std(stream).map(|stream| (stream, addr)))
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.inner.register_waker(cx.waker().clone());
                task::Poll::Pending
            }
            Err(e) => task::Poll::Ready(Err(e)),
        })
    }
}

pub struct TcpStream {
    inner: PollEvented<std::net::TcpStream>,
}

impl TcpStream {
    pub fn from_std(inner: std::net::TcpStream) -> Result<Self, io::Error> {
        inner.set_nonblocking(true)?;
        Ok(Self {
            inner: PollEvented::new(inner)?,
        })
    }

    pub fn read<'a>(
        &'a mut self,
        buf: &'a mut [u8],
    ) -> impl Future<Output = io::Result<usize>> + 'a {
        poll_fn(move |cx| match self.inner.inner.read(buf) {
            Ok(n) => task::Poll::Ready(Ok(n)),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.inner.register_waker(cx.waker().clone());
                task::Poll::Pending
            }
            Err(e) => task::Poll::Ready(Err(e)),
        })
    }

    pub fn write<'a>(&'a mut self, buf: &'a [u8]) -> impl Future<Output = io::Result<usize>> + 'a {
        poll_fn(move |cx| match self.inner.inner.write(buf) {
            Ok(n) => task::Poll::Ready(Ok(n)),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.inner.register_waker(cx.waker().clone());
                task::Poll::Pending
            }
            Err(e) => task::Poll::Ready(Err(e)),
        })
    }
}

fn main() {
    let rt = Runtime::new().unwrap();
    let handle = rt.handle();
    rt.spawn(async move {
        let listener =
            TcpListener::from_std(std::net::TcpListener::bind("0.0.0.0:8000").unwrap()).unwrap();

        while let Ok((client, addr)) = listener.accept().await {
            println!("Accepted connection from {}", addr);
            handle.spawn(async move {
                let mut client = client;
                let mut buf = [0u8; 1024];
                loop {
                    match client.read(&mut buf).await {
                        Ok(0) => {
                            println!("Connection closed by {}", addr);
                            break;
                        }
                        Ok(n) => {
                            if let Err(e) = client.write(&buf[..n]).await {
                                eprintln!("Failed to write to {}: {}", addr, e);
                                break;
                            }
                        }
                        Err(e) => {
                            eprintln!("Failed to read from {}: {}", addr, e);
                            break;
                        }
                    }
                }
            });
        }
    });
    rt.run().unwrap();
}
