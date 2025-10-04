use alloy::primitives::{Address, U256};
use amms::arbitrage::mock::{fixtures, MockArbitrageContext};
type Result<T> = eyre::Result<T>;

fn main() -> Result<()> {
    tracing_subscriber::fmt().without_time().init();

    let mut ctx = MockArbitrageContext::new();
    ctx.insert_mantle_usde_usdc_wmnt_triangle();

    if let Ok(mut reader) = csv::Reader::from_path("logs/pool_updates.csv") {
        for record in reader.records() {
            let record = record?;
            let pool = record
                .get(1)
                .and_then(|v| v.parse::<Address>().ok())
                .unwrap_or_default();
            let sqrt_price = record
                .get(3)
                .and_then(|v| v.parse::<u128>().ok())
                .map(U256::from)
                .unwrap_or_default();
            let liquidity = record
                .get(4)
                .and_then(|v| v.parse::<u128>().ok())
                .unwrap_or_default();
            let tick = record
                .get(5)
                .and_then(|v| v.parse::<i32>().ok())
                .unwrap_or_default();

            ctx.set_agni_state(pool, sqrt_price, liquidity, tick)?;
        }
    }

    let meta = fixtures::mantle_triangle_metadata();
    let scenarios = vec![
        (
            meta.pool_wmnt_usde,
            meta.token_wmnt,
            "WMNT->USDe",
            U256::from(500_000u64),
        ),
        (
            meta.pool_usde_usdc,
            meta.token_usde,
            "USDe->USDC",
            U256::from(750_000u64),
        ),
        (
            meta.pool_usdc_wmnt,
            meta.token_usdc,
            "USDC->WMNT",
            U256::from(250_000u64),
        ),
    ];

    for (idx, (pool_address, base_token, description, amount)) in scenarios.into_iter().enumerate()
    {
        println!(
            "\n=== Scenario {}: {} amount_in={} ===",
            idx + 1,
            description,
            amount
        );

        let out = ctx.apply_swap(pool_address, base_token, amount)?;
        println!(
            "Swap result: pool={:#x} base_token={:#x} amount_out={}",
            pool_address, base_token, out
        );

        let opportunities = ctx.find_opportunities_with_details()?;
        println!("Opportunities detected: {}", opportunities.len());

        for (opp_idx, (result, pools)) in opportunities.iter().enumerate() {
            println!(
                "Opportunity {}: profit={} optimal_input={} path_len={}",
                opp_idx + 1,
                result.expected_profit,
                result.optimal_input,
                result.path.hops.len()
            );

            for (hop_idx, (hop, pool)) in result.path.hops.iter().zip(pools.iter()).enumerate() {
                println!(
                    "  Hop {}: pool={:#x} base={:#x} quote={:#x}",
                    hop_idx + 1,
                    hop.pool_address,
                    hop.token_in,
                    hop.token_out
                );
                if let amms::amms::amm::AMM::AgniPool(ref agni) = pool {
                    println!(
                        "    Pool state: tick={} sqrt_price={} liquidity={} fee={} fee_protocol={}",
                        agni.tick, agni.sqrt_price, agni.liquidity, agni.fee, agni.fee_protocol
                    );
                }
            }
        }
    }

    Ok(())
}
