use anyhow::Result;
use miden_client::{
    Felt, Word,
    asset::FungibleAsset,
    crypto::FeltRng,
    note::{Note, NoteAssets, NoteTag, NoteType, PartialNoteMetadata},
    rpc::domain::account::AccountStorageRequirements,
    transaction::{ForeignAccount, TransactionRequestBuilder},
};
use miden_standards::note::P2idNoteStorage;
use xyk_pool::{
    pool_ops::{
        build_lp_local_deposit_note, build_xyk_register_note,
        build_xyk_swap_exact_tokens_for_tokens_note, build_xyk_swap_tokens_for_exact_tokens_note,
        get_combined_pool_library, get_lp_local_library,
    },
    test_utils::*,
    utils::{
        fetch_vault_for_account_from_chain, pool_id_registry_key, slot_name,
        vault_fungible_balance,
    },
};

use std::time::Duration;

#[tokio::test]
async fn swap_tokens_for_exact_tokens_happy_path_test() -> Result<()> {
    use xyk_pool::pool_ops::get_amount_in;
    use xyk_pool::utils::fetch_vault_for_account_from_chain;

    let deposit_amount: u64 = 10_000_000;
    let swap_amount_out: u64 = 100_000;

    let mut setup = setup_combined_pool_test_environment().await?;
    setup
        .maybe_fund_user_wallet(1_000_000_000, 1_000_000)
        .await?;

    let xyk_pool_lib = get_combined_pool_library()?;
    let token0_id = setup.faucets[0].faucet.id();
    let token1_id = setup.faucets[1].faucet.id();

    // ── Step 1: Deposit to seed the pool ──
    println!("\n=== DEPOSIT PHASE ===");
    let depositor = setup.user.id();
    let pool_state =
        lp_local_deposit(&mut setup, deposit_amount, deposit_amount, depositor).await?;
    let (ts_d, r0_d, r1_d, pool_balance0_d, pool_balance1_d) = (
        pool_state.total_supply,
        pool_state.reserve0,
        pool_state.reserve1,
        pool_state.pool_balance0,
        pool_state.pool_balance1,
    );
    println!("After deposit:");
    println!("  total_supply  = {}", ts_d);
    println!("  reserve0      = {}", r0_d);
    println!("  reserve1      = {}", r1_d);
    println!("  pool_balance0 = {}", pool_balance0_d);
    println!("  pool_balance1 = {}", pool_balance1_d);

    // ── Read user balances before swap ──
    let user_vault_before =
        fetch_vault_for_account_from_chain(&setup.clients.rpc_api, &setup.user.id()).await?;
    let user_balance0_before = vault_fungible_balance(&user_vault_before, token0_id).unwrap_or(0);
    let user_balance1_before = vault_fungible_balance(&user_vault_before, token1_id).unwrap_or(0);
    println!("\nUser balances BEFORE swap:");
    println!("  token0 = {}", user_balance0_before);
    println!("  token1 = {}", user_balance1_before);

    // ── Step 2: Swap token0 → token1 ──
    println!("\n=== SWAP PHASE ===");
    println!("Swapping token0 for {} token1", swap_amount_out);

    let expected_in = get_amount_in(swap_amount_out, r0_d, r1_d);
    println!("Expected amount_in (Rust): {}", expected_in);

    // let return_note_tag = NoteTag::with_account_target(setup.user.id());
    let return_note_type = NoteType::Public;
    let swap_max_input_asset = FungibleAsset::new(token0_id, expected_in + 10)?;
    let swap_output_asset = FungibleAsset::new(token1_id, swap_amount_out)?;
    let note_serial_num = setup.clients.client.rng().draw_word();

    let swap_note = build_xyk_swap_tokens_for_exact_tokens_note(
        setup.contract.id(),
        &xyk_pool_lib,
        swap_max_input_asset,
        swap_output_asset,
        0,
        setup.user.id(),
        NoteTag::with_account_target(setup.user.id())
            .as_u32()
            .into(),
        return_note_type.into(),
        note_serial_num,
    )?;

    let swap_serial_num = swap_note.serial_num();
    let p2id_serial_num: Word = [
        swap_serial_num[0] + Felt::ONE,
        swap_serial_num[1],
        swap_serial_num[2],
        swap_serial_num[3],
    ]
    .into();

    let recipient = P2idNoteStorage::new(setup.user.id()).into_recipient(p2id_serial_num);
    let tag = NoteTag::with_account_target(setup.user.id());
    let metadata = PartialNoteMetadata::new(setup.contract.id(), return_note_type).with_tag(tag);
    let vault = NoteAssets::new(vec![
        FungibleAsset::new(token1_id, swap_amount_out)?.into(),
        FungibleAsset::new(token0_id, 10)?.into(),
    ])?;
    let return_note = Note::new(vault, metadata, recipient);

    let create_swap_req = TransactionRequestBuilder::new()
        .own_output_notes([swap_note.clone()])
        .build()?;
    let _tx_id = setup
        .clients
        .client
        .submit_new_transaction(setup.user.id(), create_swap_req)
        .await?;
    setup.clients.client.sync_state().await?;

    let consume_swap_req = TransactionRequestBuilder::new()
        .input_notes([(swap_note.clone(), None)])
        .build()?;
    let _consume_id = setup
        .clients
        .client
        .submit_new_transaction(setup.contract.id(), consume_swap_req)
        .await?;
    setup.clients.client.sync_state().await?;
    println!("---------------------------Consuming swap note---------------------------");

    println!("-------------------------------- return ptid note --------------------------------");
    println!("return note digest: {:?}", return_note.recipient().digest());
    println!("return note serial: {:?}", return_note.serial_num());
    println!("return note type: {:?}", return_note.metadata().note_type());
    println!("return note tag: {:?}", return_note.metadata().tag());
    println!("return note assets: {:?}", return_note.assets());
    println!("-------------------------------- return ptid note --------------------------------");

    tokio::time::sleep(Duration::from_secs(1)).await;
    let user_consume_return_note_request =
        TransactionRequestBuilder::new().build_consume_notes(vec![return_note.clone()])?;
    let _user_consume_return_note_id = setup
        .clients
        .client
        .submit_new_transaction(setup.user.id(), user_consume_return_note_request)
        .await?;
    setup.clients.client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(10)).await;

    // ── Read pool state after swap ──
    let acc_after_swap = setup
        .clients
        .client
        .get_account(setup.contract.id())
        .await?
        .unwrap();
    let storage_after_swap = acc_after_swap.storage();
    let total_supply_after_swap =
        storage_after_swap.get_item(&slot_name("zoro::lp_local::total_supply"))?;
    let reserve_after_swap = storage_after_swap.get_item(&slot_name("zoro::lp_local::reserve"))?;
    let vault_after_swap = acc_after_swap.vault();

    let ts_s = total_supply_after_swap[0].as_canonical_u64();
    let r0_s = reserve_after_swap[1].as_canonical_u64();
    let r1_s = reserve_after_swap[0].as_canonical_u64();
    let pool_balance0_s = vault_fungible_balance(&vault_after_swap, token0_id)?;
    let pool_balance1_s = vault_fungible_balance(&vault_after_swap, token1_id)?;

    println!("\nAfter swap:");
    println!("  total_supply  = {} (was {})", ts_s, ts_d);
    println!("  reserve0      = {} (was {})", r0_s, r0_d);
    println!("  reserve1      = {} (was {})", r1_s, r1_d);
    println!(
        "  pool_balance0 = {} (was {})",
        pool_balance0_s, pool_balance0_d
    );
    println!(
        "  pool_balance1 = {} (was {})",
        pool_balance1_s, pool_balance1_d
    );

    // ── Read user balances after swap ──
    let user_vault_after =
        fetch_vault_for_account_from_chain(&setup.clients.rpc_api, &setup.user.id()).await?;
    let user_balance0_after = vault_fungible_balance(&user_vault_after, token0_id).unwrap_or(0);
    let user_balance1_after = vault_fungible_balance(&user_vault_after, token1_id).unwrap_or(0);
    println!("\nUser balances AFTER swap:");
    println!(
        "  token0 = {} (was {})",
        user_balance0_after, user_balance0_before
    );
    println!(
        "  token1 = {} (was {})",
        user_balance1_after, user_balance1_before
    );

    println!("\n=== SUMMARY ===");
    println!("Expected swap input:  {} token0", expected_in);
    println!("Swap amount out: {} token1", swap_amount_out);
    println!(
        "Reserve delta: r0 {} → {}, r1 {} → {}",
        r0_d, r0_s, r1_d, r1_s
    );

    assert_eq!(
        r0_d + expected_in,
        r0_s,
        "reserve0 should increase by expected_in"
    );
    assert_eq!(
        r1_d - swap_amount_out,
        r1_s,
        "reserve1 should decrease by swap_amount_out"
    );

    println!("\nswap_happy_path_test finished");
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}

#[tokio::test]
async fn swap_exact_tokens_for_tokens_happy_path_test() -> Result<()> {
    use xyk_pool::pool_ops::get_amount_out;
    use xyk_pool::utils::fetch_vault_for_account_from_chain;

    let deposit_amount: u64 = 10_000_000;
    let swap_amount_in: u64 = 100_000;

    let mut setup = setup_combined_pool_test_environment().await?;
    setup
        .maybe_fund_user_wallet(1_000_000_000, 1_000_000)
        .await?;

    let xyk_pool_lib = get_combined_pool_library()?;
    let token0_id = setup.faucets[0].faucet.id();
    let token1_id = setup.faucets[1].faucet.id();

    // ── Step 1: Deposit to seed the pool ──
    println!("\n=== DEPOSIT PHASE ===");
    let depositor = setup.user.id();
    let pool_state =
        lp_local_deposit(&mut setup, deposit_amount, deposit_amount, depositor).await?;
    let (ts_d, r0_d, r1_d, pool_balance0_d, pool_balance1_d) = (
        pool_state.total_supply,
        pool_state.reserve0,
        pool_state.reserve1,
        pool_state.pool_balance0,
        pool_state.pool_balance1,
    );
    println!("After deposit:");
    println!("  total_supply  = {}", ts_d);
    println!("  reserve0      = {}", r0_d);
    println!("  reserve1      = {}", r1_d);
    println!("  pool_balance0 = {}", pool_balance0_d);
    println!("  pool_balance1 = {}", pool_balance1_d);

    // ── Read user balances before swap ──
    let user_vault_before =
        fetch_vault_for_account_from_chain(&setup.clients.rpc_api, &setup.user.id()).await?;
    let user_balance0_before = vault_fungible_balance(&user_vault_before, token0_id).unwrap_or(0);
    let user_balance1_before = vault_fungible_balance(&user_vault_before, token1_id).unwrap_or(0);
    println!("\nUser balances BEFORE swap:");
    println!("  token0 = {}", user_balance0_before);
    println!("  token1 = {}", user_balance1_before);

    // ── Step 2: Swap token0 → token1 ──
    println!("\n=== SWAP PHASE ===");
    println!(
        "Swapping {} of token0 for token1 (min_out=0)",
        swap_amount_in
    );
    println!("token0_id: {} {}", token0_id.suffix(), token0_id.prefix());
    println!("token1_id: {} {}", token1_id.suffix(), token1_id.prefix());

    let expected_out = get_amount_out(swap_amount_in, r0_d, r1_d);
    println!("Expected amount_out (Rust): {}", expected_out);

    let return_note_type = NoteType::Public;
    let swap_input_asset = FungibleAsset::new(token0_id, swap_amount_in)?;
    let swap_min_output_asset = FungibleAsset::new(token1_id, expected_out - 5)?;
    let note_serial_num = setup.clients.client.rng().draw_word();

    let swap_note = build_xyk_swap_exact_tokens_for_tokens_note(
        setup.contract.id(),
        &xyk_pool_lib,
        swap_input_asset,
        swap_min_output_asset,
        0,
        setup.user.id(),
        NoteTag::with_account_target(setup.user.id())
            .as_u32()
            .into(),
        return_note_type.into(),
        note_serial_num,
    )?;

    let swap_serial_num = swap_note.serial_num();
    let p2id_serial_num: Word = [
        swap_serial_num[0] + Felt::ONE,
        swap_serial_num[1],
        swap_serial_num[2],
        swap_serial_num[3],
    ]
    .into();

    let recipient = P2idNoteStorage::new(setup.user.id()).into_recipient(p2id_serial_num);
    let tag = NoteTag::with_account_target(setup.user.id());
    let metadata = PartialNoteMetadata::new(setup.contract.id(), return_note_type).with_tag(tag);
    let vault = NoteAssets::new(vec![FungibleAsset::new(token1_id, expected_out)?.into()])?;
    let return_note = Note::new(vault, metadata, recipient);

    setup
        .clients
        .client
        .add_note_tag(NoteTag::with_account_target(setup.contract.id()))
        .await?;
    let create_swap_req = TransactionRequestBuilder::new()
        .own_output_notes([swap_note.clone()])
        .build()?;
    let _tx_id = setup
        .clients
        .client
        .submit_new_transaction(setup.user.id(), create_swap_req)
        .await?;
    setup.clients.client.sync_state().await?;

    println!(
        "\n\n swap return p2id recipient: {:?} \n\n",
        return_note.recipient().digest()
    );
    println!(
        "\n\n swap serial: {:?} \n p2id serial: {:?}\n",
        swap_note.serial_num(),
        return_note.serial_num()
    );

    let consume_swap_req = TransactionRequestBuilder::new()
        .input_notes([(swap_note.clone(), None)])
        .build()?;
    let _consume_id = setup
        .clients
        .client
        .submit_new_transaction(setup.contract.id(), consume_swap_req)
        .await?;
    setup.clients.client.sync_state().await?;
    println!("---------------------------Consuming swap note---------------------------");

    tokio::time::sleep(Duration::from_secs(5)).await;
    let user_consume_return_note_request =
        TransactionRequestBuilder::new().build_consume_notes(vec![return_note.clone()])?;
    let _user_consume_return_note_id = setup
        .clients
        .client
        .submit_new_transaction(setup.user.id(), user_consume_return_note_request)
        .await?;
    setup.clients.client.sync_state().await?;
    tokio::time::sleep(Duration::from_secs(10)).await;

    // ── Read pool state after swap ──
    let acc_after_swap = setup
        .clients
        .client
        .get_account(setup.contract.id())
        .await?
        .unwrap();
    let storage_after_swap = acc_after_swap.storage();
    let total_supply_after_swap =
        storage_after_swap.get_item(&slot_name("zoro::lp_local::total_supply"))?;
    let reserve_after_swap = storage_after_swap.get_item(&slot_name("zoro::lp_local::reserve"))?;
    let vault_after_swap = acc_after_swap.vault();

    let ts_s = total_supply_after_swap[0].as_canonical_u64();
    let r0_s = reserve_after_swap[1].as_canonical_u64();
    let r1_s = reserve_after_swap[0].as_canonical_u64();
    let pool_balance0_s = vault_fungible_balance(&vault_after_swap, token0_id)?;
    let pool_balance1_s = vault_fungible_balance(&vault_after_swap, token1_id)?;

    println!("\nAfter swap:");
    println!("  total_supply  = {} (was {})", ts_s, ts_d);
    println!("  reserve0      = {} (was {})", r0_s, r0_d);
    println!("  reserve1      = {} (was {})", r1_s, r1_d);
    println!(
        "  pool_balance0 = {} (was {})",
        pool_balance0_s, pool_balance0_d
    );
    println!(
        "  pool_balance1 = {} (was {})",
        pool_balance1_s, pool_balance1_d
    );

    // ── Read user balances after swap ──
    let user_vault_after =
        fetch_vault_for_account_from_chain(&setup.clients.rpc_api, &setup.user.id()).await?;
    let user_balance0_after = vault_fungible_balance(&user_vault_after, token0_id).unwrap_or(0);
    let user_balance1_after = vault_fungible_balance(&user_vault_after, token1_id).unwrap_or(0);
    println!("\nUser balances AFTER swap:");
    println!(
        "  token0 = {} (was {})",
        user_balance0_after, user_balance0_before
    );
    println!(
        "  token1 = {} (was {})",
        user_balance1_after, user_balance1_before
    );

    println!("\n=== SUMMARY ===");
    println!("Swap input:  {} token0", swap_amount_in);
    println!("Expected out: {} token1 (Rust calc)", expected_out);
    println!(
        "Reserve delta: r0 {} → {}, r1 {} → {}",
        r0_d, r0_s, r1_d, r1_s
    );

    assert_eq!(
        r0_d + swap_amount_in,
        r0_s,
        "reserve0 should increase by swap_amount_in"
    );
    assert_eq!(
        r1_d - expected_out,
        r1_s,
        "reserve1 should decrease by expected_out"
    );

    println!("\nswap_happy_path_test finished");
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}

#[tokio::test]
async fn register_pool_happy_path_test() -> Result<()> {
    let mut setup = setup_registry_test_environment().await?;

    let pool_id = setup.pool.id();
    let token0_id = setup.faucets[0].faucet.id();
    let token1_id = setup.faucets[1].faucet.id();

    println!(
        "register_pool: pool={}, token0={}, token1={}, registry={}",
        pool_id.to_hex(),
        token0_id.to_hex(),
        token1_id.to_hex(),
        setup.registry.id().to_hex(),
    );

    let register_note = build_xyk_register_note(
        &setup.registry.id(),
        setup.clients.client.rng().draw_word(),
        &token0_id,
        &token1_id,
        &setup.pool.id(),
        &setup.user.id(),
    )?;

    println!("====== SENDING THE REGISTER NOTE");

    let consume_req = TransactionRequestBuilder::new()
        .own_output_notes([register_note.clone()])
        .build()?;

    println!("Built request for sending register note");

    setup
        .clients
        .client
        .submit_new_transaction(setup.user.id(), consume_req)
        .await?;

    setup.clients.client.sync_state().await?;

    println!("====== CONSUMING THE REGISTER NOTE");

    let foreign = ForeignAccount::public(pool_id, AccountStorageRequirements::default())?;
    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(register_note.clone(), None)])
        .foreign_accounts([foreign])
        .build()?;

    println!("Submitting register_pool note against registry...");
    setup
        .clients
        .client
        .submit_new_transaction(setup.registry.id(), consume_req)
        .await?;
    setup.clients.client.sync_state().await?;

    let acc = setup
        .clients
        .client
        .get_account(setup.registry.id())
        .await?
        .unwrap();

    let pool_key = pool_id_registry_key(&pool_id);
    let stored_code_hash = acc
        .storage()
        .get_map_item(&slot_name("zoro::registry::pools_mapping"), pool_key)?;

    let expected = setup.pool.code().commitment();
    assert_eq!(
        stored_code_hash, expected,
        "pools_mapping should map pool_id → pool code commitment"
    );

    let register_note = build_xyk_register_note(
        &setup.registry.id(),
        setup.clients.client.rng().draw_word(),
        &token0_id,
        &token1_id,
        &setup.pool.id(),
        &setup.user.id(),
    )?;

    println!("====== SENDING THE REGISTER NOTE AGAIN (should not succeed)");

    let consume_req = TransactionRequestBuilder::new()
        .own_output_notes([register_note.clone()])
        .build()?;

    println!("Built request for sending register note");

    setup
        .clients
        .client
        .submit_new_transaction(setup.user.id(), consume_req)
        .await?;

    setup.clients.client.sync_state().await?;

    println!("====== CONSUMING THE REGISTER NOTE AGAIN (should fail)");

    let foreign = ForeignAccount::public(pool_id, AccountStorageRequirements::default())?;
    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(register_note.clone(), None)])
        .foreign_accounts([foreign])
        .build()?;

    println!("Submitting register_pool note against registry...");
    setup
        .clients
        .client
        .submit_new_transaction(setup.registry.id(), consume_req)
        .await
        .expect_err("Shouldnt be able to register same pool twice");

    setup.clients.client.sync_state().await?;

    println!("register_pool_happy_path_test passed!");
    Ok(())
}

#[tokio::test]
async fn lp_deposit_withdraw_happy_path_test() -> Result<()> {
    use xyk_pool::common::get_return_note_serial;
    use xyk_pool::pool_ops::{build_lp_local_withdraw_note, compute_expected_withdraw};

    let deposit_amount: u64 = 10_000_000;
    let withdraw_amount: u64 = 1_000_000;

    let mut setup = setup_registry_test_environment().await?;
    setup
        .maybe_fund_user_wallet(1_000_000_000, 1_000_000)
        .await?;

    let lp_lib = get_lp_local_library()?;
    let token0_id = setup.faucets[0].faucet.id();
    let token1_id = setup.faucets[1].faucet.id();
    let token0_asset = FungibleAsset::new(token0_id, deposit_amount)?;
    let token1_asset = FungibleAsset::new(token1_id, deposit_amount)?;

    // ── Step 1: Deposit to seed the pool with reserves and LP supply ──
    let deposit_note = build_lp_local_deposit_note(
        setup.pool.id(),
        &lp_lib,
        token0_asset,
        token1_asset,
        setup.user.id(),
        setup.user.id(),
        setup.clients.client.rng().draw_word(),
    )?;

    let pool_tag = NoteTag::with_account_target(setup.pool.id());
    setup.clients.client.add_note_tag(pool_tag).await?;

    let create_req = TransactionRequestBuilder::new()
        .own_output_notes([deposit_note.clone()])
        .build()?;
    let _tx_id = setup
        .clients
        .client
        .submit_new_transaction(setup.user.id(), create_req)
        .await?;
    setup.clients.client.sync_state().await?;

    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(deposit_note.clone(), None)])
        .build()?;
    let _consume_id = setup
        .clients
        .client
        .submit_new_transaction(setup.pool.id(), consume_req)
        .await?;
    setup.clients.client.sync_state().await?;

    // ── Read storage after deposit ──
    let acc_after_deposit = setup
        .clients
        .client
        .get_account(setup.pool.id())
        .await?
        .unwrap();
    let storage_after_deposit = acc_after_deposit.storage();
    let total_supply_after_deposit =
        storage_after_deposit.get_item(&slot_name("zoro::lp_local::total_supply"))?;
    let reserve_after_deposit =
        storage_after_deposit.get_item(&slot_name("zoro::lp_local::reserve"))?;
    let vault_after_deposit = acc_after_deposit.vault();
    println!(
        "after deposit token0 balance={}, token1 balance={}",
        vault_fungible_balance(&vault_after_deposit, token0_id)?,
        vault_fungible_balance(&vault_after_deposit, token1_id)?
    );

    let ts = total_supply_after_deposit[0].as_canonical_u64();
    let r0 = reserve_after_deposit[0].as_canonical_u64();
    let r1 = reserve_after_deposit[1].as_canonical_u64();
    println!(
        "After deposit: total_supply={}, reserve0={}, reserve1={}",
        ts, r0, r1
    );
    assert!(ts > 0, "total supply should be > 0 after deposit");
    assert!(r0 > 0, "reserve0 should be > 0 after deposit");
    assert!(r1 > 0, "reserve1 should be > 0 after deposit");

    let withdraw_note_serial_num = setup.clients.client.rng().draw_word();
    // ── Step 2: Build and submit the withdraw note ──
    // Return note params are placeholders; withdraw currently doesn't create the output note.
    let return_note_tag = NoteTag::with_account_target(setup.user.id());
    let return_note_type = NoteType::Public;

    let withdraw_note = build_lp_local_withdraw_note(
        setup.pool.id(),
        &lp_lib,
        withdraw_amount,
        setup.user.id(),
        return_note_tag.into(),
        return_note_type.into(),
        withdraw_note_serial_num,
    )?;

    let create_req = TransactionRequestBuilder::new()
        .own_output_notes([withdraw_note.clone()])
        .build()?;
    let _tx_id = setup
        .clients
        .client
        .submit_new_transaction(setup.user.id(), create_req)
        .await?;
    setup.clients.client.sync_state().await?;

    let return_note_serial_num = get_return_note_serial(withdraw_note_serial_num, setup.user.id());
    let return_note_recipient =
        P2idNoteStorage::new(setup.user.id()).into_recipient(return_note_serial_num);
    println!("-=-=-=-=-=-=-=-=-=-=-=user_id={:?}", setup.user.id());
    println!(
        "-=-=-=-=-=-=-=-=-=-=-=return_note_recipient={:?}",
        return_note_recipient.digest()
    );
    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(withdraw_note.clone(), None)])
        // .expected_output_recipients(vec![return_note_recipient])
        .build()?;
    let _consume_id = setup
        .clients
        .client
        .submit_new_transaction(setup.pool.id(), consume_req)
        .await?;
    setup.clients.client.sync_state().await?;

    // ── Read storage after withdraw ──
    let acc_after_withdraw = setup
        .clients
        .client
        .get_account(setup.pool.id())
        .await?
        .unwrap();
    let storage_after_withdraw = acc_after_withdraw.storage();
    let total_supply_after_withdraw =
        storage_after_withdraw.get_item(&slot_name("zoro::lp_local::total_supply"))?;
    let reserve_after_withdraw =
        storage_after_withdraw.get_item(&slot_name("zoro::lp_local::reserve"))?;

    let ts_after = total_supply_after_withdraw[0].as_canonical_u64();
    let r0_after = reserve_after_withdraw[0].as_canonical_u64();
    let r1_after = reserve_after_withdraw[1].as_canonical_u64();

    let (expected_amount0_out, expected_amount1_out) =
        compute_expected_withdraw(ts, withdraw_amount, r0, r1);

    println!(
        "After withdraw: total_supply={}, reserve0={}, reserve1={}",
        ts_after, r0_after, r1_after
    );
    println!(
        "Expected withdraw outputs: amount0={}, amount1={}",
        expected_amount0_out, expected_amount1_out
    );

    // withdraw curuser_keyrently only runs simulate_withdraw (burn/create_note commented out),
    // so state should remain unchanged
    assert_eq!(
        ts_after,
        ts - withdraw_amount,
        "total supply should decrease by withdraw_amount"
    );
    assert_eq!(
        r0_after,
        r0 - expected_amount0_out,
        "reserve0 should decrease by expected_amount0_out"
    );
    assert_eq!(
        r1_after,
        r1 - expected_amount1_out,
        "reserve1 should decrease by expected_amount1_out"
    );

    let user_key = Word::new([
        setup.user.id().suffix(),
        setup.user.id().prefix().into(),
        Felt::ZERO,
        Felt::ZERO,
    ]);
    let user_deposit_after = storage_after_withdraw.get_map_item(
        &slot_name("zoro::lp_local::user_deposits_mapping"),
        user_key,
    )?;
    println!(
        "User deposit after withdraw: {}",
        user_deposit_after[0].as_canonical_u64()
    );

    let user_deposit_before = storage_after_deposit.get_map_item(
        &slot_name("zoro::lp_local::user_deposits_mapping"),
        user_key,
    )?;
    assert_eq!(
        user_deposit_after[0].as_canonical_u64(),
        user_deposit_before[0].as_canonical_u64() - withdraw_amount,
        "user deposit should decrease by withdraw_amount"
    );

    println!("lp_deposit_withdraw_happy_path_test finished successfully");
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}

#[tokio::test]
async fn deposit_happy_path_test() -> Result<()> {
    use miden_client::note::NoteTag;

    print_phase("Setup test env");
    let mut setup = setup_registry_test_environment().await?;

    print_phase("Build deposit note");
    let lp_lib = get_lp_local_library()?;
    let token0_id = setup.faucets[0].faucet.id();
    let token1_id = setup.faucets[1].faucet.id();
    let amount0 = 1_000_000;
    let amount1 = 1_000_000;
    let token0_asset = FungibleAsset::new(token0_id, amount0)?;
    let token1_asset = FungibleAsset::new(token1_id, amount1)?;

    let deposit_note = build_lp_local_deposit_note(
        setup.pool.id(),
        &lp_lib,
        token0_asset,
        token1_asset,
        setup.user.id(),
        setup.user.id(),
        setup.clients.client.rng().draw_word(),
    )?;

    let pool_tag = NoteTag::with_account_target(setup.pool.id());
    setup.clients.client.add_note_tag(pool_tag).await?;
    print_phase("Send deposit note to node");

    let create_req = TransactionRequestBuilder::new()
        .own_output_notes([deposit_note.clone()])
        .build()?;
    let _tx_id = setup
        .clients
        .client
        .submit_new_transaction(setup.user.id(), create_req)
        .await?;
    setup.clients.client.sync_state().await?;

    print_phase("Consume deposit note into pool");
    // wait_for_note(&mut setup.clients.client, &deposit_note).await?;
    let consume_req = TransactionRequestBuilder::new()
        // .input_notes([(deposit_note.clone(), None), (deposit_note_2.clone(), None)])
        .input_notes([(deposit_note.clone(), None)])
        .build()?;

    let _consume_id = setup
        .clients
        .client
        .submit_new_transaction(setup.pool.id(), consume_req)
        .await?;
    setup.clients.client.sync_state().await?;

    print_phase("Build second deposit note");
    let deposit_note_2 = build_lp_local_deposit_note(
        setup.pool.id(),
        &lp_lib,
        token0_asset,
        token1_asset,
        setup.user.id(),
        setup.user.id(),
        setup.clients.client.rng().draw_word(),
    )?;

    let create_req_2 = TransactionRequestBuilder::new()
        .own_output_notes([deposit_note_2.clone()])
        .build()?;
    let _tx_id = setup
        .clients
        .client
        .submit_new_transaction(setup.user.id(), create_req_2)
        .await?;
    setup.clients.client.sync_state().await?;
    print_phase("Consume second deposit note into pool");

    // wait_for_note(&mut setup.clients.client, &deposit_note).await?;
    let consume_req_2 = TransactionRequestBuilder::new()
        // .input_notes([(deposit_note.clone(), None), (deposit_note_2.clone(), None)])
        .input_notes([(deposit_note_2.clone(), None)])
        .build()?;

    let _consume_id_2 = setup
        .clients
        .client
        .submit_new_transaction(setup.pool.id(), consume_req_2)
        .await?;
    setup.clients.client.sync_state().await?;
    print_phase("Check if pool states are correct");

    // read the storage items: total supply, reserve0, reserve1, user_deposits_mapping value for the user
    let acc_after = setup
        .clients
        .client
        .get_account(setup.pool.id())
        .await?
        .unwrap();

    let acc_after_storage = acc_after.storage();
    let total_supply = acc_after_storage.get_item(&slot_name("zoro::lp_local::total_supply"))?;
    let reserve = acc_after_storage.get_item(&slot_name("zoro::lp_local::reserve"))?;
    let user_key = Word::new([
        Felt::ZERO,
        Felt::ZERO,
        setup.user.id().suffix(),
        setup.user.id().prefix().into(),
    ]);
    let user_deposit_balance = acc_after_storage.get_map_item(
        &slot_name("zoro::lp_local::user_deposits_mapping"),
        user_key,
    )?;

    println!(
        "total_supply={}\nreserve0={}\nreserve1={}\nuser_deposit_balance={}\n",
        total_supply[0].as_canonical_u64(),
        reserve[0].as_canonical_u64(),
        reserve[1].as_canonical_u64(),
        user_deposit_balance[0].as_canonical_u64(),
    );

    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}

#[tokio::test]
async fn deposit_initial_underflow_test() -> Result<()> {
    let mut setup = setup_lp_local_test_environment().await?;
    setup
        .maybe_fund_user_wallet(1_000_000_000, 1_000_000)
        .await?;

    let lp_lib = get_lp_local_library()?;
    let token0_id = setup.faucets[0].faucet.id();
    let token1_id = setup.faucets[1].faucet.id();
    let amount0 = 10u64;
    let amount1 = 10u64;
    let token0_asset = FungibleAsset::new(token0_id, amount0)?;
    let token1_asset = FungibleAsset::new(token1_id, amount1)?;

    let deposit_note = build_lp_local_deposit_note(
        setup.contract.id(),
        &lp_lib,
        token0_asset,
        token1_asset,
        setup.user.id(),
        setup.user.id(),
        setup.clients.client.rng().draw_word(),
    )?;

    let pool_tag = NoteTag::with_account_target(setup.contract.id());
    setup.clients.client.add_note_tag(pool_tag).await?;

    let create_req = TransactionRequestBuilder::new()
        .own_output_notes([deposit_note.clone()])
        .build()?;
    let _tx_id = setup
        .clients
        .client
        .submit_new_transaction(setup.user.id(), create_req)
        .await?;
    setup.clients.client.sync_state().await?;

    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(deposit_note, None)])
        .build()?;
    let result = setup
        .clients
        .client
        .submit_new_transaction(setup.contract.id(), consume_req)
        .await;

    assert!(
        result.is_err(),
        "deposit with amount0=10 amount1=10 should fail (sqrt(100)-100 underflows)"
    );
    println!(
        "deposit_initial_underflow_test: correctly failed with {:?}",
        result.unwrap_err()
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}

#[tokio::test]
async fn smoke_test() -> Result<()> {
    let mut setup = setup_test_environment().await?;
    setup
        .maybe_fund_user_wallet(1_000_000_000, 1_000_000)
        .await?;

    println!("Setup: {:?}", setup.user.id());
    Ok(())
}
