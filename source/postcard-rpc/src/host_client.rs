//! A post-dispatch host client
//!
//! This library is meant to be used with the `Dispatch` type and the
//! post-dispatch wire protocol.

use std::{
    collections::HashMap,
    marker::PhantomData,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};

use crate::{
    accumulator::raw::{CobsAccumulator, FeedResult},
    headered::extract_header_from_bytes,
    Endpoint, Key, Topic, WireHeader,
};
use cobs::encode_vec;
use maitake_sync::{
    wait_map::{WaitError, WakeOutcome},
    WaitMap,
};
use postcard::experimental::schema::Schema;
use serde::{de::DeserializeOwned, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    select,
    sync::mpsc::{Receiver, Sender},
};
use tokio_serial::{SerialPortBuilderExt, SerialStream};

/// Host Error Kind
#[derive(Debug, PartialEq)]
pub enum HostErr<WireErr> {
    /// An error of the user-specified wire error type
    Wire(WireErr),
    /// We got a response that didn't match the expected value or the
    /// user specified wire error type
    BadResponse,
    /// Deserialization of the message failed
    Postcard(postcard::Error),
    /// The interface has been closed, and no further messages are possible
    Closed,
}

impl<T> From<postcard::Error> for HostErr<T> {
    fn from(value: postcard::Error) -> Self {
        Self::Postcard(value)
    }
}

impl<T> From<WaitError> for HostErr<T> {
    fn from(_: WaitError) -> Self {
        Self::Closed
    }
}

async fn wire_worker(mut port: SerialStream, ctx: WireContext) {
    let mut buf = [0u8; 1024];
    let mut acc = CobsAccumulator::<1024>::new();
    let mut subs: HashMap<Key, Sender<RpcFrame>> = HashMap::new();

    let WireContext {
        mut outgoing,
        incoming,
        mut new_subs,
    } = ctx;

    loop {
        // Wait for EITHER a serialized request, OR some data from the embedded device
        select! {
            sub = new_subs.recv() => {
                let Some(si) = sub else {
                    return;
                };

                subs.insert(si.key, si.tx);
            }
            out = outgoing.recv() => {
                // Receiver returns None when all Senders have hung up
                let Some(msg) = out else {
                    return;
                };

                // Turn the serialized message into a COBS encoded message
                //
                // TODO: this is a little wasteful, payload is already a vec,
                // then we serialize it to a second vec, then encode that to
                // a third cobs-encoded vec. Oh well.
                let msg = msg.to_bytes();
                let mut msg = encode_vec(&msg);
                msg.push(0);

                // And send it!
                if port.write_all(&msg).await.is_err() {
                    // I guess the serial port hung up.
                    return;
                }
            }
            inc = port.read(&mut buf) => {
                // if read errored, we're done
                let Ok(used) = inc else {
                    return;
                };
                let mut window = &buf[..used];

                'cobs: while !window.is_empty() {
                    window = match acc.feed(window) {
                        // Consumed the whole USB frame
                        FeedResult::Consumed => break 'cobs,
                        // Silently ignore line errors
                        // TODO: probably add tracing here
                        FeedResult::OverFull(new_wind) => new_wind,
                        FeedResult::DeserError(new_wind) => new_wind,
                        // We got a message! Attempt to dispatch it
                        FeedResult::Success { data, remaining } => {
                            // Attempt to extract a header so we can get the sequence number
                            if let Ok((hdr, body)) = extract_header_from_bytes(data) {
                                // Got a header, turn it into a frame
                                let frame = RpcFrame { header: hdr.clone(), body: body.to_vec() };

                                // Give priority to subscriptions. TBH I only do this because I know a hashmap
                                // lookup is cheaper than a waitmap search.
                                if let Some(tx) = subs.get_mut(&hdr.key) {
                                    // Yup, we have a subscription
                                    if tx.send(frame).await.is_err() {
                                        // But if sending failed, the listener is gone, so drop it
                                        subs.remove(&hdr.key);
                                    }
                                } else {
                                    // Wake the given sequence number. If the WaitMap is closed, we're done here
                                    if let Err(ProcessError::Closed) = incoming.process(frame) {
                                        return;
                                    }
                                }
                            }

                            remaining
                        }
                    };
                }
            }
        }
    }
}

/// The [HostClient] is the primary PC-side interface.
///
/// It is generic over a single type, `WireErr`, which can be used by the
/// embedded system when a request was not understood, or some other error
/// has occurred.
///
/// [HostClient]s can be cloned, and used across multiple tasks/threads.
pub struct HostClient<WireErr> {
    ctx: Arc<HostContext>,
    out: Sender<RpcFrame>,
    subber: Sender<SubInfo>,
    err_key: Key,
    _pd: PhantomData<fn() -> WireErr>,
}

/// # Constructor Methods
impl<WireErr> HostClient<WireErr>
where
    WireErr: DeserializeOwned + Schema,
{
    /// Create a new manually implemented [HostClient].
    ///
    /// This allows you to implement your own "Wire" abstraction, if you
    /// aren't using a COBS-encoded serial port.
    ///
    /// This is temporary solution until Rust 1.76 when async traits are
    /// stable, and we can have users provide a `Wire` trait that acts as
    /// a bidirectional [RpcFrame] sink/source.
    pub fn new_manual(err_uri_path: &str, outgoing_depth: usize) -> (Self, WireContext) {
        let (tx_pc, rx_pc) = tokio::sync::mpsc::channel(outgoing_depth);
        let (tx_si, rx_si) = tokio::sync::mpsc::channel(outgoing_depth);

        let ctx = Arc::new(HostContext {
            map: WaitMap::new(),
            seq: AtomicU32::new(0),
        });

        let err_key = Key::for_path::<WireErr>(err_uri_path);

        let me = HostClient {
            ctx: ctx.clone(),
            out: tx_pc,
            err_key,
            _pd: PhantomData,
            subber: tx_si.clone(),
        };

        let wire = WireContext {
            outgoing: rx_pc,
            incoming: ctx,
            new_subs: rx_si,
        };

        (me, wire)
    }

    #[deprecated = "use `Self::new_serial_cobs`"]
    pub fn new(serial_path: &str, err_uri_path: &str) -> Self {
        Self::new_serial_cobs(serial_path, err_uri_path, 8, 115_200)
    }

    /// Create a new [HostClient]
    ///
    /// `serial_path` is the path to the serial port used. `err_uri_path` is
    /// the path associated with the `WireErr` message type.
    ///
    /// Panics if we couldn't open the serial port
    pub fn new_serial_cobs(
        serial_path: &str,
        err_uri_path: &str,
        outgoing_depth: usize,
        baud: u32,
    ) -> Self {
        let (me, wire) = Self::new_manual(err_uri_path, outgoing_depth);

        let port = tokio_serial::new(serial_path, baud)
            .open_native_async()
            .unwrap();

        tokio::task::spawn(async move { wire_worker(port, wire).await });

        me
    }
}

/// # Interface Methods
impl<WireErr> HostClient<WireErr>
where
    WireErr: DeserializeOwned + Schema,
{
    /// Send a message of type [Endpoint::Request][Endpoint] to `path`, and await
    /// a response of type [Endpoint::Response][Endpoint] (or WireErr) to `path`.
    ///
    /// This function will wait potentially forever. Consider using with a timeout.
    pub async fn send_resp<E: Endpoint>(
        &self,
        t: &E::Request,
    ) -> Result<E::Response, HostErr<WireErr>>
    where
        E::Request: Serialize + Schema,
        E::Response: DeserializeOwned + Schema,
    {
        let seq_no = self.ctx.seq.fetch_add(1, Ordering::Relaxed);
        let msg = postcard::to_stdvec(&t).expect("Allocations should not ever fail");
        let frame = RpcFrame {
            header: WireHeader {
                key: E::REQ_KEY,
                seq_no,
            },
            body: msg,
        };
        self.out.send(frame).await.map_err(|_| HostErr::Closed)?;
        let ok_resp = self.ctx.map.wait(WireHeader {
            seq_no,
            key: E::RESP_KEY,
        });
        let err_resp = self.ctx.map.wait(WireHeader {
            seq_no,
            key: self.err_key,
        });

        select! {
            o = ok_resp => {
                let resp = o?;
                let r = postcard::from_bytes::<E::Response>(&resp)?;
                Ok(r)
            },
            e = err_resp => {
                let resp = e?;
                let r = postcard::from_bytes::<WireErr>(&resp)?;
                Err(HostErr::Wire(r))
            },
        }
    }

    /// Publish a [Topic] [Message][Topic::Message].
    ///
    /// There is no feedback if the server received our message. If the I/O worker is
    /// closed, an error is returned.
    pub async fn publish<T: Topic>(&self, seq_no: u32, msg: &T::Message) -> Result<(), IoClosed>
    where
        T::Message: Serialize,
    {
        let smsg = postcard::to_stdvec(msg).expect("alloc should never fail");
        self.out
            .send(RpcFrame {
                header: WireHeader {
                    key: T::TOPIC_KEY,
                    seq_no,
                },
                body: smsg,
            })
            .await
            .map_err(|_| IoClosed)
    }

    /// Begin listening to a [Topic], receiving a [Subscription] that will give a
    /// stream of [Message][Topic::Message]s.
    ///
    /// If you subscribe to the same topic multiple times, the previous subscription
    /// will be closed (there can be only one).
    ///
    /// Returns an Error if the I/O worker is closed.
    pub async fn subscribe<T: Topic>(
        &self,
        depth: usize,
    ) -> Result<Subscription<T::Message>, IoClosed>
    where
        T::Message: DeserializeOwned,
    {
        let (tx, rx) = tokio::sync::mpsc::channel(depth);
        self.subber
            .send(SubInfo {
                key: T::TOPIC_KEY,
                tx,
            })
            .await
            .map_err(|_| IoClosed)?;
        Ok(Subscription {
            rx,
            _pd: PhantomData,
        })
    }
}

/// A structure that represents a subscription to the given topic
pub struct Subscription<M> {
    rx: Receiver<RpcFrame>,
    _pd: PhantomData<M>,
}

impl<M> Subscription<M>
where
    M: DeserializeOwned,
{
    /// Await a message for the given subscription.
    ///
    /// Returns [None]` if the subscription was closed
    pub async fn recv(&mut self) -> Option<M> {
        loop {
            let frame = self.rx.recv().await?;
            if let Ok(m) = postcard::from_bytes(&frame.body) {
                return Some(m);
            }
        }
    }
}

// Manual Clone impl because WireErr may not impl Clone
impl<WireErr> Clone for HostClient<WireErr> {
    fn clone(&self) -> Self {
        Self {
            ctx: self.ctx.clone(),
            out: self.out.clone(),
            err_key: self.err_key,
            _pd: PhantomData,
            subber: self.subber.clone(),
        }
    }
}

/// A new subscription that should be accounted for
pub struct SubInfo {
    pub key: Key,
    pub tx: Sender<RpcFrame>,
}

/// Items necessary for implementing a custom I/O Task
pub struct WireContext {
    /// This is a stream of frames that should be placed on the
    /// wire towards the server.
    pub outgoing: Receiver<RpcFrame>,
    /// This shared information contains the WaitMap used for replying to
    /// open requests.
    pub incoming: Arc<HostContext>,
    /// This is a stream of new subscriptions that should be tracked
    pub new_subs: Receiver<SubInfo>,
}

/// A single postcard-rpc frame
pub struct RpcFrame {
    /// The wire header
    pub header: WireHeader,
    /// The serialized message payload
    pub body: Vec<u8>,
}

impl RpcFrame {
    /// Serialize the `RpcFrame` into a Vec of bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = postcard::to_stdvec(&self.header).expect("Alloc should never fail");
        out.extend_from_slice(&self.body);
        out
    }
}

/// Shared context between [HostClient] and the I/O worker task
pub struct HostContext {
    map: WaitMap<WireHeader, Vec<u8>>,
    seq: AtomicU32,
}

/// The I/O worker has closed.
#[derive(Debug)]
pub struct IoClosed;

/// Error for [HostContext::process].
#[derive(Debug, PartialEq)]
pub enum ProcessError {
    /// All [HostClient]s have been dropped, no further requests
    /// will be made and no responses will be processed.
    Closed,
}

impl HostContext {
    pub fn process(&self, frame: RpcFrame) -> Result<(), ProcessError> {
        if let WakeOutcome::Closed(_) = self.map.wake(&frame.header, frame.body) {
            Err(ProcessError::Closed)
        } else {
            Ok(())
        }
    }
}
