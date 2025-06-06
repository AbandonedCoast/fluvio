use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_channel::Sender;
use fluvio_future::timer::sleep;
use fluvio_protocol::{link::ErrorCode, record::ConsumerRecord as Record};
use futures_util::stream::select_all;
use futures_util::{future::try_join_all, ready, FutureExt};
use futures_util::Stream;
use tokio::select;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use super::config::OffsetManagementStrategy;
use super::{offset::OffsetLocalStore, StreamToServer};

#[cfg(not(target_arch = "wasm32"))]
pub type ConsumerBoxFuture<'a> = futures_util::future::BoxFuture<'a, Result<(), ErrorCode>>;

#[cfg(target_arch = "wasm32")]
pub type ConsumerBoxFuture<'a> = futures_util::future::LocalBoxFuture<'a, Result<(), ErrorCode>>;

/// Extension of [`Stream`] trait with offset management capabilities.
pub trait ConsumerStream: Stream<Item = Result<Record, ErrorCode>> + Unpin {
    /// Mark the offset of the last yelded record as committed. Depending on [`OffsetManagementStrategy`]
    /// it may require a subsequent `offset_flush()` call to take any effect.
    fn offset_commit(&mut self) -> ConsumerBoxFuture<'_>;

    /// Send the committed offset to the server. The method waits for the server's acknowledgment before it finishes.
    fn offset_flush(&mut self) -> ConsumerBoxFuture<'_>;
}

pub struct MultiplePartitionConsumerStream<T> {
    partition_streams: futures_util::stream::SelectAll<SinglePartitionConsumerStream<T>>,
    offset_mgnts: Vec<Arc<OffsetManagement>>,
}

pub struct SinglePartitionConsumerStream<T> {
    offset_mngt: Arc<OffsetManagement>,
    inner: T,
}

impl<T> Drop for SinglePartitionConsumerStream<T> {
    fn drop(&mut self) {
        let offset_mngt = self.offset_mngt.clone();
        if let OffsetManagement::Auto { auto_flusher, .. } = &*offset_mngt {
            auto_flusher.stop_background.notify_one();
        }
    }
}

enum OffsetManagement {
    None,
    Manual {
        offset_store: OffsetLocalStore,
    },
    Auto {
        auto_flusher: AutomaticFlusher,
        flush_period: Duration,
        flusher_check_period: Duration,
        offset_store: OffsetLocalStore,
        last_flush_time: AtomicU64,
    },
}

pub struct AutomaticFlusher {
    stop_background: Arc<Notify>,
}

impl AutomaticFlusher {
    fn new() -> Self {
        Self {
            stop_background: Arc::new(Notify::new()),
        }
    }
}

impl<T: Stream<Item = Result<Record, ErrorCode>> + Unpin> MultiplePartitionConsumerStream<T> {
    pub(crate) fn new<I>(streams: I) -> Self
    where
        I: IntoIterator<Item = SinglePartitionConsumerStream<T>>,
    {
        let mut partition_streams = Vec::new();
        let mut offset_mgnts = Vec::new();
        for partition_stream in streams.into_iter() {
            offset_mgnts.push(partition_stream.offset_mngt.clone());
            partition_streams.push(partition_stream);
        }
        let partition_streams = select_all(partition_streams);
        Self {
            partition_streams,
            offset_mgnts,
        }
    }
}

impl<T> SinglePartitionConsumerStream<T> {
    pub(super) fn new(
        inner: T,
        offset_strategy: OffsetManagementStrategy,
        flush_period: Duration,
        flusher_check_period: Duration,
        stream_to_server: Sender<StreamToServer>,
    ) -> Self {
        let offset_mngt = match offset_strategy {
            OffsetManagementStrategy::None => OffsetManagement::None,
            OffsetManagementStrategy::Manual => OffsetManagement::Manual {
                offset_store: OffsetLocalStore::new(stream_to_server),
            },
            OffsetManagementStrategy::Auto => OffsetManagement::Auto {
                auto_flusher: AutomaticFlusher::new(),
                offset_store: OffsetLocalStore::new(stream_to_server),
                flush_period,
                flusher_check_period,
                last_flush_time: AtomicU64::new(0),
            },
        };
        let offset_mngt = Arc::new(offset_mngt);

        // Start the background flusher for auto strategy
        let mngt = offset_mngt.clone();
        if matches!(*mngt, OffsetManagement::Auto { .. }) {
            fluvio_future::task::spawn(async move {
                mngt.start_background_flusher().await;
            });
        }

        Self { offset_mngt, inner }
    }
}

impl<T: Stream<Item = Result<Record, ErrorCode>> + Unpin> Stream
    for SinglePartitionConsumerStream<T>
{
    type Item = Result<Record, ErrorCode>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let self_mut = self.get_mut();
        let pinned = std::pin::pin!(&mut self_mut.inner);
        match ready!(pinned.poll_next(cx)) {
            Some(Ok(last)) => {
                self_mut.offset_mngt.update(last.offset);
                std::task::Poll::Ready(Some(Ok(last)))
            }
            other => {
                self_mut.offset_mngt.run_auto_flush();
                std::task::Poll::Ready(other)
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl ConsumerStream for Pin<Box<dyn ConsumerStream + Send>> {
    fn offset_commit(&mut self) -> ConsumerBoxFuture {
        Box::pin(async move { self.as_mut().offset_commit().await })
    }

    fn offset_flush(&mut self) -> ConsumerBoxFuture {
        Box::pin(async move { self.as_mut().offset_flush().await })
    }
}

#[cfg(target_arch = "wasm32")]
impl ConsumerStream for Pin<Box<dyn ConsumerStream>> {
    fn offset_commit(&mut self) -> ConsumerBoxFuture {
        Box::pin(async move { self.as_mut().offset_commit().await })
    }

    fn offset_flush(&mut self) -> ConsumerBoxFuture {
        Box::pin(async move { self.as_mut().offset_flush().await })
    }
}

impl<T: Stream<Item = Result<Record, ErrorCode>> + Unpin> ConsumerStream
    for SinglePartitionConsumerStream<T>
{
    fn offset_commit(&mut self) -> ConsumerBoxFuture {
        Box::pin(async { self.offset_mngt.commit() })
    }

    fn offset_flush(&mut self) -> ConsumerBoxFuture {
        Box::pin(self.offset_mngt.flush())
    }
}

impl<T: Stream<Item = Result<Record, ErrorCode>> + Unpin> ConsumerStream
    for MultiplePartitionConsumerStream<T>
{
    fn offset_commit(&mut self) -> ConsumerBoxFuture {
        for partition in &self.offset_mgnts {
            if let Err(err) = partition.commit() {
                return Box::pin(async { Err(err) });
            }
        }

        Box::pin(async { Ok(()) })
    }

    fn offset_flush(&mut self) -> ConsumerBoxFuture {
        let futures: Vec<_> = self.offset_mgnts.iter().map(|p| p.flush()).collect();
        Box::pin(try_join_all(futures).map(|r| r.map(|_| ())))
    }
}

impl<T: Stream<Item = Result<Record, ErrorCode>> + Unpin> Stream
    for MultiplePartitionConsumerStream<T>
{
    type Item = T::Item;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let self_mut = self.get_mut();
        let pinned = std::pin::pin!(&mut self_mut.partition_streams);
        pinned.poll_next(cx)
    }
}

impl OffsetManagement {
    async fn start_background_flusher(&self) {
        match self {
            OffsetManagement::Auto { auto_flusher, .. } => loop {
                let check_period = sleep(std::time::Duration::from_millis(100));
                select! {
                    _ = auto_flusher.stop_background.notified() => {
                        self.run_auto_flush();
                        break;
                    }
                    _ = check_period => {
                        self.run_auto_flush();
                    }
                }
            },
            _ => {
                // No background task for None or Manual
            }
        }
    }

    fn update(&self, offset: i64) {
        match self {
            OffsetManagement::None => {}
            OffsetManagement::Manual { offset_store } => {
                offset_store.update(offset);
            }
            OffsetManagement::Auto { offset_store, .. } => {
                offset_store.update(offset);
                offset_store.commit();
                self.run_auto_flush();
            }
        };
    }

    fn run_auto_flush(&self) {
        if let OffsetManagement::Auto {
            flush_period,
            offset_store,
            last_flush_time,
            ..
        } = self
        {
            if Duration::from_secs(now_timestamp_secs() - last_flush_time.load(Ordering::Relaxed))
                >= *flush_period
            {
                debug!("auto flush offset");
                if let Err(err) = offset_store.try_flush() {
                    warn!("auto flush failed: {err:?}");
                }
                last_flush_time.store(now_timestamp_secs(), Ordering::Relaxed);
            } else {
                debug!("auto flush skipped");
            }
        };
    }

    fn commit(&self) -> Result<(), ErrorCode> {
        match self {
            OffsetManagement::None => Err(ErrorCode::OffsetManagementDisabled),
            OffsetManagement::Manual { offset_store } => {
                offset_store.commit();
                Ok(())
            }
            OffsetManagement::Auto { offset_store, .. } => {
                offset_store.commit();
                Ok(())
            }
        }
    }

    async fn flush(&self) -> Result<(), ErrorCode> {
        match self {
            OffsetManagement::None => Err(ErrorCode::OffsetManagementDisabled),
            OffsetManagement::Manual { offset_store } => offset_store.flush().await,
            OffsetManagement::Auto {
                offset_store,
                last_flush_time,
                ..
            } => {
                offset_store
                    .flush()
                    .await
                    .map_err(|e| ErrorCode::Other(e.to_string()))?;
                last_flush_time.store(now_timestamp_secs(), Ordering::Relaxed);
                Ok(())
            }
        }
    }
}

impl Drop for OffsetManagement {
    fn drop(&mut self) {
        if let &mut OffsetManagement::Auto {
            ref mut offset_store,
            ref auto_flusher,
            ..
        } = self
        {
            offset_store.commit();
            auto_flusher.stop_background.notify_one();
            if let Err(err) = offset_store.try_flush() {
                warn!("flush on drop failed: {err:?}");
            }
            info!("offsets flushed on drop, with: {}", offset_store);
        }
    }
}

fn now_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use std::vec::IntoIter;

    use fluvio_future::timer::sleep;
    use fluvio_protocol::record::Batch;
    use fluvio_smartmodule::RecordData;
    use fluvio_types::PartitionId;
    use futures_util::{stream::Iter, StreamExt};

    use super::*;

    #[fluvio_future::test]
    async fn test_single_partition_stream_works() {
        //given
        let (tx, _rx) = async_channel::unbounded();
        let partition_stream = SinglePartitionConsumerStream::new(
            records_stream(0, ["1", "2"]),
            Default::default(),
            Default::default(),
            Duration::from_millis(100),
            tx,
        );

        //when
        let result: Vec<_> = partition_stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("no error")
            .into_iter()
            .map(|r| String::from_utf8_lossy(r.as_ref()).to_string())
            .collect();

        //then
        assert_eq!(result, ["1", "2"]);
    }

    #[fluvio_future::test]
    async fn test_multi_partition_stream_works() {
        //given
        let (tx, _rx) = async_channel::unbounded();
        let partition_stream1 = SinglePartitionConsumerStream::new(
            records_stream(0, ["1"]),
            Default::default(),
            Default::default(),
            Duration::from_millis(100),
            tx,
        );
        let (tx, _rx) = async_channel::unbounded();
        let partition_stream2 = SinglePartitionConsumerStream::new(
            records_stream(1, ["2", "4", "6"]),
            Default::default(),
            Default::default(),
            Duration::from_millis(100),
            tx,
        );
        let (tx, _rx) = async_channel::unbounded();
        let partition_stream3 = SinglePartitionConsumerStream::new(
            records_stream(2, ["3", "5"]),
            Default::default(),
            Default::default(),
            Duration::from_millis(100),
            tx,
        );
        let multi_stream = MultiplePartitionConsumerStream::new([
            partition_stream1,
            partition_stream2,
            partition_stream3,
        ]);

        //when
        let result: Vec<_> = multi_stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("no error")
            .into_iter()
            .map(|r| String::from_utf8_lossy(r.as_ref()).to_string())
            .collect();

        //then
        assert_eq!(result, ["1", "2", "3", "4", "5", "6"]);
    }

    #[fluvio_future::test]
    async fn test_none_offset_strategy_raise_error_on_commit() {
        //given
        let (tx, _rx) = async_channel::unbounded();
        let mut partition_stream = SinglePartitionConsumerStream::new(
            records_stream(0, []),
            OffsetManagementStrategy::None,
            Default::default(),
            Duration::from_millis(100),
            tx,
        );

        //when
        let res = partition_stream.offset_commit().await;

        //then
        assert_eq!(res, Err(ErrorCode::OffsetManagementDisabled));
    }

    #[fluvio_future::test]
    async fn test_none_offset_strategy_raise_error_on_flush() {
        //given
        let (tx, _rx) = async_channel::unbounded();
        let mut partition_stream = SinglePartitionConsumerStream::new(
            records_stream(0, []),
            OffsetManagementStrategy::None,
            Default::default(),
            Duration::from_millis(100),
            tx,
        );

        //when
        let res = partition_stream.offset_flush().await;

        //then
        assert_eq!(res, Err(ErrorCode::OffsetManagementDisabled));
    }

    #[fluvio_future::test]
    async fn test_single_partition_stream_commit_and_flush_on_manual() {
        //given
        let (tx, rx) = async_channel::unbounded();
        let mut partition_stream = SinglePartitionConsumerStream::new(
            records_stream(0, ["1", "2", "3", "4"]),
            OffsetManagementStrategy::Manual,
            Default::default(),
            Duration::from_millis(100),
            tx,
        );

        //when
        assert!(partition_stream.next().await.is_some()); // seen = 0
        assert!(partition_stream.next().await.is_some()); // seen = 1
        let _ = partition_stream.offset_commit().await; // comitted = 1
        assert!(partition_stream.next().await.is_some()); // seen = 2
        let _ = partition_stream.offset_commit().await; // comitted = 2

        //then
        fluvio_future::task::spawn(async move {
            //then
            let message = rx.recv().await;
            assert!(matches!(
                message,
                Ok(StreamToServer::FlushManagedOffset { callback: _, offset }) if offset == 2
            ));
            if let Ok(StreamToServer::FlushManagedOffset {
                offset: _,
                callback,
            }) = message
            {
                callback.send(ErrorCode::None).await;
            }
        });

        assert!(partition_stream.offset_flush().await.is_ok());
        assert!(partition_stream.offset_flush().await.is_ok()); // ignored, nothing to flush
    }

    #[fluvio_future::test]
    async fn test_multi_partition_stream_commit_and_flush_on_manual() {
        //given
        let (tx1, rx1) = async_channel::unbounded();
        let partition_stream1 = SinglePartitionConsumerStream::new(
            records_stream(0, ["1"]),
            OffsetManagementStrategy::Manual,
            Default::default(),
            Duration::from_millis(100),
            tx1,
        );
        let (tx2, rx2) = async_channel::unbounded();
        let partition_stream2 = SinglePartitionConsumerStream::new(
            records_stream(1, ["2", "4", "6"]),
            OffsetManagementStrategy::Manual,
            Default::default(),
            Duration::from_millis(100),
            tx2,
        );
        let mut multi_stream =
            MultiplePartitionConsumerStream::new([partition_stream1, partition_stream2]);

        //when
        assert!(multi_stream.next().await.is_some()); // p1 seen = 0
        assert!(multi_stream.next().await.is_some()); // p2 seen = 0
        let _ = multi_stream.offset_commit().await; // both comitted = 0
        assert!(multi_stream.next().await.is_some()); // p2 seen = 1
        let _ = multi_stream.offset_commit().await; // comitted p1 = 0, p2 = 1

        //then
        fluvio_future::task::spawn(async move {
            //then
            let message = rx1.recv().await;
            assert!(
                matches!(
                    message,
                    Ok(StreamToServer::FlushManagedOffset { callback: _, offset }) if offset == 0
                ),
                "{message:?}"
            );
            if let Ok(StreamToServer::FlushManagedOffset {
                offset: _,
                callback,
            }) = message
            {
                callback.send(ErrorCode::None).await;
            }
        });
        fluvio_future::task::spawn(async move {
            //then
            let message = rx2.recv().await;
            assert!(
                matches!(
                    message,
                    Ok(StreamToServer::FlushManagedOffset { callback: _, offset }) if offset == 1
                ),
                "{message:?}"
            );
            if let Ok(StreamToServer::FlushManagedOffset {
                offset: _,
                callback,
            }) = message
            {
                callback.send(ErrorCode::None).await;
            }
        });

        assert!(multi_stream.offset_flush().await.is_ok());
        assert!(multi_stream.offset_flush().await.is_ok()); // ignored, nothing to flush
    }

    #[fluvio_future::test]
    async fn test_single_partition_stream_auto_commit_and_flush_on_drop() {
        //given
        let (tx, rx) = async_channel::unbounded();
        let mut partition_stream = SinglePartitionConsumerStream::new(
            records_stream(0, ["1", "2", "3", "4"]),
            OffsetManagementStrategy::Auto,
            Duration::from_secs(1000),
            Duration::from_millis(100),
            tx,
        );

        //when
        assert!(partition_stream.next().await.is_some()); // seen = 0
        assert!(partition_stream.next().await.is_some()); // seen = 1
        assert!(partition_stream.next().await.is_some()); // seen = 2
        drop(partition_stream);

        //then
        let message1 = rx.recv().await;
        assert!(
            matches!(
                message1,
                Ok(StreamToServer::FlushManagedOffset { callback: _, offset }) if offset == 0
            ),
            "{message1:?}"
        );
        let message2 = rx.recv().await;
        assert!(
            matches!(
                message2,
                Ok(StreamToServer::FlushManagedOffset { callback: _, offset }) if offset == 2
            ),
            "{message2:?}"
        );

        let message3 = rx.try_recv();
        assert!(message3.is_err(), "{message3:?}")
    }

    #[fluvio_future::test]
    async fn test_multi_partition_stream_auto_commit_and_flush_on_drop() {
        //given
        let (tx1, rx1) = async_channel::unbounded();
        let partition_stream1 = SinglePartitionConsumerStream::new(
            records_stream(0, ["1"]),
            OffsetManagementStrategy::Auto,
            Duration::from_secs(1000),
            Duration::from_millis(100),
            tx1,
        );
        let (tx2, rx2) = async_channel::unbounded();
        let partition_stream2 = SinglePartitionConsumerStream::new(
            records_stream(1, ["2", "4", "6"]),
            OffsetManagementStrategy::Auto,
            Duration::from_secs(1000),
            Duration::from_millis(100),
            tx2,
        );
        let mut multi_stream =
            MultiplePartitionConsumerStream::new([partition_stream1, partition_stream2]);

        //when
        assert!(multi_stream.next().await.is_some()); // p1 seen = 0
        assert!(multi_stream.next().await.is_some()); // p2 seen = 0
        assert!(multi_stream.next().await.is_some()); // p2 seen = 1
        sleep(Duration::from_secs(1)).await;
        drop(multi_stream);

        //then
        {
            let message1 = rx1.recv().await;
            assert!(
                matches!(
                    message1,
                    Ok(StreamToServer::FlushManagedOffset { callback: _, offset }) if offset == 0
                ),
                "{message1:?}"
            );
            let message2 = rx1.try_recv();
            assert!(message2.is_err(), "{message2:?}");
        }
        {
            let message1 = rx2.recv().await;
            assert!(
                matches!(
                    message1,
                    Ok(StreamToServer::FlushManagedOffset { callback: _, offset }) if offset == 0
                ),
                "{message1:?}"
            );
            let message2 = rx2.recv().await;
            assert!(
                matches!(
                    message2,
                    Ok(StreamToServer::FlushManagedOffset { callback: _, offset }) if offset == 1
                ),
                "{message2:?}"
            );
            let message3 = rx2.try_recv();
            assert!(message3.is_err(), "{message3:?}");
        }
    }

    #[fluvio_future::test]
    async fn test_single_partition_stream_periodic_and_background_flush() {
        //given
        let (tx, rx) = async_channel::unbounded();
        let mut partition_stream = SinglePartitionConsumerStream::new(
            records_stream(0, ["1", "2", "3", "4"]),
            OffsetManagementStrategy::Auto,
            Duration::from_secs(1),
            Duration::from_millis(100),
            tx,
        );

        //when
        assert!(partition_stream.next().await.is_some()); // seen = 0
        sleep(Duration::from_secs(2)).await;
        assert!(partition_stream.next().await.is_some()); // seen = 1, flushed = 1

        //then
        let message1 = rx.recv().await;
        assert!(
            matches!(
                message1,
                Ok(StreamToServer::FlushManagedOffset { callback: _, offset }) if offset == 0
            ),
            "{message1:?}"
        );

        let message2 = rx.recv().await;
        assert!(
            matches!(
                message2,
                Ok(StreamToServer::FlushManagedOffset { callback: _, offset }) if offset == 1
            ),
            "{message2:?}"
        );

        let message3 = rx.try_recv();
        assert!(message3.is_err(), "{message3:?}")
    }

    #[fluvio_future::test]
    async fn test_multi_partition_stream_periodic_and_drop_flush() {
        //given
        let (tx1, rx1) = async_channel::unbounded();
        let partition_stream1 = SinglePartitionConsumerStream::new(
            records_stream(0, ["1"]),
            OffsetManagementStrategy::Auto,
            Duration::from_secs(1),
            Duration::from_millis(100),
            tx1,
        );
        let (tx2, rx2) = async_channel::unbounded();
        let partition_stream2 = SinglePartitionConsumerStream::new(
            records_stream(1, ["2", "4", "6"]),
            OffsetManagementStrategy::Auto,
            Duration::from_secs(1),
            Duration::from_millis(100),
            tx2,
        );
        let mut multi_stream =
            MultiplePartitionConsumerStream::new([partition_stream1, partition_stream2]);

        //when
        assert!(multi_stream.next().await.is_some()); // p1 seen = 0
        assert!(multi_stream.next().await.is_some()); // p2 seen = 0
        sleep(Duration::from_secs(2)).await;
        assert!(multi_stream.next().await.is_some()); // p2 seen = 1
        drop(multi_stream);

        //then
        {
            let message1 = rx1.recv().await;
            assert!(
                matches!(
                    message1,
                    Ok(StreamToServer::FlushManagedOffset { callback: _, offset }) if offset == 0
                ),
                "{message1:?}"
            );
            let message2 = rx1.try_recv();
            assert!(message2.is_err(), "{message2:?}");
        }
        {
            let message1 = rx2.recv().await;
            assert!(
                matches!(
                    message1,
                    Ok(StreamToServer::FlushManagedOffset { callback: _, offset }) if offset == 0
                ),
                "{message1:?}"
            );
            let message2 = rx2.recv().await;
            assert!(
                matches!(
                    message2,
                    Ok(StreamToServer::FlushManagedOffset { callback: _, offset }) if offset == 1
                ),
                "{message2:?}"
            );
            let message3 = rx2.try_recv();
            assert!(message3.is_err(), "{message3:?}");
        }
    }

    #[fluvio_future::test]
    async fn test_single_partition_stream_flush_error_propagated() {
        //given
        let (tx, rx) = async_channel::unbounded();
        let mut partition_stream = SinglePartitionConsumerStream::new(
            records_stream(0, ["1", "2", "3", "4"]),
            OffsetManagementStrategy::Manual,
            Default::default(),
            Duration::from_millis(100),
            tx,
        );

        //when
        fluvio_future::task::spawn(async move {
            //then
            let message = rx.recv().await;
            assert!(
                matches!(
                    message,
                    Ok(StreamToServer::FlushManagedOffset { callback: _, offset }) if offset == 0
                ),
                "{message:?}"
            );
            if let Ok(StreamToServer::FlushManagedOffset {
                offset: _,
                callback,
            }) = message
            {
                callback.send(ErrorCode::SpuOffline).await;
            }
        });

        assert!(partition_stream.next().await.is_some()); // seen = 0
        let _ = partition_stream.offset_commit().await;
        let flush_res = partition_stream.offset_flush().await;

        //then
        assert_eq!(flush_res, Err(ErrorCode::SpuOffline), "{flush_res:?}");
    }

    #[fluvio_future::test]
    async fn test_multi_partition_stream_flush_error_propagated() {
        //given
        let (tx1, rx1) = async_channel::unbounded();
        let partition_stream1 = SinglePartitionConsumerStream::new(
            records_stream(0, ["1"]),
            OffsetManagementStrategy::Manual,
            Default::default(),
            Duration::from_millis(100),
            tx1,
        );
        let (tx2, rx2) = async_channel::unbounded();
        let partition_stream2 = SinglePartitionConsumerStream::new(
            records_stream(1, ["2", "4", "6"]),
            OffsetManagementStrategy::Manual,
            Default::default(),
            Duration::from_millis(100),
            tx2,
        );
        let mut multi_stream =
            MultiplePartitionConsumerStream::new([partition_stream1, partition_stream2]);

        //when
        assert!(multi_stream.next().await.is_some()); // p1 seen = 0
        assert!(multi_stream.next().await.is_some()); // p2 seen = 0
        let _ = multi_stream.offset_commit().await;
        fluvio_future::task::spawn(async move {
            let message = rx1.recv().await;
            assert!(
                matches!(
                    message,
                    Ok(StreamToServer::FlushManagedOffset { callback: _, offset }) if offset == 0
                ),
                "{message:?}"
            );
            if let Ok(StreamToServer::FlushManagedOffset {
                offset: _,
                callback,
            }) = message
            {
                callback.send(ErrorCode::SpuOffline).await;
            }
        });
        fluvio_future::task::spawn(async move {
            let message = rx2.recv().await;
            if let Ok(StreamToServer::FlushManagedOffset {
                callback,
                offset: _,
            }) = message
            {
                callback.send(ErrorCode::None).await;
            }
        });
        let flush_res = multi_stream.offset_flush().await;

        //then
        assert_eq!(flush_res, Err(ErrorCode::SpuOffline), "{flush_res:?}");
    }

    fn records_stream(
        partition: PartitionId,
        input: impl IntoIterator<Item = &'static str>,
    ) -> Iter<IntoIter<Result<Record, ErrorCode>>> {
        let mut records: Vec<_> = input
            .into_iter()
            .map(|item| fluvio_protocol::record::Record::new(RecordData::from(item.as_bytes())))
            .collect();
        let mut batch = Batch::default();
        batch.add_records(&mut records);
        let consumer_records: Vec<_> = batch
            .into_consumer_records_iter(partition)
            .map(Ok)
            .collect();
        futures_util::stream::iter(consumer_records)
    }
}
