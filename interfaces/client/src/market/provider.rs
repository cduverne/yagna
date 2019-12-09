use awc::Client;
use futures::{Future, TryFutureExt};
use std::sync::Arc;

use super::ApiConfiguration;
use crate::Error;
use ya_model::market::{AgreementProposal, Offer, Proposal, ProviderEvent};

pub struct ProviderApi {
    configuration: Arc<ApiConfiguration>,
}

impl ProviderApi {
    pub fn new(configuration: Arc<ApiConfiguration>) -> Self {
        ProviderApi { configuration }
    }

    /// Publish Provider’s service capabilities (Offer) on the market to declare an
    /// interest in Demands meeting specified criteria.
    pub fn subscribe(&self, offer: Offer) -> impl Future<Output = Result<String, Error>> {
        let endpoint_url = self.configuration.api_endpoint("offers");
        async move {
            let vec = Client::default()
                .post(endpoint_url)
                .send_json(&offer)
                .await?
                .body()
                .await?
                .to_vec();
            Ok(String::from_utf8(vec)?)
        }
    }

    /// Stop subscription by invalidating a previously published Offer.
    pub fn unsubscribe(&self, subscription_id: &str) -> impl Future<Output = Result<(), Error>> {
        //        Box::pin(async {
        //            Client::default()
        //                .delete(self.configuration.api_endpoint(format!("/offers/{}", subscription_id))?)
        //                .send_json(&Offer::new(serde_json::json!({"zima":"już"}), "()".into()))
        //                .await
        //                .expect("Offers POST request failed")
        //        })
        async { unimplemented!() }
    }

    /// Get events which have arrived from the market in response to the Offer
    /// published by the Provider via  [subscribe](self::subscribe).
    /// Returns collection of [ProviderEvents](ProviderEvent) or timeout.
    pub fn collect(
        &self,
        subscription_id: &str,
        timeout: f32,
        max_events: i64,
    ) -> impl Future<Output = Result<Vec<ProviderEvent>, Error>> {
        //            "/offers/{subscriptionId}/events",
        async { unimplemented!() }
    }

    /// TODO doc
    pub fn create_proposal(
        &self,
        subscription_id: &str,
        proposal_id: &str,
        proposal: Proposal,
    ) -> impl Future<Output = Result<String, Error>> {
        //            "/offers/{subscriptionId}/proposals/{proposalId}/offer".to_string(),
        async { unimplemented!() }
    }

    /// TODO doc
    pub fn get_proposal(
        &self,
        subscription_id: &str,
        proposal_id: &str,
    ) -> impl Future<Output = Result<AgreementProposal, Error>> {
        //            "/offers/{subscriptionId}/proposals/{proposalId}".to_string(),
        async { unimplemented!() }
    }

    /// TODO doc
    pub fn reject_proposal(
        &self,
        subscription_id: &str,
        proposal_id: &str,
    ) -> impl Future<Output = Result<(), Error>> {
        //            "/offers/{subscriptionId}/proposals/{proposalId}".to_string(),
        async { unimplemented!() }
    }

    /// Confirms the Agreement received from the Requestor.
    /// Mutually exclusive with [reject_agreement](self::reject_agreement).
    pub fn approve_agreement(&self, agreement_id: &str) -> impl Future<Output = Result<(), Error>> {
        //            "/agreements/{agreementId}/approve".to_string(),
        async { unimplemented!() }
    }

    /// Rejects the Agreement received from the Requestor.
    /// Mutually exclusive with [approve_agreement](self::approve_agreement).
    pub fn reject_agreement(&self, agreement_id: &str) -> impl Future<Output = Result<(), Error>> {
        //            "/agreements/{agreementId}/reject".to_string(),
        async { unimplemented!() }
    }
}
