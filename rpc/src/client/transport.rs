//! Transport layer abstraction for the Tendermint RPC client.

use crate::client::subscription::{Subscription, SubscriptionId};
use crate::endpoint::{subscribe, unsubscribe};
use crate::event::Event;
use crate::{Error, Request};
use async_trait::async_trait;
use std::fmt::Debug;
use tokio::sync::mpsc;

pub mod http_ws;

/// Transport layer abstraction for interacting with real or mocked Tendermint
/// full nodes.
///
/// The transport is separated into one part responsible for request/response
/// mechanics and another for event subscription mechanics. This allows for
/// lazy instantiation of subscription mechanism because, depending on the
/// transport layer, they generally require more resources than the
/// request/response mechanism.
#[async_trait]
pub trait Transport: ClosableTransport {
    type SubscriptionTransport;

    // TODO(thane): Do we also need this request method to operate on a mutable
    //              `self`? If the underlying transport were purely WebSockets,
    //              it would need to be due to the need to mutate channels when
    //              communicating with the async task. This would then have
    //              mutability implications for the RPC client.
    /// Perform a request to the remote endpoint, expecting a response.
    async fn request<R>(&self, request: R) -> Result<R::Response, Error>
    where
        R: Request;

    /// Produces a transport layer interface specifically for handling event
    /// subscriptions.
    async fn subscription_transport(&self) -> Result<Self::SubscriptionTransport, Error>;
}

/// A layer intended to be superimposed upon the [`Transport`] layer that only
/// provides subscription mechanics.
#[async_trait]
pub trait SubscriptionTransport: ClosableTransport + Debug + Sized {
    /// Send a subscription request via the transport. For this we need the
    /// body of the request, as well as a return path (the sender half of an
    /// [`mpsc` channel]) for the events generated by this subscription.
    ///
    /// On success, returns the ID of the subscription that's just been
    /// created.
    ///
    /// We ignore any responses returned by the remote endpoint right now.
    ///
    /// [`mpsc` channel]: tokio::sync::mpsc
    ///
    async fn subscribe(
        &mut self,
        request: subscribe::Request,
        event_tx: mpsc::Sender<Event>,
    ) -> Result<SubscriptionId, Error>;

    /// Send an unsubscribe request via the transport. The subscription is
    /// terminated and consumed.
    ///
    /// We ignore any responses returned by the remote endpoint right now.
    async fn unsubscribe(
        &mut self,
        request: unsubscribe::Request,
        subscription: Subscription,
    ) -> Result<(), Error>;
}

/// A transport that can be gracefully closed.
#[async_trait]
pub trait ClosableTransport {
    /// Attempt to gracefully close the transport, consuming it in the process.
    async fn close(self) -> Result<(), Error>;
}
