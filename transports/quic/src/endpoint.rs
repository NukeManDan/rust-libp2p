// Copyright 2017-2020 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.
//
use crate::{connection::EndpointMessage, error::Error, socket, Config, Upgrade};
use async_macros::ready;
use async_std::net::SocketAddr;
use futures::{channel::mpsc, prelude::*};
use libp2p_core::{
    multiaddr::{Multiaddr, Protocol},
    transport::{ListenerEvent, TransportError},
    Transport,
};
use log::{debug, trace, warn};
use parking_lot::{Mutex, MutexGuard};
use quinn_proto::{Connection, ConnectionHandle};
use std::{
    collections::HashMap,
    pin::Pin,
    sync::{Arc, Weak},
    task::{Context, Poll},
    time::Instant,
};

#[derive(Debug)]
pub(super) struct EndpointInner {
    inner: quinn_proto::Endpoint,
    muxers: HashMap<ConnectionHandle, Weak<Mutex<super::connection::Muxer>>>,
    driver: Option<async_std::task::JoinHandle<Result<(), Error>>>,
    pending: socket::Pending,
    /// Used to receive events from connections
    event_receiver: mpsc::Receiver<EndpointMessage>,
}

impl EndpointInner {
    pub(super) fn handle_event(
        &mut self,
        handle: quinn_proto::ConnectionHandle,
        event: quinn_proto::EndpointEvent,
    ) -> Option<quinn_proto::ConnectionEvent> {
        if event.is_drained() {
            let res = self.inner.handle_event(handle, event);
            self.muxers.remove(&handle);
            res
        } else {
            self.inner.handle_event(handle, event)
        }
    }

    fn drive_events(&mut self, cx: &mut Context) {
        while let Poll::Ready(e) = self.event_receiver.poll_next_unpin(cx).map(|e| e.unwrap()) {
            match e {
                EndpointMessage::ConnectionAccepted => {
                    debug!("accepting connection!");
                    self.inner.accept();
                }
                EndpointMessage::EndpointEvent { handle, event } => {
                    debug!("we have an event from connection {:?}", handle);
                    match self.muxers.get(&handle).and_then(|e| e.upgrade()) {
                        None => drop(self.muxers.remove(&handle)),
                        Some(connection) => {
                            let event = self.inner.handle_event(handle, event);
                            connection.lock().process_connection_events(self, event)
                        }
                    }
                }
            }
        }
    }

    fn drive_receive(
        &mut self,
        socket: &socket::Socket,
        cx: &mut Context,
    ) -> Poll<Result<(ConnectionHandle, Connection), Error>> {
        use quinn_proto::DatagramEvent;
        let mut buf = vec![0; 65535];
        loop {
            let (bytes, peer) = ready!(socket.recv_from(cx, &mut buf[..])?);
            let (handle, event) =
                match self
                    .inner
                    .handle(Instant::now(), peer, None, buf[..bytes].into())
                {
                    Some(e) => e,
                    None => continue,
                };
            trace!("have an event!");
            match event {
                DatagramEvent::ConnectionEvent(connection_event) => {
                    match self.muxers.get(&handle).and_then(|e| e.upgrade()) {
                        Some(connection) => connection
                            .lock()
                            .process_connection_events(self, Some(connection_event)),
                        None => {
                            debug!("lost our connection!");
                            assert!(self
                                .handle_event(handle, quinn_proto::EndpointEvent::drained())
                                .is_none())
                        }
                    }
                }
                DatagramEvent::NewConnection(connection) => {
                    debug!("new connection detected!");
                    break Poll::Ready(Ok((handle, connection)));
                }
            }
            trace!("event processed!")
        }
    }

    fn poll_transmit_pending(
        &mut self,
        socket: &socket::Socket,
        cx: &mut Context,
    ) -> Poll<Result<(), Error>> {
        let Self { inner, pending, .. } = self;
        pending
            .send_packet(cx, socket, &mut || inner.poll_transmit())
            .map_err(Error::IO)
    }
}

#[derive(Debug)]
pub(super) struct EndpointData {
    /// The single UDP socket used for I/O
    socket: socket::Socket,
    /// A `Mutex` protecting the QUIC state machine.
    inner: Mutex<EndpointInner>,
    /// The channel on which new connections are sent.  This is bounded in practice by the accept
    /// backlog.
    new_connections: mpsc::UnboundedSender<Result<ListenerEvent<Upgrade>, Error>>,
    /// The channel used to receive new connections.
    receive_connections:
        Mutex<Option<mpsc::UnboundedReceiver<Result<ListenerEvent<Upgrade>, Error>>>>,
    /// Connections send their events to this
    event_channel: mpsc::Sender<EndpointMessage>,
    /// The `Multiaddr`
    address: Multiaddr,
    /// The configuration
    config: Config,
}

impl EndpointData {
    pub(super) fn event_channel(&self) -> mpsc::Sender<EndpointMessage> {
        self.event_channel.clone()
    }

    pub(super) fn socket(&self) -> &socket::Socket {
        &self.socket
    }
}

/// A QUIC endpoint.  Each endpoint has its own configuration and listening socket.
///
/// You generally need only one of these per process.  Endpoints are `Send` and `Sync`, so you
/// can share them among as many threads as you like.  However, performance may be better if you
/// have one per CPU core, as this reduces lock contention.  Most applications will not need to
/// worry about this.  `QuicEndpoint` tries to use fine-grained locking to reduce the overhead.
///
/// `QuicEndpoint` wraps the underlying data structure in an `Arc`, so cloning it just bumps the
/// reference count.  All state is shared between the clones.  For example, you can pass different
/// clones to `listen_on`.  Each incoming connection will be received by exactly one of them.
///
/// The **only** valid `Multiaddr` to pass to `listen_on` or `dial` is the one used to create the
/// `QuicEndpoint`.  You can obtain this via the `addr` method.  If you pass a different one, you
/// will get `TransportError::MultiaddrNotSuppported`.
#[derive(Debug, Clone)]
pub struct Endpoint(Arc<EndpointData>);

struct EndpointDriver(Arc<EndpointData>);

impl Endpoint {
    fn inner(&self) -> MutexGuard<'_, EndpointInner> {
        trace!("acquiring lock!");
        let q = self.0.inner.lock();
        trace!("lock acquired!");
        q
    }

    /// Construct a `Endpoint` with the given `Config` and `Multiaddr`.
    pub fn new(
        config: Config,
        address: Multiaddr,
    ) -> Result<Self, TransportError<<&'static Self as Transport>::Error>> {
        let socket_addr = if let Ok(sa) = multiaddr_to_socketaddr(&address) {
            sa
        } else {
            return Err(TransportError::MultiaddrNotSupported(address));
        };
        // NOT blocking, as per man:bind(2), as we pass an IP address.
        let socket = std::net::UdpSocket::bind(&socket_addr)
            .map_err(Error::IO)?
            .into();
        let (new_connections, receive_connections) = mpsc::unbounded();
        let (event_channel, event_receiver) = mpsc::channel(0);
        let return_value = Self(Arc::new(EndpointData {
            socket: socket::Socket::new(socket),
            inner: Mutex::new(EndpointInner {
                inner: quinn_proto::Endpoint::new(
                    config.endpoint_config.clone(),
                    Some(config.server_config.clone()),
                ),
                muxers: HashMap::new(),
                driver: None,
                event_receiver,
                pending: Default::default(),
            }),
            address: address.clone(),
            receive_connections: Mutex::new(Some(receive_connections)),
            new_connections,
            event_channel,
            config,
        }));
        return_value.inner().driver = Some(async_std::task::spawn(EndpointDriver(
            return_value.0.clone(),
        )));
        return_value
            .0
            .new_connections
            .unbounded_send(Ok(ListenerEvent::NewAddress(address)))
            .expect("we have a reference to the peer, so this will not fail; qed");
        Ok(return_value)
    }
}

fn create_muxer(
    endpoint: Arc<EndpointData>,
    connection: Connection,
    handle: ConnectionHandle,
    inner: &mut EndpointInner,
) -> Upgrade {
    super::connection::ConnectionDriver::spawn(endpoint, connection, handle, |weak| {
        inner.muxers.insert(handle, weak);
    })
}

impl EndpointDriver {
    fn accept_muxer(
        &self,
        connection: Connection,
        handle: ConnectionHandle,
        inner: &mut EndpointInner,
    ) {
        let upgrade = create_muxer(self.0.clone(), connection, handle, &mut *inner);
        if self
            .0
            .new_connections
            .unbounded_send(Ok(ListenerEvent::Upgrade {
                upgrade,
                local_addr: self.0.address.clone(),
                remote_addr: self.0.address.clone(),
            }))
            .is_err()
        {
            inner.inner.accept();
            inner.inner.reject_new_connections();
        }
    }
}

impl Future for EndpointDriver {
    type Output = Result<(), Error>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Error>> {
        let this = self.get_mut();
        loop {
            let mut inner = this.0.inner.lock();
            trace!("driving events");
            inner.drive_events(cx);
            trace!("driving incoming packets");
            match inner.drive_receive(&this.0.socket, cx)? {
                Poll::Pending => {
                    drop(inner.poll_transmit_pending(&this.0.socket, cx)?);
                    trace!("returning Pending");
                    break Poll::Pending;
                }
                Poll::Ready((handle, connection)) => {
                    trace!("have a new connection");
                    this.accept_muxer(connection, handle, &mut *inner);
                    trace!("connection accepted");
                    match inner.poll_transmit_pending(&this.0.socket, cx)? {
                        Poll::Pending => break Poll::Pending,
                        Poll::Ready(()) if Arc::strong_count(&this.0) == 1 => {
                            break Poll::Ready(Ok(()))
                        }
                        Poll::Ready(()) => break Poll::Pending,
                    }
                }
            }
        }
    }
}

/// A QUIC listener
#[derive(Debug)]
pub struct Listener {
    reference: Arc<EndpointData>,
    channel: mpsc::UnboundedReceiver<Result<ListenerEvent<Upgrade>, Error>>,
}

impl Unpin for Listener {}

impl Stream for Listener {
    type Item = Result<ListenerEvent<Upgrade>, Error>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        self.get_mut().channel.poll_next_unpin(cx)
    }
}

impl Transport for &Endpoint {
    type Output = (libp2p_core::PeerId, super::Muxer);
    type Error = Error;
    type Listener = Listener;
    type ListenerUpgrade = Upgrade;
    type Dial = Upgrade;

    fn listen_on(self, addr: Multiaddr) -> Result<Self::Listener, TransportError<Self::Error>> {
        if addr != self.0.address {
            return Err(TransportError::MultiaddrNotSupported(addr));
        }
        let channel = (self.0)
            .receive_connections
            .lock()
            .take()
            .ok_or_else(|| TransportError::Other(Error::AlreadyListening))?;
        let mut inner = self.inner();
        let reference = self.0.clone();
        if inner.driver.is_none() {
            inner.driver = Some(async_std::task::spawn(EndpointDriver(reference.clone())));
        }
        Ok(Listener { channel, reference })
    }

    fn dial(self, addr: Multiaddr) -> Result<Self::Dial, TransportError<Self::Error>> {
        let socket_addr = if let Ok(socket_addr) = multiaddr_to_socketaddr(&addr) {
            if socket_addr.port() == 0 || socket_addr.ip().is_unspecified() {
                debug!("Instantly refusing dialing {}, as it is invalid", addr);
                return Err(TransportError::MultiaddrNotSupported(addr));
            }
            socket_addr
        } else {
            return Err(TransportError::MultiaddrNotSupported(addr));
        };
        let mut inner = self.inner();
        if inner.driver.is_none() {
            inner.driver = Some(async_std::task::spawn(EndpointDriver(self.0.clone())))
        }

        let s: Result<(_, Connection), _> = inner
            .inner
            .connect(
                self.0.config.client_config.clone(),
                socket_addr,
                "localhost",
            )
            .map_err(|e| {
                warn!("Connection error: {:?}", e);
                TransportError::Other(Error::CannotConnect(e))
            });
        let (handle, conn) = s?;
        Ok(create_muxer(self.0.clone(), conn, handle, &mut inner))
    }
}

// This type of logic should probably be moved into the multiaddr package
fn multiaddr_to_socketaddr(addr: &Multiaddr) -> Result<SocketAddr, ()> {
    let mut iter = addr.iter();
    let proto1 = iter.next().ok_or(())?;
    let proto2 = iter.next().ok_or(())?;
    let proto3 = iter.next().ok_or(())?;

    if iter.next().is_some() {
        return Err(());
    }

    match (proto1, proto2, proto3) {
        (Protocol::Ip4(ip), Protocol::Udp(port), Protocol::Quic) => {
            Ok(SocketAddr::new(ip.into(), port))
        }
        (Protocol::Ip6(ip), Protocol::Udp(port), Protocol::Quic) => {
            Ok(SocketAddr::new(ip.into(), port))
        }
        _ => Err(()),
    }
}

#[test]
fn multiaddr_to_udp_conversion() {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    crate::tests::init();
    assert!(
        multiaddr_to_socketaddr(&"/ip4/127.0.0.1/udp/1234".parse::<Multiaddr>().unwrap()).is_err()
    );

    assert!(
        multiaddr_to_socketaddr(&"/ip4/127.0.0.1/tcp/1234".parse::<Multiaddr>().unwrap()).is_err()
    );

    assert_eq!(
        multiaddr_to_socketaddr(
            &"/ip4/127.0.0.1/udp/12345/quic"
                .parse::<Multiaddr>()
                .unwrap()
        ),
        Ok(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            12345,
        ))
    );
    assert_eq!(
        multiaddr_to_socketaddr(
            &"/ip4/255.255.255.255/udp/8080/quic"
                .parse::<Multiaddr>()
                .unwrap()
        ),
        Ok(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(255, 255, 255, 255)),
            8080,
        ))
    );
    assert_eq!(
        multiaddr_to_socketaddr(&"/ip6/::1/udp/12345/quic".parse::<Multiaddr>().unwrap()),
        Ok(SocketAddr::new(
            IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)),
            12345,
        ))
    );
    assert_eq!(
        multiaddr_to_socketaddr(
            &"/ip6/ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/udp/8080/quic"
                .parse::<Multiaddr>()
                .unwrap()
        ),
        Ok(SocketAddr::new(
            IpAddr::V6(Ipv6Addr::new(
                65535, 65535, 65535, 65535, 65535, 65535, 65535, 65535,
            )),
            8080,
        ))
    );
}
