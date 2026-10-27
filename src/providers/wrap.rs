use std::{future::Future, pin::Pin};

use crate::{
    route::ProviderRoute, shared_utils::convert_to_nep141, Amount, DexId, Provider, SwapRequest,
};

pub struct WrapProvider;

impl Provider for WrapProvider {
    fn dex_id(&self) -> DexId {
        DexId::Wrap
    }

    fn route(
        &self,
        request: SwapRequest,
    ) -> Pin<Box<dyn Future<Output = Option<ProviderRoute>> + Send>> {
        Box::pin(async move {
            let (_, nep141_in) = convert_to_nep141(&request.token_in, None, 0).await?;
            let (_, nep141_out) = convert_to_nep141(&request.token_out, None, 0).await?;

            if nep141_in == nep141_out {
                // Identity: fee-free conversion is handled in main.rs
                return Some(ProviderRoute {
                    estimated_amount: match request.amount {
                        Amount::AmountIn(amount) => Amount::AmountOut(amount),
                        Amount::AmountOut(amount) => Amount::AmountIn(amount),
                    },
                    worst_case_amount: match request.amount {
                        Amount::AmountIn(amount) => Amount::AmountOut(amount),
                        Amount::AmountOut(amount) => Amount::AmountIn(amount),
                    },
                    dex_id: DexId::Wrap,
                    transactions: vec![],
                    discount: None,
                    token_output: request.token_in,
                });
            }

            None
        })
    }
}
