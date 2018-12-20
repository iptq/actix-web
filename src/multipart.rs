//! Multipart requests support
use std::cell::{RefCell, UnsafeCell};
use std::marker::PhantomData;
use std::rc::Rc;
use std::{cmp, fmt};

use bytes::Bytes;
use futures::task::{current as current_task, Task};
use futures::{Async, Poll, Stream};
use http::header::{self, ContentDisposition, HeaderMap, HeaderName, HeaderValue};
use http::HttpTryFrom;
use httparse;
use mime;

use error::{MultipartError, ParseError, PayloadError};
use payload::PayloadBuffer;

const MAX_HEADERS: usize = 32;

/// The server-side implementation of `multipart/form-data` requests.
///
/// This will parse the incoming stream into `MultipartItem` instances via its
/// Stream implementation.
/// `MultipartItem::Field` contains multipart field. `MultipartItem::Multipart`
/// is used for nested multipart streams.
pub struct Multipart<S> {
    safety: Safety,
    error: Option<MultipartError>,
    inner: Option<Rc<RefCell<InnerMultipart<S>>>>,
}

///
pub enum MultipartItem<S> {
    /// Multipart field
    Field(Field<S>),
    /// Nested multipart stream
    Nested(Multipart<S>),
}

enum InnerMultipartItem<S> {
    None,
    Field(Rc<RefCell<InnerField<S>>>),
    Multipart(Rc<RefCell<InnerMultipart<S>>>),
}

#[derive(PartialEq, Debug)]
enum InnerState {
    /// Stream eof
    Eof,
    /// Skip data until first boundary
    FirstBoundary,
    /// Reading boundary
    Boundary,
    /// Reading Headers,
    Headers,
}

struct InnerMultipart<S> {
    payload: PayloadRef<S>,
    boundary: String,
    state: InnerState,
    item: InnerMultipartItem<S>,
}

impl Multipart<()> {
    /// Extract boundary info from headers.
    pub fn boundary(headers: &HeaderMap) -> Result<String, MultipartError> {
        Ok(headers
            .get(header::CONTENT_TYPE)
            .ok_or_else(|| MultipartError::NoContentType)?
            .to_str()
            .map_err(|_| MultipartError::ParseContentType)?
            .parse::<mime::Mime>()
            .map_err(|_| MultipartError::ParseContentType)?
            .get_param(mime::BOUNDARY)
            .ok_or_else(|| MultipartError::MissingBoundary)?
            .as_str()
            .to_owned())
    }
}

impl<S> Multipart<S>
where
    S: Stream<Item = Bytes, Error = PayloadError>,
{
    /// Create multipart instance for boundary.
    pub fn new(boundary: Result<String, MultipartError>, stream: S) -> Multipart<S> {
        match boundary {
            Ok(boundary) => Multipart {
                error: None,
                safety: Safety::new(),
                inner: Some(Rc::new(RefCell::new(InnerMultipart {
                    boundary,
                    payload: PayloadRef::new(PayloadBuffer::new(stream)),
                    state: InnerState::FirstBoundary,
                    item: InnerMultipartItem::None,
                }))),
            },
            Err(err) => Multipart {
                error: Some(err),
                safety: Safety::new(),
                inner: None,
            },
        }
    }
}

impl<S> Stream for Multipart<S>
where
    S: Stream<Item = Bytes, Error = PayloadError>,
{
    type Item = MultipartItem<S>;
    type Error = MultipartError;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        if let Some(err) = self.error.take() {
            Err(err)
        } else if self.safety.current() {
            self.inner.as_mut().unwrap().borrow_mut().poll(&self.safety)
        } else {
            Ok(Async::NotReady)
        }
    }
}

impl<S> InnerMultipart<S>
where
    S: Stream<Item = Bytes, Error = PayloadError>,
{
    /// Read headers from the stream, until `b"\r\n\r\n"` is reached.
    fn read_headers(payload: &mut PayloadBuffer<S>) -> Poll<HeaderMap, MultipartError> {
        match payload.read_until(b"\r\n\r\n")? {
            Async::NotReady => Ok(Async::NotReady),
            Async::Ready(None) => Err(MultipartError::Incomplete),
            Async::Ready(Some(bytes)) => {
                let mut hdrs = [httparse::EMPTY_HEADER; MAX_HEADERS];
                match httparse::parse_headers(&bytes, &mut hdrs) {
                    Ok(httparse::Status::Complete((_, hdrs))) => {
                        // convert headers
                        let mut headers = HeaderMap::with_capacity(hdrs.len());
                        for h in hdrs {
                            if let Ok(name) = HeaderName::try_from(h.name) {
                                if let Ok(value) = HeaderValue::try_from(h.value) {
                                    headers.append(name, value);
                                } else {
                                    return Err(ParseError::Header.into());
                                }
                            } else {
                                return Err(ParseError::Header.into());
                            }
                        }
                        Ok(Async::Ready(headers))
                    }
                    Ok(httparse::Status::Partial) => Err(ParseError::Header.into()),
                    Err(err) => Err(ParseError::from(err).into()),
                }
            }
        }
    }

    fn read_boundary(
        payload: &mut PayloadBuffer<S>,
        boundary: &str,
    ) -> Poll<bool, MultipartError> {
        // TODO: need to read epilogue
        let mut delimiter = b"\r\n--".to_vec();
        delimiter.extend_from_slice(boundary.as_bytes());
        match payload.read_until(&delimiter)? {
            Async::NotReady => Ok(Async::NotReady),
            Async::Ready(None) => Err(MultipartError::Incomplete),
            Async::Ready(Some(chunk)) => {
                eprintln!("chunk is: {:?}", chunk);
                Ok(Async::Ready(chunk.starts_with(b"--")))
                // if chunk.len() == boundary.len() + 4
                //     && &chunk[..2] == b"--"
                //     && &chunk[2..boundary.len() + 2] == boundary.as_bytes()
                // {
                //     Ok(Async::Ready(false))
                // } else if chunk.len() == boundary.len() + 6
                //     && &chunk[..2] == b"--"
                //     && &chunk[2..boundary.len() + 2] == boundary.as_bytes()
                //     && &chunk[boundary.len() + 2..boundary.len() + 4] == b"--"
                // {
                //     Ok(Async::Ready(true))
                // } else {
                //     eprintln!("error boundary: chunk={:?}", chunk);
                //     Err(MultipartError::MissingBoundary)
                // }
            }
        }
    }

    fn skip_until_boundary(
        payload: &mut PayloadBuffer<S>,
        boundary: &str,
    ) -> Poll<bool, MultipartError> {
        let mut eof = false;
        loop {
            match payload.readline()? {
                Async::Ready(Some(chunk)) => {
                    if chunk.is_empty() {
                        //ValueError("Could not find starting boundary %r"
                        //% (self._boundary))
                    }
                    if chunk.len() < boundary.len() {
                        continue;
                    }
                    if &chunk[..2] == b"--"
                        && &chunk[2..chunk.len() - 2] == boundary.as_bytes()
                    {
                        break;
                    } else {
                        if chunk.len() < boundary.len() + 2 {
                            continue;
                        }
                        let b: &[u8] = boundary.as_ref();
                        if &chunk[..boundary.len()] == b
                            && &chunk[boundary.len()..boundary.len() + 2] == b"--"
                        {
                            eof = true;
                            break;
                        }
                    }
                }
                Async::NotReady => return Ok(Async::NotReady),
                Async::Ready(None) => return Err(MultipartError::Incomplete),
            }
        }
        Ok(Async::Ready(eof))
    }

    fn poll(
        &mut self,
        safety: &Safety,
    ) -> Poll<Option<MultipartItem<S>>, MultipartError> {
        eprintln!("POLLING: Current state = {:?}", self.state);
        if self.state == InnerState::Eof {
            Ok(Async::Ready(None))
        } else {
            // release field
            loop {
                eprintln!("LOOPING");
                // Nested multipart streams of fields has to be consumed
                // before switching to next
                if safety.current() {
                    let stop = match self.item {
                        InnerMultipartItem::Field(ref mut field) => {
                            match field.borrow_mut().poll(safety)? {
                                Async::NotReady => return Ok(Async::NotReady),
                                Async::Ready(Some(_)) => continue,
                                Async::Ready(None) => true,
                            }
                        }
                        InnerMultipartItem::Multipart(ref mut multipart) => {
                            match multipart.borrow_mut().poll(safety)? {
                                Async::NotReady => return Ok(Async::NotReady),
                                Async::Ready(Some(_)) => continue,
                                Async::Ready(None) => true,
                            }
                        }
                        _ => false,
                    };
                    if stop {
                        self.item = InnerMultipartItem::None;
                    }
                    if let InnerMultipartItem::None = self.item {
                        break;
                    }
                }
            }

            let headers = if let Some(payload) = self.payload.get_mut(safety) {
                match self.state {
                    // read until first boundary
                    InnerState::FirstBoundary => {
                        match InnerMultipart::skip_until_boundary(
                            payload,
                            &self.boundary,
                        )? {
                            Async::Ready(eof) => {
                                if eof {
                                    self.state = InnerState::Eof;
                                    return Ok(Async::Ready(None));
                                } else {
                                    self.state = InnerState::Headers;
                                }
                            }
                            Async::NotReady => return Ok(Async::NotReady),
                        }
                    }
                    // read boundary
                    InnerState::Boundary => {
                        match InnerMultipart::read_boundary(payload, &self.boundary)? {
                            Async::NotReady => return Ok(Async::NotReady),
                            Async::Ready(eof) => {
                                if eof {
                                    self.state = InnerState::Eof;
                                    return Ok(Async::Ready(None));
                                } else {
                                    self.state = InnerState::Headers;
                                }
                            }
                        }
                    }
                    _ => (),
                }

                // read field headers for next field
                if self.state == InnerState::Headers {
                    if let Async::Ready(headers) = InnerMultipart::read_headers(payload)?
                    {
                        eprintln!("Headers: {:?}", headers);
                        self.state = InnerState::Boundary;
                        headers
                    } else {
                        eprintln!("aaaaaa");
                        return Ok(Async::NotReady);
                    }
                } else {
                    unreachable!()
                }
            } else {
                debug!("NotReady: field is in flight");
                return Ok(Async::NotReady);
            };

            // content type
            let mut mt = mime::APPLICATION_OCTET_STREAM;
            if let Some(content_type) = headers.get(header::CONTENT_TYPE) {
                if let Ok(content_type) = content_type.to_str() {
                    if let Ok(ct) = content_type.parse::<mime::Mime>() {
                        mt = ct;
                    }
                }
            }

            eprintln!("reached A, mt = {:?}", mt);
            self.state = InnerState::Boundary;

            // nested multipart stream
            if mt.type_() == mime::MULTIPART {
                let inner = if let Some(boundary) = mt.get_param(mime::BOUNDARY) {
                    Rc::new(RefCell::new(InnerMultipart {
                        payload: self.payload.clone(),
                        boundary: boundary.as_str().to_owned(),
                        state: InnerState::FirstBoundary,
                        item: InnerMultipartItem::None,
                    }))
                } else {
                    eprintln!("error boundary2: {:?}", mt);
                    return Err(MultipartError::MissingBoundary);
                };

                self.item = InnerMultipartItem::Multipart(Rc::clone(&inner));

                Ok(Async::Ready(Some(MultipartItem::Nested(Multipart {
                    safety: safety.clone(),
                    error: None,
                    inner: Some(inner),
                }))))
            } else {
                eprintln!("reached B");
                let field = Rc::new(RefCell::new(InnerField::new(
                    self.payload.clone(),
                    self.boundary.clone(),
                    &headers,
                )?));
                self.item = InnerMultipartItem::Field(Rc::clone(&field));

                Ok(Async::Ready(Some(MultipartItem::Field(Field::new(
                    safety.clone(),
                    headers,
                    mt,
                    field,
                )))))
            }
        }
    }
}

impl<S> Drop for InnerMultipart<S> {
    fn drop(&mut self) {
        // InnerMultipartItem::Field has to be dropped first because of Safety.
        self.item = InnerMultipartItem::None;
    }
}

/// A single field in a multipart stream
pub struct Field<S> {
    ct: mime::Mime,
    headers: HeaderMap,
    inner: Rc<RefCell<InnerField<S>>>,
    safety: Safety,
}

impl<S> Field<S>
where
    S: Stream<Item = Bytes, Error = PayloadError>,
{
    fn new(
        safety: Safety,
        headers: HeaderMap,
        ct: mime::Mime,
        inner: Rc<RefCell<InnerField<S>>>,
    ) -> Self {
        Field {
            ct,
            headers,
            inner,
            safety,
        }
    }

    /// Get a map of headers
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Get the content type of the field
    pub fn content_type(&self) -> &mime::Mime {
        &self.ct
    }

    /// Get the content disposition of the field, if it exists
    pub fn content_disposition(&self) -> Option<ContentDisposition> {
        // RFC 7578: 'Each part MUST contain a Content-Disposition header field
        // where the disposition type is "form-data".'
        if let Some(content_disposition) =
            self.headers.get(::http::header::CONTENT_DISPOSITION)
        {
            ContentDisposition::from_raw(content_disposition).ok()
        } else {
            None
        }
    }
}

impl<S> Stream for Field<S>
where
    S: Stream<Item = Bytes, Error = PayloadError>,
{
    type Item = Bytes;
    type Error = MultipartError;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        if self.safety.current() {
            self.inner.borrow_mut().poll(&self.safety)
        } else {
            Ok(Async::NotReady)
        }
    }
}

impl<S> fmt::Debug for Field<S> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "\nMultipartField: {}", self.ct)?;
        writeln!(f, "  boundary: {}", self.inner.borrow().boundary)?;
        writeln!(f, "  headers:")?;
        for (key, val) in self.headers.iter() {
            writeln!(f, "    {:?}: {:?}", key, val)?;
        }
        Ok(())
    }
}

struct InnerField<S> {
    payload: Option<PayloadRef<S>>,
    boundary: String,
    eof: bool,
    length: Option<u64>,
}

impl<S> InnerField<S>
where
    S: Stream<Item = Bytes, Error = PayloadError>,
{
    fn new(
        payload: PayloadRef<S>,
        boundary: String,
        headers: &HeaderMap,
    ) -> Result<InnerField<S>, PayloadError> {
        let len = if let Some(len) = headers.get(header::CONTENT_LENGTH) {
            if let Ok(s) = len.to_str() {
                if let Ok(len) = s.parse::<u64>() {
                    Some(len)
                } else {
                    return Err(PayloadError::Incomplete);
                }
            } else {
                return Err(PayloadError::Incomplete);
            }
        } else {
            None
        };

        Ok(InnerField {
            boundary,
            payload: Some(payload),
            eof: false,
            length: len,
        })
    }

    /// Reads body part content chunk of the specified size.
    /// The body part must has `Content-Length` header with proper value.
    #[allow(dead_code)]
    fn read_len(
        payload: &mut PayloadBuffer<S>,
        size: &mut u64,
    ) -> Poll<Option<Bytes>, MultipartError> {
        if *size == 0 {
            Ok(Async::Ready(None))
        } else {
            match payload.readany() {
                Ok(Async::NotReady) => Ok(Async::NotReady),
                Ok(Async::Ready(None)) => Err(MultipartError::Incomplete),
                Ok(Async::Ready(Some(mut chunk))) => {
                    let len = cmp::min(chunk.len() as u64, *size);
                    *size -= len;
                    let ch = chunk.split_to(len as usize);
                    if !chunk.is_empty() {
                        payload.unprocessed(chunk);
                    }
                    Ok(Async::Ready(Some(ch)))
                }
                Err(err) => Err(err.into()),
            }
        }
    }

    /// Reads content chunk of body part with unknown length.
    /// The `Content-Length` header for body part is not necessary.
    fn read_stream(
        payload: &mut PayloadBuffer<S>,
        boundary: &str,
    ) -> Poll<Option<Bytes>, MultipartError> {
        let mut delimiter = b"\r\n--".to_vec();
        delimiter.extend_from_slice(boundary.as_bytes());

        match payload.read_until(&delimiter)? {
            Async::NotReady => Ok(Async::NotReady),
            Async::Ready(None) => Err(MultipartError::Incomplete),
            Async::Ready(Some(mut chunk)) => {
                // take off the delimiter that was found
                let len = chunk.len() - delimiter.len();
                if len == 0 {
                    Ok(Async::Ready(None))
                } else {
                    let ch = chunk.split_to(len);
                    payload.unprocessed(chunk);
                    Ok(Async::Ready(Some(ch)))
                }
            }
        }

        // match payload.read_until(b"\r")? {
        //     Async::NotReady => Ok(Async::NotReady),
        //     Async::Ready(None) => Err(MultipartError::Incomplete),
        //     Async::Ready(Some(mut chunk)) => {
        //         if chunk.len() == 1 {
        //             payload.unprocessed(chunk);

        //             // what the fuck??
        //             match payload.read_exact(boundary.len() + 4)? {
        //                 Async::NotReady => Ok(Async::NotReady),
        //                 Async::Ready(None) => Err(MultipartError::Incomplete),
        //                 Async::Ready(Some(mut chunk)) => {
        //                     if &chunk[..2] == b"\r\n"
        //                         && &chunk[2..4] == b"--"
        //                         && &chunk[4..] == boundary.as_bytes()
        //                     {
        //                         payload.unprocessed(chunk);
        //                         Ok(Async::Ready(None))
        //                     } else {
        //                         // \r might be part of data stream
        //                         let ch = chunk.split_to(1);
        //                         payload.unprocessed(chunk);
        //                         Ok(Async::Ready(Some(ch)))
        //                     }
        //                 }
        //             }
        //         } else {
        //             let to = chunk.len() - 1;
        //             let ch = chunk.split_to(to);
        //             payload.unprocessed(chunk);
        //             Ok(Async::Ready(Some(ch)))
        //         }
        //     }
        // }
    }

    fn poll(&mut self, s: &Safety) -> Poll<Option<Bytes>, MultipartError> {
        if self.payload.is_none() {
            return Ok(Async::Ready(None));
        }

        let result = if let Some(payload) = self.payload.as_ref().unwrap().get_mut(s) {
            // let res = if let Some(ref mut len) = self.length {
            //     InnerField::read_len(payload, len)?
            // } else {
            //     InnerField::read_stream(payload, &self.boundary)?
            // };

            // RFC doesn't say anything about length, so always read stream here
            let res = InnerField::read_stream(payload, &self.boundary)?;

            match res {
                Async::NotReady => Async::NotReady,
                Async::Ready(Some(bytes)) => Async::Ready(Some(bytes)),
                Async::Ready(None) => {
                    self.eof = true;
                    match payload.readline()? {
                        Async::NotReady => Async::NotReady,
                        Async::Ready(None) => Async::Ready(None),
                        Async::Ready(Some(line)) => {
                            if line.as_ref() != b"\r\n" {
                                warn!("multipart field did not read all the data or it is malformed");
                            }
                            Async::Ready(None)
                        }
                    }
                }
            }
        } else {
            Async::NotReady
        };

        if Async::Ready(None) == result {
            self.payload.take();
        }
        Ok(result)
    }
}

struct PayloadRef<S> {
    payload: Rc<UnsafeCell<PayloadBuffer<S>>>,
}

impl<S> PayloadRef<S>
where
    S: Stream<Item = Bytes, Error = PayloadError>,
{
    fn new(payload: PayloadBuffer<S>) -> PayloadRef<S> {
        PayloadRef {
            payload: Rc::new(payload.into()),
        }
    }

    fn get_mut<'a, 'b>(&'a self, s: &'b Safety) -> Option<&'a mut PayloadBuffer<S>>
    where
        'a: 'b,
    {
        // Unsafe: Invariant is inforced by Safety Safety is used as ref counter,
        // only top most ref can have mutable access to payload.
        if s.current() {
            let payload: &mut PayloadBuffer<S> = unsafe { &mut *self.payload.get() };
            Some(payload)
        } else {
            None
        }
    }
}

impl<S> Clone for PayloadRef<S> {
    fn clone(&self) -> PayloadRef<S> {
        PayloadRef {
            payload: Rc::clone(&self.payload),
        }
    }
}

/// Counter. It tracks of number of clones of payloads and give access to
/// payload only to top most task panics if Safety get destroyed and it not top
/// most task.
#[derive(Debug)]
struct Safety {
    task: Option<Task>,
    level: usize,
    payload: Rc<PhantomData<bool>>,
}

impl Safety {
    fn new() -> Safety {
        let payload = Rc::new(PhantomData);
        Safety {
            task: None,
            level: Rc::strong_count(&payload),
            payload,
        }
    }

    fn current(&self) -> bool {
        Rc::strong_count(&self.payload) == self.level
    }
}

impl Clone for Safety {
    fn clone(&self) -> Safety {
        let payload = Rc::clone(&self.payload);
        Safety {
            task: Some(current_task()),
            level: Rc::strong_count(&payload),
            payload,
        }
    }
}

impl Drop for Safety {
    fn drop(&mut self) {
        // parent task is dead
        if Rc::strong_count(&self.payload) != self.level {
            panic!("Safety get dropped but it is not from top-most task");
        }
        if let Some(task) = self.task.take() {
            task.notify()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::future::{lazy, result};
    use payload::{Payload, PayloadWriter};
    use tokio::runtime::current_thread::Runtime;

    #[test]
    fn test_boundary() {
        let headers = HeaderMap::new();
        match Multipart::boundary(&headers) {
            Err(MultipartError::NoContentType) => (),
            _ => unreachable!("should not happen"),
        }

        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("test"),
        );

        match Multipart::boundary(&headers) {
            Err(MultipartError::ParseContentType) => (),
            _ => unreachable!("should not happen"),
        }

        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("multipart/mixed"),
        );
        match Multipart::boundary(&headers) {
            Err(MultipartError::MissingBoundary) => (),
            _ => unreachable!("should not happen"),
        }

        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static(
                "multipart/mixed; boundary=\"5c02368e880e436dab70ed54e1c58209\"",
            ),
        );

        assert_eq!(
            Multipart::boundary(&headers).unwrap(),
            "5c02368e880e436dab70ed54e1c58209"
        );
    }

    #[test]
    fn test_multipart() {
        Runtime::new()
            .unwrap()
            .block_on(lazy(|| {
                let (mut sender, payload) = Payload::new(false);

                let bytes = Bytes::from(
                    "testasdadsad\r\n--abbc761f78ff4d7cb7573b5a23f96ef0\r\nContent-Disposition: form-data; name=\"file\"; filename=\"fn.txt\"\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 4\r\n\r\ntest\r\n--abbc761f78ff4d7cb7573b5a23f96ef0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 5\r\n\r\ndataa\r\n--abbc761f78ff4d7cb7573b5a23f96ef0--\r\n",
                );
                sender.feed_data(bytes);

                let mut multipart = Multipart::new(
                    Ok("abbc761f78ff4d7cb7573b5a23f96ef0".to_owned()),
                    payload,
                );
                match multipart.poll() {
                    Ok(Async::Ready(Some(item))) => match item {
                        MultipartItem::Field(mut field) => {
                            {
                                use http::header::{DispositionParam, DispositionType};
                                let cd = field.content_disposition().unwrap();
                                assert_eq!(cd.disposition, DispositionType::FormData);
                                assert_eq!(
                                    cd.parameters[0],
                                    DispositionParam::Name("file".into())
                                );
                            }
                            assert_eq!(field.content_type().type_(), mime::TEXT);
                            assert_eq!(field.content_type().subtype(), mime::PLAIN);

                            match field.poll() {
                                Ok(Async::Ready(Some(chunk))) => {
                                    assert_eq!(chunk, "test")
                                }
                                _ => unreachable!(),
                            }
                            match field.poll() {
                                Ok(Async::Ready(None)) => (),
                                _ => unreachable!(),
                            }
                        }
                        _ => unreachable!(),
                    },
                    _ => unreachable!(),
                }

                match multipart.poll() {
                    Ok(Async::Ready(Some(item))) => match item {
                        MultipartItem::Field(mut field) => {
                            assert_eq!(field.content_type().type_(), mime::TEXT);
                            assert_eq!(field.content_type().subtype(), mime::PLAIN);

                            match field.poll() {
                                Ok(Async::Ready(Some(chunk))) => {
                                    assert_eq!(chunk, "dataa")
                                }
                                _ => unreachable!(),
                            }
                            match field.poll() {
                                Ok(Async::Ready(None)) => (),
                                _ => unreachable!(),
                            }
                        }
                        _ => unreachable!(),
                    },
                    Ok(Async::Ready(None)) => unreachable!("ready(none)"),
                    Ok(Async::NotReady) => unreachable!("not ready"),
                    Err(err) => eprintln!("err: {:?}", err),
                }

                match multipart.poll() {
                    Ok(Async::Ready(None)) => (),
                    Err(err) => eprintln!("err2: {:?}", err),
                    _ => unreachable!(),
                }

                let res: Result<(), ()> = Ok(());
                result(res)
            })).unwrap();
    }

    #[test]
    fn rfc1341_appendix_c_example() {
        // source: https://tools.ietf.org/html/rfc1341#page-62
        Runtime::new()
            .unwrap()
            .block_on(lazy(|| {
                let (mut sender, payload) = Payload::new(false);
                let bytes = Bytes::from(
                    "
                    This is the preamble area of a multipart message.
                    Mail readers that understand multipart format
                    should ignore this preamble.
                    If you are reading this text, you might want to
                    consider changing to a mail reader that understands
                    how to properly display multipart messages.
                    \r\n--unique-boundary-1\r\n\
                    \r\n\
                    ...Some text appears here...
                    [Note that the preceding blank line means
                    no header fields were given and this is text,
                    with charset US ASCII.  It could have been
                    done with explicit typing as in the next part.]
                    \r\n--unique-boundary-1\r\n\
                    Content-type: text/plain; charset=US-ASCII\r\n\
                    \r\n\
                    This could have been part of the previous part,
                    but illustrates explicit versus implicit
                    typing of body parts.
                    \r\n--unique-boundary-1\r\n\
                    Content-Type: multipart/parallel; boundary=unique-boundary-2\r\n\
                    \r\n--unique-boundary-2\r\n\
                    Content-Type: audio/basic\r\n\
                    Content-Transfer-Encoding: base64\r\n\
                    \r\n\
                    ... base64-encoded 8000 Hz single-channel
                    u-law-format audio data goes here....
                    \r\n--unique-boundary-2\r\n\
                    Content-Type: image/gif\r\n\
                    Content-Transfer-Encoding: Base64\r\n\
                    \r\n\
                    ... base64-encoded image data goes here....
                    \r\n--unique-boundary-2--\r\n\
                    \r\n--unique-boundary-1\r\n\
                    Content-type: text/richtext\r\n\
                    \r\n\
                    This is <bold><italic>richtext.</italic></bold>
                    <nl><nl>Isn't it
                    <bigger><bigger>cool?</bigger></bigger>
                    \r\n--unique-boundary-1\r\n\
                    Content-Type: message/rfc822\r\n\
                    \r\n\
                    From: (name in US-ASCII)
                    Subject: (subject in US-ASCII)
                    Content-Type: Text/plain; charset=ISO-8859-1
                    Content-Transfer-Encoding: Quoted-printable

                    ... Additional text in ISO-8859-1 goes here ...
                    \r\n--unique-boundary-1--",
                );
                sender.feed_data(bytes);

                let mut multipart =
                    Multipart::new(Ok("unique-boundary-1".to_owned()), payload);

                loop {
                    match multipart.poll() {
                        Ok(Async::Ready(Some(item))) => match item {
                            MultipartItem::Field(mut field) => {
                                eprintln!("field: {:?}", field);

                                loop {
                                    match field.poll() {
                                        Ok(Async::Ready(Some(chunk))) => {
                                            eprintln!("data: {:?}", chunk)
                                        }
                                        Ok(Async::Ready(None)) => break,
                                        _ => unreachable!(),
                                    }
                                }
                            }
                            _ => unreachable!(),
                        },
                        Ok(Async::NotReady) => eprintln!("not ready"),
                        Ok(Async::Ready(None)) => break,
                        Err(err) => panic!("err: {:?}", err),
                    }
                }

                result(Ok::<_, ()>(()))
            })).unwrap();
    }
}
