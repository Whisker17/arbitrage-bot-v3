use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
};

use alloy::{dyn_abi::DynSolType, network::Network, primitives::Address, providers::Provider, sol};
use error::{AMMError, BatchContractError};
use futures::{stream::FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};

pub mod agni;
pub mod amm;
pub mod batch_create;
pub mod consts;
pub mod error;
pub mod factory;
pub mod float;
pub mod logs;
pub mod moe;
pub mod uniswap_v2;
pub mod uniswap_v3;

sol! {
    #[sol(rpc)]
    GetTokenDecimalsBatchRequest,
    "src/amms/abi/GetTokenDecimalsBatchRequest.json",
}

sol! {
    #[sol(rpc)]
    GetMoeLBPairSlot0BatchRequest,
    "src/amms/abi/GetMoeLBPairSlot0BatchRequest.json",
}

sol! {
    #[sol(rpc)]
    GetMoeLBPairBinDataBatchRequest,
    "src/amms/abi/GetMoeLBPairBinDataBatchRequest.json",
}

sol!(
#[derive(Debug, PartialEq, Eq)]
#[sol(rpc)]
contract IERC20 {
    function decimals() external view returns (uint8);
});

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Token {
    pub address: Address,
    pub decimals: u8,
    // TODO: add optional tax
}

impl Token {
    pub async fn new<N, P>(address: Address, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let decimals = IERC20::new(address, provider).decimals().call().await?;

        Ok(Self { address, decimals })
    }

    pub const fn new_with_decimals(address: Address, decimals: u8) -> Self {
        Self { address, decimals }
    }

    pub const fn address(&self) -> &Address {
        &self.address
    }

    pub const fn decimals(&self) -> u8 {
        self.decimals
    }
}

impl From<Address> for Token {
    fn from(address: Address) -> Self {
        Self {
            address,
            decimals: 0,
        }
    }
}

impl Hash for Token {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.address.hash(state);
    }
}

/// Fetches the decimal precision for a list of ERC-20 tokens.
///
/// # Returns
/// A map of token addresses to their decimal precision.
pub async fn get_token_decimals<N, P>(
    tokens: Vec<Address>,
    provider: P,
) -> Result<HashMap<Address, u8>, BatchContractError>
where
    N: Network,
    P: Provider<N> + Clone + Clone,
{
    // Size-derived from `uint8[]` return (1 ABI word/item + array head).
    // Old hard-coded `step = 765` exceeded the 50% EIP-170 budget (~382).
    // Fixed-size payload — documented; no split wrapper required (WHI-929 audit).
    let step = batch_create::max_items_for_return_size(
        batch_create::TOKEN_DECIMALS_RETURN_BYTES_PER,
        batch_create::ABI_DYNAMIC_ARRAY_OVERHEAD,
    );
    tracing::info!(
        target: "amms.batch_create",
        path = "token_decimals",
        item_count = tokens.len(),
        chunk_size = step,
        per_item_bytes = batch_create::TOKEN_DECIMALS_RETURN_BYTES_PER,
        "token decimals batch CREATE"
    );

    let mut futures = FuturesUnordered::new();
    tokens.chunks(step).for_each(|group| {
        let provider = provider.clone();

        futures.push(async move {
            (
                group,
                GetTokenDecimalsBatchRequest::deploy_builder(provider, group.to_vec())
                    .call_raw()
                    .await,
            )
        });
    });

    let mut token_decimals = HashMap::new();
    let return_type = DynSolType::Array(Box::new(DynSolType::Uint(8)));

    while let Some(res) = futures.next().await {
        let (token_addresses, return_data) = res;

        let return_data = return_type.abi_decode_sequence(&return_data?)?;

        if let Some(tokens_arr) = return_data.as_array() {
            for (decimals, token_address) in tokens_arr.iter().zip(token_addresses.iter()) {
                token_decimals.insert(
                    *token_address,
                    decimals.as_uint().expect("Could not get uint").0.to::<u8>(),
                );
            }
        }
    }
    Ok(token_decimals)
}
