use std::{collections::BTreeMap, time::Duration};

use anyhow::Result;
use miden_client::transaction::{AdviceInputs, TransactionRequestBuilder};
use xyk_pool::{pool_ops::compile_storage_fuzz_tx_script, test_utils::*};

#[tokio::test]
async fn add_to_storage_item_fuzz_test() -> Result<()> {
    use rand::Rng;

    let iterations: usize = 5;
    let min_inc: u64 = 1;
    let max_inc: u64 = 1_000_000_000;

    let initial_value = 1;
    let initial_map_value = 1;
    let mut setup = setup_storage_fuzz_environment(initial_value, initial_map_value).await?;
    let mut rng = rand::rng();

    let edge_cases: Vec<u64> = vec![0, 1];
    let mut accumulated: u64 = initial_value;

    for (i, inc) in edge_cases
        .into_iter()
        .chain((0..iterations).map(|_| rng.random_range(min_inc..=max_inc)))
        .enumerate()
    {
        let source = format!(
            "use zoro::storage_fuzz_dummy\n\
             #use zoro::storage_utils\n\
             use miden::core::sys\n
             const VALUE_SLOT = word(\"zoro::storage_fuzz_dummy::value_slot\")\n
             const MAP_SLOT = word(\"zoro::storage_fuzz_dummy::map_slot\")\n
             begin\n\
                 push.{inc}\n\
                 push.VALUE_SLOT[0..2]\n\
                 call.storage_fuzz_dummy::add_to_storage_item\n 
                 call.storage_fuzz_dummy::get_value\n
                 exec.sys::truncate_stack\n\
             end"
        );
        let script = compile_storage_fuzz_tx_script(&source)?;

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
        accumulated = accumulated.saturating_add(inc);
        let expected = accumulated;

        println!(
            "[{}] inc={} => got={}, expected={}",
            i + 1,
            inc,
            got,
            expected,
        );

        let tx_request = TransactionRequestBuilder::new()
            .custom_script(script.clone())
            .build()?;

        setup
            .clients
            .client
            .submit_new_transaction(setup.contract.id(), tx_request)
            .await?;

        setup.clients.client.sync_state().await?;

        assert_eq!(
            got,
            expected,
            "Mismatch at iteration {}: inc={}, got={}, expected={}",
            i + 1,
            inc,
            got,
            expected,
        );
    }

    println!("All add_to_storage_item fuzz iterations passed.");
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}

#[tokio::test]
async fn add_sub_storage_item_fuzz_test() -> Result<()> {
    use rand::Rng;

    let iterations: usize = 10;
    let min_amount: u64 = 1;
    let max_add: u64 = 1_000_000;
    let max_sub: u64 = 1_000_000;

    let initial_value = 1_000_000_000;
    let mut setup = setup_storage_fuzz_environment(initial_value, 10).await?;
    let mut rng = rand::rng();
    let mut accumulated: u64 = initial_value;

    for i in 0..iterations {
        let op_add = accumulated == 0 || rng.random_range(0..2) == 0;

        let (source, expected, amount, op_name) = if op_add {
            let amount = rng.random_range(min_amount..=max_add);
            let new_acc = accumulated.saturating_add(amount);
            (
                format!(
                    "use zoro::storage_fuzz_dummy\n\
                     use miden::core::sys\n\
                     const VALUE_SLOT = word(\"zoro::storage_fuzz_dummy::value_slot\")\n\
                     const MAP_SLOT = word(\"zoro::storage_fuzz_dummy::map_slot\")\n\
                     begin\n\
                         push.{amount}\n\
                         push.VALUE_SLOT[0..2]\n\
                         call.storage_fuzz_dummy::add_to_storage_item\n\
                         exec.sys::truncate_stack\n\
                     end"
                ),
                new_acc,
                amount,
                "add",
            )
        } else {
            let amount = rng.random_range(min_amount..=accumulated.min(max_sub));
            let new_acc = accumulated - amount;
            (
                format!(
                    "use zoro::storage_fuzz_dummy\n\
                     use miden::core::sys\n\
                     const VALUE_SLOT = word(\"zoro::storage_fuzz_dummy::value_slot\")\n\
                     const MAP_SLOT = word(\"zoro::storage_fuzz_dummy::map_slot\")\n\
                     begin\n\
                         push.{amount}\n\
                         push.VALUE_SLOT[0..2]\n\
                         call.storage_fuzz_dummy::sub_from_storage_item\n\
                         exec.sys::truncate_stack\n\
                     end"
                ),
                new_acc,
                amount,
                "sub",
            )
        };

        accumulated = expected;

        let script = compile_storage_fuzz_tx_script(&source)?;

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

        println!(
            "[{}/{}] {} {} => got={}, expected={}",
            i + 1,
            iterations,
            op_name,
            amount,
            got,
            expected,
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
            got,
            expected,
            "Mismatch at iteration {}: got={}, expected={}",
            i + 1,
            got,
            expected,
        );
    }

    println!(
        "All add_sub_storage_item fuzz iterations ({} add+sub) passed.",
        iterations
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}

#[tokio::test]
async fn add_to_map_item_fuzz_test() -> Result<()> {
    use rand::Rng;

    let iterations: usize = 5;
    let min_inc: u64 = 1;
    let max_inc: u64 = 1_000_000_000;

    let mut setup = setup_storage_fuzz_environment(1, 1).await?;
    let mut rng = rand::rng();

    let mut accumulated: u64 = 0;

    let key_0 = 0;
    let key_1 = 0;
    let key_2 = rng.random_range(0..=u32::MAX as u64);
    let key_3 = rng.random_range(0..=u32::MAX as u64);

    for i in 0..iterations {
        let increment_by = rng.random_range(min_inc..=max_inc);

        let add_source = format!(
            "use zoro::storage_fuzz_dummy\n\
             use miden::core::sys\n
             const VALUE_SLOT = word(\"zoro::storage_fuzz_dummy::value_slot\")\n
             const MAP_SLOT = word(\"zoro::storage_fuzz_dummy::map_slot\")\n
             begin\n\
                 push.{increment_by}\n\
                 push.{key_0}.{key_1}.{key_2}.{key_3}\n\
                 push.MAP_SLOT[0..2]\n\
                 call.storage_fuzz_dummy::add_to_map_item\n\
                 exec.sys::truncate_stack\n\
             end"
        );

        let add_script = compile_storage_fuzz_tx_script(&add_source)?;

        let stack = setup
            .clients
            .client
            .execute_program(
                setup.contract.id(),
                add_script.clone(),
                AdviceInputs::default(),
                BTreeMap::new(),
            )
            .await?;

        let got = stack[0].as_canonical_u64();
        accumulated = accumulated.saturating_add(increment_by);
        let expected = accumulated;

        println!(
            "[{}] key=({}, {}, {}, {}), inc={} => got={}, expected={}",
            i + 1,
            key_0,
            key_1,
            key_2,
            key_3,
            increment_by,
            got,
            expected,
        );

        let tx_request = TransactionRequestBuilder::new()
            .custom_script(add_script.clone())
            .build()?;

        setup
            .clients
            .client
            .submit_new_transaction(setup.contract.id(), tx_request)
            .await?;

        setup.clients.client.sync_state().await?;

        assert_eq!(
            got,
            expected,
            "Mismatch at iteration {}: inc={}, got={}, expected={}",
            i + 1,
            increment_by,
            got,
            expected,
        );
    }

    println!("All add_to_map_item fuzz iterations passed.");
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}

#[tokio::test]
async fn sub_from_map_item_fuzz_test() -> Result<()> {
    use rand::Rng;

    let iterations: usize = 5;
    let min_inc: u64 = 1;
    let max_inc: u64 = 1_000_000;

    let initial_map_value = 1_000_000_000;
    let mut setup = setup_storage_fuzz_environment(1, initial_map_value).await?;
    let mut rng = rand::rng();

    let mut accumulated: u64 = initial_map_value;

    let key_0 = 0;
    let key_1 = 0;
    let key_2 = 0;
    let key_3 = 0;

    for i in 0..iterations {
        let sub_by = rng.random_range(min_inc..=max_inc);

        let sub_source = format!(
            "use zoro::storage_fuzz_dummy\n\
             use miden::core::sys\n
             const VALUE_SLOT = word(\"zoro::storage_fuzz_dummy::value_slot\")\n
             const MAP_SLOT = word(\"zoro::storage_fuzz_dummy::map_slot\")\n
             begin\n\
                 push.{sub_by}\n\
                 push.{key_0}.{key_1}.{key_2}.{key_3}\n\
                 push.MAP_SLOT[0..2]\n\
                 call.storage_fuzz_dummy::sub_from_map_item\n\
                 exec.sys::truncate_stack\n\
             end"
        );

        let sub_script = compile_storage_fuzz_tx_script(&sub_source)?;

        let stack = setup
            .clients
            .client
            .execute_program(
                setup.contract.id(),
                sub_script.clone(),
                AdviceInputs::default(),
                BTreeMap::new(),
            )
            .await?;

        let got = stack[0].as_canonical_u64();
        accumulated = accumulated.saturating_sub(sub_by);
        let expected = accumulated;

        println!(
            "[{}] key=({}, {}, {}, {}), sub_by={} => got={}, expected={}",
            i + 1,
            key_0,
            key_1,
            key_2,
            key_3,
            sub_by,
            got,
            expected,
        );

        let tx_request = TransactionRequestBuilder::new()
            .custom_script(sub_script.clone())
            .build()?;

        setup
            .clients
            .client
            .submit_new_transaction(setup.contract.id(), tx_request)
            .await?;

        setup.clients.client.sync_state().await?;

        assert_eq!(
            got,
            expected,
            "Mismatch at iteration {}: sub_by={}, got={}, expected={}",
            i + 1,
            sub_by,
            got,
            expected,
        );
    }

    println!("All add_to_map_item fuzz iterations passed.");
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}

#[tokio::test]
async fn sub_from_storage_item_test() -> Result<()> {
    let initial_value = 100;
    let mut setup = setup_storage_fuzz_environment(initial_value, 10).await?;

    let sub_by = 30;
    let source = format!(
        "use zoro::storage_fuzz_dummy\n
         use miden::core::sys\n
         const VALUE_SLOT = word(\"zoro::storage_fuzz_dummy::value_slot\")\n
         const MAP_SLOT = word(\"zoro::storage_fuzz_dummy::map_slot\")\n
         begin\n\
             push.{sub_by}\n\
             push.VALUE_SLOT[0..2]\n\
             call.storage_fuzz_dummy::sub_from_storage_item\n\
             call.storage_fuzz_dummy::get_value\n\
             exec.sys::truncate_stack\n\
         end"
    );

    let sub_script = compile_storage_fuzz_tx_script(&source)?;

    let stack = setup
        .clients
        .client
        .execute_program(
            setup.contract.id(),
            sub_script.clone(),
            AdviceInputs::default(),
            BTreeMap::new(),
        )
        .await?;

    let got = stack[0].as_canonical_u64();

    let expected = initial_value - sub_by; // 100 - 30

    println!(
        "sub_from_storage_item: add 100, sub 30 => got={}, expected={}",
        got, expected
    );

    assert_eq!(
        got, expected,
        "sub_from_storage_item mismatch: got={}, expected={}",
        got, expected,
    );

    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}

#[tokio::test]
async fn sub_from_storage_item_underflow_test() -> Result<()> {
    let initial_value = 100;
    let mut setup = setup_storage_fuzz_environment(initial_value, 10).await?;

    let sub_by = 3000;
    let source = format!(
        "use zoro::storage_fuzz_dummy\n
         use miden::core::sys\n
         const VALUE_SLOT = word(\"zoro::storage_fuzz_dummy::value_slot\")\n
         const MAP_SLOT = word(\"zoro::storage_fuzz_dummy::map_slot\")\n
         begin\n\
             push.{sub_by}\n\
             push.VALUE_SLOT[0..2]\n\
             call.storage_fuzz_dummy::sub_from_storage_item\n\
             exec.sys::truncate_stack\n\
         end"
    );

    let sub_fail_script = compile_storage_fuzz_tx_script(&source)?;

    let result = setup
        .clients
        .client
        .execute_program(
            setup.contract.id(),
            sub_fail_script,
            AdviceInputs::default(),
            BTreeMap::new(),
        )
        .await;

    assert!(
        result.is_err(),
        "sub_from_storage_item should fail with underflow when subtracting 3000 from 100, got Ok"
    );

    println!(
        "sub_from_storage_item underflow: 50 - 100 correctly failed with: {:?}",
        result.unwrap_err()
    );

    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}
