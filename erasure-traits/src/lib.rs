use core_futures_io::{AsyncRead, AsyncWrite};
use futures::{Sink, TryFuture, TryStream};

pub trait RawTransportCoalesce<T, U: AsyncRead, V: AsyncWrite, S> {
    type Coalesce: TryFuture<Ok = T>;

    fn coalesce(reader: U, writer: V, spawner: S) -> Self::Coalesce;
}

pub trait FramedTransportCoalesce<T, U: TryStream<Ok = Vec<u8>>, V: Sink<Vec<u8>>, S> {
    type Coalesce: TryFuture<Ok = T>;

    fn coalesce(stream: U, sink: V, spawner: S) -> Self::Coalesce;
}

pub trait RawTransportUnravel<T, U: AsyncRead, V: AsyncWrite, S> {
    type Unravel: TryFuture<Ok = ()>;

    fn unravel(item: T, reader: U, writer: V, spawner: S) -> Self::Unravel;
}

pub trait FramedTransportUnravel<T, U: TryStream<Ok = Vec<u8>>, V: Sink<Vec<u8>>, S> {
    type Unravel: TryFuture<Ok = ()>;

    fn unravel(item: T, stream: U, sink: V, spawner: S) -> Self::Unravel;
}
