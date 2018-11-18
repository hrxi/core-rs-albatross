use std::fmt;
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use futures::prelude::*;
use futures::sync::mpsc::*;
use parking_lot::Mutex;
use parking_lot::RwLock;
use tokio::prelude::Stream;

use crate::consensus::base::primitive::hash::Argon2dHash;
use crate::network::connection::close_type::CloseType;
use crate::network::connection::network_connection::AddressInfo;
use crate::network::connection::network_connection::NetworkConnection;
use crate::network::message::Message;
use crate::network::peer::Peer;
use crate::network::websocket::NimiqMessageStreamError;
use crate::network::websocket::SharedNimiqMessageStream;
use crate::utils::observer::{Listener, ListenerHandle, Notifier, PassThroughListener, PassThroughNotifier};

pub trait Agent: Send {
    /// Initialize the protocol.
    fn initialize(&mut self);

    /// Handle a message.
    fn on_message(&mut self, msg: &Message);

    /// On disconnect.
    fn on_close(&mut self);
}

#[derive(Clone)]
pub struct PeerChannel {
    stream_notifier: Arc<RwLock<PassThroughNotifier<'static, PeerStreamEvent>>>,
    pub notifier: Arc<RwLock<Notifier<'static, PeerChannelEvent>>>,
    peer_sink: PeerSink,
    pub address_info: AddressInfo,
    closed: Arc<AtomicBool>,
}

impl PeerChannel {
    pub fn new(network_connection: &NetworkConnection) -> Self {
        let notifier = Arc::new(RwLock::new(Notifier::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let address_info = network_connection.address_info();

        let inner_closed = closed.clone();
        let bubble_notifier = notifier.clone();
        network_connection.notifier.write().register(move |e: PeerStreamEvent| {
            let event = PeerChannelEvent::from(e);
            match event {
                PeerChannelEvent::Close(ty) => {
                    // Don't fire close event again when already closed.
                    if !inner_closed.swap(true, Ordering::Relaxed) {
                        bubble_notifier.read().notify(event);
                    }
                },
                event => bubble_notifier.read().notify(event),
            };
        });

        PeerChannel {
            stream_notifier: network_connection.notifier.clone(),
            notifier,
            peer_sink: network_connection.peer_sink(),
            address_info,
            closed,
        }
    }

    pub fn send(&self, msg: Message) -> Result<(), SendError<Message>> {
        self.peer_sink.send(msg)
    }

    pub fn closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

impl Drop for PeerChannel {
    fn drop(&mut self) {
        self.stream_notifier.write().deregister();
    }
}

impl Debug for PeerChannel {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "PeerChannel {{}}")
    }
}

#[derive(Clone)]
pub enum PeerChannelEvent {
    Message(Message),
    Close(CloseType),
    Error, // cannot use `NimiqMessageStreamError`, because `tungstenite::Error` is not `Clone`
}

impl From<PeerStreamEvent> for PeerChannelEvent {
    fn from(e: PeerStreamEvent) -> Self {
        match e {
            PeerStreamEvent::Message(msg) => PeerChannelEvent::Message(msg),
            PeerStreamEvent::Close(ty) => PeerChannelEvent::Close(ty),
            PeerStreamEvent::Error => PeerChannelEvent::Error,
        }
    }
}

#[derive(Clone)]
pub struct PeerSink {
    sink: UnboundedSender<Message>,
}

impl PeerSink {
    pub fn new(channel: UnboundedSender<Message>) -> Self {
        PeerSink {
            sink: channel.clone(),
        }
    }

    pub fn send(&self, msg: Message) -> Result<(), SendError<Message>> {
        self.sink.unbounded_send(msg)
    }
}

impl Debug for PeerSink {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(f, "PeerSink {{}}")
    }
}

#[derive(Clone)]
pub enum PeerStreamEvent {
    Message(Message),
    Close(CloseType),
    Error, // cannot use `NimiqMessageStreamError`, because `tungstenite::Error` is not `Clone`
}

pub struct PeerStream {
    stream: SharedNimiqMessageStream,
    notifier: Arc<RwLock<PassThroughNotifier<'static, PeerStreamEvent>>>,
}

impl PeerStream {
    pub fn new(stream: SharedNimiqMessageStream, notifier: Arc<RwLock<PassThroughNotifier<'static, PeerStreamEvent>>>) -> Self {
        PeerStream {
            stream,
            notifier,
        }
    }

    pub fn process_stream(self) -> impl Future<Item=(), Error=NimiqMessageStreamError> + 'static {
        let stream = self.stream;
        let msg_notifier = self.notifier.clone();
        let error_notifier = self.notifier.clone();
        let close_notifier = self.notifier;

        let process_message = stream.for_each(move |msg| {
            msg_notifier.read().notify(PeerStreamEvent::Message(msg));
            Ok(())
        }).or_else(move |error| {
            error_notifier.read().notify(PeerStreamEvent::Error);
            Err(error)
        }).and_then(move |result| {
            close_notifier.read().notify(PeerStreamEvent::Close(CloseType::ClosedByRemote));
            Ok(result)
        });

        process_message
    }
}

impl Debug for PeerStream {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        self.stream.fmt(f)
    }
}
