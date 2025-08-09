use std::{future::Future, pin::Pin};

use crate::{DexId, Provider, Route, SwapRequest};

pub struct LinearProvider;

impl Provider for LinearProvider {
    fn dex_id(&self) -> DexId {
        DexId::Linear
    }

    fn route(&self, request: SwapRequest) -> Pin<Box<dyn Future<Output = Option<Route>> + Send>> {
        Box::pin(async move {
            // let (_, nep141_in) = convert_to_nep141(&request.token_in, None, 0).await?;
            // let (_, nep141_out) = convert_to_nep141(&request.token_out, None, 0).await?;

            // if nep141_in == nep141_out {
            //     // Identity: fee-free conversion is handled in main.rs
            //     return Some(Route {
            //         deadline: None,
            //         has_slippage: false,
            //         estimated_amount: match request.amount {
            //             Amount::AmountIn(amount) => Amount::AmountOut(amount),
            //             Amount::AmountOut(amount) => Amount::AmountIn(amount),
            //         },
            //         worst_case_amount: match request.amount {
            //             Amount::AmountIn(amount) => Amount::AmountOut(amount),
            //             Amount::AmountOut(amount) => Amount::AmountIn(amount),
            //         },
            //         dex_id: DexId::Wrap,
            //         execution_instructions: vec![],
            //         has_leftover_after_slippage_that_needs_unwrapping: false,
            //         token_output: request.token_in,
            //     });
            // }

            None
        })
    }
}
