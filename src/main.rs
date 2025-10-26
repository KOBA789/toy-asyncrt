use std::{
    collections::VecDeque,
    future::poll_fn,
    io::{self, PipeReader, Read, Write as _},
    net::SocketAddr,
    os::fd::{AsRawFd, FromRawFd, RawFd},
    pin::Pin,
    sync::{Arc, Mutex},
    task,
};

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
    pub fn new() -> io::Result<Self> {
        let ret = unsafe { libc::epoll_create1(0) };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { epoll_fd: ret })
    }

    pub fn register(&self, fd: RawFd, data: *const ()) -> io::Result<()> {
        let mut event = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLOUT | libc::EPOLLET) as u32,
            u64: data as u64,
        };
        let ret = unsafe {
            libc::epoll_ctl(self.epoll_fd, libc::EPOLL_CTL_ADD, fd, &mut event as *mut _)
        };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn deregister(&self, fd: RawFd) -> io::Result<()> {
        let ret = unsafe {
            libc::epoll_ctl(self.epoll_fd, libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut())
        };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn wait(&self, events: &mut Vec<Event>) -> io::Result<()> {
        events.resize_with(events.capacity().min(100), || {
            Event(libc::epoll_event { events: 0, u64: 0 })
        });
        let ret = unsafe {
            libc::epoll_wait(
                self.epoll_fd,
                events.as_mut_ptr() as *mut _,
                events.len() as i32,
                -1,
            )
        };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }
        events.truncate(ret as usize);
        Ok(())
    }
}

pub struct PollEvented<T: AsRawFd> {
    inner: T, // TcpStream or TcpListener
    waker: Arc<Mutex<Option<task::Waker>>>,

    epoll: Arc<Epoll>,
}

impl<T: AsRawFd> PollEvented<T> {
    pub fn new(inner: T, epoll: Arc<Epoll>) -> io::Result<PollEvented<T>> {
        let waker = Arc::new(Mutex::new(None));
        let waker_ptr = Arc::as_ptr(&waker);

        epoll.register(inner.as_raw_fd(), waker_ptr as *const ())?;

        Ok(Self {
            inner,
            waker,
            epoll,
        })
    }

    pub fn register_waker(&self, waker: task::Waker) {
        self.waker.lock().unwrap().replace(waker);
    }
}

impl<T: AsRawFd> Drop for PollEvented<T> {
    fn drop(&mut self) {
        let _ = self.epoll.deregister(self.inner.as_raw_fd());
    }
}

pub struct TcpListener {
    inner: PollEvented<std::net::TcpListener>,
}

impl TcpListener {
    pub fn from_std(inner: std::net::TcpListener, epoll: Arc<Epoll>) -> io::Result<Self> {
        inner.set_nonblocking(true)?;
        let inner = PollEvented::new(inner, epoll)?;
        Ok(Self { inner })
    }

    pub fn accept(
        &self,
    ) -> impl Future<Output = io::Result<(TcpStream, SocketAddr)>> + Send + Sync {
        poll_fn(|cx| match self.inner.inner.accept() {
            Ok((std_peer, addr)) => {
                let epoll = self.inner.epoll.clone();
                task::Poll::Ready(
                    TcpStream::from_std(std_peer, epoll).map(|async_peer| (async_peer, addr)),
                )
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                // なにかあったら起こしてくれ
                self.inner.register_waker(cx.waker().clone());
                task::Poll::Pending
            }
            Err(err) => task::Poll::Ready(Err(err)),
        })
    }
}

pub struct TcpStream {
    inner: PollEvented<std::net::TcpStream>,
}

impl TcpStream {
    pub fn from_std(inner: std::net::TcpStream, epoll: Arc<Epoll>) -> io::Result<Self> {
        inner.set_nonblocking(true)?;
        let inner = PollEvented::new(inner, epoll)?;
        Ok(Self { inner })
    }

    pub fn read(
        &mut self,
        buf: &mut [u8],
    ) -> impl Future<Output = io::Result<usize>> + Send + Sync {
        poll_fn(|cx| match self.inner.inner.read(buf) {
            Ok(len) => task::Poll::Ready(Ok(len)),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                self.inner.register_waker(cx.waker().clone());
                task::Poll::Pending
            }
            Err(err) => task::Poll::Ready(Err(err)),
        })
    }

    pub fn write(
        &mut self,
        buf: &mut [u8],
    ) -> impl Future<Output = io::Result<usize>> + Send + Sync {
        poll_fn(|cx| match self.inner.inner.write(buf) {
            Ok(len) => task::Poll::Ready(Ok(len)),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                self.inner.register_waker(cx.waker().clone());
                task::Poll::Pending
            }
            Err(err) => task::Poll::Ready(Err(err)),
        })
    }
}

pub struct Sleep {
    inner: PollEvented<PipeReader>,
}

impl Sleep {
    pub fn new(sec: i64, epoll: Arc<Epoll>) -> io::Result<Self> {
        let ret = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_NONBLOCK) };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }
        let fd = ret;
        let itimerspec = libc::itimerspec {
            it_value: libc::timespec {
                tv_sec: sec,
                tv_nsec: 0,
            },
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
        };
        let ret = unsafe { libc::timerfd_settime(fd, 0, &itimerspec, std::ptr::null_mut()) };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }
        let reader = unsafe { PipeReader::from_raw_fd(fd) };
        Ok(Self {
            inner: PollEvented::new(reader, epoll)?,
        })
    }

    pub fn wait(&mut self) -> impl Future<Output = io::Result<()>> + Send + Sync {
        let mut buf = [0u8; 8];
        poll_fn(move |cx| match self.inner.inner.read(&mut buf) {
            Ok(_len) => task::Poll::Ready(Ok(())),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                self.inner.register_waker(cx.waker().clone());
                task::Poll::Pending
            }
            Err(err) => task::Poll::Ready(Err(err)),
        })
    }
}

pub struct Task {
    future: Mutex<Pin<Box<dyn Future<Output = ()> + Send + Sync + 'static>>>,
    handle: Handle,
}

impl task::Wake for Task {
    fn wake(self: Arc<Self>) {
        self.handle.schedule(self.clone());
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        println!("task dropped")
    }
}

pub struct RunQueue {
    queue: VecDeque<Arc<Task>>,
}

impl RunQueue {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }

    pub fn schedule(&mut self, task: Arc<Task>) {
        self.queue.push_back(task);
    }

    pub fn pop_next(&mut self) -> Option<Arc<Task>> {
        self.queue.pop_front()
    }
}

pub struct Runtime {
    run_queue: Arc<Mutex<RunQueue>>,
    epoll: Arc<Epoll>,
}

impl Runtime {
    pub fn new() -> io::Result<Self> {
        let epoll = Arc::new(Epoll::new()?);
        Ok(Self {
            run_queue: Arc::new(Mutex::new(RunQueue::new())),
            epoll,
        })
    }

    pub fn epoll(&self) -> &Arc<Epoll> {
        &self.epoll
    }

    pub fn spawn(&self, fut: impl Future<Output = ()> + Send + Sync + 'static) {
        let task = Task {
            future: Mutex::new(Box::pin(fut)),
            handle: self.handle(),
        };
        let task = Arc::new(task);
        self.schedule(task);
    }

    pub fn schedule(&self, task: Arc<Task>) {
        self.run_queue.lock().unwrap().schedule(task);
    }

    pub fn run(self) -> io::Result<()> {
        loop {
            while let Some(task) = { self.run_queue.lock().unwrap().pop_next() } {
                let waker = task::Waker::from(task.clone());
                let mut cx = task::Context::from_waker(&waker);
                let mut future = task.future.lock().unwrap();
                let _ = future.as_mut().poll(&mut cx);
            }

            let mut events = Vec::with_capacity(100);
            self.epoll.wait(&mut events)?;

            for event in events {
                let waker = unsafe { &*(event.data() as *const Mutex<Option<task::Waker>>) };
                if let Some(waker) = waker.lock().unwrap().take() {
                    waker.wake();
                }
            }
        }
    }

    pub fn handle(&self) -> Handle {
        Handle {
            run_queue: self.run_queue.clone(),
        }
    }

    pub fn block_on(self, fut: impl Future<Output = ()> + Send + Sync + 'static) -> io::Result<()> {
        self.spawn(fut);
        self.run()
    }
}

#[derive(Clone)]
pub struct Handle {
    run_queue: Arc<Mutex<RunQueue>>,
}

impl Handle {
    pub fn spawn(&self, fut: impl Future<Output = ()> + Send + Sync + 'static) {
        let task = Task {
            future: Mutex::new(Box::pin(fut)),
            handle: self.clone(),
        };
        let task = Arc::new(task);
        self.schedule(task);
    }

    pub fn schedule(&self, task: Arc<Task>) {
        self.run_queue.lock().unwrap().schedule(task);
    }
}

fn main() -> io::Result<()> {
    let rt = Runtime::new()?;
    let handle = rt.handle();
    let epoll = rt.epoll.clone();
    let epoll2 = rt.epoll.clone();
    rt.spawn(async move {
        loop {
            println!("tick");
            Sleep::new(1, epoll2.clone()).unwrap().wait().await.unwrap();
        }
    });

    rt.block_on(async move {
        let std_listener = std::net::TcpListener::bind("0.0.0.0:8000").unwrap();
        let async_listener = TcpListener::from_std(std_listener, epoll).unwrap();
        while let Ok((mut peer, addr)) = async_listener.accept().await {
            println!("connected from {:?}", addr);
            handle.spawn(async move {
                loop {
                    let mut buf = [0; 1024];
                    let len = peer.read(&mut buf).await.unwrap();
                    if len == 0 {
                        break;
                    }

                    let mut buf = &mut buf[..len];
                    while !buf.is_empty() {
                        let written = peer.write(buf).await.unwrap();
                        buf = &mut buf[written..];
                    }
                }
            });
        }
    })?;

    Ok(())
}
