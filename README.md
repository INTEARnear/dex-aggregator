# DEX Aggregator

This is the code that runs on router.intear.tech

## User documentation

Refer to https://docs.intear.tech/docs/dex-aggregator/ for integration instruction. This README can help you understand how DEX Aggregator actually works in-depth, so it's recommended to read as well.

## Architecture

It's separated into 4 crates:

- `swap-router`: the main aggregator crate, hosts the API and builds transactions. Example URL: https://router.intear.tech/route?token_in=rhea-nep141:wrap.near&token_out=juij.launch.intear.near&amount_in=1000000000000000000000000&slippage_type=Fixed&slippage=0.01&dexes=Rhea%2CWrap%2CRheaDcl%2CPlach&trader_account_id=intear.near&signing_public_key=ed25519%3A4WZAP6JoruNHR2W4mi9fM219viZKYskqLRDYkwBHCUY8&referrer_id=intear.near
- `pathfinder`: library that indexes the pools of Rhea, Intear Plach and Rhea DCL and finds the best route within one of them. Rhea DCL swap math in `pathfinder::dcl` is ported from iZiSwap (GPL-2.0-or-later). `swap-router` runs it in the same process, so a route is found with a function call: `pathfinder::run` follows every final block and keeps each DEX's pool snapshot up to date, and `pathfinder::<dex>::find_path` searches the newest snapshot on a blocking thread.
- `pool-indexer`: keeps a copy of the contract storage that pools are built from=
- `swap-router-types`: the API's types, the app fee split, and filling OTC DEX intents, shared by `swap-router` and its hooks. With the `hook` feature, `Hook` and `export_hook!` build a hook library without writing its C ABI.

Routing is deterministic, so running `quote` again on the same block heights and queries gives identical output.

## Routing strategies

- `Rhea`: uses `pathfinder`
- `Wrap`: a no-op route (later converted to the necessary location, check [Different output token locations](#different-output-token-locations))
- `RheaDcl`: uses `pathfinder`
- `Plach`: uses `pathfinder`
- `hook:<name>`: a hook, loaded at startup from `hooks/<name>.so` in the working directory. After the other DEXes are routed, it gets the request data and current quotes, and can add quotes of its own, which `swap-router` builds the routes of. Today a quote is a trade intent signed for the Intear DEX OTC. Limited to 10ms execution time. The ABI is in `swap_router_types::hook`.

## Different output token locations

DEX Aggregator tries to intelligently convert tokens to their necessary location before / after swap. Locations are defined as representations of the same token. For example, NEAR can be either native NEAR, wrap.near, wrap.near stored in the inner Rhea balance, NEAR or wrap.near stored in Intear DEX balance, etc.

Tokens can be passed as input parameters in this format:
- `near` (fixed string; native NEAR)
- `nep141:wrap.near`
- `wrap.near` (if does not equal to `near`, it's an alias to `nep141:<string>`)
- `rhea-nep141:wrap.near`
- `intear-dex:<asset id>`, where the asset id is the one Intear DEX itself uses: `intear-dex:near`, `intear-dex:nep141:wrap.near`, `intear-dex:nep245:token.near:token-id`, `intear-dex:nep171:token.near:token-id`

Upcoming variants (if/when adopted, just to show the vision):
- `nep245:token.near:token-id`
- `nep171:token.near:token-id`

Certain steps can be omitted to optimize transaction count & speed. For example, [Bettear Bot](https://t.me/bettearbot) stores all user tokens in `rhea-nep141:` inner balances, so swaps could be just a single `swap` transaction and 1 receipt instead of `ft_transfer_call` the input token + `ft_on_transfer` on rhea + `ft_transfer` the output token.

Some DEXes (such as Rhea) only support NEP-141 tokens, but the user might want to use a native NEAR coin, so at the beginning of the swap the aggregator does `near_deposit` to mint `wrap.near` and then use the wrapped NEAR. Similarly, at the end of the swap, if the output amount is known (e.g. a DEX has no slippage, or request is exact-output), the aggregator tries to add a conversion to the desired token location. If it's not possible (e.g. output is not exactly known due to possible slippage), the route response includes `token_output` which can be different from the specified token output in your request. If it's different, call [`/route/outcome`](#checking-what-a-route-did) after the swap: its `next_steps` convert what was received to your desired token. You can also track how many tokens were received yourself and request a second quote, from `token_output` to your desired token, with only `Wrap` as DEXes, and execute that route as a second step. It's guaranteed to have no slippage and be a 1-to-1 conversion. Reference implementation of this behavior can be found in Intear Wallet.

As a post-processing step, chained transactions to the same contract are merged into one (e.g. `storage_deposit` + `near_deposit` + `ft_transfer_call` for `wrap.near`) to optimize transaction count. When ordering of transactions doesn't matter (e.g. storage deposit needed for output token & location conversion needed for input token), DEX Aggregator tries to arrange them in a way that is more likely to be optimizable this way.
