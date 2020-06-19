use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use ya_client::model::market::{Demand, Offer, Proposal};
use ya_client::model::ErrorMessage;
use ya_persistence::executor::DbExecutor;
use ya_persistence::executor::Error as DbError;
use ya_service_api_web::middleware::Identity;

use crate::db::dao::*;
use crate::db::models::Demand as ModelDemand;
use crate::db::models::Offer as ModelOffer;
use crate::db::models::{SubscriptionId, SubscriptionParseError};
use crate::db::*;
use crate::migrations;
use crate::protocol::{
    Discovery, DiscoveryError, DiscoveryInitError, Propagate, StopPropagateReason,
};
use crate::protocol::{OfferReceived, OfferUnsubscribed, RetrieveOffers};

#[derive(Error, Debug)]
pub enum DemandError {
    #[error("Failed to save Demand. Error: {0}.")]
    SaveDemandFailure(#[from] DbError),
    #[error("Failed to remove Demand [{1}]. Error: {0}.")]
    RemoveDemandFailure(DbError, String),
    #[error("Demand [{0}] doesn't exist.")]
    DemandNotExists(SubscriptionId),
}

#[derive(Error, Debug)]
pub enum OfferError {
    #[error("Failed to save Offer. Error: {0}.")]
    SaveOfferFailure(#[from] DbError),
    #[error("Failed to remove Offer [{1}]. Error: {0}.")]
    UnsubscribeOfferFailure(UnsubscribeError, SubscriptionId),
    #[error("Offer [{0}] doesn't exist.")]
    OfferNotExists(SubscriptionId),
}

#[derive(Error, Debug)]
pub enum MatcherError {
    #[error(transparent)]
    DemandError(#[from] DemandError),
    #[error(transparent)]
    OfferError(#[from] OfferError),
    #[error("Internal error: {0}.")]
    InternalError(String),
}

#[derive(Error, Debug)]
pub enum MatcherInitError {
    #[error("Failed to initialize Discovery interface. Error: {0}.")]
    DiscoveryError(#[from] DiscoveryInitError),
    #[error("Failed to initialize database. Error: {0}.")]
    DatabaseError(#[from] DbError),
    #[error("Failed to migrate market database. Error: {0}.")]
    MigrationError(#[from] anyhow::Error),
}

/// Receivers for events, that can be emitted from Matcher.
pub struct EventsListeners {
    pub proposal_receiver: UnboundedReceiver<Proposal>,
}

/// Responsible for storing Offers and matching them with demands.
#[derive(Clone)]
pub struct Matcher {
    db: DbExecutor,
    discovery: Discovery,
    proposal_emitter: UnboundedSender<Proposal>,
}

impl Matcher {
    pub fn new(db: &DbExecutor) -> Result<(Matcher, EventsListeners), MatcherInitError> {
        // TODO: Implement Discovery callbacks.

        let database1 = db.clone();
        let database2 = db.clone();
        let discovery = Discovery::new(
            move |_caller: String, msg: OfferReceived| {
                let database = database1.clone();
                on_offer_received(database, msg)
            },
            move |_caller: String, msg: OfferUnsubscribed| {
                let database = database2.clone();
                on_offer_unsubscribed(database, msg)
            },
            move |caller: String, msg: RetrieveOffers| async move {
                log::info!("Offers request received from: {}. Unimplemented.", caller);
                Ok(vec![])
            },
        )?;
        let (emitter, receiver) = unbounded_channel::<Proposal>();

        let matcher = Matcher {
            db: db.clone(),
            discovery,
            proposal_emitter: emitter,
        };
        let listeners = EventsListeners {
            proposal_receiver: receiver,
        };

        Ok((matcher, listeners))
    }

    pub async fn bind_gsb(
        &self,
        public_prefix: &str,
        private_prefix: &str,
    ) -> Result<(), MatcherInitError> {
        Ok(self
            .discovery
            .bind_gsb(public_prefix, private_prefix)
            .await?)
    }

    // =========================================== //
    // Offer/Demand subscription
    // =========================================== //

    pub async fn subscribe_offer(&self, model_offer: &ModelOffer) -> Result<(), MatcherError> {
        self.db
            .as_dao::<OfferDao>()
            .create_offer(model_offer)
            .await
            .map_err(OfferError::SaveOfferFailure)?;

        // TODO: Run matching to find local matching demands. We shouldn't wait here.
        // TODO: Handle broadcast errors. Maybe we should retry if it failed.
        let _ = self
            .discovery
            .broadcast_offer(model_offer.clone())
            .await
            .map_err(|error| {
                log::warn!(
                    "Failed to broadcast offer [{1}]. Error: {0}.",
                    error,
                    model_offer.id,
                );
            });
        Ok(())
    }

    pub async fn subscribe_demand(&self, model_demand: &ModelDemand) -> Result<(), MatcherError> {
        self.db
            .as_dao::<DemandDao>()
            .create_demand(model_demand)
            .await
            .map_err(DemandError::SaveDemandFailure)?;

        // TODO: Try to match demand with offers currently existing in database.
        //  We shouldn't await here on this.
        Ok(())
    }

    pub async fn unsubscribe_offer(
        &self,
        id: Identity,
        subscription_id: &str,
    ) -> Result<(), MatcherError> {
        let subscription_id = SubscriptionId::from_str(subscription_id)?;
        self.db
            .as_dao::<OfferDao>()
            .mark_offer_as_unsubscribed(&subscription_id)
            .await
            .map_err(|error| OfferError::UnsubscribeOfferFailure(error, subscription_id.clone()))?;

        // Broadcast only, if no Error occurred in previous step.
        // We ignore broadcast errors. Unsubscribing was finished successfully, so:
        // - We shouldn't bother agent with broadcasts
        // - Unsubscribe message probably will reach other markets, but later.
        let _ = self
            .discovery
            .broadcast_unsubscribe(id.identity.to_string(), subscription_id.clone())
            .await
            .map_err(|error| {
                log::warn!(
                    "Failed to broadcast unsubscribe offer [{1}]. Error: {0}.",
                    error,
                    subscription_id
                );
            });
        Ok(())
    }

    pub async fn unsubscribe_demand(&self, subscription_id: &str) -> Result<(), MatcherError> {
        let subscription_id = SubscriptionId::from_str(subscription_id)?;
        let removed = self
            .db
            .as_dao::<DemandDao>()
            .remove_demand(&subscription_id)
            .await
            .map_err(|error| {
                DemandError::RemoveDemandFailure(error, subscription_id.to_string())
            })?;

        if !removed {
            Err(DemandError::DemandNotExists(subscription_id))?;
        }
        Ok(())
    }

    // =========================================== //
    // Offer/Demand query
    // =========================================== //

    pub async fn get_offer<Str: AsRef<str>>(
        &self,
        subscription_id: Str,
    ) -> Result<Option<Offer>, MatcherError> {
        let model_offer: Option<ModelOffer> = self
            .db
            .as_dao::<OfferDao>()
            .get_offer(&SubscriptionId::from_str(subscription_id.as_ref())?)
            .await?;

        match model_offer {
            Some(model_offer) => Ok(Some(model_offer.into_client_offer()?)),
            None => Ok(None),
        }
    }

    pub async fn get_demand<Str: AsRef<str>>(
        &self,
        subscription_id: Str,
    ) -> Result<Option<Demand>, MatcherError> {
        let model_demand: Option<ModelDemand> = self
            .db
            .as_dao::<DemandDao>()
            .get_demand(&SubscriptionId::from_str(subscription_id.as_ref())?)
            .await?;

        match model_demand {
            Some(model_demand) => Ok(Some(model_demand.into_client_offer()?)),
            None => Ok(None),
        }
    }
}

async fn on_offer_received(db: DbExecutor, msg: OfferReceived) -> Result<Propagate, ()> {
    async move {
        // We shouldn't propagate Offer, if we already have it in our database.
        // Note that when, we broadcast our Offer, it will reach us too, so it concerns
        // not only Offers from other nodes.
        //
        // Note: Infinite broadcasting is possible here, if we would just use get_offer function,
        // because it filters expired and unsubscribed Offers. Note what happens in such case:
        // We think that Offer doesn't exist, so we insert it to database every time it reaches us,
        // because get_offer will never return it. So we will never meet stop condition of broadcast!!
        // So be careful.
        let propagate = match db
            .as_dao::<OfferDao>()
            .get_offer_state(&msg.offer.id)
            .await?
        {
            OfferState::Active(_) => Propagate::False(StopPropagateReason::AlreadyExists),
            OfferState::Unsubscribed(_) => {
                Propagate::False(StopPropagateReason::AlreadyUnsubscribed)
            }
            OfferState::Expired(_) => Propagate::False(StopPropagateReason::Expired),
            OfferState::NotFound => Propagate::True,
        };

        if let Propagate::True = propagate {
            // Will reject Offer, if hash was computed incorrectly. In most cases
            // it could mean, that it could be some kind of attack.
            msg.offer.validate()?;

            let model_offer = msg.offer;
            db.as_dao::<OfferDao>()
                .create_offer(&model_offer)
                .await
                .map_err(OfferError::SaveOfferFailure)?;

            // TODO: Spawn matching with Demands.
        }
        Result::<_, MatcherError>::Ok(propagate)
    }
    .await
    .or_else(|error| {
        let reason = StopPropagateReason::Error(format!("{}", error));
        Ok(Propagate::False(reason))
    })
}

async fn on_offer_unsubscribed(db: DbExecutor, msg: OfferUnsubscribed) -> Result<Propagate, ()> {
    async move {
        db.as_dao::<OfferDao>()
            .mark_offer_as_unsubscribed(&msg.subscription_id)
            .await?;

        // We store only our Offers to keep history. Offers from other nodes
        // should be removed.
        // We are sure that we don't remove our Offer here, because we would got
        // `AlreadyUnsubscribed` error from `mark_offer_as_unsubscribed` above,
        // as it was already invoked before broadcast in `unsubscribe_offer`.
        // TODO: Maybe we should add check here, to be sure, that we don't remove own Offers.
        log::debug!("Removing unsubscribed Offer [{}].", &msg.subscription_id);
        let _ = db
            .as_dao::<OfferDao>()
            .remove_offer(&msg.subscription_id)
            .await
            .map_err(|error| {
                log::warn!(
                    "Failed to remove offer [{}] during unsubscribe.",
                    &msg.subscription_id
                );
            });
        Result::<_, UnsubscribeError>::Ok(Propagate::True)
    }
    .await
    .or_else(|error| {
        let reason = match error {
            UnsubscribeError::OfferExpired(_) => StopPropagateReason::Expired,
            UnsubscribeError::AlreadyUnsubscribed(_) => StopPropagateReason::AlreadyUnsubscribed,
            _ => StopPropagateReason::Error(error.to_string()),
        };
        Ok(Propagate::False(reason))
    })
}

// =========================================== //
// Errors From impls
// =========================================== //

impl From<ErrorMessage> for MatcherError {
    fn from(e: ErrorMessage) -> Self {
        MatcherError::InternalError(e.to_string())
    }
}

impl From<SubscriptionParseError> for MatcherError {
    fn from(e: SubscriptionParseError) -> Self {
        MatcherError::InternalError(e.to_string())
    }
}

impl From<DbError> for MatcherError {
    fn from(e: DbError) -> Self {
        MatcherError::InternalError(e.to_string())
    }
}
