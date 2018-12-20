use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Instant;
use std::{cmp, io, mem};

use bytes::{Buf, Bytes};
use futures::{Async, Future, Poll, Stream};
use http2::server::{self, Connection, Handshake, SendResponse};
use http2::{Reason, RecvStream};
use modhttp::request::Parts;
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_timer::Delay;

use error::{Error, PayloadError};
use extensions::Extensions;
use http::{StatusCode, Version};
use payload::{Payload, PayloadStatus, PayloadWriter};
use uri::Url;

use super::error::{HttpDispatchError, ServerError};
use super::h2writer::H2Writer;
use super::input::PayloadType;
use super::settings::ServiceConfig;
use super::{HttpHandler, HttpHandlerTask, IoStream, Writer};

bitflags! {
    struct Flags: u8 {
        const DISCONNECTED = 0b0000_0001;
        const SHUTDOWN = 0b0000_0010;
    }
}

/// HTTP/2 Transport
pub(crate) struct Http2<T, H>
where
    T: AsyncRead + AsyncWrite + 'static,
    H: HttpHandler + 'static,
{
    flags: Flags,
    settings: ServiceConfig<H>,
    addr: Option<SocketAddr>,
    state: State<IoWrapper<T>>,
    tasks: VecDeque<Entry<H>>,
    extensions: Option<Rc<Extensions>>,
    ka_expire: Instant,
    ka_timer: Option<Delay>,
}

enum State<T: AsyncRead + AsyncWrite> {
    Handshake(Handshake<T, Bytes>),
    Connection(Connection<T, Bytes>),
    Empty,
}

impl<T, H> Http2<T, H>
where
    T: IoStream + 'static,
    H: HttpHandler + 'static,
{
    pub fn new(
        settings: ServiceConfig<H>, io: T, buf: Bytes, keepalive_timer: Option<Delay>,
    ) -> Self {
        let addr = io.peer_addr();
        let extensions = io.extensions();

        // keep-alive timeout
        let (ka_expire, ka_timer) = if let Some(delay) = keepalive_timer {
            (delay.deadline(), Some(delay))
        } else if let Some(delay) = settings.keep_alive_timer() {
            (delay.deadline(), Some(delay))
        } else {
            (settings.now(), None)
        };

        Http2 {
            flags: Flags::empty(),
            tasks: VecDeque::new(),
            state: State::Handshake(server::handshake(IoWrapper {
                unread: if buf.is_empty() { None } else { Some(buf) },
                inner: io,
            })),
            addr,
            settings,
            extensions,
            ka_expire,
            ka_timer,
        }
    }

    pub fn poll(&mut self) -> Poll<(), HttpDispatchError> {
        self.poll_keepalive()?;

        // server
        if let State::Connection(ref mut conn) = self.state {
            loop {
                // shutdown connection
                if self.flags.contains(Flags::SHUTDOWN) {
                    return conn.poll_close().map_err(|e| e.into());
                }

                let mut not_ready = true;
                let disconnected = self.flags.contains(Flags::DISCONNECTED);

                // check in-flight connections
                for item in &mut self.tasks {
                    // read payload
                    if !disconnected {
                        item.poll_payload();
                    }

                    if !item.flags.contains(EntryFlags::EOF) {
                        if disconnected {
                            item.flags.insert(EntryFlags::EOF);
                        } else {
                            let retry = item.payload.need_read() == PayloadStatus::Read;
                            loop {
                                match item.task.poll_io(&mut item.stream) {
                                    Ok(Async::Ready(ready)) => {
                                        if ready {
                                            item.flags.insert(
                                                EntryFlags::EOF | EntryFlags::FINISHED,
                                            );
                                        } else {
                                            item.flags.insert(EntryFlags::EOF);
                                        }
                                        not_ready = false;
                                    }
                                    Ok(Async::NotReady) => {
                                        if item.payload.need_read()
                                            == PayloadStatus::Read
                                            && !retry
                                        {
                                            continue;
                                        }
                                    }
                                    Err(err) => {
                                        error!("Unhandled error: {}", err);
                                        item.flags.insert(
                                            EntryFlags::EOF
                                                | EntryFlags::ERROR
                                                | EntryFlags::WRITE_DONE,
                                        );
                                        item.stream.reset(Reason::INTERNAL_ERROR);
                                    }
                                }
                                break;
                            }
                        }
                    }

                    if item.flags.contains(EntryFlags::EOF)
                        && !item.flags.contains(EntryFlags::FINISHED)
                    {
                        match item.task.poll_completed() {
                            Ok(Async::NotReady) => (),
                            Ok(Async::Ready(_)) => {
                                item.flags.insert(
                                    EntryFlags::FINISHED | EntryFlags::WRITE_DONE,
                                );
                            }
                            Err(err) => {
                                item.flags.insert(
                                    EntryFlags::ERROR
                                        | EntryFlags::WRITE_DONE
                                        | EntryFlags::FINISHED,
                                );
                                error!("Unhandled error: {}", err);
                            }
                        }
                    }

                    if item.flags.contains(EntryFlags::FINISHED)
                        && !item.flags.contains(EntryFlags::WRITE_DONE)
                        && !disconnected
                    {
                        match item.stream.poll_completed(false) {
                            Ok(Async::NotReady) => (),
                            Ok(Async::Ready(_)) => {
                                not_ready = false;
                                item.flags.insert(EntryFlags::WRITE_DONE);
                            }
                            Err(_) => {
                                item.flags.insert(EntryFlags::ERROR);
                            }
                        }
                    }
                }

                // cleanup finished tasks
                while !self.tasks.is_empty() {
                    if self.tasks[0].flags.contains(EntryFlags::FINISHED)
                        && self.tasks[0].flags.contains(EntryFlags::WRITE_DONE)
                        || self.tasks[0].flags.contains(EntryFlags::ERROR)
                    {
                        self.tasks.pop_front();
                    } else {
                        break;
                    }
                }

                // get request
                if !self.flags.contains(Flags::DISCONNECTED) {
                    match conn.poll() {
                        Ok(Async::Ready(None)) => {
                            not_ready = false;
                            self.flags.insert(Flags::DISCONNECTED);
                            for entry in &mut self.tasks {
                                entry.task.disconnected()
                            }
                        }
                        Ok(Async::Ready(Some((req, resp)))) => {
                            not_ready = false;
                            let (parts, body) = req.into_parts();

                            // update keep-alive expire
                            if self.ka_timer.is_some() {
                                if let Some(expire) = self.settings.keep_alive_expire() {
                                    self.ka_expire = expire;
                                }
                            }

                            self.tasks.push_back(Entry::new(
                                parts,
                                body,
                                resp,
                                self.addr,
                                self.settings.clone(),
                                self.extensions.clone(),
                            ));
                        }
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Err(err) => {
                            trace!("Connection error: {}", err);
                            self.flags.insert(Flags::SHUTDOWN);
                            for entry in &mut self.tasks {
                                entry.task.disconnected()
                            }
                            continue;
                        }
                    }
                }

                if not_ready {
                    if self.tasks.is_empty() && self.flags.contains(Flags::DISCONNECTED)
                    {
                        return conn.poll_close().map_err(|e| e.into());
                    } else {
                        return Ok(Async::NotReady);
                    }
                }
            }
        }

        // handshake
        self.state = if let State::Handshake(ref mut handshake) = self.state {
            match handshake.poll() {
                Ok(Async::Ready(conn)) => State::Connection(conn),
                Ok(Async::NotReady) => return Ok(Async::NotReady),
                Err(err) => {
                    trace!("Error handling connection: {}", err);
                    return Err(err.into());
                }
            }
        } else {
            mem::replace(&mut self.state, State::Empty)
        };

        self.poll()
    }

    /// keep-alive timer. returns `true` is keep-alive, otherwise drop
    fn poll_keepalive(&mut self) -> Result<(), HttpDispatchError> {
        if let Some(ref mut timer) = self.ka_timer {
            match timer.poll() {
                Ok(Async::Ready(_)) => {
                    // if we get timer during shutdown, just drop connection
                    if self.flags.contains(Flags::SHUTDOWN) {
                        return Err(HttpDispatchError::ShutdownTimeout);
                    }
                    if timer.deadline() >= self.ka_expire {
                        // check for any outstanding request handling
                        if self.tasks.is_empty() {
                            return Err(HttpDispatchError::ShutdownTimeout);
                        } else if let Some(dl) = self.settings.keep_alive_expire() {
                            timer.reset(dl);
                            let _ = timer.poll();
                        }
                    } else {
                        timer.reset(self.ka_expire);
                        let _ = timer.poll();
                    }
                }
                Ok(Async::NotReady) => (),
                Err(e) => {
                    error!("Timer error {:?}", e);
                    return Err(HttpDispatchError::Unknown);
                }
            }
        }

        Ok(())
    }
}

bitflags! {
    struct EntryFlags: u8 {
        const EOF = 0b0000_0001;
        const REOF = 0b0000_0010;
        const ERROR = 0b0000_0100;
        const FINISHED = 0b0000_1000;
        const WRITE_DONE = 0b0001_0000;
    }
}

enum EntryPipe<H: HttpHandler> {
    Task(H::Task),
    Error(Box<HttpHandlerTask>),
}

impl<H: HttpHandler> EntryPipe<H> {
    fn disconnected(&mut self) {
        match *self {
            EntryPipe::Task(ref mut task) => task.disconnected(),
            EntryPipe::Error(ref mut task) => task.disconnected(),
        }
    }
    fn poll_io(&mut self, io: &mut Writer) -> Poll<bool, Error> {
        match *self {
            EntryPipe::Task(ref mut task) => task.poll_io(io),
            EntryPipe::Error(ref mut task) => task.poll_io(io),
        }
    }
    fn poll_completed(&mut self) -> Poll<(), Error> {
        match *self {
            EntryPipe::Task(ref mut task) => task.poll_completed(),
            EntryPipe::Error(ref mut task) => task.poll_completed(),
        }
    }
}

struct Entry<H: HttpHandler + 'static> {
    task: EntryPipe<H>,
    payload: PayloadType,
    recv: RecvStream,
    stream: H2Writer<H>,
    flags: EntryFlags,
}

impl<H: HttpHandler + 'static> Entry<H> {
    fn new(
        parts: Parts, recv: RecvStream, resp: SendResponse<Bytes>,
        addr: Option<SocketAddr>, settings: ServiceConfig<H>,
        extensions: Option<Rc<Extensions>>,
    ) -> Entry<H>
    where
        H: HttpHandler + 'static,
    {
        // Payload and Content-Encoding
        let (psender, payload) = Payload::new(false);

        let mut msg = settings.get_request();
        {
            let inner = msg.inner_mut();
            inner.url = Url::new(parts.uri);
            inner.method = parts.method;
            inner.version = parts.version;
            inner.headers = parts.headers;
            inner.stream_extensions = extensions;
            *inner.payload.borrow_mut() = Some(payload);
            inner.addr = addr;
        }

        // Payload sender
        let psender = PayloadType::new(msg.headers(), psender);

        // start request processing
        let task = match settings.handler().handle(msg) {
            Ok(task) => EntryPipe::Task(task),
            Err(_) => EntryPipe::Error(ServerError::err(
                Version::HTTP_2,
                StatusCode::NOT_FOUND,
            )),
        };

        Entry {
            task,
            recv,
            payload: psender,
            stream: H2Writer::new(resp, settings),
            flags: EntryFlags::empty(),
        }
    }

    fn poll_payload(&mut self) {
        while !self.flags.contains(EntryFlags::REOF)
            && self.payload.need_read() == PayloadStatus::Read
        {
            match self.recv.poll() {
                Ok(Async::Ready(Some(chunk))) => {
                    let l = chunk.len();
                    self.payload.feed_data(chunk);
                    if let Err(err) = self.recv.release_capacity().release_capacity(l) {
                        self.payload.set_error(PayloadError::Http2(err));
                        break;
                    }
                }
                Ok(Async::Ready(None)) => {
                    self.flags.insert(EntryFlags::REOF);
                    self.payload.feed_eof();
                }
                Ok(Async::NotReady) => break,
                Err(err) => {
                    self.payload.set_error(PayloadError::Http2(err));
                    break;
                }
            }
        }
    }
}

struct IoWrapper<T> {
    unread: Option<Bytes>,
    inner: T,
}

impl<T: Read> Read for IoWrapper<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(mut bytes) = self.unread.take() {
            let size = cmp::min(buf.len(), bytes.len());
            buf[..size].copy_from_slice(&bytes[..size]);
            if bytes.len() > size {
                bytes.split_to(size);
                self.unread = Some(bytes);
            }
            Ok(size)
        } else {
            self.inner.read(buf)
        }
    }
}

impl<T: Write> Write for IoWrapper<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<T: AsyncRead + 'static> AsyncRead for IoWrapper<T> {
    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [u8]) -> bool {
        self.inner.prepare_uninitialized_buffer(buf)
    }
}

impl<T: AsyncWrite + 'static> AsyncWrite for IoWrapper<T> {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        self.inner.shutdown()
    }
    fn write_buf<B: Buf>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
        self.inner.write_buf(buf)
    }
}
