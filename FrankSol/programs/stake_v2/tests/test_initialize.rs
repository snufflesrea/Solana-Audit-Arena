use anchor_lang_v2::{AnchorAccount, solana_program};
use base64::decode;
use pinocchio::entrypoint::deserialize;
use spl_associated_token_account::instruction::create_associated_token_account;
use base64::{engine::general_purpose, Engine};
use stake_v2::StakeEvent;
use stake_v2::utils::{franksol_to_sol, sol_to_franksol};
use {
    anchor_lang_v2::{
        bytemuck,
        prelude::{Address, Discriminator},
        programs::{System, Token},
        Id, InstructionData, ToAccountMetas,
    },
    litesvm::LiteSVM,
    solana_instruction::Instruction,
    solana_keypair::Keypair,
    solana_message::{Message, VersionedMessage},
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_transaction::versioned::VersionedTransaction,
    stake_v2::{accounts, instruction, state::Pool},
    std::{fs, mem, path::PathBuf},
};

const ONE_SOL: u64 = 1_000_000_000;

fn try_load_program_binary() -> Option<Vec<u8>> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let mut candidate_paths = vec![
        PathBuf::from(manifest_dir).join("../../target/deploy/stake_v2.so"),
        PathBuf::from(manifest_dir).join("../../target/sbf-solana-solana/release/stake_v2.so"),
    ];

    if let Ok(target_dir) = std::env::var("CARGO_TARGET_DIR") {
        candidate_paths.push(PathBuf::from(&target_dir).join("deploy/stake_v2.so"));
        candidate_paths.push(PathBuf::from(target_dir).join("sbf-solana-solana/release/stake_v2.so"));
    }

    for path in candidate_paths {
        if let Ok(bytes) = fs::read(path) {
            return Some(bytes);
        }
    }

    None
}

fn addr(pubkey: &Pubkey) -> Address {
    Address::from(pubkey.to_bytes())
}

fn send_ix(
    svm: &mut LiteSVM,
    payer: &Keypair,
    instruction: Instruction,
) -> litesvm::types::TransactionResult {
    let blockhash = svm.latest_blockhash();
    let msg = Message::new_with_blockhash(&[instruction], Some(&payer.pubkey()), &blockhash);
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[payer])
        .expect("transaction should sign");
    svm.send_transaction(tx)
}

fn read_pool(svm: &LiteSVM, pool: &Pubkey) -> Pool {
    let account = svm.get_account(pool).expect("pool account should exist");
    assert_eq!(&account.data[..8], Pool::DISCRIMINATOR);
    *bytemuck::from_bytes::<Pool>(&account.data[8..8 + mem::size_of::<Pool>()])
}

#[test]
fn stake_unstake_round_trip_math() {
    let total_sol = 1_000_000_000_u64;
    let supply = 1_000_000_000_u64;
    let stake_in = 250_000_000_u64;
    let minted = sol_to_franksol(stake_in, total_sol, supply).unwrap();
    assert_eq!(minted, stake_in);

    let new_total = total_sol.checked_add(stake_in).unwrap();
    let new_supply = supply.checked_add(minted).unwrap();
    let unstake_out = franksol_to_sol(minted, new_total, new_supply).unwrap();
    assert_eq!(unstake_out, stake_in);
}

#[test]
fn share_price_appreciation_math_with_yield() {
    let total_sol = 1_000_000_000_u64;
    let supply = 1_000_000_000_u64;
    let yield_added = 100_000_000_u64;
    let appreciated_total = total_sol.checked_add(yield_added).unwrap();

    let burn_amount = 100_000_000_u64;
    let sol_out = franksol_to_sol(burn_amount, appreciated_total, supply).unwrap();
    assert_eq!(sol_out, 110_000_000_u64);
}

#[test]
fn bootstrap_is_one_to_one() {
    let stake_in = 500_000_000_u64;
    let minted = sol_to_franksol(stake_in, 0, 0).unwrap();
    assert_eq!(minted, stake_in);
}

#[test]
fn test_event_stake() {
    let program_id = stake_v2::id();
    let user = Keypair::new();
    let admin = Keypair::new();
    let fund_manager = Keypair::new();
    let treasury = Keypair::new();
    let mut svm = LiteSVM::new();

    let Some(program_bytes) = try_load_program_binary() else {
        eprintln!("Skipping LiteSVM execution: no stake_v2 .so found. Build with `anchor build`.");
        return;
    };
    svm.add_program(program_id, &program_bytes)
        .expect("program must load");
    svm.airdrop(&user.pubkey(), 100 * ONE_SOL).unwrap();
    svm.airdrop(&admin.pubkey(), 100 * ONE_SOL).unwrap();

    let (pool_pda, _) = Pubkey::find_program_address(&[b"pool"], &program_id);
    let (vault_pda, _) = Pubkey::find_program_address(&[b"vault"], &program_id);
    let (mint_authority_pda, _) = Pubkey::find_program_address(&[b"mint_auth"], &program_id);
    let (franksol_mint_pda, _) = Pubkey::find_program_address(&[b"franksol_mint"], &program_id);

    let admin_init = Instruction::new_with_bytes(
        program_id,
        &instruction::Initialize {}.data(),
        accounts::Initialize {
            admin: admin.pubkey(),
            fund_manager: fund_manager.pubkey(),
            treasury: treasury.pubkey(),
            pool: pool_pda,
            mint_authority: mint_authority_pda,
            franksol_mint: franksol_mint_pda,
            vault: vault_pda,
            token_program: Token::id(),
            system_program: System::id(),
        }
        .to_account_metas(None),
    );
    send_ix(&mut svm, &admin, admin_init).expect("admin initializes pool");

    let pool = read_pool(&svm, &pool_pda);
    assert_eq!(pool.total_sol, 0);
    assert_eq!(pool.franksol_supply, 0);

    // Create the user's frankSOL ATA first
    let create_ata_ix = create_associated_token_account(
    &user.pubkey(),      // funding payer
    &user.pubkey(),      // ATA owner
    &franksol_mint_pda,  // mint
    &Token::id(),        // token program
    );

    send_ix(&mut svm, &user, create_ata_ix)
    .expect("create user frankSOL ATA should succeed");

    let (user_position, _) = Pubkey::find_program_address(&[b"user_position", user.pubkey().as_ref()], &program_id);
    const STAKE_AMOUNT: u64 = 100_000_000; // 0.1 SOL
    let user_stake = Instruction::new_with_bytes(
        program_id,
        &instruction::Stake { amount_sol: STAKE_AMOUNT, min_franksol_out: STAKE_AMOUNT }.data(),
        accounts::Stake {
            user: user.pubkey(),
            pool: pool_pda,
            user_position: user_position,
            vault: vault_pda,
            franksol_mint: franksol_mint_pda,
            user_franksol_ata: spl_associated_token_account::get_associated_token_address(&user.pubkey(), &franksol_mint_pda),
            mint_authority: mint_authority_pda,
            token_program: Token::id(),
            system_program: System::id(),
        }
        .to_account_metas(None),
    );

    let meta =send_ix(&mut svm, &user, user_stake).expect("user stakes");
    
    // Pool account has the correct post-deposit values
    let pool_after = read_pool(&svm, &pool_pda);
    assert_eq!(pool_after.total_sol.get(), STAKE_AMOUNT,
        "pool account correctly reflects deposit");
    assert_eq!(pool_after.franksol_supply.get(), STAKE_AMOUNT,
        "pool account correctly reflects minted frankSOL");
        

    let event_data_log = meta.logs.iter()
        .find(|log| log.starts_with("Program data:"))
            .expect("StakeEvent log should exist");

    let b64 = event_data_log.strip_prefix("Program data: ").unwrap();
    let bytes = general_purpose::STANDARD.decode(b64).unwrap();
    let event = unsafe { &*(bytes[8..].as_ptr() as *const StakeEvent) };
    
    println!("event.pool_total_sol:  {}", event.pool_total_sol);
    println!("event.franksol_supply: {}", event.franksol_supply);

    // // This confirms the bug — event emits pre-update zeros
     assert_eq!(event.pool_total_sol, 0,
         "BUG: pool_total_sol should be {} but event emitted 0", STAKE_AMOUNT);
     assert_eq!(event.franksol_supply, 0,
         "BUG: franksol_supply should be {} but event emitted 0", STAKE_AMOUNT);

}

#[test]
fn test_stake() {
    let program_id = stake_v2::id();
    let user = Keypair::new();
    let admin = Keypair::new();
    let fund_manager = Keypair::new();
    let treasury = Keypair::new();
    let mut svm = LiteSVM::new();

    let Some(program_bytes) = try_load_program_binary() else {
        eprintln!("Skipping LiteSVM execution: no stake_v2 .so found. Build with `anchor build`.");
        return;
    };
    svm.add_program(program_id, &program_bytes)
        .expect("program must load");
    svm.airdrop(&user.pubkey(), 100 * ONE_SOL).unwrap();
    svm.airdrop(&admin.pubkey(), 100 * ONE_SOL).unwrap();

    let (pool_pda, _) = Pubkey::find_program_address(&[b"pool"], &program_id);
    let (vault_pda, _) = Pubkey::find_program_address(&[b"vault"], &program_id);
    let (mint_authority_pda, _) = Pubkey::find_program_address(&[b"mint_auth"], &program_id);
    let (franksol_mint_pda, _) = Pubkey::find_program_address(&[b"franksol_mint"], &program_id);

    let admin_init = Instruction::new_with_bytes(
        program_id,
        &instruction::Initialize {}.data(),
        accounts::Initialize {
            admin: admin.pubkey(),
            fund_manager: fund_manager.pubkey(),
            treasury: treasury.pubkey(),
            pool: pool_pda,
            mint_authority: mint_authority_pda,
            franksol_mint: franksol_mint_pda,
            vault: vault_pda,
            token_program: Token::id(),
            system_program: System::id(),
        }
        .to_account_metas(None),
    );
    send_ix(&mut svm, &admin, admin_init).expect("admin initializes pool");

    // Confirm ATA does NOT exist — fresh user, never created it
    let user_ata = spl_associated_token_account::get_associated_token_address(
        &user.pubkey(),
        &franksol_mint_pda,
    );
    assert!(
        svm.get_account(&user_ata).is_none(),
        "ATA must not exist to reproduce the DoS"
    );

    let (user_position_pda, _) = Pubkey::find_program_address(
        &[b"user_position", user.pubkey().as_ref()],
        &program_id,
    );

    const STAKE_AMOUNT: u64 = 100_000_000; // 0.1 SOL

    // Attempt to stake WITHOUT creating the ATA first — this is the normal user flow
    let user_stake = Instruction::new_with_bytes(
        program_id,
        &instruction::Stake {
            amount_sol: STAKE_AMOUNT,
            min_franksol_out: STAKE_AMOUNT,
        }
        .data(),
        accounts::Stake {
            user: user.pubkey(),
            pool: pool_pda,
            user_position: user_position_pda,
            vault: vault_pda,
            franksol_mint: franksol_mint_pda,
            user_franksol_ata: user_ata, // ← does not exist
            mint_authority: mint_authority_pda,
            token_program: Token::id(),
            system_program: System::id(),
        }
        .to_account_metas(None),
    );

    let result = send_ix(&mut svm, &user, user_stake);

    // BUG: stake fails for first-time user because ATA was never created
    assert!(
        result.is_err(),
        "expected stake to fail when ATA does not exist"
    );
    let err = result.unwrap_err();
    let logs = &err.meta.logs;
    assert!(
        logs.iter().any(|l| l.contains("account data too small")),
        "expected AccountDataTooSmall error, got logs: {:#?}", logs
    );
    println!("DoS confirmed — stake fails for first-time user with: {:?}", err.err);
}

#[test]
fn test_sol_to_franksol_precision_loss() {
    let program_id = stake_v2::id();
    let user_a = Keypair::new(); // first depositor — fills pool to 10 SOL
    let user_b = Keypair::new(); // second depositor — 1.5 SOL stake, should succeed
    let admin = Keypair::new();
    let fund_manager = Keypair::new();
    let treasury = Keypair::new();
    let mut svm = LiteSVM::new();

    let Some(program_bytes) = try_load_program_binary() else {
        eprintln!("Skipping: no stake_v2 .so found.");
        return;
    };
    svm.add_program(program_id, &program_bytes)
        .expect("program must load");
    svm.airdrop(&user_a.pubkey(), 100 * ONE_SOL).unwrap();
    svm.airdrop(&user_b.pubkey(), 100 * ONE_SOL).unwrap();
    svm.airdrop(&admin.pubkey(), 100 * ONE_SOL).unwrap();

    let (pool_pda, _) = Pubkey::find_program_address(&[b"pool"], &program_id);
    let (vault_pda, _) = Pubkey::find_program_address(&[b"vault"], &program_id);
    let (mint_authority_pda, _) = Pubkey::find_program_address(&[b"mint_auth"], &program_id);
    let (franksol_mint_pda, _) = Pubkey::find_program_address(&[b"franksol_mint"], &program_id);

    // Initialize pool
    let admin_init = Instruction::new_with_bytes(
        program_id,
        &instruction::Initialize {}.data(),
        accounts::Initialize {
            admin: admin.pubkey(),
            fund_manager: fund_manager.pubkey(),
            treasury: treasury.pubkey(),
            pool: pool_pda,
            mint_authority: mint_authority_pda,
            franksol_mint: franksol_mint_pda,
            vault: vault_pda,
            token_program: Token::id(),
            system_program: System::id(),
        }
        .to_account_metas(None),
    );
    send_ix(&mut svm, &admin, admin_init).expect("admin initializes pool");

    // user_a stakes 10 SOL — bootstraps the pool (first depositor, 1:1 path)
    const FIRST_STAKE: u64 = 10 * ONE_SOL;
    let user_a_ata = spl_associated_token_account::get_associated_token_address(
        &user_a.pubkey(), &franksol_mint_pda,
    );
    let (user_a_position, _) = Pubkey::find_program_address(
        &[b"user_position", user_a.pubkey().as_ref()], &program_id,
    );
    send_ix(&mut svm, &user_a,
        create_associated_token_account(&user_a.pubkey(), &user_a.pubkey(), &franksol_mint_pda, &Token::id()),
    ).expect("create user_a ATA");
    send_ix(&mut svm, &user_a, Instruction::new_with_bytes(
        program_id,
        &instruction::Stake { amount_sol: FIRST_STAKE, min_franksol_out: FIRST_STAKE }.data(),
        accounts::Stake {
            user: user_a.pubkey(), pool: pool_pda, user_position: user_a_position,
            vault: vault_pda, franksol_mint: franksol_mint_pda,
            user_franksol_ata: user_a_ata, mint_authority: mint_authority_pda,
            token_program: Token::id(), system_program: System::id(),
        }.to_account_metas(None),
    )).expect("user_a stakes 10 SOL");

    // Pool now has total_sol=10 SOL, supply=10 frankSOL
    let pool = read_pool(&svm, &pool_pda);
    assert_eq!(pool.total_sol.get(), FIRST_STAKE);
    assert_eq!(pool.franksol_supply.get(), FIRST_STAKE);

    // user_b attempts to stake 1.5 SOL — economically valid, should receive ~1.5 frankSOL
    // BUG: sol_in (1.5e9) / total_sol (10e9) = 0 in integer math → InvalidAmount
    const SECOND_STAKE: u64 = 1_500_000_000; // 1.5 SOL < total_sol (10 SOL)
    let user_b_ata = spl_associated_token_account::get_associated_token_address(
        &user_b.pubkey(), &franksol_mint_pda,
    );
    let (user_b_position, _) = Pubkey::find_program_address(
        &[b"user_position", user_b.pubkey().as_ref()], &program_id,
    );
    send_ix(&mut svm, &user_b,
        create_associated_token_account(&user_b.pubkey(), &user_b.pubkey(), &franksol_mint_pda, &Token::id()),
    ).expect("create user_b ATA");

    let result = send_ix(&mut svm, &user_b, Instruction::new_with_bytes(
        program_id,
        &instruction::Stake { amount_sol: SECOND_STAKE, min_franksol_out: 1 }.data(),
        accounts::Stake {
            user: user_b.pubkey(), pool: pool_pda, user_position: user_b_position,
            vault: vault_pda, franksol_mint: franksol_mint_pda,
            user_franksol_ata: user_b_ata, mint_authority: mint_authority_pda,
            token_program: Token::id(), system_program: System::id(),
        }.to_account_metas(None),
    ));

    // BUG: valid 1.5 SOL stake rejected because division truncates to 0
    assert!(
        result.is_err(),
        "expected stake to fail due to sol_to_franksol precision loss"
    );
    println!(
        "Precision loss DoS confirmed — 1.5 SOL stake into 10 SOL pool rejected: {:?}",
        result.unwrap_err().err
    );

    // Verify the correct off-chain calculation would have succeeded
    let expected_franksol = (SECOND_STAKE as u128)
        .checked_mul(FIRST_STAKE as u128).unwrap()
        .checked_div(FIRST_STAKE as u128).unwrap() as u64;
    assert_eq!(expected_franksol, SECOND_STAKE,
        "correct formula yields {} frankSOL — program should have accepted this", expected_franksol);
}
