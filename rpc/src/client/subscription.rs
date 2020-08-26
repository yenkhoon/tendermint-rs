//! Subscription- and subscription management-related functionality.

use crate::client::sync::{unbounded, ChannelRx, ChannelTx};
use crate::client::ClosableClient;
use crate::event::Event;
use crate::{Error, Id, Result};
use async_trait::async_trait;
use futures::task::{Context, Poll};
use futures::Stream;
use getrandom::getrandom;
use std::collections::HashMap;
use std::convert::TryInto;
use std::pin::Pin;

/// A client that exclusively provides [`Event`] subscription capabilities,
/// without any other RPC method support.
///
/// To build a full-featured client, implement both this trait as well as the
/// [`Client`] trait.
///
/// [`Event`]: ./events/struct.Event.html
/// [`Client`]: trait.Client.html
#[async_trait]
pub trait SubscriptionClient: ClosableClient {
    /// `/subscribe`: subscribe to receive events produced by the given query.
    ///
    /// Allows for specification of the `buf_size` parameter, which determines
    /// how many events can be buffered in the resulting [`Subscription`]. Set
    /// to 0 to use an unbounded buffer (i.e. the buffer size will only be
    /// limited by the amount of memory available to your application).
    ///
    /// [`Subscription`]: struct.Subscription.html
    async fn subscribe_with_buf_size(
        &mut self,
        query: String,
        buf_size: usize,
    ) -> Result<Subscription>;

    /// `/subscribe`: subscribe to receive events produced by the given query.
    ///
    /// Uses an unbounded buffer for the resulting [`Subscription`] (i.e. this
    /// is the same as calling `subscribe_with_buf_size` with `buf_size` set to
    /// 0).
    ///
    /// [`Subscription`]: struct.Subscription.html
    async fn subscribe(&mut self, query: String) -> Result<Subscription> {
        self.subscribe_with_buf_size(query, 0).await
    }
}

/// An interface that can be used to asynchronously receive [`Event`]s for a
/// particular subscription.
///
/// ## Examples
///
/// ```
/// use tendermint_rpc::{SubscriptionId, Subscription};
/// use futures::StreamExt;
///
/// /// Prints `count` events from the given subscription.
/// async fn print_events(subs: &mut Subscription, count: usize) {
///     let mut counter = 0_usize;
///     while let Some(res) = subs.next().await {
///         // Technically, a subscription produces `Result<Event, Error>`
///         // instances. Errors can be produced by the remote endpoint at any
///         // time and need to be handled here.
///         let ev = res.unwrap();
///         println!("Got incoming event: {:?}", ev);
///         counter += 1;
///         if counter >= count {
///             break
///         }
///     }
/// }
/// ```
///
/// [`Event`]: ./event/struct.Event.html
#[derive(Debug)]
pub struct Subscription {
    /// The query for which events will be produced.
    pub query: String,
    /// The ID of this subscription (automatically assigned).
    pub id: SubscriptionId,
    // Our internal result event receiver for this subscription.
    event_rx: ChannelRx<Result<Event>>,
    // Allows us to gracefully terminate this subscription.
    terminate_tx: ChannelTx<TerminateSubscription>,
}

impl Stream for Subscription {
    type Item = Result<Event>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.event_rx.poll_recv(cx)
    }
}

impl Subscription {
    pub(crate) fn new(
        id: SubscriptionId,
        query: String,
        event_rx: ChannelRx<Result<Event>>,
        terminate_tx: ChannelTx<TerminateSubscription>,
    ) -> Self {
        Self {
            id,
            query,
            event_rx,
            terminate_tx,
        }
    }

    /// Gracefully terminate this subscription.
    ///
    /// This can be called from any asynchronous context. It only returns once
    /// it receives confirmation of termination.
    pub async fn terminate(mut self) -> Result<()> {
        let (result_tx, mut result_rx) = unbounded();
        self.terminate_tx
            .send(TerminateSubscription {
                id: self.id.clone(),
                query: self.query.clone(),
                result_tx,
            })
            .await?;
        result_rx.recv().await.ok_or_else(|| {
            Error::client_internal_error(
                "failed to hear back from subscription termination request".to_string(),
            )
        })?
    }
}

/// A message sent to the subscription driver to terminate the subscription
/// with the given parameters.
///
/// We expect the driver to use the `result_tx` channel to communicate the
/// result of the termination request to the original caller.
#[derive(Debug, Clone)]
pub struct TerminateSubscription {
    pub id: SubscriptionId,
    pub query: String,
    pub result_tx: ChannelTx<Result<()>>,
}

/// Each new subscription is automatically assigned an ID.
///
/// By default, we generate random [UUIDv4] IDs for each subscription to
/// minimize chances of collision.
///
/// [UUIDv4]: https://en.wikipedia.org/wiki/Universally_unique_identifier#Version_4_(random)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SubscriptionId(String);

impl Default for SubscriptionId {
    fn default() -> Self {
        let mut bytes = [0; 16];
        getrandom(&mut bytes).expect("RNG failure!");

        let uuid = uuid::Builder::from_bytes(bytes)
            .set_variant(uuid::Variant::RFC4122)
            .set_version(uuid::Version::Random)
            .build();

        Self(uuid.to_string())
    }
}

impl std::fmt::Display for SubscriptionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Into<Id> for SubscriptionId {
    fn into(self) -> Id {
        Id::Str(self.0)
    }
}

impl TryInto<SubscriptionId> for Id {
    type Error = Error;

    fn try_into(self) -> std::result::Result<SubscriptionId, Self::Error> {
        match self {
            Id::Str(s) => Ok(SubscriptionId(s)),
            Id::Num(i) => Ok(SubscriptionId(format!("{}", i))),
            Id::None => Err(Error::client_internal_error(
                "cannot convert an empty JSON-RPC ID into a subscription ID",
            )),
        }
    }
}

impl AsRef<str> for SubscriptionId {
    fn as_ref(&self) -> &str {
        self.0.as_ref()
    }
}

impl From<&str> for SubscriptionId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

#[derive(Debug)]
struct PendingSubscribe {
    id: SubscriptionId,
    query: String,
    event_tx: ChannelTx<Result<Event>>,
    result_tx: ChannelTx<Result<()>>,
}

#[derive(Debug)]
struct PendingUnsubscribe {
    id: SubscriptionId,
    query: String,
    result_tx: ChannelTx<Result<()>>,
}

/// The current state of a subscription.
#[derive(Debug, Clone, PartialEq)]
pub enum SubscriptionState {
    Pending,
    Active,
    Cancelling,
    NotFound,
}

/// Provides a mechanism for tracking [`Subscription`]s and routing [`Event`]s
/// to those subscriptions.
///
/// [`Subscription`]: struct.Subscription.html
/// [`Event`]: ./event/struct.Event.html
#[derive(Debug)]
pub struct SubscriptionRouter {
    subscriptions: HashMap<String, HashMap<SubscriptionId, ChannelTx<Result<Event>>>>,
    // A map of JSON-RPC request IDs (for `/subscribe` requests) to pending
    // subscription requests.
    pending_subscribe: HashMap<String, PendingSubscribe>,
    // A map of JSON-RPC request IDs (for the `/unsubscribe` requests) to pending
    // unsubscribe requests.
    pending_unsubscribe: HashMap<String, PendingUnsubscribe>,
}

impl SubscriptionRouter {
    /// Publishes the given event to all of the subscriptions to which the
    /// event is relevant. At present, it matches purely based on the query
    /// associated with the event, and only queries that exactly match that of
    /// the event's.
    pub async fn publish(&mut self, ev: Event) {
        let subs_for_query = match self.subscriptions.get_mut(&ev.query) {
            Some(s) => s,
            None => return,
        };
        let mut disconnected = Vec::<SubscriptionId>::new();
        for (id, event_tx) in subs_for_query {
            // TODO(thane): Right now we automatically remove any disconnected
            //              or full channels. We must handle full channels
            //              differently to disconnected ones.
            if event_tx.send(Ok(ev.clone())).await.is_err() {
                disconnected.push(id.clone());
            }
        }
        let subs_for_query = self.subscriptions.get_mut(&ev.query).unwrap();
        for id in disconnected {
            subs_for_query.remove(&id);
        }
    }

    /// Immediately add a new subscription to the router without waiting for
    /// confirmation.
    pub fn add(&mut self, id: &SubscriptionId, query: String, event_tx: ChannelTx<Result<Event>>) {
        let subs_for_query = match self.subscriptions.get_mut(&query) {
            Some(s) => s,
            None => {
                self.subscriptions.insert(query.clone(), HashMap::new());
                self.subscriptions.get_mut(&query).unwrap()
            }
        };
        subs_for_query.insert(id.clone(), event_tx);
    }

    /// Keep track of a pending subscription, which can either be confirmed or
    /// cancelled.
    ///
    /// `req_id` must be a unique identifier for this particular pending
    /// subscription request operation, where `subs_id` must be the unique ID
    /// of the subscription we eventually want added.
    pub fn pending_add(
        &mut self,
        req_id: &str,
        subs_id: &SubscriptionId,
        query: String,
        event_tx: ChannelTx<Result<Event>>,
        result_tx: ChannelTx<Result<()>>,
    ) {
        self.pending_subscribe.insert(
            req_id.to_string(),
            PendingSubscribe {
                id: subs_id.clone(),
                query,
                event_tx,
                result_tx,
            },
        );
    }

    /// Attempts to confirm the pending subscription request with the given ID.
    ///
    /// Returns an error if it fails to respond to the original caller to
    /// indicate success.
    pub async fn confirm_add(&mut self, req_id: &str) -> Result<()> {
        match self.pending_subscribe.remove(req_id) {
            Some(mut pending_subscribe) => {
                self.add(
                    &pending_subscribe.id,
                    pending_subscribe.query.clone(),
                    pending_subscribe.event_tx,
                );
                Ok(pending_subscribe.result_tx.send(Ok(())).await?)
            }
            None => Ok(()),
        }
    }

    /// Attempts to cancel the pending subscription with the given ID, sending
    /// the specified error to the original creator of the attempted
    /// subscription.
    pub async fn cancel_add(&mut self, req_id: &str, err: impl Into<Error>) -> Result<()> {
        match self.pending_subscribe.remove(req_id) {
            Some(mut pending_subscribe) => Ok(pending_subscribe
                .result_tx
                .send(Err(err.into()))
                .await
                .map_err(|_| {
                    Error::client_internal_error(format!(
                        "failed to communicate result of pending subscription with ID: {}",
                        pending_subscribe.id,
                    ))
                })?),
            None => Ok(()),
        }
    }

    /// Immediately remove the subscription with the given query and ID.
    pub fn remove(&mut self, id: &SubscriptionId, query: String) {
        let subs_for_query = match self.subscriptions.get_mut(&query) {
            Some(s) => s,
            None => return,
        };
        subs_for_query.remove(id);
    }

    /// Keeps track of a pending unsubscribe request, which can either be
    /// confirmed or cancelled.
    pub fn pending_remove(
        &mut self,
        req_id: &str,
        subs_id: &SubscriptionId,
        query: String,
        result_tx: ChannelTx<Result<()>>,
    ) {
        self.pending_unsubscribe.insert(
            req_id.to_string(),
            PendingUnsubscribe {
                id: subs_id.clone(),
                query,
                result_tx,
            },
        );
    }

    /// Confirm the pending unsubscribe request for the subscription with the
    /// given ID.
    pub async fn confirm_remove(&mut self, req_id: &str) -> Result<()> {
        match self.pending_unsubscribe.remove(req_id) {
            Some(mut pending_unsubscribe) => {
                self.remove(&pending_unsubscribe.id, pending_unsubscribe.query.clone());
                Ok(pending_unsubscribe.result_tx.send(Ok(())).await?)
            }
            None => Ok(()),
        }
    }

    /// Cancel the pending unsubscribe request for the subscription with the
    /// given ID, responding with the given error.
    pub async fn cancel_remove(&mut self, req_id: &str, err: impl Into<Error>) -> Result<()> {
        match self.pending_unsubscribe.remove(req_id) {
            Some(mut pending_unsubscribe) => {
                Ok(pending_unsubscribe.result_tx.send(Err(err.into())).await?)
            }
            None => Ok(()),
        }
    }

    /// Helper to check whether the subscription with the given ID is
    /// currently active.
    pub fn is_active(&self, id: &SubscriptionId) -> bool {
        self.subscriptions
            .iter()
            .any(|(_query, subs_for_query)| subs_for_query.contains_key(id))
    }

    /// Obtain a mutable reference to the subscription with the given ID (if it
    /// exists).
    pub fn get_active_subscription_mut(
        &mut self,
        id: &SubscriptionId,
    ) -> Option<&mut ChannelTx<Result<Event>>> {
        self.subscriptions
            .iter_mut()
            .find(|(_query, subs_for_query)| subs_for_query.contains_key(id))
            .and_then(|(_query, subs_for_query)| subs_for_query.get_mut(id))
    }

    /// Utility method to determine the current state of the subscription with
    /// the given ID.
    pub fn subscription_state(&self, req_id: &str) -> SubscriptionState {
        if self.pending_subscribe.contains_key(req_id) {
            return SubscriptionState::Pending;
        }
        if self.pending_unsubscribe.contains_key(req_id) {
            return SubscriptionState::Cancelling;
        }
        if self.is_active(&SubscriptionId::from(req_id)) {
            return SubscriptionState::Active;
        }
        SubscriptionState::NotFound
    }
}

impl Default for SubscriptionRouter {
    fn default() -> Self {
        Self {
            subscriptions: HashMap::new(),
            pending_subscribe: HashMap::new(),
            pending_unsubscribe: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::client::sync::unbounded;
    use crate::event::{Event, WrappedEvent};
    use std::path::PathBuf;
    use tokio::fs;
    use tokio::time::{self, Duration};

    async fn read_json_fixture(name: &str) -> String {
        fs::read_to_string(PathBuf::from("./tests/support/").join(name.to_owned() + ".json"))
            .await
            .unwrap()
    }

    async fn read_event(name: &str) -> Event {
        serde_json::from_str::<WrappedEvent>(read_json_fixture(name).await.as_str())
            .unwrap()
            .into_result()
            .unwrap()
    }

    async fn must_recv<T>(ch: &mut ChannelRx<T>, timeout_ms: u64) -> T {
        let mut delay = time::delay_for(Duration::from_millis(timeout_ms));
        tokio::select! {
            _ = &mut delay, if !delay.is_elapsed() => panic!("timed out waiting for recv"),
            Some(v) = ch.recv() => v,
        }
    }

    async fn must_not_recv<T>(ch: &mut ChannelRx<T>, timeout_ms: u64)
    where
        T: std::fmt::Debug,
    {
        let mut delay = time::delay_for(Duration::from_millis(timeout_ms));
        tokio::select! {
            _ = &mut delay, if !delay.is_elapsed() => (),
            Some(v) = ch.recv() => panic!("got unexpected result from channel: {:?}", v),
        }
    }

    #[tokio::test]
    async fn router_basic_pub_sub() {
        let mut router = SubscriptionRouter::default();

        let (subs1_id, subs2_id, subs3_id) = (
            SubscriptionId::default(),
            SubscriptionId::default(),
            SubscriptionId::default(),
        );
        let (subs1_event_tx, mut subs1_event_rx) = unbounded();
        let (subs2_event_tx, mut subs2_event_rx) = unbounded();
        let (subs3_event_tx, mut subs3_event_rx) = unbounded();

        // Two subscriptions with the same query
        router.add(&subs1_id, "query1".into(), subs1_event_tx);
        router.add(&subs2_id, "query1".into(), subs2_event_tx);
        // Another subscription with a different query
        router.add(&subs3_id, "query2".into(), subs3_event_tx);

        let mut ev = read_event("event_new_block_1").await;
        ev.query = "query1".into();
        router.publish(ev.clone()).await;

        let subs1_ev = must_recv(&mut subs1_event_rx, 500).await.unwrap();
        let subs2_ev = must_recv(&mut subs2_event_rx, 500).await.unwrap();
        must_not_recv(&mut subs3_event_rx, 50).await;
        assert_eq!(ev, subs1_ev);
        assert_eq!(ev, subs2_ev);

        ev.query = "query2".into();
        router.publish(ev.clone()).await;

        must_not_recv(&mut subs1_event_rx, 50).await;
        must_not_recv(&mut subs2_event_rx, 50).await;
        let subs3_ev = must_recv(&mut subs3_event_rx, 500).await.unwrap();
        assert_eq!(ev, subs3_ev);
    }

    #[tokio::test]
    async fn router_pending_subscription() {
        let mut router = SubscriptionRouter::default();
        let subs_id = SubscriptionId::default();
        let (event_tx, mut event_rx) = unbounded();
        let (result_tx, mut result_rx) = unbounded();
        let query = "query".to_string();
        let mut ev = read_event("event_new_block_1").await;
        ev.query = query.clone();

        assert_eq!(
            SubscriptionState::NotFound,
            router.subscription_state(&subs_id.to_string())
        );
        router.pending_add(
            subs_id.as_ref(),
            &subs_id,
            query.clone(),
            event_tx,
            result_tx,
        );
        assert_eq!(
            SubscriptionState::Pending,
            router.subscription_state(subs_id.as_ref())
        );
        router.publish(ev.clone()).await;
        must_not_recv(&mut event_rx, 50).await;

        router.confirm_add(subs_id.as_ref()).await.unwrap();
        assert_eq!(
            SubscriptionState::Active,
            router.subscription_state(subs_id.as_ref())
        );
        must_not_recv(&mut event_rx, 50).await;
        let _ = must_recv(&mut result_rx, 500).await;

        router.publish(ev.clone()).await;
        let received_ev = must_recv(&mut event_rx, 500).await.unwrap();
        assert_eq!(ev, received_ev);

        let (result_tx, mut result_rx) = unbounded();
        router.pending_remove(subs_id.as_ref(), &subs_id, query.clone(), result_tx);
        assert_eq!(
            SubscriptionState::Cancelling,
            router.subscription_state(subs_id.as_ref()),
        );

        router.confirm_remove(subs_id.as_ref()).await.unwrap();
        assert_eq!(
            SubscriptionState::NotFound,
            router.subscription_state(subs_id.as_ref())
        );
        router.publish(ev.clone()).await;
        if must_recv(&mut result_rx, 500).await.is_err() {
            panic!("we should have received successful confirmation of the unsubscribe request")
        }
    }

    #[tokio::test]
    async fn router_cancel_pending_subscription() {
        let mut router = SubscriptionRouter::default();
        let subs_id = SubscriptionId::default();
        let (event_tx, mut event_rx) = unbounded::<Result<Event>>();
        let (result_tx, mut result_rx) = unbounded::<Result<()>>();
        let query = "query".to_string();
        let mut ev = read_event("event_new_block_1").await;
        ev.query = query.clone();

        assert_eq!(
            SubscriptionState::NotFound,
            router.subscription_state(subs_id.as_ref())
        );
        router.pending_add(subs_id.as_ref(), &subs_id, query, event_tx, result_tx);
        assert_eq!(
            SubscriptionState::Pending,
            router.subscription_state(subs_id.as_ref())
        );
        router.publish(ev.clone()).await;
        must_not_recv(&mut event_rx, 50).await;

        let cancel_error = Error::client_internal_error("cancelled");
        router
            .cancel_add(subs_id.as_ref(), cancel_error.clone())
            .await
            .unwrap();
        assert_eq!(
            SubscriptionState::NotFound,
            router.subscription_state(subs_id.as_ref())
        );
        assert_eq!(Err(cancel_error), must_recv(&mut result_rx, 500).await);

        router.publish(ev.clone()).await;
        must_not_recv(&mut event_rx, 50).await;
    }
}
