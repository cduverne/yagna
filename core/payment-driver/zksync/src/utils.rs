// External uses
use bigdecimal::BigDecimal;
use futures3::{Future, FutureExt};
use num::bigint::ToBigInt;
use num::pow::pow;
use num::BigUint;
use std::pin::Pin;
use tokio::task;
use zksync::utils::{closest_packable_token_amount, is_token_amount_packable};
use zksync_eth_signer::error::SignerError;

// Workspace uses
use ya_client_model::NodeId;
use ya_core_model::driver::GenericError;
use ya_core_model::identity;
use ya_service_bus::{typed as bus, RpcEndpoint};

// Copied from core/payment-driver/gnt/utils.rs

// TODO: Get token decimals from zksync-provider / wallet
const PRECISION: u64 = 1_000_000_000_000_000_000;

pub fn sign_tx(
    node_id: NodeId,
    payload: Vec<u8>,
) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, SignerError>> + Send>> {
    let fut = task::spawn_local(async move {
        let signature = bus::service(identity::BUS_ID)
            .send(identity::Sign { node_id, payload })
            .await
            .map_err(|e| SignerError::SigningFailed(format!("{:?}", e)))?
            .map_err(|e| SignerError::SigningFailed(format!("{:?}", e)))?;
        Ok(signature)
    });
    let fut = fut.map(|res| match res {
        Ok(res) => res,
        Err(e) => Err(SignerError::SigningFailed(e.to_string())),
    });
    Box::pin(fut)
}

pub fn big_dec_to_big_uint(v: BigDecimal) -> Result<BigUint, GenericError> {
    let v = v * Into::<BigDecimal>::into(PRECISION);
    let v = v
        .to_bigint()
        .ok_or(GenericError::new("Failed to convert to bigint"))?;
    let v = v
        .to_biguint()
        .ok_or(GenericError::new("Failed to convert to biguint"))?;
    Ok(v)
}

pub fn big_uint_to_big_dec(v: BigUint) -> Result<BigDecimal, GenericError> {
    let v: BigDecimal = v.to_string().parse().map_err(GenericError::new)?;
    Ok(v / Into::<BigDecimal>::into(PRECISION))
}

/// Find the closest **bigger** packable amount
pub fn pack_up(amount: &BigUint) -> BigUint {
    let mut packable_amount = closest_packable_token_amount(&amount);
    while (&packable_amount < amount) || !is_token_amount_packable(&packable_amount) {
        packable_amount = increase_least_significant_digit(&packable_amount);
    }
    packable_amount
}

fn increase_least_significant_digit(amount: &BigUint) -> BigUint {
    let digits = amount.to_radix_le(10);
    for i in 0..(digits.len()) {
        if digits[i] != 0 {
            return amount + pow(BigUint::from(10u32), i);
        }
    }
    amount.clone() // zero
}
