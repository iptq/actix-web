use std::cell::{RefCell, UnsafeCell};
use std::marker::PhantomData;
use std::rc::Rc;

use bytes::Bytes;
use futures::{
    task::{current as current_task, Task},
    Async, Future, Poll, Stream,
};
use http::{
    header::{self, HeaderMap, HeaderName, HeaderValue},
    HttpTryFrom,
};

use error::{MultipartError, ParseError, PayloadError};
use payload::PayloadBuffer;

const MAX_HEADERS: usize = 32;

/// The server-side implementation of `multipart/form-data` requests.
///
/// `S` is generic over some Stream of Bytes.
pub struct Multipart<S> {
    safety: Safety,
    error: Option<MultipartError>,
    inner: Option<Rc<RefCell<InnerMultipart<S>>>>,
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
    pub fn new(boundary: Result<String, MultipartError>, stream: S) -> Self {
        match boundary {
            Ok(boundary) => Multipart {
                error: None,
                safety: Safety::new(),
                inner: Some(Rc::new(RefCell::new(InnerMultipart {
                    boundary,
                    payload: PayloadRef::new(PayloadBuffer::new(stream)),
                    state: InnerState::FirstBoundary,
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

enum InnerState {
    EOF,
    /// Looking for the first boundary, ignoring everything until it.
    FirstBoundary,
    /// Looking for headers for the current item.
    Headers,
    /// Looking for the boundary at the end of the current item.
    Boundary(HeaderMap),
}

struct InnerMultipart<S> {
    boundary: String,
    payload: PayloadRef<S>,
    state: InnerState,
}

impl<S> InnerMultipart<S>
where
    S: Stream<Item = Bytes, Error = PayloadError>,
{
    fn poll(
        &mut self, safety: &Safety,
    ) -> Poll<Option<MultipartItem<S>>, MultipartError> {
        let mut delimiter = b"\r\n--".to_vec();
        delimiter.extend_from_slice(self.boundary.as_bytes());

        match self.state {
            InnerState::EOF => Ok(Async::Ready(None)),
            InnerState::FirstBoundary => self
                .payload
                .get_mut(safety)
                .map_or_else(
                    || Ok(Async::NotReady),
                    |payload| payload.read_until(&delimiter),
                )
                .map_err(|err| err.into())
                .and_then(|_| {
                    self.state = InnerState::Headers;
                    self.poll(safety)
                }),
            InnerState::Headers => self
                .payload
                .get_mut(safety)
                .map_or_else(
                    || Ok(Async::NotReady),
                    |payload| payload.read_until(b"\r\n\r\n"),
                )
                .map_err(|err| err.into())
                .map(|fut| {
                    fut.map(|opt| {
                        opt.map_or_else(
                            || Err(MultipartError::Incomplete),
                            |bytes| {
                                let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
                                httparse::parse_headers(&bytes, &mut headers)
                                    .map_err(|err| ParseError::from(err).into())
                            },
                        )
                    })
                    .map(|res| match res {
                        Ok(httparse::Status::Complete((_, headers))) => {
                            let headers = headers
                                .iter()
                                .filter_map(|header| {
                                    HeaderName::try_from(header.name).ok().and_then(
                                        |name| {
                                            HeaderValue::try_from(header.value)
                                                .ok()
                                                .map(|value| (name, value))
                                        },
                                    )
                                })
                                .collect::<HeaderMap<_>>();
                            self.state = InnerState::Boundary(headers);
                            self.poll(safety)
                        }
                        Ok(httparse::Status::Partial) => Err(ParseError::Header.into()),
                        Err(err) => Err(err),
                    })
                }),
            InnerState::Boundary(_) => unimplemented!(),
        }
    }
}

pub enum MultipartItem<S> {
    Field(Field<S>),
    Nested(Multipart<S>),
}

#[derive(Debug)]
pub struct Field<S> {
    a: ::std::marker::PhantomData<S>,
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
            }))
            .unwrap();
    }
}
