use super::{
    active_packetizer::ActivePacketizer, request, watch::WatchType, Request, Response,
    ZooKeeperTransport,
};
use byteorder::{BigEndian, WriteBytesExt};
use failure;
use futures::{
    future::Either,
    sync::{mpsc, oneshot},
};
use slog;
use std::mem;
use tokio;
use tokio::prelude::*;
use {Watch, WatchedEvent, ZkError};

pub(crate) struct Packetizer<S>
where
    S: ZooKeeperTransport,
{
    /// ZooKeeper address
    addr: S::Addr,

    /// Current state
    state: PacketizerState<S>,

    /// Watcher to send watch events to.
    default_watcher: mpsc::UnboundedSender<WatchedEvent>,

    /// Incoming requests
    rx: mpsc::UnboundedReceiver<(Request, oneshot::Sender<Result<Response, ZkError>>)>,

    /// Next xid to issue
    xid: i32,

    logger: slog::Logger,

    exiting: bool,
}

impl<S> Packetizer<S>
where
    S: ZooKeeperTransport,
{
    pub(crate) fn new(
        addr: S::Addr,
        stream: S,
        log: slog::Logger,
        default_watcher: mpsc::UnboundedSender<WatchedEvent>,
    ) -> Enqueuer
    where
        S: Send + 'static + AsyncRead + AsyncWrite,
    {
        let (tx, rx) = mpsc::unbounded();

        let exitlogger = log.clone();
        tokio::spawn(
            Packetizer {
                addr,
                state: PacketizerState::Connected(ActivePacketizer::new(stream)),
                xid: 0,
                default_watcher,
                rx: rx,
                logger: log,
                exiting: false,
            }.map_err(move |e| {
                error!(exitlogger, "packetizer exiting: {:?}", e);
                drop(e);
            }),
        );

        Enqueuer(tx)
    }
}

enum PacketizerState<S> {
    Connected(ActivePacketizer<S>),
    Reconnecting(Box<Future<Item = ActivePacketizer<S>, Error = failure::Error> + Send + 'static>),
}

impl<S> PacketizerState<S>
where
    S: AsyncRead + AsyncWrite,
{
    fn poll(
        &mut self,
        exiting: bool,
        logger: &mut slog::Logger,
        default_watcher: &mut mpsc::UnboundedSender<WatchedEvent>,
    ) -> Result<Async<()>, failure::Error> {
        let ap = match *self {
            PacketizerState::Connected(ref mut ap) => {
                return ap.poll(exiting, logger, default_watcher)
            }
            PacketizerState::Reconnecting(ref mut c) => try_ready!(c.poll()),
        };

        // we are now connected!
        mem::replace(self, PacketizerState::Connected(ap));
        self.poll(exiting, logger, default_watcher)
    }
}

impl<S> Packetizer<S>
where
    S: ZooKeeperTransport,
{
    fn poll_enqueue(&mut self) -> Result<Async<()>, ()> {
        while let PacketizerState::Connected(ref mut ap) = self.state {
            let (mut item, tx) = match try_ready!(self.rx.poll()) {
                Some((request, response)) => (request, response),
                None => return Err(()),
            };
            debug!(self.logger, "enqueueing request {:?}", item; "xid" => self.xid);

            match item {
                Request::GetData {
                    ref path,
                    ref mut watch,
                    ..
                }
                | Request::GetChildren {
                    ref path,
                    ref mut watch,
                    ..
                }
                | Request::Exists {
                    ref path,
                    ref mut watch,
                    ..
                } => {
                    if let Watch::Custom(_) = *watch {
                        // set to Global so that watch will be sent as 1u8
                        let w = mem::replace(watch, Watch::Global);
                        if let Watch::Custom(w) = w {
                            let wtype = match item {
                                Request::GetData { .. } => WatchType::Data,
                                Request::GetChildren { .. } => WatchType::Child,
                                Request::Exists { .. } => WatchType::Exist,
                                _ => unreachable!(),
                            };
                            trace!(
                                self.logger,
                                "adding pending watcher";
                                "xid" => self.xid,
                                "path" => path,
                                "wtype" => ?wtype
                            );
                            ap.pending_watchers
                                .insert(self.xid, (path.to_string(), w, wtype));
                        } else {
                            unreachable!();
                        }
                    }
                }
                _ => {}
            }

            ap.enqueue(self.xid, item, tx);
            self.xid += 1;
        }
        Ok(Async::NotReady)
    }
}

impl<S> Future for Packetizer<S>
where
    S: ZooKeeperTransport,
{
    type Item = ();
    type Error = failure::Error;

    fn poll(&mut self) -> Result<Async<Self::Item>, Self::Error> {
        trace!(self.logger, "packetizer polled");
        if !self.exiting {
            trace!(self.logger, "poll_enqueue");
            match self.poll_enqueue() {
                Ok(_) => {}
                Err(()) => {
                    // no more requests will be enqueued
                    self.exiting = true;

                    if let PacketizerState::Connected(ref mut ap) = self.state {
                        // send CloseSession
                        // length is fixed
                        ap.outbox
                            .write_i32::<BigEndian>(8)
                            .expect("Vec::write should never fail");
                        // xid
                        ap.outbox
                            .write_i32::<BigEndian>(0)
                            .expect("Vec::write should never fail");
                        // opcode
                        ap.outbox
                            .write_i32::<BigEndian>(request::OpCode::CloseSession as i32)
                            .expect("Vec::write should never fail");
                    } else {
                        unreachable!("poll_enqueue will never return Err() if not connected");
                    }
                }
            }
        }

        self.state.poll(self.exiting, &mut self.logger, &mut self.default_watcher)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Enqueuer(
    mpsc::UnboundedSender<(Request, oneshot::Sender<Result<Response, ZkError>>)>,
);

impl Enqueuer {
    pub(crate) fn enqueue(
        &self,
        request: Request,
    ) -> impl Future<Item = Result<Response, ZkError>, Error = failure::Error> {
        let (tx, rx) = oneshot::channel();
        match self.0.unbounded_send((request, tx)) {
            Ok(()) => {
                Either::A(rx.map_err(|e| format_err!("Error processing request: {:?}", e)))
            }
            Err(e) => {
                Either::B(Err(format_err!("failed to enqueue new request: {:?}", e)).into_future())
            }
        }
    }
}
