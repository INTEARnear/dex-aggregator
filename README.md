# DEX Aggregator

This is the code that runs on router.intear.tech

## User documentation

Refer to https://docs.intear.tech/docs/dex-aggregator/ for integration instruction. This README can help you understand how DEX Aggregator actually works in-depth, so it's recommended to read as well.

## Architecture

It's separated into 3 crates:

- `swap-router`: the main aggregator crate, hosts the API and builds transactions. Example URL: https://router.intear.tech/route?token_in=rhea-nep141:wrap.near&token_out=juij.launch.intear.near&amount_in=1000000000000000000000000&max_wait_ms=2000&slippage_type=Fixed&slippage=0.01&dexes=Rhea%2CAidols%2CWrap%2CRheaDcl%2CMetaPool%2CLinear%2CXRhea%2CRNear%2CPlach&trader_account_id=intear.near&signing_public_key=ed25519%3A4WZAP6JoruNHR2W4mi9fM219viZKYskqLRDYkwBHCUY8&referrer_id=intear.near
- `rhea-pathfinder`: smartrouter.ref.finance-almost-compatible internal API that finds the best route among all Rhea pools. Example URL: http://localhost:12345/findPath?amountIn=1000000000000000000000&tokenIn=wrap.near&tokenOut=jambo-1679.meme-cooking.near&maxHops=Four&slippage=0.05
- `plach-pathfinder`: same thing as `rhea-pathfinder` but for Intear Plach. Example URL: http://localhost:12346/findPath?amountIn=1000000000000000000000000000&tokenIn=near&tokenOut=nep141:juij.launch.intear.near&maxHops=Four&slippage=0.0005, http://localhost:12346/findPath?amountOut=1000000000000000000000000000000&tokenIn=near&tokenOut=nep141:juij.launch.intear.near&maxHops=DirectOnly&slippage=0.005

## Routing strategies

- `Rhea`: uses `rhea-pathfinder`
- `Aidols`: calls `emulate_swap` or `emulate_swap_by_out` for *.aidols.near tokens. Fails if the token has already bonded to Rhea
- `Wrap`: a no-op route (later converted to the necessary location, check [Different output token locations](#different-output-token-locations))
- `RheaDcl`: scans all direct pairs (max possible by contract is 4 between 2 tokens, due to 4 different fee tiers) and emulates each of them on chain using `quote` view method. Advanced cross-pool routing is not implemented due to there being only ~6 pools with over $0 daily volume, and DCL contract not being open source
- `MetaPool`: staking NEAR into STNEAR & liquid withdrawal. Slippage is only ever possible between epoch boundaries, so API assumes there's no slippage
- `Linear`: staking NEAR into LiNEAR. Doesn't implement liquid withdrawal (official way is to sell on Rhea). Slippage is only ever possible between epoch boundaries, so API assumes there's no slippage
- `XRhea`: staking RHEA into xRHEA & unstaking. There's no staking lock or fee.
- `RNear`: liquid staking provider based on LiNEAR, so behavior copies `Linear`
- `Plach`: uses `plach-pathfinder`

## Different output token locations

DEX Aggregator tries to intelligently convert tokens to their necessary location before / after swap. Locations are defined as representations of the same token. For example, NEAR can be either native NEAR, wrap.near, wrap.near stored in the inner Rhea balance, NEAR or wrap.near stored in Intear DEX balance, etc.

Tokens can be passed as input parameters in this format:
- `near` (fixed string; native NEAR)
- `nep141:wrap.near`
- `wrap.near` (if does not equal to `near`, it's an alias to `nep141:<string>`)
- `rhea-nep141:wrap.near`
- `intear-dex:<asset id>`, where the asset id is the one Intear DEX itself uses: `intear-dex:near`, `intear-dex:nep141:wrap.near`, `intear-dex:nep245:token.near:token-id`, `intear-dex:nep171:token.near:token-id`

Upcoming variants (no timeline, just to show the vision):
- `nep245:token.near:token-id`
- `nep171:token.near:token-id`

Certain steps can be omitted to optimize transaction count & speed. For example, [Bettear Bot](https://t.me/bettearbot) stores all user tokens in `rhea-nep141:` inner balances, so swaps could be just a single `swap` transaction and 1 receipt instead of `ft_transfer_call` the input token + `ft_on_transfer` on rhea + `ft_transfer` the output token.

Some DEXes (such as Rhea) only support NEP-141 tokens, but the user might want to use a native NEAR coin, so at the beginning of the swap the aggregator does `near_deposit` to mint `wrap.near` and then use the wrapped NEAR. Similarly, at the end of the swap, if the output amount is known (e.g. a DEX has no slippage, or request is exact-output), the aggregator tries to add a conversion to the desired token location. If it's not possible (e.g. output is not exactly known due to possible slippage), the route response includes `token_output` which can be different from the specified token output in your request. If it's different, you have to track how many tokens were received from the swap (recommended way is to parse logs from execution outcome) and request a second quote, from `token_output` to your desired token, with only `Wrap` as DEXes, and execute that route as a second step. It's guaranteed to have no slippage and be a 1-to-1 conversion. Reference implementation of this behavior: https://github.com/INTEARnear/dex-frontend/blob/a4d724e78cce026f5530e1ef845c2dadad689f3c/src/lib/SwapForm.svelte

As a post-processing step, chained transactions to the same contract are merged into one (e.g. `storage_deposit` + `near_deposit` + `ft_transfer_call` for `wrap.near`) to optimize transaction count. When ordering of transactions doesn't matter (e.g. storage deposit needed for output token & location conversion needed for input token), DEX Aggregator tries to arrange them in a way that is more likely to be optimizable this way.
