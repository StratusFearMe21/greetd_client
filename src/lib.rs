//! # `greetd` IPC protocol library
//!
//! This library implements the [greetd](https://git.sr.ht/~kennylevinsen/greetd) IPC protocol.
//!
//! The library exposes a [Request](enum.Request.html) and a
//! [Response](enum.Response.html) enum, representing the valid protocol
//! messages. Furthermore, codec implementations are available to serialize
//! these to/from both sync and async readers/writers. The availability of
//! these are controlled by feature flags.
//!
//! Additional types are part of the different request and response values.
//!
//! See `agreety` for a simple example use of this library.
//!
//! # Format
//!
//! The message format is as follows:
//!
//! ```text
//! +----------+-------------------+
//! | len: u32 | JSON payload: str |
//! +----------+-------------------+
//! ```
//!
//! Length is in native byte-order. The JSON payload is a variant of the
//! Request or Response enums.
//!
//! # Request and response types
//!
//! See [Request](enum.Request.html) and [Response](enum.Response.html) for
//! information about the request and response types, as well as their
//! serialization.
//!

use calloop::{EventSource, Interest, PostAction, Token};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    fmt::{Debug, Display},
    io::{Read, Write},
    os::unix::{net::UnixStream, prelude::AsRawFd},
    rc::Rc,
    sync::atomic::AtomicBool,
};

use writeable::{LengthHint, Writeable};

pub struct Greetd {
    socket: Rc<RefCell<UnixStream>>,
    started_session: Rc<AtomicBool>,
    request_in_queue: Rc<AtomicBool>,
    finishing: Rc<AtomicBool>,
}

pub struct GreetdSource {
    socket: Rc<RefCell<UnixStream>>,
    request_in_queue: Rc<AtomicBool>,
    started_session: Rc<AtomicBool>,
    token: Token,
    old_fd: Option<UnixStream>,
    finishing: Rc<AtomicBool>,
}

impl EventSource for GreetdSource {
    type Event = Response;
    type Metadata = ();
    type Ret = ();

    fn process_events<F>(
        &mut self,
        _readiness: calloop::Readiness,
        _token: Token,
        mut callback: F,
    ) -> Result<PostAction, std::io::Error>
    where
        F: FnMut(Self::Event, &mut Self::Metadata) -> Self::Ret,
    {
        let response = Response::read_from(&mut *self.socket.borrow_mut())?;
        match response {
            Response::Success => {
                if self.finishing.load(std::sync::atomic::Ordering::Acquire) {
                    callback(Response::Finish, &mut ());
                    self.request_in_queue
                        .store(false, std::sync::atomic::Ordering::Release);
                    return Ok(PostAction::Remove);
                } else {
                    callback(response, &mut ());
                }
            }
            Response::Error {
                error_type: ErrorType::AuthError,
                ..
            } => {
                self.started_session
                    .store(false, std::sync::atomic::Ordering::Release);
                self.write_msg(Request::CancelSession)?;
                self.old_fd = Some(std::mem::replace(
                    &mut *self.socket.borrow_mut(),
                    UnixStream::connect(
                        std::env::var("GREETD_SOCK")
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e))?,
                    )?,
                ));
                self.request_in_queue
                    .store(false, std::sync::atomic::Ordering::Release);
                callback(response, &mut ());
                return Ok(PostAction::Reregister);
            }
            _ => callback(response, &mut ()),
        }
        self.request_in_queue
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(PostAction::Continue)
    }

    fn register(
        &mut self,
        poll: &mut calloop::Poll,
        token_factory: &mut calloop::TokenFactory,
    ) -> Result<(), std::io::Error> {
        self.token = token_factory.token();
        poll.register(
            self.socket.borrow().as_raw_fd(),
            Interest::READ,
            calloop::Mode::Level,
            self.token,
        )
    }

    fn reregister(
        &mut self,
        poll: &mut calloop::Poll,
        token_factory: &mut calloop::TokenFactory,
    ) -> Result<(), std::io::Error> {
        self.token = token_factory.token();
        if let Some(old_fd) = self.old_fd.take() {
            poll.unregister(old_fd.as_raw_fd())?;
            poll.register(
                self.socket.borrow().as_raw_fd(),
                Interest::READ,
                calloop::Mode::Level,
                self.token,
            )
        } else {
            poll.reregister(
                self.socket.borrow().as_raw_fd(),
                Interest::READ,
                calloop::Mode::Level,
                self.token,
            )
        }
    }

    fn unregister(&mut self, poll: &mut calloop::Poll) -> Result<(), std::io::Error> {
        self.token = Token::invalid();
        poll.unregister(self.socket.borrow().as_raw_fd())
    }
}

impl GreetdSource {
    #[inline]
    fn write_msg(&self, rq: Request) -> Result<(), std::io::Error> {
        let len = rq.write_len().0;
        let mut buf = String::with_capacity(len + 4);
        unsafe { buf.as_mut_vec().write_all(&(len as u32).to_ne_bytes())? };
        rq.write_to(&mut buf)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.socket.borrow_mut().write_all(buf.as_bytes())
    }
}

impl AsRawFd for Greetd {
    #[inline]
    fn as_raw_fd(&self) -> std::os::unix::prelude::RawFd {
        self.socket.borrow().as_raw_fd()
    }
}

impl Drop for Greetd {
    fn drop(&mut self) {
        if self
            .started_session
            .load(std::sync::atomic::Ordering::Acquire)
            && !self.finishing.load(std::sync::atomic::Ordering::Acquire)
        {
            self.write_msg(Request::CancelSession).unwrap();
        }
    }
}

impl Greetd {
    #[inline]
    pub fn new() -> Result<Self, std::io::Error> {
        Ok(Self {
            socket: Rc::new(RefCell::new(UnixStream::connect(
                std::env::var("GREETD_SOCK")
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e))?,
            )?)),
            request_in_queue: Rc::new(AtomicBool::new(false)),
            finishing: Rc::new(AtomicBool::new(false)),
            started_session: Rc::new(AtomicBool::new(false)),
        })
    }

    #[inline]
    pub fn create_session(&mut self, username: &str) -> Result<(), std::io::Error> {
        if self
            .started_session
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "Cannot start multiple sessions",
            ));
        }
        if self
            .request_in_queue
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "greetd cannot process multiple events at once",
            ))
        } else {
            self.write_msg(Request::CreateSession { username })
        }
    }

    #[inline]
    pub fn authentication_response(
        &mut self,
        response: Option<&str>,
    ) -> Result<(), std::io::Error> {
        if self
            .request_in_queue
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "greetd cannot process multiple events at once",
            ))
        } else {
            self.write_msg(Request::PostAuthMessageResponse { response })
        }
    }

    #[inline]
    pub fn start_session(&mut self, cmd: &[&str]) -> Result<(), std::io::Error> {
        if self
            .finishing
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "Already started a session",
            ));
        }
        if self
            .request_in_queue
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "greetd cannot process multiple events at once",
            ))
        } else {
            self.write_msg(Request::StartSession { cmd })
        }
    }

    #[inline]
    pub fn event_source(&self) -> GreetdSource {
        GreetdSource {
            socket: self.socket.clone(),
            request_in_queue: self.request_in_queue.clone(),
            token: Token::invalid(),
            finishing: self.finishing.clone(),
            started_session: self.started_session.clone(),
            old_fd: None,
        }
    }

    #[inline]
    fn write_msg(&self, rq: Request) -> Result<(), std::io::Error> {
        let len = rq.write_len().0;
        let mut buf = String::with_capacity(len + 4);
        unsafe { buf.as_mut_vec().write_all(&(len as u32).to_ne_bytes())? };
        rq.write_to(&mut buf)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.socket.borrow_mut().write_all(buf.as_bytes())
    }
}

/// A request from a greeter to greetd. The request type is internally tagged
/// with the"type" field, with the type written in snake_case.
///
/// Example serialization:
///
/// ```json
/// {
///    "type": "create_session",
///    "username": "bob"
/// }
/// ```
#[derive(Debug)]
pub enum Request<'a> {
    /// CreateSession initiates a login attempt for the given user.
    /// CreateSession returns either a Response::AuthMessage,
    /// Response::Success or Response::Failure.
    ///
    /// If an auth message is returned, it should be answered with a
    /// Request::PostAuthMessageResponse. If a success is returned, the session
    /// can then be started with Request::StartSession.
    ///
    /// If a login flow needs to be aborted at any point, send
    /// Request::CancelSession. Note that the session is cancelled
    /// automatically on error.
    CreateSession { username: &'a str },

    /// PostAuthMessageResponse responds to the last auth message, and returns
    /// either a Response::AuthMessage, Response::Success or Response::Failure.
    ///
    /// If an auth message is returned, it should be answered with a
    /// Request::PostAuthMessageResponse. If a success is returned, the session
    /// can then be started with Request::StartSession.
    PostAuthMessageResponse { response: Option<&'a str> },

    /// Start a successfully logged in session. This will fail if the session
    /// has pending messages or has encountered an error.
    StartSession { cmd: &'a [&'a str] },

    /// Cancel a session. This can only be done if the session has not been
    /// started. Cancel does not have to be called if an error has been
    /// encountered in its setup or login flow.
    CancelSession,
}

impl<'a> Display for Request<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.write_to(f)
    }
}

impl<'a> Writeable for Request<'a> {
    fn write_to<W: core::fmt::Write + ?Sized>(&self, sink: &mut W) -> core::fmt::Result {
        sink.write_str("{\"type\":\"")?;
        sink.write_str(match *self {
            Request::StartSession { .. } => "start_session",
            Request::CreateSession { .. } => "create_session",
            Request::PostAuthMessageResponse { .. } => "post_auth_message_response",
            Request::CancelSession => "cancel_session",
        })?;
        sink.write_char('\"')?;
        match *self {
            Request::StartSession { cmd } => {
                sink.write_str(",\"cmd\":[")?;

                let mut iter = cmd.iter();

                if let Some(next) = iter.next() {
                    sink.write_char('\"')?;
                    next.write_to(sink)?;
                    sink.write_char('\"')?;
                }

                iter.try_for_each(|f| {
                    sink.write_str(",\"")?;
                    f.write_to(sink)?;
                    sink.write_char('\"')
                })?;
                sink.write_char(']')?;
            }
            Request::CreateSession { username } => {
                sink.write_str(",\"username\":\"")?;
                username.write_to(sink)?;
                sink.write_char('\"')?;
            }
            Request::CancelSession => {}
            Request::PostAuthMessageResponse { response } => {
                if let Some(res) = response {
                    sink.write_str(",\"response\":\"")?;
                    res.write_to(sink)?;
                    sink.write_char('\"')?;
                }
            }
        }
        sink.write_char('}')
    }

    fn write_len(&self) -> writeable::LengthHint {
        match *self {
            Request::CancelSession => "{\"type\":\"cancel_session\"}".write_len(),
            Request::CreateSession { username } => {
                "{\"type\":\"create_session\",\"username\":\"\"}".write_len() + username.write_len()
            }
            Request::PostAuthMessageResponse { response } => {
                let mut len = "{\"type\":\"post_auth_message_response\"}".write_len();
                if let Some(res) = response {
                    len += ",\"response\":\"\"".write_len() + res.write_len();
                }
                len
            }
            Request::StartSession { cmd } => {
                let mut len = "{\"type\":\"start_session\",\"cmd\":[]}".write_len();
                let add_len = cmd.len();

                if add_len != 0 {
                    len += (LengthHint::exact(3) * (add_len - 1)) + LengthHint::exact(2);
                }
                for i in cmd {
                    len += i.write_len();
                }

                len
            }
        }
    }
}

/// An error type for Response::Error. Serialized as snake_case.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorType {
    /// A generic error. See the error description for more details.
    Error,

    /// An error caused by failed authentication.
    AuthError,
}

/// A message type for a Response::AuthMessage. Serialized as snake_case.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMessageType {
    /// A question whose answer should be visible during input.
    Visible,

    /// A question whose answer should be kept secret during input.
    Secret,

    /// An information message.
    Info,

    /// An error message.
    Error,
}

/// A response from greetd to a greeter. The request type is internally tagged
/// with the"type" field, with the type written in snake_case.
///
/// Example serialization:
///
/// ```json
/// {
///    "type": "auth_message",
///    "message": "Password:",
///    "message_type": "secret"
/// }
/// ```
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum Response {
    /// The request was successful.
    Success,
    Finish,

    /// The request failed. See the type and/or description for more
    /// information about this failure.
    Error {
        error_type: ErrorType,
        description: String,
    },

    /// An authentication message needs to be answered to continue through the
    /// authentication flow.
    ///
    /// An authentication message can consist of anything. While it will
    /// commonly just be a request for the users' password, it could also ask
    /// for TOTP codes, or whether or not you felt sad when Littlefoot's mother
    /// died in the original "Land Before Time". It is therefore important that
    /// no assumptions are made about the questions that will be asked, and
    /// attempts to automatically answer these questions should not be made.
    AuthMessage {
        auth_message_type: AuthMessageType,
        auth_message: String,
    },
}

impl Response {
    #[inline]
    pub fn read_from<R: Read>(stream: &mut R) -> Result<Response, std::io::Error> {
        let mut len_bytes = [0; 4];
        stream.read_exact(&mut len_bytes)?;
        let len = u32::from_ne_bytes(len_bytes);
        let mut resp_buf = vec![0; len as usize];
        stream.read_exact(&mut resp_buf)?;
        serde_json::from_slice(&mut resp_buf).map_err(|e| e.into())
    }
}

#[cfg(test)]
mod tests {
    use writeable::Writeable;

    use crate::Request;

    #[test]
    fn create_session() {
        let username = "isaacm";
        let wtr = Request::CreateSession {
            username: &username,
        };
        let result = "{\"type\":\"create_session\",\"username\":\"isaacm\"}";
        let mut written = String::with_capacity(wtr.write_len().0);
        wtr.write_to(&mut written).unwrap();

        assert_eq!(result, written);
        assert_eq!(result.len(), wtr.write_len().0);
    }

    #[test]
    fn start_session() {
        let session = &["/etc/ly/wsetup.sh", "dwl"];
        let wtr = Request::StartSession { cmd: session };
        let result = "{\"type\":\"start_session\",\"cmd\":[\"/etc/ly/wsetup.sh\",\"dwl\"]}";
        let mut written = String::with_capacity(wtr.write_len().0);
        wtr.write_to(&mut written).unwrap();
        assert_eq!(result, written);
        assert_eq!(wtr.write_len().0, result.len());
    }
}
