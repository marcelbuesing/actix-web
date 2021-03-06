#![cfg_attr(feature = "cargo-clippy", allow(redundant_field_names))]

use bytes::{Bytes, BytesMut};
use futures::{Async, Poll};
use http2::server::SendResponse;
use http2::{Reason, SendStream};
use modhttp::Response;
use std::rc::Rc;
use std::{cmp, io};

use http::header::{HeaderValue, CONNECTION, CONTENT_LENGTH, DATE, TRANSFER_ENCODING};
use http::{HttpTryFrom, Method, Version};

use super::helpers;
use super::message::Request;
use super::output::{Output, ResponseInfo};
use super::settings::WorkerSettings;
use super::{Writer, WriterState, MAX_WRITE_BUFFER_SIZE};
use body::{Binary, Body};
use header::ContentEncoding;
use httpresponse::HttpResponse;

const CHUNK_SIZE: usize = 16_384;

bitflags! {
    struct Flags: u8 {
        const STARTED = 0b0000_0001;
        const DISCONNECTED = 0b0000_0010;
        const EOF = 0b0000_0100;
        const RESERVED = 0b0000_1000;
    }
}

pub(crate) struct H2Writer<H: 'static> {
    respond: SendResponse<Bytes>,
    stream: Option<SendStream<Bytes>>,
    flags: Flags,
    written: u64,
    buffer: Output,
    buffer_capacity: usize,
    settings: Rc<WorkerSettings<H>>,
}

impl<H: 'static> H2Writer<H> {
    pub fn new(
        respond: SendResponse<Bytes>, settings: Rc<WorkerSettings<H>>,
    ) -> H2Writer<H> {
        H2Writer {
            stream: None,
            flags: Flags::empty(),
            written: 0,
            buffer: Output::Buffer(settings.get_bytes()),
            buffer_capacity: 0,
            respond,
            settings,
        }
    }

    pub fn reset(&mut self, reason: Reason) {
        if let Some(mut stream) = self.stream.take() {
            stream.send_reset(reason)
        }
    }
}

impl<H: 'static> Drop for H2Writer<H> {
    fn drop(&mut self) {
        self.settings.release_bytes(self.buffer.take());
    }
}

impl<H: 'static> Writer for H2Writer<H> {
    fn written(&self) -> u64 {
        self.written
    }

    #[inline]
    fn set_date(&mut self) {
        self.settings.set_date(self.buffer.as_mut(), true)
    }

    #[inline]
    fn buffer(&mut self) -> &mut BytesMut {
        self.buffer.as_mut()
    }

    fn start(
        &mut self, req: &Request, msg: &mut HttpResponse, encoding: ContentEncoding,
    ) -> io::Result<WriterState> {
        // prepare response
        self.flags.insert(Flags::STARTED);
        let mut info = ResponseInfo::new(req.inner.method == Method::HEAD);
        self.buffer.for_server(&mut info, &req.inner, msg, encoding);

        // http2 specific
        msg.headers_mut().remove(CONNECTION);
        msg.headers_mut().remove(TRANSFER_ENCODING);

        // using helpers::date is quite a lot faster
        if !msg.headers().contains_key(DATE) {
            let mut bytes = BytesMut::with_capacity(29);
            self.settings.set_date(&mut bytes, false);
            msg.headers_mut()
                .insert(DATE, HeaderValue::try_from(bytes.freeze()).unwrap());
        }

        let body = msg.replace_body(Body::Empty);
        match body {
            Body::Binary(ref bytes) => {
                if bytes.is_empty() {
                    msg.headers_mut()
                        .insert(CONTENT_LENGTH, HeaderValue::from_static("0"));
                    self.flags.insert(Flags::EOF);
                } else {
                    let mut val = BytesMut::new();
                    helpers::convert_usize(bytes.len(), &mut val);
                    let l = val.len();
                    msg.headers_mut().insert(
                        CONTENT_LENGTH,
                        HeaderValue::try_from(val.split_to(l - 2).freeze()).unwrap(),
                    );
                }
            }
            Body::Empty => {
                self.flags.insert(Flags::EOF);
                msg.headers_mut()
                    .insert(CONTENT_LENGTH, HeaderValue::from_static("0"));
            }
            _ => (),
        }

        let mut resp = Response::new(());
        *resp.status_mut() = msg.status();
        *resp.version_mut() = Version::HTTP_2;
        for (key, value) in msg.headers().iter() {
            resp.headers_mut().insert(key, value.clone());
        }

        match self
            .respond
            .send_response(resp, self.flags.contains(Flags::EOF))
        {
            Ok(stream) => self.stream = Some(stream),
            Err(_) => return Err(io::Error::new(io::ErrorKind::Other, "err")),
        }

        trace!("Response: {:?}", msg);

        if let Body::Binary(bytes) = body {
            if bytes.is_empty() {
                Ok(WriterState::Done)
            } else {
                self.flags.insert(Flags::EOF);
                self.written = bytes.len() as u64;
                self.buffer.write(bytes.as_ref())?;
                if let Some(ref mut stream) = self.stream {
                    self.flags.insert(Flags::RESERVED);
                    stream.reserve_capacity(cmp::min(self.buffer.len(), CHUNK_SIZE));
                }
                Ok(WriterState::Pause)
            }
        } else {
            msg.replace_body(body);
            self.buffer_capacity = msg.write_buffer_capacity();
            Ok(WriterState::Done)
        }
    }

    fn write(&mut self, payload: &Binary) -> io::Result<WriterState> {
        self.written = payload.len() as u64;

        if !self.flags.contains(Flags::DISCONNECTED) {
            if self.flags.contains(Flags::STARTED) {
                // TODO: add warning, write after EOF
                self.buffer.write(payload.as_ref())?;
            } else {
                // might be response for EXCEPT
                error!("Not supported");
            }
        }

        if self.buffer.len() > self.buffer_capacity {
            Ok(WriterState::Pause)
        } else {
            Ok(WriterState::Done)
        }
    }

    fn write_eof(&mut self) -> io::Result<WriterState> {
        self.flags.insert(Flags::EOF);
        if !self.buffer.write_eof()? {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "Last payload item, but eof is not reached",
            ))
        } else if self.buffer.len() > MAX_WRITE_BUFFER_SIZE {
            Ok(WriterState::Pause)
        } else {
            Ok(WriterState::Done)
        }
    }

    fn poll_completed(&mut self, _shutdown: bool) -> Poll<(), io::Error> {
        if !self.flags.contains(Flags::STARTED) {
            return Ok(Async::NotReady);
        }

        if let Some(ref mut stream) = self.stream {
            // reserve capacity
            if !self.flags.contains(Flags::RESERVED) && !self.buffer.is_empty() {
                self.flags.insert(Flags::RESERVED);
                stream.reserve_capacity(cmp::min(self.buffer.len(), CHUNK_SIZE));
            }

            loop {
                match stream.poll_capacity() {
                    Ok(Async::NotReady) => return Ok(Async::NotReady),
                    Ok(Async::Ready(None)) => return Ok(Async::Ready(())),
                    Ok(Async::Ready(Some(cap))) => {
                        let len = self.buffer.len();
                        let bytes = self.buffer.split_to(cmp::min(cap, len));
                        let eof =
                            self.buffer.is_empty() && self.flags.contains(Flags::EOF);
                        self.written += bytes.len() as u64;

                        if let Err(e) = stream.send_data(bytes.freeze(), eof) {
                            return Err(io::Error::new(io::ErrorKind::Other, e));
                        } else if !self.buffer.is_empty() {
                            let cap = cmp::min(self.buffer.len(), CHUNK_SIZE);
                            stream.reserve_capacity(cap);
                        } else {
                            self.flags.remove(Flags::RESERVED);
                            return Ok(Async::NotReady);
                        }
                    }
                    Err(e) => return Err(io::Error::new(io::ErrorKind::Other, e)),
                }
            }
        }
        Ok(Async::NotReady)
    }
}
