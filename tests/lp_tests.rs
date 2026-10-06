use anyhow::Result;
use miden_client::{
    Felt, Word,
    transaction::{AdviceInputs, TransactionRequestBuilder},
};
use xyk_pool::{
    pool_ops::{
        compile_custom_tx_script, compile_lp_local_fuzz_tx_script, compute_expected_lp,
        compute_expected_withdraw, get_lp_local_library,
    },
    test_utils::*,
    utils::slot_name,
};

use std::{collections::BTreeMap, time::Duration};

#[tokio::test]
async fn get_lp_amount_out_fuzz_test() -> Result<()> {
    use rand::Rng;

    let min_reserve: u64 = 1_000;
    let max_reserve: u64 = 1_000_000_000;
    let min_amount: u64 = 1;
    let max_amount: u64 = 100_000_000;
    let iterations: usize = 100;

    let mut setup = setup_lightweight_environment().await?;
    let lp_local_library = get_lp_local_library()?;
    let mut rng = rand::rng();

    let felt_max: u64 = u64::MAX - u32::MAX as u64;
    let edge_cases: Vec<(u64, u64, u64, u64, u64)> = vec![
        (0, 100, 100, 0, 0),
        (0, 1_000_000, 1_000_000, 0, 0),
        (0, felt_max, felt_max, 0, 0),
        (1000, 100, 100, 1000, 1000),
        (10000, 500, 500, 50000, 50000),
        (felt_max, felt_max, felt_max, felt_max, felt_max),
    ];

    for (i, (total_supply, amount_0, amount_1, reserve_0, reserve_1)) in edge_cases
        .into_iter()
        .chain((0..iterations).map(|_| {
            let total_supply = if rng.random_range(0..2) == 0 {
                0
            } else {
                rng.random_range(min_reserve..=max_reserve)
            };
            let amount_0 = rng.random_range(min_amount..=max_amount);
            let amount_1 = rng.random_range(min_amount..=max_amount);
            let reserve_0 = if total_supply == 0 {
                0
            } else {
                rng.random_range(min_reserve..=max_reserve)
            };
            let reserve_1 = if total_supply == 0 {
                0
            } else {
                rng.random_range(min_reserve..=max_reserve)
            };
            (total_supply, amount_0, amount_1, reserve_0, reserve_1)
        }))
        .enumerate()
    {
        if total_supply > 0 && (reserve_0 == 0 || reserve_1 == 0) {
            continue;
        }

        let source = format!(
            "use zoro::lp_local\n\
             use miden::core::sys\n\
             begin\n\
                 push.{reserve_1}.{reserve_0}.{amount_1}.{amount_0}.{total_supply}\n\
                 call.lp_local::get_lp_amount_out\n\
                 exec.sys::truncate_stack\n\
             end"
        );

        let script = compile_custom_tx_script(&lp_local_library, &source)?;

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
        let expected = compute_expected_lp(amount_0, amount_1, reserve_0, reserve_1, total_supply);

        if got == expected {
            println!(
                "[{}] ts={} a0={} a1={} r0={} r1={} => got={}, expected={}",
                i + 1,
                total_supply,
                amount_0,
                amount_1,
                reserve_0,
                reserve_1,
                got,
                expected,
            );
        };
        assert_eq!(
            got,
            expected,
            "Mismatch at iteration {}: total_supply={}, amount_0={}, amount_1={}, reserve_0={}, reserve_1={}, got={}, expected={}",
            i + 1,
            total_supply,
            amount_0,
            amount_1,
            reserve_0,
            reserve_1,
            got,
            expected,
        );
    }

    println!("All get_lp_amount_out fuzz iterations passed.");
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}

#[tokio::test]
async fn simulate_withdraw_fuzz_test() -> Result<()> {
    use rand::Rng;

    let min_reserve: u64 = 1_000;
    let max_reserve: u64 = 1_000_000_000;
    let min_amount: u64 = 1;
    let max_amount: u64 = 100_000_000;
    let iterations: usize = 100;

    let mut setup = setup_lightweight_environment().await?;
    let lp_local_library = get_lp_local_library()?;
    let mut rng = rand::rng();

    let felt_max: u64 = u64::MAX - u32::MAX as u64;
    let edge_cases: Vec<(u64, u64, u64, u64)> = vec![
        (100, 100, 100, 0),
        (1_000_000, 1_000_000, 1_000_000, 0),
        (1000, 100, 100, 1000),
        (10000, 500, 500, 50000),
        (felt_max, felt_max, felt_max, felt_max),
    ];

    for (i, (total_supply, lp_amount, reserve_0, reserve_1)) in edge_cases
        .into_iter()
        .chain((0..iterations).map(|_| {
            let total_supply = if rng.random_range(0..2) == 0 {
                0
            } else {
                rng.random_range(min_reserve..=max_reserve)
            };
            let lp_amount = rng.random_range(min_amount..=max_amount);
            let reserve_0 = if total_supply == 0 {
                0
            } else {
                rng.random_range(min_reserve..=max_reserve)
            };
            let reserve_1 = if total_supply == 0 {
                0
            } else {
                rng.random_range(min_reserve..=max_reserve)
            };
            (total_supply, lp_amount, reserve_0, reserve_1)
        }))
        .enumerate()
    {
        if total_supply == 0 || (total_supply > 0 && (reserve_0 == 0 || reserve_1 == 0)) {
            continue;
        }
        let expected = compute_expected_withdraw(total_supply, lp_amount, reserve_0, reserve_1);

        let source = format!(
            "use zoro::lp_local\n\
             use miden::core::sys\n\
             begin\n\
                 push.{reserve_1}.{reserve_0}.{lp_amount}.{total_supply}\n\
                 call.lp_local::simulate_withdraw\n\
                 exec.sys::truncate_stack\n\
             end"
        );

        let script = compile_custom_tx_script(&lp_local_library, &source)?;

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

        let got = (stack[0].as_canonical_u64(), stack[1].as_canonical_u64());

        println!(
            "[{}] ts={} lp={} ar0={} r1={} => got=({},{}), expected=({},{})",
            i + 1,
            total_supply,
            lp_amount,
            reserve_0,
            reserve_1,
            got.0,
            got.1,
            expected.0,
            expected.1,
        );

        assert_eq!(
            got,
            expected,
            "Mismatch at iteration {}: total_supply={}, lp_amount={}, reserve_0={}, reserve_1={}, got=({},{}), expected=({},{})",
            i + 1,
            total_supply,
            lp_amount,
            reserve_0,
            reserve_1,
            got.0,
            got.1,
            expected.0,
            expected.1,
        );
    }

    println!("All get_lp_amount_out fuzz iterations passed.");
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}

#[tokio::test]
async fn lp_local_asset_getters_test() -> Result<()> {
    let mut setup = setup_lp_local_test_environment().await?;
    let lp_local_library = get_lp_local_library()?;
    let token0_id = setup.faucets[0].faucet.id();
    let token1_id = setup.faucets[1].faucet.id();
    let getters_source = format!(
        "use zoro::lp_local\n\
         use miden::core::sys\n\
         begin\n\
             push.{}.{}\n\
             call.lp_local::get_asset_index\n\
             push.{}.{}\n\
             call.lp_local::get_asset_index\n\
             exec.sys::truncate_stack\n\
         end",
        token1_id.suffix().as_canonical_u64(),
        token1_id.prefix().as_u64(),
        token0_id.suffix().as_canonical_u64(),
        token0_id.prefix().as_u64()
    );

    let script = compile_custom_tx_script(&lp_local_library, &getters_source)?;

    let expected = vec![0, 1];
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

    let got: Vec<u64> = stack[0..2].iter().map(|x| x.as_canonical_u64()).collect();

    assert_eq!(
        got, expected,
        "lp_local_asset_getters_test mismatch: got={:?}, expected={:?}",
        got, expected
    );

    Ok(())
}

#[tokio::test]
async fn lp_local_reserve_by_asset_id_test() -> Result<()> {
    use rand::Rng;

    let mut setup = setup_lp_local_test_environment().await?;
    let lp_local_library = get_lp_local_library()?;
    let mut rng = rand::rng();

    let token0_id = setup.faucets[0].faucet.id();
    let token1_id = setup.faucets[1].faucet.id();

    let iterations = 100u32;
    let min_reserve = 50000000u64;
    let max_reserve = 500000000000u64;
    let max_add = 200000u64;
    let max_sub = 200000u64;

    for i in 0..iterations {
        let start_reserves_0 = rng.random_range(min_reserve..=max_reserve);
        let start_reserves_1 = rng.random_range(min_reserve..=max_reserve);
        let add_to_token0_amount = rng.random_range(1..=max_add);
        let sub_from_token1_amount = rng.random_range(1..=max_sub.min(start_reserves_1));

        let source = format!(
            "use zoro::lp_local\n\
             use miden::core::sys\n\
             begin\n\
                 push.{start_reserves_1}.{start_reserves_0}\n\
                 call.lp_local::set_reserves\n\
                 push.{t0_suffix}.{t0_prefix}.{add_to_token0_amount}\n\
                 call.lp_local::add_to_reserve_by_asset_id\n\
                 push.{t1_suffix}.{t1_prefix}.{sub_from_token1_amount}\n\
                 call.lp_local::sub_from_reserve_by_asset_id\n\
                 push.{t1_suffix}.{t1_prefix}\n\
                 call.lp_local::get_reserve_by_asset_id\n\
                 push.{t0_suffix}.{t0_prefix}\n\
                 call.lp_local::get_reserve_by_asset_id\n\
                 exec.sys::truncate_stack\n\
             end",
            t0_prefix = token0_id.prefix().as_u64(),
            t0_suffix = token0_id.suffix().as_canonical_u64(),
            t1_prefix = token1_id.prefix().as_u64(),
            t1_suffix = token1_id.suffix().as_canonical_u64(),
        );

        let script = compile_custom_tx_script(&lp_local_library, &source)?;

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

        let reserve_0 = stack[0].as_canonical_u64();
        let reserve_1 = stack[1].as_canonical_u64();
        let expected_reserve_0 = start_reserves_0 + add_to_token0_amount;
        let expected_reserve_1 = start_reserves_1 - sub_from_token1_amount;
        println!(
            "iteration {}: start_reserves_0={}, start_reserves_1={}, add_to_token0_amount={}, sub_from_token1_amount={}, got (reserve_0={}, reserve_1={}), expected (reserve_0={}, reserve_1={})",
            i + 1,
            start_reserves_0,
            start_reserves_1,
            add_to_token0_amount,
            sub_from_token1_amount,
            reserve_0,
            reserve_1,
            expected_reserve_0,
            expected_reserve_1
        );
        assert_eq!(
            (reserve_0, reserve_1),
            (expected_reserve_0, expected_reserve_1),
            "reserve_by_asset_id: expected (reserve_0={}, reserve_1={}), got (reserve_0={}, reserve_1={})",
            expected_reserve_0,
            expected_reserve_1,
            reserve_0,
            reserve_1,
        );
    }

    Ok(())
}

#[tokio::test]
async fn lp_local_reserve_by_asset_id_unknown_asset_fails_test() -> Result<()> {
    let mut setup = setup_lp_local_test_environment().await?;
    let lp_local_library = get_lp_local_library()?;

    // Non-pool asset id (0, 0) so get_reserve_by_asset_id hits ERR_UNKNOWN_ASSET.
    let source = "use zoro::lp_local\n\
         use miden::core::sys\n\
         begin\n\
             push.100.200\n\
             call.lp_local::set_reserves\n\
             push.0.0\n\
             call.lp_local::get_reserve_by_asset_id\n\
             exec.sys::truncate_stack\n\
         end";

    let script = compile_custom_tx_script(&lp_local_library, source)?;

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
        "get_reserve_by_asset_id with non-pool asset id should fail with ERR_UNKNOWN_ASSET"
    );

    Ok(())
}

#[tokio::test]
async fn lp_mint_fuzz_test() -> Result<()> {
    use rand::Rng;

    let iterations: usize = 10;
    let min_amount: u64 = 1;
    let max_amount: u64 = 1_000_000;

    let mut setup = setup_lp_local_fuzz_environment().await?;
    let mut rng = rand::rng();

    let prefix = setup.contract.id().prefix().as_felt();
    let suffix = setup.contract.id().suffix();

    let mut expected_total_supply: u64 = 0;
    let mut expected_user_balance: u64 = 0;

    for i in 0..iterations {
        let amount = rng.random_range(min_amount..=max_amount);

        let source = format!(
            "use zoro::lp_local\n\
             use miden::core::sys\n\
             begin\n\
                 push.{suffix}.{prefix}.{amount}\n\
                 call.lp_local::mint\n\
                 push.{suffix}.{prefix}\n\
                 call.lp_local::get_user_deposit\n\
                 call.lp_local::total_supply\n\
                 exec.sys::truncate_stack\n\
             end"
        );

        let script = compile_lp_local_fuzz_tx_script(&source)?;

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

        expected_total_supply = expected_total_supply.saturating_add(amount);
        expected_user_balance = expected_user_balance.saturating_add(amount);

        let total_supply = stack[0].as_canonical_u64();
        let user_deposit = stack[1].as_canonical_u64();

        println!(
            "[{}/{}] mint amount={} => total_supply={}, user_deposit={} (expected {} {})",
            i + 1,
            iterations,
            amount,
            total_supply,
            user_deposit,
            expected_total_supply,
            expected_user_balance,
        );

        let tx_request = TransactionRequestBuilder::new()
            .custom_script(script)
            .build()?;
        setup
            .clients
            .client
            .submit_new_transaction(setup.contract.id(), tx_request)
            .await?;
        setup.clients.client.sync_state().await?;

        assert_eq!(
            total_supply,
            expected_total_supply,
            "total_supply mismatch at iteration {}",
            i + 1
        );
        assert_eq!(
            user_deposit,
            expected_user_balance,
            "user_deposit mismatch at iteration {}",
            i + 1
        );
    }

    println!("All lp_mint fuzz iterations passed.");
    Ok(())
}

#[tokio::test]
async fn lp_burn_fuzz_test() -> Result<()> {
    use rand::Rng;

    let iterations: usize = 10;
    let min_burn: u64 = 1;
    let max_burn: u64 = 100_000;
    let initial_mint: u64 = 1_000_000_000;

    print_phase("test setup");

    let mut setup = setup_lp_local_fuzz_environment().await?;
    let mut rng = rand::rng();

    let prefix = setup.contract.id().prefix().as_felt();
    let suffix = setup.contract.id().suffix();

    print_phase("initial mint");

    // Initial mint
    let mint_source = format!(
        "use zoro::lp_local\n\
         use miden::core::sys\n\
         begin\n\
             push.{suffix}.{prefix}.{initial_mint}\n\
             call.lp_local::mint\n\
             exec.sys::truncate_stack\n\
         end"
    );
    let mint_script = compile_lp_local_fuzz_tx_script(&mint_source)?;
    let tx_request = TransactionRequestBuilder::new()
        .custom_script(mint_script)
        .build()?;
    setup
        .clients
        .client
        .submit_new_transaction(setup.contract.id(), tx_request)
        .await?;
    setup.clients.client.sync_state().await?;

    print_phase("check account");

    let acc_after = setup
        .clients
        .client
        .get_account(setup.contract.id())
        .await?
        .unwrap();

    let acc_after_storage = acc_after.storage();
    let usr_key = Word::new([Felt::ZERO, Felt::ZERO, suffix, prefix]);
    let usr_depo = acc_after_storage
        .get_map_item(&slot_name("zoro::lp_local::user_deposits_mapping"), usr_key)?;
    println!("usr_depo: after mint {:?}", usr_depo);

    let mut expected_total_supply: u64 = initial_mint;
    let mut expected_user_balance: u64 = initial_mint;

    print_phase("burning");

    for i in 0..iterations {
        if expected_user_balance == 0 {
            break;
        }
        let burn_amount = rng.random_range(min_burn..=max_burn.min(expected_user_balance));
        if burn_amount == 0 {
            continue;
        }

        let source = format!(
            "use zoro::lp_local\n\
             use miden::core::sys\n\
             begin\n\
                 push.{suffix}.{prefix}.{burn_amount}\n\
                 call.lp_local::burn\n\
                 push.{suffix}.{prefix}\n\
                 call.lp_local::get_user_deposit\n\
                 call.lp_local::total_supply\n\
                 exec.sys::truncate_stack\n\
             end"
        );

        let script = compile_lp_local_fuzz_tx_script(&source)?;

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

        expected_total_supply = expected_total_supply.saturating_sub(burn_amount);
        expected_user_balance = expected_user_balance.saturating_sub(burn_amount);

        let total_supply = stack[0].as_canonical_u64();
        let user_deposit = stack[1].as_canonical_u64();

        println!(
            "[{}/{}] burn amount={} => total_supply={}, user_deposit={} (expected {} {})",
            i + 1,
            iterations,
            burn_amount,
            total_supply,
            user_deposit,
            expected_total_supply,
            expected_user_balance,
        );

        let tx_request = TransactionRequestBuilder::new()
            .custom_script(script)
            .build()?;
        setup
            .clients
            .client
            .submit_new_transaction(setup.contract.id(), tx_request)
            .await?;
        setup.clients.client.sync_state().await?;

        assert_eq!(
            total_supply,
            expected_total_supply,
            "total_supply mismatch at iteration {}",
            i + 1
        );
        assert_eq!(
            user_deposit,
            expected_user_balance,
            "user_deposit mismatch at iteration {}",
            i + 1
        );
    }

    println!("All lp_burn fuzz iterations passed.");
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}
