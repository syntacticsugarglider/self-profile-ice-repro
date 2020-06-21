use bincode::{deserialize as from_slice, serialize as to_vec};
use core_error::Error;
use futures::{
    channel::{
        mpsc::{channel as mpsc, unbounded, Sender as MpscSender, UnboundedSender},
        oneshot::{channel, Receiver as OneshotReceiver},
    },
    future::Shared,
    lock::Mutex,
    ready,
    task::{Spawn, SpawnError, SpawnExt},
    FutureExt as _, Sink, Stream, StreamExt, TryStream, TryStreamExt,
};
use piper::{chan, Receiver, Sender};
use protocol::{
    future::MapOk,
    future::{ok, ready, Ready},
    CloneContext, ContextReference, Contextualize, Dispatch, Finalize, FinalizeImmediate, Fork,
    Future as _, FutureExt, Join, Notify, Read, ReferenceContext, ShareContext, Write,
};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    borrow::BorrowMut,
    collections::HashMap,
    convert::TryInto,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    task::{Context, Poll},
};
use thiserror::Error;

#[derive(Hash, PartialEq, Eq, Clone, Copy)]
pub struct ContextHandle(u32);

#[derive(Debug, Error, Clone)]
#[bounds(where E: Error + 'static)]
pub enum SerdeReadError<E> {
    #[error("error in underlying stream: {0}")]
    Stream(E),
    #[error("serde error")]
    Serde,
    #[error("received insufficient buffer")]
    Insufficient,
    #[error("stream completed early")]
    Terminated,
}

#[derive(Debug, Error, Clone)]
#[bounds(where E: Error + 'static)]
pub enum SerdeWriteError<E> {
    #[error("error in underlying sink: {0}")]
    Sink(#[source] E),
    #[error("serde error")]
    Serde,
}

#[derive(Debug, Error)]
#[bounds(where E: Error + 'static)]
pub enum WithSpawnError<E> {
    #[error("error in underlying protocol: {0}")]
    Protocol(#[source] E),
    #[error("spawn error: {0}")]
    Spawn(#[source] SpawnError),
}

pub struct Transport<S: Spawn, StreamError, SinkError, P> {
    id: ContextHandle,
    next_index: Arc<AtomicU32>,
    spawner: S,
    receiver: Receiver<Vec<u8>>,
    sender: MpscSender<Vec<u8>>,
    sink_error: Shared<OneshotReceiver<SerdeWriteError<SinkError>>>,
    stream_error: Shared<OneshotReceiver<SerdeReadError<StreamError>>>,
    new_channel_sender: UnboundedSender<(ContextHandle, Sender<Vec<u8>>)>,
    _marker: PhantomData<P>,
}

impl<S: Spawn + Clone, StreamError, SinkError, P> Transport<S, StreamError, SinkError, P> {
    fn next_id(&self) -> Self {
        let id = ContextHandle(self.next_index.fetch_add(2, Ordering::SeqCst));

        let (sender, receiver) = chan(1);
        let _ = self.new_channel_sender.unbounded_send((id, sender));

        Self {
            id,
            next_index: self.next_index.clone(),
            spawner: self.spawner.clone(),
            receiver,
            sender: self.sender.clone(),
            sink_error: self.sink_error.clone(),
            stream_error: self.stream_error.clone(),
            new_channel_sender: self.new_channel_sender.clone(),
            _marker: PhantomData,
        }
    }

    fn with_id(&self, id: ContextHandle) -> Self {
        let (sender, receiver) = chan(1);
        let _ = self.new_channel_sender.unbounded_send((id, sender));

        Self {
            id,
            next_index: self.next_index.clone(),
            spawner: self.spawner.clone(),
            receiver,
            sender: self.sender.clone(),
            sink_error: self.sink_error.clone(),
            stream_error: self.stream_error.clone(),
            new_channel_sender: self.new_channel_sender.clone(),
            _marker: PhantomData,
        }
    }
}

impl<S: Spawn + Clone, StreamError, SinkError, P> Clone
    for Transport<S, StreamError, SinkError, P>
{
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            next_index: self.next_index.clone(),
            spawner: self.spawner.clone(),
            receiver: self.receiver.clone(),
            stream_error: self.stream_error.clone(),
            sender: self.sender.clone(),
            sink_error: self.sink_error.clone(),
            new_channel_sender: self.new_channel_sender.clone(),
            _marker: PhantomData,
        }
    }
}

impl<S: Spawn, T, U, P> Unpin for Transport<S, T, U, P> {}

impl<S: Spawn, I: DeserializeOwned, T, U, P> Read<I> for Transport<S, T, U, P>
where
    T: Clone,
{
    type Error = SerdeReadError<T>;

    fn read(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<I, Self::Error>> {
        let this = &mut *self;

        if let Poll::Ready(Ok(error)) = Pin::new(&mut this.stream_error).poll(cx) {
            return Poll::Ready(Err(error));
        }

        let data =
            ready!(Pin::new(&mut this.receiver).poll_next(cx)).ok_or(SerdeReadError::Terminated)?;

        Poll::Ready(from_slice(&data[4..]).map_err(|_| SerdeReadError::Serde))
    }
}

impl<S: Spawn, I: Serialize, T, U, P> Write<I> for Transport<S, T, U, P>
where
    U: Clone,
{
    type Error = SerdeWriteError<U>;

    fn write(mut self: Pin<&mut Self>, item: I) -> Result<(), Self::Error> {
        let this = &mut *self;

        let mut data = this.id.0.to_be_bytes().as_ref().to_owned();
        data.append(&mut to_vec(&item).map_err(|_| SerdeWriteError::Serde)?);

        Pin::new(&mut this.sender).start_send(data).unwrap();

        Ok(())
    }

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let this = &mut *self;

        if let Poll::Ready(Ok(error)) = Pin::new(&mut this.sink_error).poll(cx) {
            return Poll::Ready(Err(error));
        }

        ready!(Pin::new(&mut this.sender).poll_ready(cx)).unwrap();

        Poll::Ready(Ok(()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        let this = &mut *self;

        if let Poll::Ready(Ok(error)) = Pin::new(&mut this.sink_error).poll(cx) {
            return Poll::Ready(Err(error));
        }

        ready!(Pin::new(&mut this.sender).poll_flush(cx)).unwrap();

        Poll::Ready(Ok(()))
    }
}

pub struct Coalesce<
    T: TryStream<Ok = Vec<u8>>,
    U: Sink<Vec<u8>>,
    S: Spawn,
    P: protocol::Coalesce<Transport<S, T::Error, U::Error, P>>,
> where
    P::Future: Unpin,
{
    fut: P::Future,
    transport: Transport<S, T::Error, U::Error, P>,
    initializer: Option<Box<dyn FnOnce() -> Result<(), SpawnError> + Send>>,
}

enum UnravelState<T, U> {
    Target(T),
    Finalize(U),
}

pub struct Unravel<
    T: TryStream<Ok = Vec<u8>>,
    U: Sink<Vec<u8>>,
    S: Spawn,
    P: protocol::Unravel<Transport<S, T::Error, U::Error, P>>,
> where
    P::Target: Unpin,
    P::Finalize: Unpin,
{
    fut: UnravelState<P::Target, P::Finalize>,
    transport: Transport<S, T::Error, U::Error, P>,
    initializer: Option<Box<dyn FnOnce() -> Result<(), SpawnError> + Send>>,
}

impl<
        T: TryStream<Ok = Vec<u8>>,
        U: Sink<Vec<u8>>,
        S: Spawn,
        P: protocol::Unravel<Transport<S, T::Error, U::Error, P>>,
    > Future for Unravel<T, U, S, P>
where
    P::Target: Unpin,
    P::Finalize: Unpin,
{
    type Output = Result<
        (),
        WithSpawnError<<P::Target as protocol::Future<Transport<S, T::Error, U::Error, P>>>::Error>,
    >;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = &mut *self;

        if let Some(initializer) = this.initializer.take() {
            (initializer)().map_err(WithSpawnError::Spawn)?;
        }

        loop {
            match &mut this.fut {
                UnravelState::Target(future) => {
                    let finalize = ready!(Pin::new(future).poll(cx, &mut this.transport))
                        .map_err(WithSpawnError::Protocol)?;
                    this.fut = UnravelState::Finalize(finalize);
                }
                UnravelState::Finalize(future) => {
                    ready!(Pin::new(future).poll(cx, &mut this.transport))
                        .map_err(WithSpawnError::Protocol)?;
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

impl<
        T: TryStream<Ok = Vec<u8>>,
        U: Sink<Vec<u8>>,
        S: Spawn,
        P: protocol::Coalesce<Transport<S, T::Error, U::Error, P>>,
    > Future for Coalesce<T, U, S, P>
where
    P::Future: Unpin,
{
    type Output = Result<
        P,
        WithSpawnError<<P::Future as protocol::Future<Transport<S, T::Error, U::Error, P>>>::Error>,
    >;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = &mut *self;

        if let Some(initializer) = this.initializer.take() {
            (initializer)().map_err(WithSpawnError::Spawn)?;
        }

        Pin::new(&mut this.fut)
            .poll(cx, &mut this.transport)
            .map_err(WithSpawnError::Protocol)
    }
}

impl<
        T: Unpin + TryStream<Ok = Vec<u8>> + Send + 'static,
        U: Sink<Vec<u8>> + Send + 'static,
        S: Clone + Send + Spawn + 'static,
        P: protocol::Coalesce<Transport<S, T::Error, U::Error, P>>,
    > Coalesce<T, U, S, P>
where
    P::Future: Unpin,
    T::Error: Clone + Send,
    U::Error: Clone + Send,
{
    pub fn new(stream: T, sink: U, spawner: S) -> Self {
        let (b_sender, receiver) = chan(1);
        let (sender, b_receiver) = mpsc(1);

        let (sink_error_sender, sink_error) = channel();
        let (stream_error_sender, stream_error) = channel();

        let sink_error = sink_error.shared();
        let stream_error = stream_error.shared();

        let (new_channel_sender, mut new_channels) = unbounded();

        let mut channels = HashMap::<ContextHandle, Storage<Vec<u8>>>::new();

        channels.insert(ContextHandle(0), Storage::Channel(b_sender));

        let channels = Arc::new(Mutex::new(channels));

        let channels_handle = channels.clone();

        let s = spawner.clone();

        let initializer = move || {
            spawner.spawn(async move {
                if let Err(e) = b_receiver.map(Ok).forward(sink).await {
                    let _ = sink_error_sender.send(SerdeWriteError::Sink(e));
                }
            })?;

            spawner.spawn(async move {
                while let Some((handle, channel)) = new_channels.next().await {
                    channels_handle
                        .lock()
                        .await
                        .entry(handle)
                        .or_insert_with(|| Storage::Temporary(vec![]))
                        .upgrade(channel)
                        .await;
                }
            })?;

            let mut stream = stream.into_stream();

            spawner.spawn(async move {
                while let Some(data) = stream.next().await {
                    match data {
                        Err(e) => {
                            let _ = stream_error_sender.send(SerdeReadError::Stream(e));
                            break;
                        }
                        Ok(data) => {
                            let id = data.get(..4);
                            match id {
                                Some(id) => {
                                    let handle =
                                        ContextHandle(u32::from_be_bytes(id.try_into().unwrap()));
                                    channels
                                        .lock()
                                        .await
                                        .entry(handle)
                                        .or_insert_with(|| Storage::Temporary(vec![]))
                                        .send(data)
                                        .await;
                                }
                                None => {
                                    let _ = stream_error_sender.send(SerdeReadError::Insufficient);
                                    break;
                                }
                            }
                        }
                    }
                }
            })?;

            Ok(())
        };

        Coalesce {
            transport: Transport {
                next_index: Arc::new(AtomicU32::new(2)),
                spawner: s,
                sender,
                receiver,
                sink_error,
                id: ContextHandle(0),
                _marker: PhantomData,
                stream_error,
                new_channel_sender,
            },
            initializer: Some(Box::new(initializer)),
            fut: P::coalesce(),
        }
    }
}

enum Storage<T> {
    Temporary(Vec<T>),
    Channel(Sender<T>),
}

impl<T> Storage<T> {
    async fn send(&mut self, item: T) {
        match self {
            Storage::Temporary(data) => data.push(item),
            Storage::Channel(sender) => sender.send(item).await,
        }
    }

    async fn upgrade(&mut self, channel: Sender<T>) {
        match self {
            Storage::Temporary(data) => {
                for item in data.drain(..) {
                    channel.send(item).await;
                }
                *self = Storage::Channel(channel);
            }
            Storage::Channel(_) => panic!("attempted redundant channel upgrade"),
        }
    }
}

impl<
        T: TryStream<Ok = Vec<u8>> + Unpin + Send + 'static,
        U: Sink<Vec<u8>> + Send + 'static,
        S: Clone + Send + Spawn + 'static,
        P: protocol::Unravel<Transport<S, T::Error, U::Error, P>>,
    > Unravel<T, U, S, P>
where
    P::Target: Unpin,
    T::Error: Clone + Send,
    U::Error: Clone + Send,
    P::Finalize: Unpin,
{
    pub fn new(stream: T, sink: U, spawner: S, item: P) -> Self {
        let (b_sender, receiver) = chan(1);
        let (sender, b_receiver) = mpsc(1);

        let (sink_error_sender, sink_error) = channel();
        let (stream_error_sender, stream_error) = channel();

        let sink_error = sink_error.shared();
        let stream_error = stream_error.shared();

        let (new_channel_sender, mut new_channels) = unbounded();

        let mut channels = HashMap::<ContextHandle, Storage<Vec<u8>>>::new();

        channels.insert(ContextHandle(0), Storage::Channel(b_sender));

        let channels = Arc::new(Mutex::new(channels));

        let channels_handle = channels.clone();

        let s = spawner.clone();

        let initializer = move || {
            spawner.spawn(async move {
                if let Err(e) = b_receiver.map(|item| Ok(item)).forward(sink).await {
                    let _ = sink_error_sender.send(SerdeWriteError::Sink(e));
                }
            })?;

            spawner.spawn(async move {
                while let Some((handle, channel)) = new_channels.next().await {
                    channels_handle
                        .lock()
                        .await
                        .entry(handle)
                        .or_insert_with(|| Storage::Temporary(vec![]))
                        .upgrade(channel)
                        .await;
                }
            })?;

            let mut stream = stream.into_stream();

            spawner.spawn(async move {
                while let Some(data) = stream.next().await {
                    match data {
                        Err(e) => {
                            let _ = stream_error_sender.send(SerdeReadError::Stream(e));
                            break;
                        }
                        Ok(data) => {
                            let id = data.get(..4);
                            match id {
                                Some(id) => {
                                    let handle =
                                        ContextHandle(u32::from_be_bytes(id.try_into().unwrap()));
                                    channels
                                        .lock()
                                        .await
                                        .entry(handle)
                                        .or_insert_with(|| Storage::Temporary(vec![]))
                                        .send(data)
                                        .await;
                                }
                                None => {
                                    let _ = stream_error_sender.send(SerdeReadError::Insufficient);
                                    break;
                                }
                            }
                        }
                    }
                }
            })?;

            Ok(())
        };

        Unravel {
            transport: Transport {
                next_index: Arc::new(AtomicU32::new(1)),
                spawner: s,
                sender,
                receiver,
                sink_error,
                id: ContextHandle(0),
                _marker: PhantomData,
                stream_error,
                new_channel_sender,
            },
            initializer: Some(Box::new(initializer)),
            fut: UnravelState::Target(item.unravel()),
        }
    }
}

impl<S: Spawn, T, U, P: protocol::Coalesce<Self>, M> Dispatch<P> for Transport<S, T, U, M> {
    type Handle = ();
}

impl<S: Spawn, T, U, P, M> Dispatch<Notification<P>> for Transport<S, T, U, M> {
    type Handle = ();
}

impl<S: Spawn, T, U, P: protocol::Coalesce<Self> + protocol::Unravel<Self>, M> Fork<P>
    for Transport<S, T, U, M>
where
    <P as protocol::Unravel<Self>>::Target: Unpin,
{
    type Finalize = <P as protocol::Unravel<Self>>::Finalize;
    type Target = <P as protocol::Unravel<Self>>::Target;
    type Future = Ready<(Self::Target, ())>;

    fn fork(&mut self, item: P) -> Self::Future {
        ok((item.unravel(), ()))
    }
}

impl<S: Spawn, T, U, P: protocol::Coalesce<Self>, M> Join<P> for Transport<S, T, U, M> {
    type Future = <P as protocol::Coalesce<Self>>::Future;

    fn join(&mut self, _: ()) -> Self::Future {
        P::coalesce()
    }
}

pub struct Notification<P>(P);

impl<S: Spawn, T, U, P: protocol::Coalesce<Self>, M> Join<Notification<P>> for Transport<S, T, U, M>
where
    <P as protocol::Coalesce<Self>>::Future: Unpin,
{
    type Future = MapOk<<P as protocol::Coalesce<Self>>::Future, fn(P) -> Notification<P>>;

    fn join(&mut self, _: ()) -> Self::Future {
        P::coalesce().map_ok(Notification)
    }
}

impl<S: Spawn, T, U, P: protocol::Unravel<Self>, M> Fork<Notification<P>> for Transport<S, T, U, M>
where
    <P as protocol::Unravel<Self>>::Target: Unpin,
{
    type Finalize = <P as protocol::Unravel<Self>>::Finalize;
    type Target = <P as protocol::Unravel<Self>>::Target;
    type Future = Ready<(Self::Target, ())>;

    fn fork(&mut self, item: Notification<P>) -> Self::Future {
        ok((item.0.unravel(), ()))
    }
}

impl<S: Spawn, T, U, P: protocol::Unravel<Self> + protocol::Coalesce<Self> + Unpin, M> Notify<P>
    for Transport<S, T, U, M>
where
    <P as protocol::Unravel<Self>>::Target: Unpin,
    <P as protocol::Coalesce<Self>>::Future: Unpin,
{
    type Notification = Notification<P>;
    type Wrap = Ready<Notification<P>>;
    type Unwrap = Ready<P>;

    fn unwrap(&mut self, notification: Self::Notification) -> Self::Unwrap {
        ok(notification.0)
    }

    fn wrap(&mut self, item: P) -> Self::Wrap {
        ok(Notification(item))
    }
}

pub struct Contextualized<S: Spawn, T, U, F, P> {
    fut: F,
    transport: Transport<S, T, U, P>,
}

impl<S: Spawn + Unpin, T, U, F: Unpin + protocol::Future<Transport<S, T, U, P>>, P> Future
    for Contextualized<S, T, U, F, P>
{
    type Output = Result<F::Ok, F::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = &mut *self;

        Pin::new(&mut this.fut).poll(cx, &mut this.transport)
    }
}

impl<S: Spawn, T, U, P> Contextualize for Transport<S, T, U, P> {
    type Handle = u32;
}

impl<S: Spawn + Clone + Unpin, T, U, P> CloneContext for Transport<S, T, U, P> {
    type Context = Transport<S, T, U, P>;
    type ForkOutput = Ready<(Transport<S, T, U, P>, u32)>;
    type JoinOutput = Ready<Transport<S, T, U, P>>;

    fn fork_owned(&mut self) -> Self::ForkOutput {
        let tport = self.next_id();
        let id = tport.id.0;
        ok((tport, id))
    }

    fn join_owned(&mut self, id: Self::Handle) -> Self::JoinOutput {
        ok(self.with_id(ContextHandle(id)))
    }
}

impl<S: Spawn + Clone + Unpin, T, U, P> ShareContext for Transport<S, T, U, P> {
    type Context = Transport<S, T, U, P>;
    type ForkOutput = Ready<(Transport<S, T, U, P>, u32)>;
    type JoinOutput = Ready<Transport<S, T, U, P>>;

    fn fork_shared(&mut self) -> Self::ForkOutput {
        let tport = self.next_id();
        let id = tport.id.0;
        ok((tport, id))
    }

    fn join_shared(&mut self, id: Self::Handle) -> Self::JoinOutput {
        ok(self.with_id(ContextHandle(id)))
    }
}

impl<S: Spawn, T, U, P> ContextReference<Transport<S, T, U, P>> for Transport<S, T, U, P> {
    type Target = Transport<S, T, U, P>;

    fn with<'a, 'b: 'a, R: BorrowMut<Transport<S, T, U, P>> + 'b>(
        &'a mut self,
        _: R,
    ) -> &'a mut Self::Target {
        self
    }
}

impl<S: Spawn + Clone + Unpin, T, U, P> ReferenceContext for Transport<S, T, U, P> {
    type Context = Transport<S, T, U, P>;
    type ForkOutput = Ready<(Transport<S, T, U, P>, u32)>;
    type JoinOutput = Ready<Transport<S, T, U, P>>;

    fn fork_ref(&mut self) -> Self::ForkOutput {
        let tport = self.next_id();
        let id = tport.id.0;
        ok((tport, id))
    }

    fn join_ref(&mut self, id: Self::Handle) -> Self::JoinOutput {
        ok(self.with_id(ContextHandle(id)))
    }
}

impl<
        F: Send + Unpin + protocol::Future<Self> + 'static,
        S: Send + Unpin + Spawn + Clone + 'static,
        T: Send + Sync + 'static,
        U: Send + Sync + 'static,
        P: Send + 'static,
    > Finalize<F> for Transport<S, T, U, P>
{
    type Target = Self;
    type Output = Ready<(), SpawnError>;

    fn finalize(&mut self, fut: F) -> Self::Output {
        ready(
            self.spawner.spawn(
                Contextualized {
                    fut,
                    transport: self.clone(),
                }
                .map(|_| ()),
            ),
        )
    }
}

impl<
        F: Send + Unpin + protocol::Future<Self> + 'static,
        S: Send + Unpin + Spawn + Clone + 'static,
        T: Send + Sync + 'static,
        U: Send + Sync + 'static,
        P: Send + 'static,
    > FinalizeImmediate<F> for Transport<S, T, U, P>
{
    type Target = Self;
    type Error = SpawnError;

    fn finalize_immediate(&mut self, fut: F) -> Result<(), SpawnError> {
        self.spawner.spawn(
            Contextualized {
                fut,
                transport: self.clone(),
            }
            .map(|_| ()),
        )
    }
}

#[cfg(feature = "vessels")]
pub struct ProtocolMveTransport;

#[cfg(feature = "vessels")]
mod vessels {
    use super::{Coalesce, ProtocolMveTransport, Transport, Unravel};
    use erasure_traits::{FramedTransportCoalesce, FramedTransportUnravel};
    use futures::{task::Spawn, Sink, TryStream};

    impl<
            U: TryStream<Ok = Vec<u8>>,
            V: Sink<Vec<u8>>,
            T: protocol::Coalesce<Transport<S, U::Error, V::Error, T>>,
            S: Spawn + Unpin,
        > FramedTransportCoalesce<T, U, V, S> for ProtocolMveTransport
    where
        T::Future: Unpin,
        U::Error: Clone + Send,
        V::Error: Clone + Send,
        U: Send + Unpin + 'static,
        V: Send + 'static,
        S: Clone + Send + 'static,
    {
        type Coalesce = Coalesce<U, V, S, T>;

        fn coalesce(stream: U, sink: V, spawner: S) -> Self::Coalesce {
            Coalesce::new(stream, sink, spawner)
        }
    }

    impl<
            U: TryStream<Ok = Vec<u8>>,
            V: Sink<Vec<u8>>,
            T: protocol::Unravel<Transport<S, U::Error, V::Error, T>>,
            S: Spawn + Unpin,
        > FramedTransportUnravel<T, U, V, S> for ProtocolMveTransport
    where
        T::Target: Unpin,
        T::Finalize: Unpin,
        U::Error: Clone + Send,
        V::Error: Clone + Send,
        U: Send + Unpin + 'static,
        V: Send + 'static,
        S: Clone + Send + 'static,
    {
        type Unravel = Unravel<U, V, S, T>;

        fn unravel(item: T, stream: U, sink: V, spawner: S) -> Self::Unravel {
            Unravel::new(stream, sink, spawner, item)
        }
    }
}
