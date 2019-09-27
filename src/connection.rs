use crate::codec::{BackendMessage, BackendMessages, Framed, FrontendMessage, PostgresCodec};
use crate::copy_in::CopyInReceiver;
use crate::Error;
use bytes::BytesMut;
use crossbeam::queue::SegQueue;
use fallible_iterator::FallibleIterator;
use log::error;
use may::coroutine::JoinHandle;
use may::go;
use may::net::TcpStream;
use may::sync::{mpsc, Mutex};
use may_queue::spsc;
use postgres_protocol::message::backend::Message;
use postgres_protocol::message::frontend;
use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub enum RequestMessages {
    Single(FrontendMessage),
    CopyIn(CopyInReceiver),
}

pub struct Request {
    pub messages: RequestMessages,
    pub sender: mpsc::Sender<BackendMessages>,
}

pub struct Response {
    sender: mpsc::Sender<BackendMessages>,
}

struct ConnectionWriteHalf {
    data_count: AtomicUsize,
    data_queue: SegQueue<Request>,
    writer: Mutex<TcpStream>,
    responses: Arc<spsc::Queue<Response>>,
}

impl ConnectionWriteHalf {
    /// send a request to the connection
    fn send(&self, req: Request) -> std::io::Result<()> {
        self.data_queue.push(req);
        let mut cnt = self.data_count.fetch_add(1, Ordering::AcqRel);
        if cnt == 0 {
            let mut buf = BytesMut::with_capacity(1024);
            let mut writer = self.writer.lock().unwrap();

            loop {
                while let Ok(req) = self.data_queue.pop() {
                    match req.messages {
                        RequestMessages::Single(msg) => PostgresCodec.encode(msg, &mut buf)?,
                        RequestMessages::CopyIn(rcv) => {
                            for msg in rcv {
                                PostgresCodec.encode(msg, &mut buf)?;
                            }
                        }
                    }

                    self.responses.push(Response { sender: req.sender });
                    cnt += 1;
                }
                let len = buf.len();
                let data = buf.split_to(len);
                if let Err(e) = writer.write_all(&data) {
                    error!("QueuedWriter failed, err={}", e);
                    return Err(e);
                }

                if self.data_count.fetch_sub(cnt, Ordering::AcqRel) == cnt {
                    break;
                }

                cnt = 0;
            }
        }
        Ok(())
    }
}

/// A connection to a PostgreSQL database.
pub(crate) struct Connection {
    writer: Arc<ConnectionWriteHalf>,
    handle: JoinHandle<()>,
    thread_writer: JoinHandle<()>,
    thread_writer_tx: mpsc::Sender<Request>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        let bg = self.handle.coroutine();
        let sd = self.thread_writer.coroutine();
        unsafe {
            bg.cancel();
            sd.cancel();
        }
    }
}

impl Connection {
    pub(crate) fn new(
        mut stream: Framed<TcpStream>,
        mut parameters: HashMap<String, String>,
    ) -> Connection {
        let writer = stream
            .inner_mut()
            .try_clone()
            .expect("failed to clone stream for wirter");
        let responses = Arc::new(spsc::Queue::<Response>::new());
        let rsps = responses.clone();
        let writer_half = Arc::new(ConnectionWriteHalf {
            data_count: AtomicUsize::new(0),
            data_queue: SegQueue::new(),
            writer: Mutex::new(writer),
            responses,
        });
        let writer_half_share = writer_half.clone();
        let handle = go!(move || {
            let mut main = || -> Result<(), Error> {
                #[allow(clippy::while_let_on_iterator)]
                while let Some(rsp) = stream.next() {
                    match rsp.map_err(Error::io)? {
                        BackendMessage::Async(Message::NoticeResponse(_body)) => {}
                        BackendMessage::Async(Message::NotificationResponse(_body)) => {}
                        BackendMessage::Async(Message::ParameterStatus(body)) => {
                            parameters.insert(
                                body.name().map_err(Error::parse)?.to_string(),
                                body.value().map_err(Error::parse)?.to_string(),
                            );
                        }
                        BackendMessage::Async(_) => unreachable!(),
                        BackendMessage::Normal {
                            mut messages,
                            request_complete,
                        } => {
                            let response = match unsafe { rsps.peek() } {
                                Some(response) => response,
                                None => match messages.next().map_err(Error::parse)? {
                                    Some(Message::ErrorResponse(error)) => {
                                        return Err(Error::db(error))
                                    }
                                    _ => return Err(Error::unexpected_message()),
                                },
                            };

                            response.sender.send(messages).ok();

                            if request_complete {
                                rsps.pop();
                            }
                        }
                    }
                }
                Ok(())
            };

            if let Err(e) = main() {
                error!("receiver closed. err={}", e);
                let mut request = vec![];
                frontend::terminate(&mut request);
                let (tx, _rx) = mpsc::channel();
                let req = Request {
                    messages: RequestMessages::Single(FrontendMessage::Raw(request)),
                    sender: tx,
                };
                writer_half_share.send(req).ok();
            }
            stream.inner_mut().shutdown(std::net::Shutdown::Both).ok();
        });

        let writer_1 = writer_half.clone();
        let (tx, rx) = mpsc::channel();
        let thread_writer = go!(move || {
            while let Ok(req) = rx.recv() {
                writer_1.send(req).ok();
            }
        });

        Connection {
            writer: writer_half,
            handle,
            thread_writer,
            thread_writer_tx: tx,
        }
    }

    /// send a request to the connection
    pub fn send(&self, req: Request) -> io::Result<()> {
        if may::coroutine::is_coroutine() {
            self.writer.send(req)
        } else {
            self.thread_writer_tx
                .send(req)
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "send req failed"))
        }
    }
}
