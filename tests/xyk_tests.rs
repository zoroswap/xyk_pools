use anyhow::Result;
use miden_client::{transaction::AdviceInputs, Felt};
use xyk_pool::{
    pool_ops::{
        compile_custom_tx_script, get_math_library, get_pool_library, get_registry_library, isqrt,
    },
    test_utils::*,
    utils::order_assets_as_felts,
};

use std::{collections::BTreeMap, time::Duration};

/// Largest felt `x` such that `x * factor * x` fits in a u128.
fn largest_symmetric_product(factor: u128) -> u64 {
    let mut lo = 1u64;
    let mut hi = u64::MAX - u32::MAX as u64;
    while lo < hi {
        let mid = lo + (hi - lo + 1) / 2;
        let fits = (mid as u128)
            .checked_mul(factor)
            .and_then(|v| v.checked_mul(mid as u128))
            .is_some();
        if fits {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

#[tokio::test]
async fn get_amount_out_u64_fuzz_test() -> Result<()> {
    use rand::Rng;

    let min_reserve: u64 = 1_000_000_000;
    let max_reserve: u64 = 100_000_000_000;
    let min_amount_in: u64 = 100_000;
    let max_amount_in: u64 = 100_000;
    let iterations: usize = 50;

    let mut setup = setup_lightweight_environment().await?;

    let pool_library = get_pool_library()?;
    let mut rng = rand::rng();
    let near = largest_symmetric_product(997);
    let edge_cases = [(near, near, near), (1, 1, 1), (1_000, 50_000, 50_000)];

    for (i, (amount_in_raw, reserve_in_raw, reserve_out_raw)) in edge_cases
        .into_iter()
        .chain((0..iterations).map(|_| {
            (
                rng.random_range(min_amount_in..=max_amount_in),
                rng.random_range(min_reserve..=max_reserve),
                rng.random_range(min_reserve..=max_reserve),
            )
        }))
        .enumerate()
    {
        let reserve_in = Felt::new(reserve_in_raw).unwrap();
        let reserve_out = Felt::new(reserve_out_raw).unwrap();
        let amount_in = Felt::new(amount_in_raw).unwrap();

        let source = format!(
            "use zoro::xyk_pool\n\
             use miden::core::sys\n\
             begin\n\
                 push.{reserve_out}.{reserve_in}.{amount_in}\n\
                 call.xyk_pool::get_amount_out_u64\n\
                 exec.sys::truncate_stack\n\
             end"
        );

        let script = compile_custom_tx_script(&pool_library, &source)?;

        let stack = setup
            .clients
            .client
            .execute_program(
                setup.contract.id(),
                script.clone(),
                AdviceInputs::default(),
                BTreeMap::new(),
            )
            .await?;

        let expected = expected_amount_out(amount_in, reserve_in, reserve_out);

        println!(
            "[{}/{}] reserve_in={}, reserve_out={}, amount_in={} => got={}, expected={}",
            i + 1,
            iterations,
            reserve_in.as_canonical_u64(),
            reserve_out.as_canonical_u64(),
            amount_in.as_canonical_u64(),
            stack[0].as_canonical_u64(),
            expected.as_canonical_u64(),
        );

        assert_eq!(
            stack[0],
            expected,
            "Mismatch at iteration {}: reserve_in={}, reserve_out={}, amount_in={}",
            i + 1,
            reserve_in.as_canonical_u64(),
            reserve_out.as_canonical_u64(),
            amount_in.as_canonical_u64(),
        );

        // let tx_request = TransactionRequestBuilder::new()
        //     .custom_script(script)
        //     .build()?;

        // let tx_result = setup
        //     .clients
        //     .client
        //     .execute_transaction(setup.contract.id(), tx_request)
        //     .await?;
    }
    println!("All {iterations} fuzz iterations passed.");
    Ok(())
}

#[tokio::test]
async fn quote_fuzz_test() -> Result<()> {
    use rand::Rng;

    let min_reserve: u64 = 1_000_000_000;
    let max_reserve: u64 = 100_000_000_000;
    let min_amount: u64 = 100_000;
    let max_amount: u64 = 100_000;
    let iterations: usize = 50;

    let mut setup = setup_lightweight_environment().await?;
    let pool_library = get_pool_library()?;
    let mut rng = rand::rng();
    let felt_max: u64 = u64::MAX - u32::MAX as u64;
    let edge_cases = [(felt_max, felt_max, felt_max), (1, 1, 1)];

    for (i, (amount_raw, reserve_a_raw, reserve_b_raw)) in edge_cases
        .into_iter()
        .chain((0..iterations).map(|_| {
            (
                rng.random_range(min_amount..=max_amount),
                rng.random_range(min_reserve..=max_reserve),
                rng.random_range(min_reserve..=max_reserve),
            )
        }))
        .enumerate()
    {
        let reserve_a = Felt::new(reserve_a_raw).unwrap();
        let reserve_b = Felt::new(reserve_b_raw).unwrap();
        let amount_a = Felt::new(amount_raw).unwrap();

        let source = format!(
            "use zoro::xyk_pool\n\
             use miden::core::sys\n\
             begin\n\
                 push.{reserve_b}.{reserve_a}.{amount_a}\n\
                 call.xyk_pool::quote\n\
                 exec.sys::truncate_stack\n\
             end"
        );

        let script = compile_custom_tx_script(&pool_library, &source)?;
        let stack = setup
            .clients
            .client
            .execute_program(
                setup.contract.id(),
                script.clone(),
                AdviceInputs::default(),
                BTreeMap::new(),
            )
            .await?;

        let expected = expected_quote(amount_a, reserve_a, reserve_b);
        println!(
            "[{}/{}] amount_a={}, reserve_a={}, reserve_b={} => got={}, expected={}",
            i + 1,
            iterations,
            amount_a.as_canonical_u64(),
            reserve_a.as_canonical_u64(),
            reserve_b.as_canonical_u64(),
            stack[0].as_canonical_u64(),
            expected.as_canonical_u64(),
        );
        assert_eq!(
            stack[0],
            expected,
            "Mismatch at iteration {}: amount_a={}, reserve_a={}, reserve_b={}",
            i + 1,
            amount_a.as_canonical_u64(),
            reserve_a.as_canonical_u64(),
            reserve_b.as_canonical_u64(),
        );
    }

    println!("All {iterations} quote fuzz iterations passed.");
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}

#[tokio::test]
async fn get_amount_in_u64_fuzz_test() -> Result<()> {
    use rand::Rng;

    let min_reserve: u64 = 1_000_000_000;
    let max_reserve: u64 = 100_000_000_000;
    let min_amount_out: u64 = 100_000;
    let max_amount_out: u64 = 100_000;
    let iterations: usize = 50;

    let mut setup = setup_lightweight_environment().await?;

    let pool_library = get_pool_library()?;
    let mut rng = rand::rng();
    let near = largest_symmetric_product(1000);
    let edge_cases = [(near, near, near * 2), (1, 1, 2)];

    for (i, (amount_out_raw, reserve_in_raw, reserve_out_raw)) in edge_cases
        .into_iter()
        .chain((0..iterations).map(|_| {
            (
                rng.random_range(min_amount_out..=max_amount_out),
                rng.random_range(min_reserve..=max_reserve),
                rng.random_range(min_reserve..=max_reserve),
            )
        }))
        .enumerate()
    {
        let reserve_in = Felt::new(reserve_in_raw).unwrap();
        let reserve_out = Felt::new(reserve_out_raw).unwrap();
        let amount_out = Felt::new(amount_out_raw).unwrap();

        let source = format!(
            "use zoro::xyk_pool\n\
             use miden::core::sys\n\
             begin\n\
                 push.{reserve_out}.{reserve_in}.{amount_out}\n\
                 call.xyk_pool::get_amount_in_u64\n\
                 exec.sys::truncate_stack\n\
             end"
        );

        let script = compile_custom_tx_script(&pool_library, &source)?;

        let stack = setup
            .clients
            .client
            .execute_program(
                setup.contract.id(),
                script.clone(),
                AdviceInputs::default(),
                BTreeMap::new(),
            )
            .await?;

        let expected = expected_amount_in(amount_out, reserve_in, reserve_out);

        println!(
            "[{}/{}] reserve_in={}, reserve_out={}, amount_out={} => got={}, expected={}",
            i + 1,
            iterations,
            reserve_in.as_canonical_u64(),
            reserve_out.as_canonical_u64(),
            amount_out.as_canonical_u64(),
            stack[0].as_canonical_u64(),
            expected.as_canonical_u64(),
        );

        assert_eq!(
            stack[0],
            expected,
            "Mismatch at iteration {}: reserve_in={}, reserve_out={}, amount_out={}",
            i + 1,
            reserve_in.as_canonical_u64(),
            reserve_out.as_canonical_u64(),
            amount_out.as_canonical_u64(),
        );

        // let tx_request = TransactionRequestBuilder::new()
        //     .custom_script(script)
        //     .build()?;

        // let tx_result = setup
        //     .clients
        //     .client
        //     .execute_transaction(setup.contract.id(), tx_request)
        //     .await?;
    }

    println!("All {iterations} fuzz iterations passed.");
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}

#[tokio::test]
async fn sqrt_u32_fuzz_test() -> Result<()> {
    use rand::Rng;

    let min_n: u32 = 0;
    let max_n: u32 = u32::MAX;
    let iterations: usize = 100;

    let mut setup = setup_lightweight_environment().await?;
    let math_library = get_math_library()?;
    let mut rng = rand::rng();

    let edge_cases: Vec<u32> = vec![0, 1, 2, 3, 4, 9, 15, 16, 255, 65535, u32::MAX - 1, u32::MAX];

    for (i, n) in edge_cases
        .into_iter()
        .chain((0..iterations).map(|_| rng.random_range(min_n..=max_n)))
        .enumerate()
    {
        let source = format!(
            "use zoro::math\n\
             begin\n\
                 push.{n}\n\
                 exec.math::sqrt_u32\n\
             end"
        );

        let script = compile_custom_tx_script(&math_library, &source)?;

        let stack = setup
            .clients
            .client
            .execute_program(
                setup.contract.id(),
                script.clone(),
                AdviceInputs::default(),
                BTreeMap::new(),
            )
            .await?;

        let got = stack[0].as_canonical_u64();
        let expected = isqrt(n as u128) as u64;

        println!("[{}] n={} => got={}, expected={}", i + 1, n, got, expected,);

        assert_eq!(
            got,
            expected,
            "Mismatch at iteration {}: n={}, got={}, expected={}",
            i + 1,
            n,
            got,
            expected,
        );
    }

    println!("All sqrt_u32 fuzz iterations passed.");
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}

#[tokio::test]
async fn sqrt_felt_fuzz_test() -> Result<()> {
    use rand::Rng;

    let min_n: u64 = 0;
    let max_n: u64 = u64::MAX >> 1;
    let iterations: usize = 100;

    let mut setup = setup_lightweight_environment().await?;
    let math_library = get_math_library()?;
    let mut rng = rand::rng();

    let edge_cases: Vec<u64> = vec![
        0,
        1,
        2,
        3,
        4,
        9,
        15,
        16,
        255,
        65535,
        u32::MAX as u64 - 1,
        u32::MAX as u64,
        u32::MAX as u64 + 1,
        1_000_000_000_000,
        u64::MAX >> 1,
    ];

    for (i, n) in edge_cases
        .into_iter()
        .chain((0..iterations).map(|_| rng.random_range(min_n..=max_n)))
        .enumerate()
    {
        let source = format!(
            "use zoro::math\n\
             use miden::core::sys\n\
             begin\n\
                 push.{n}\n\
                 exec.math::sqrt\n\
                 exec.sys::truncate_stack\n\
             end"
        );

        let script = compile_custom_tx_script(&math_library, &source)?;

        // let tx_request = TransactionRequestBuilder::new()
        //     .custom_script(script.clone())
        //     .build()?;

        // let tx_result = setup
        //     .clients
        //     .client
        //     .execute_transaction(setup.contract.id(), tx_request)
        //     .await?;

        let stack = setup
            .clients
            .client
            .execute_program(
                setup.contract.id(),
                script.clone(),
                AdviceInputs::default(),
                BTreeMap::new(),
            )
            .await?;

        let got = stack[0].as_canonical_u64();
        let expected = isqrt(n as u128) as u64;

        println!("[{}] n={} => got={}, expected={}", i + 1, n, got, expected);

        assert_eq!(
            got,
            expected,
            "Mismatch at iteration {}: n={}, got={}, expected={}",
            i + 1,
            n,
            got,
            expected,
        );
    }

    println!("All sqrt felt fuzz iterations passed.");
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}

#[tokio::test]
async fn order_assets_fuzz_test() -> Result<()> {
    use rand::Rng;

    let iterations: usize = 100;
    let max_val: u64 = u64::MAX >> 1;

    let mut setup = setup_lightweight_environment().await?;
    let registry_library = get_registry_library()?;
    let mut rng = rand::rng();

    let edge_cases: Vec<(u64, u64, u64, u64)> = vec![
        (0, 0, 0, 1),
        (0, 1, 0, 0),
        (1, 0, 0, 0),
        (0, 0, 1, 0),
        (100, 200, 100, 300),
        (100, 300, 100, 200),
        (1, 1, 2, 2),
        (2, 2, 1, 1),
        (max_val, max_val, 0, 0),
        (0, 0, max_val, max_val),
    ];

    for (i, (a0_pfx, a0_sfx, a1_pfx, a1_sfx)) in edge_cases
        .into_iter()
        .chain((0..iterations).map(|_| {
            (
                rng.random_range(0..=max_val),
                rng.random_range(0..=max_val),
                rng.random_range(0..=max_val),
                rng.random_range(0..=max_val),
            )
        }))
        .filter(|(a0_pfx, a0_sfx, a1_pfx, a1_sfx)| a0_pfx != a1_pfx || a0_sfx != a1_sfx)
        .enumerate()
    {
        let source = format!(
            "use zoro::registry\n\
             use miden::core::sys\n\
             begin\n\
                 push.{a1_sfx}.{a1_pfx}.{a0_sfx}.{a0_pfx}\n\
                 call.registry::order_assets\n\
                 exec.sys::truncate_stack\n\
             end"
        );

        let script = compile_custom_tx_script(&registry_library, &source)?;

        let stack = setup
            .clients
            .client
            .execute_program(
                setup.contract.id(),
                script,
                AdviceInputs::default(),
                BTreeMap::new(),
            )
            .await?;

        let (exp_lo_pfx, exp_lo_sfx, exp_hi_pfx, exp_hi_sfx) = order_assets_as_felts(
            Felt::new(a0_pfx)?,
            Felt::new(a0_sfx)?,
            Felt::new(a1_pfx)?,
            Felt::new(a1_sfx)?,
        )?;

        let got = (
            stack[0].as_canonical_u64(),
            stack[1].as_canonical_u64(),
            stack[2].as_canonical_u64(),
            stack[3].as_canonical_u64(),
        );
        let expected = (
            exp_lo_pfx.as_canonical_u64(),
            exp_lo_sfx.as_canonical_u64(),
            exp_hi_pfx.as_canonical_u64(),
            exp_hi_sfx.as_canonical_u64(),
        );

        println!(
            "[{}] in=({},{},{},{}) => got={:?}, expected={:?}",
            i + 1,
            a0_pfx,
            a0_sfx,
            a1_pfx,
            a1_sfx,
            got,
            expected,
        );

        assert_eq!(
            got,
            expected,
            "Mismatch at iteration {}: input=({},{},{},{})",
            i + 1,
            a0_pfx,
            a0_sfx,
            a1_pfx,
            a1_sfx,
        );
    }

    println!("All order_assets fuzz iterations passed.");
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}

#[tokio::test]
async fn order_assets_same_asset_fails_test() -> Result<()> {
    use rand::Rng;

    let mut setup = setup_lightweight_environment().await?;
    let registry_library = get_registry_library()?;
    let mut rng = rand::rng();

    let pfx: u64 = rng.random_range(0..=(u64::MAX >> 1));
    let sfx: u64 = rng.random_range(0..=(u64::MAX >> 1));

    let source = format!(
        "use zoro::registry\n\
         use miden::core::sys\n\
         begin\n\
             push.{sfx}.{pfx}.{sfx}.{pfx}\n\
             call.registry::order_assets\n\
             exec.sys::truncate_stack\n\
         end"
    );

    let script = compile_custom_tx_script(&registry_library, &source)?;

    let result = setup
        .clients
        .client
        .execute_program(
            setup.contract.id(),
            script,
            AdviceInputs::default(),
            BTreeMap::new(),
        )
        .await;

    assert!(
        result.is_err(),
        "order_assets with identical assets should fail, but got Ok({:?})",
        result.unwrap()
    );
    println!(
        "order_assets_same_asset_fails_test: correctly failed with: {:?}",
        result.unwrap_err()
    );

    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}
