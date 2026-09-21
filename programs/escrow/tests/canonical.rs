//! Solana Summer canonical suite — Assignment 02, Escrow.
//!
//! SEALED. This file's blob SHA is pinned by the submission checker; editing
//! it fails your submission before a single assertion runs. Your own tests go
//! in `tests/test_*.rs`, which you are free to change.
//!
//! It is deliberately self-contained — no `mod common`, no shared fixtures —
//! because every helper you can edit is a helper that can be made to lie.
//!
//! What it checks, all of it Checkpoint 6 (the cancel time lock):
//!
//!   * `make` records `created_at` from the on-chain clock.
//!   * A cancel before the delay is refused with a declared program error.
//!   * A refused cancel moves nothing: the vault and the escrow survive intact.
//!   * A cancel at exactly `created_at + CANCEL_DELAY_SECONDS` succeeds. This
//!     is the boundary — `>` instead of `>=` fails here and nowhere else.
//!   * One second earlier is still refused.
//!   * After the delay, the maker gets every token back and both accounts close.

use anchor_lang::{
    prelude::Clock, solana_program::instruction::Instruction, AccountDeserialize, InstructionData,
    ToAccountMetas,
};
use litesvm::LiteSVM;
use solana_account::Account;
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_program_option::COption;
use solana_program_pack::Pack;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use spl_associated_token_account_interface::address::get_associated_token_address;
use spl_token_interface::{
    state::{Account as TokenAccount, AccountState, Mint},
    ID as TOKEN_PROGRAM_ID,
};

const SEED: u16 = 7;
const AMOUNT_A: u64 = 1_000_000;
const AMOUNT_B: u64 = 500_000;

/// The tutorial's delay. Kept as a literal rather than read from the crate so
/// that redefining the constant cannot move the boundary this suite tests.
const DELAY: i64 = 300;

// ---------------------------------------------------------------- fixtures --

fn setup_mint(svm: &mut LiteSVM, mint: &Keypair, authority: &Pubkey, decimals: u8) {
    let state = Mint {
        mint_authority: COption::Some(*authority),
        supply: 0,
        decimals,
        is_initialized: true,
        freeze_authority: COption::None,
    };
    let mut data = [0u8; Mint::LEN];
    Mint::pack(state, &mut data).unwrap();
    svm.set_account(
        mint.pubkey(),
        Account {
            lamports: 1_000_000_000,
            data: data.to_vec(),
            owner: TOKEN_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

fn setup_token_account(
    svm: &mut LiteSVM,
    address: Pubkey,
    mint: Pubkey,
    owner: Pubkey,
    amount: u64,
) {
    let state = TokenAccount {
        mint,
        owner,
        amount,
        delegate: COption::None,
        state: AccountState::Initialized,
        is_native: COption::None,
        delegated_amount: 0,
        close_authority: COption::None,
    };
    let mut data = [0u8; TokenAccount::LEN];
    TokenAccount::pack(state, &mut data).unwrap();
    svm.set_account(
        address,
        Account {
            lamports: 1_000_000_000,
            data: data.to_vec(),
            owner: TOKEN_PROGRAM_ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

fn setup_svm() -> LiteSVM {
    let mut svm = LiteSVM::new();
    let bytes = include_bytes!("../../../target/deploy/escrow.so");
    svm.add_program(escrow::id(), bytes).unwrap();
    svm
}

fn send(
    svm: &mut LiteSVM,
    payer: &Keypair,
    ix: Instruction,
) -> Result<litesvm::types::TransactionMetadata, litesvm::types::FailedTransactionMetadata> {
    let blockhash = svm.latest_blockhash();
    let msg = Message::new_with_blockhash(&[ix], Some(&payer.pubkey()), &blockhash);
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[payer]).unwrap();
    svm.send_transaction(tx)
}

fn token_amount(svm: &LiteSVM, address: &Pubkey) -> u64 {
    let account = svm.get_account(address).expect("token account should exist");
    TokenAccount::unpack(&account.data)
        .expect("should deserialize as a token account")
        .amount
}

fn read_escrow(svm: &LiteSVM, pda: &Pubkey) -> escrow::Escrow {
    let raw = svm.get_account(pda).expect("escrow account should exist");
    escrow::Escrow::try_deserialize(&mut raw.data.as_slice())
        .expect("escrow account should deserialize")
}

fn now(svm: &LiteSVM) -> i64 {
    svm.get_sysvar::<Clock>().unix_timestamp
}

/// Moves the validator clock to `ts` and expires the blockhash, so a retry of
/// an identical instruction is a new transaction rather than a duplicate.
fn warp_to(svm: &mut LiteSVM, ts: i64) {
    let mut clock = svm.get_sysvar::<Clock>();
    clock.unix_timestamp = ts;
    svm.set_sysvar(&clock);
    svm.expire_blockhash();
}

/// A funded maker with a live escrow holding AMOUNT_A of mint A.
///
/// Returns (maker, escrow PDA, mint A, maker's ATA for A, vault).
fn setup_escrow(svm: &mut LiteSVM) -> (Keypair, Pubkey, Pubkey, Pubkey, Pubkey) {
    let maker = Keypair::new();
    let mint_a = Keypair::new();
    let mint_b = Keypair::new();

    let maker_pk = maker.pubkey();
    let mint_a_pk = mint_a.pubkey();
    let mint_b_pk = mint_b.pubkey();

    svm.airdrop(&maker_pk, 10_000_000_000).unwrap();

    setup_mint(svm, &mint_a, &maker_pk, 6);
    setup_mint(svm, &mint_b, &maker_pk, 6);

    let maker_ata_a = get_associated_token_address(&maker_pk, &mint_a_pk);
    setup_token_account(svm, maker_ata_a, mint_a_pk, maker_pk, AMOUNT_A);

    let (escrow_pda, _bump) = Pubkey::find_program_address(
        &[b"escrow", maker_pk.as_ref(), &SEED.to_le_bytes()],
        &escrow::id(),
    );
    let vault_a = get_associated_token_address(&escrow_pda, &mint_a_pk);

    let ix = Instruction::new_with_bytes(
        escrow::id(),
        &escrow::instruction::Make {
            seed: SEED,
            amount_a: AMOUNT_A,
            amount_b: AMOUNT_B,
        }
        .data(),
        escrow::accounts::Make {
            maker: maker_pk,
            mint_a: mint_a_pk,
            mint_b: mint_b_pk,
            escrow: escrow_pda,
            maker_ata_a,
            vault_a,
            system_program: anchor_lang::system_program::ID,
            token_program: TOKEN_PROGRAM_ID,
            associated_token_program: spl_associated_token_account_interface::program::ID,
        }
        .to_account_metas(None),
    );

    send(svm, &maker, ix).expect("make should succeed");

    (maker, escrow_pda, mint_a_pk, maker_ata_a, vault_a)
}

fn cancel_ix(
    maker: &Pubkey,
    escrow_pda: Pubkey,
    mint_a: Pubkey,
    maker_ata_a: Pubkey,
    vault_a: Pubkey,
) -> Instruction {
    Instruction::new_with_bytes(
        escrow::id(),
        &escrow::instruction::Cancel {}.data(),
        escrow::accounts::Cancel {
            maker: *maker,
            escrow: escrow_pda,
            mint_a,
            maker_ata_a,
            vault_a,
            token_program: TOKEN_PROGRAM_ID,
        }
        .to_account_metas(None),
    )
}

/// The custom error code in a failed transaction, if it failed with one.
///
/// Read out of the Debug rendering rather than by matching on a concrete
/// `TransactionError`, so this suite does not pin a solana crate version of
/// its own just to name one variant.
///
/// Anchor numbers `#[error_code]` variants from 6000. Anything below that is a
/// runtime failure — a seeds mismatch, a constraint, a panic — not a rejection
/// you wrote.
fn custom_code(rendered: &str) -> Option<u64> {
    let tail = rendered.split("Custom(").nth(1)?;
    let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn assert_declared_error(rendered: &str, context: &str) {
    let code = custom_code(rendered).unwrap_or_else(|| {
        panic!(
            "{context}\n\
             Expected the program to reject this with an error you declared in \
             `#[error_code]`, but the transaction failed some other way:\n{rendered}"
        )
    });
    assert!(
        code >= 6000,
        "{context}\n\
         Expected a declared program error (Anchor numbers those from 6000), \
         got Custom({code}). That is a runtime failure, not a rejection you \
         wrote — check the require! in `cancel`, not the accounts.\n{rendered}"
    );
}

// ------------------------------------------------------------------- tests --

#[test]
fn make_records_the_creation_time() {
    let mut svm = setup_svm();
    let at = now(&svm);
    let (_maker, escrow_pda, _mint_a, _maker_ata_a, _vault_a) = setup_escrow(&mut svm);

    let state = read_escrow(&svm, &escrow_pda);
    assert_eq!(
        state.created_at, at,
        "make should store the clock's unix_timestamp in `created_at`. \
         Checkpoint 6: `created_at: Clock::get()?.unix_timestamp` in set_inner."
    );
}

#[test]
fn make_still_moves_the_deposit_into_the_vault() {
    let mut svm = setup_svm();
    let (_maker, escrow_pda, mint_a, maker_ata_a, vault_a) = setup_escrow(&mut svm);

    assert_eq!(token_amount(&svm, &maker_ata_a), 0, "maker should be empty after make");
    assert_eq!(token_amount(&svm, &vault_a), AMOUNT_A, "vault should hold the deposit");

    let state = read_escrow(&svm, &escrow_pda);
    assert_eq!(state.amount_a, AMOUNT_A);
    assert_eq!(state.amount_b, AMOUNT_B);
    assert_eq!(state.mint_a, mint_a);
    assert_eq!(state.seed, SEED);
}

#[test]
fn cancel_immediately_is_refused() {
    let mut svm = setup_svm();
    let (maker, escrow_pda, mint_a, maker_ata_a, vault_a) = setup_escrow(&mut svm);

    let failed = send(
        &mut svm,
        &maker,
        cancel_ix(&maker.pubkey(), escrow_pda, mint_a, maker_ata_a, vault_a),
    )
    .expect_err("cancelling the instant the offer was made should be refused");

    assert_declared_error(
        &format!("{:?}", failed.err),
        "Cancelled with zero seconds elapsed.",
    );
}

#[test]
fn a_refused_cancel_moves_nothing() {
    let mut svm = setup_svm();
    let (maker, escrow_pda, mint_a, maker_ata_a, vault_a) = setup_escrow(&mut svm);
    let created_at = read_escrow(&svm, &escrow_pda).created_at;

    let _ = send(
        &mut svm,
        &maker,
        cancel_ix(&maker.pubkey(), escrow_pda, mint_a, maker_ata_a, vault_a),
    );

    assert_eq!(
        token_amount(&svm, &vault_a),
        AMOUNT_A,
        "a refused cancel must not move tokens out of the vault — the time \
         check has to run BEFORE the transfer, not after it"
    );
    assert_eq!(
        token_amount(&svm, &maker_ata_a),
        0,
        "a refused cancel must not credit the maker"
    );

    let state = read_escrow(&svm, &escrow_pda);
    assert_eq!(
        state.created_at, created_at,
        "the escrow account should be untouched by a refused cancel"
    );
}

#[test]
fn cancel_one_second_early_is_refused() {
    let mut svm = setup_svm();
    let (maker, escrow_pda, mint_a, maker_ata_a, vault_a) = setup_escrow(&mut svm);
    let created_at = read_escrow(&svm, &escrow_pda).created_at;

    warp_to(&mut svm, created_at + DELAY - 1);

    let failed = send(
        &mut svm,
        &maker,
        cancel_ix(&maker.pubkey(), escrow_pda, mint_a, maker_ata_a, vault_a),
    )
    .expect_err("299 seconds is not 300 — this cancel should still be refused");

    assert_declared_error(
        &format!("{:?}", failed.err),
        "Cancelled one second before the lock expires.",
    );
}

#[test]
fn cancel_at_the_exact_boundary_succeeds() {
    let mut svm = setup_svm();
    let (maker, escrow_pda, mint_a, maker_ata_a, vault_a) = setup_escrow(&mut svm);
    let created_at = read_escrow(&svm, &escrow_pda).created_at;

    // The whole point of the checkpoint: the lock elapses AT created_at + 300,
    // not after it. `>` instead of `>=` passes every other test in this file.
    warp_to(&mut svm, created_at + DELAY);

    send(
        &mut svm,
        &maker,
        cancel_ix(&maker.pubkey(), escrow_pda, mint_a, maker_ata_a, vault_a),
    )
    .expect(
        "at exactly created_at + CANCEL_DELAY_SECONDS the lock has elapsed. \
         Use `now >= unlocks_at`, not `now > unlocks_at`.",
    );
}

#[test]
fn cancel_after_the_delay_returns_everything() {
    let mut svm = setup_svm();
    let (maker, escrow_pda, mint_a, maker_ata_a, vault_a) = setup_escrow(&mut svm);
    let created_at = read_escrow(&svm, &escrow_pda).created_at;

    warp_to(&mut svm, created_at + DELAY + 60);

    send(
        &mut svm,
        &maker,
        cancel_ix(&maker.pubkey(), escrow_pda, mint_a, maker_ata_a, vault_a),
    )
    .expect("cancel should succeed once the lock has elapsed");

    assert_eq!(
        token_amount(&svm, &maker_ata_a),
        AMOUNT_A,
        "the maker should have every token back"
    );
    assert!(
        svm.get_account(&vault_a).is_none_or(|a| a.data.is_empty()),
        "the vault should be closed"
    );
    assert!(
        svm.get_account(&escrow_pda).is_none_or(|a| a.data.is_empty()),
        "the escrow should be closed"
    );
}

#[test]
fn the_lock_does_not_expire_early_for_a_later_offer() {
    // A second escrow made after the clock has moved gets its own countdown —
    // a time lock measured from a hardcoded zero, or from the epoch start,
    // would let this one be cancelled immediately.
    let mut svm = setup_svm();
    warp_to(&mut svm, 1_000_000);

    let (maker, escrow_pda, mint_a, maker_ata_a, vault_a) = setup_escrow(&mut svm);
    let created_at = read_escrow(&svm, &escrow_pda).created_at;
    assert_eq!(created_at, 1_000_000, "created_at should come from the clock");

    let failed = send(
        &mut svm,
        &maker,
        cancel_ix(&maker.pubkey(), escrow_pda, mint_a, maker_ata_a, vault_a),
    )
    .expect_err("an offer made at t=1_000_000 cannot be cancelled at t=1_000_000");

    assert_declared_error(
        &format!("{:?}", failed.err),
        "Cancelled immediately, on a clock that did not start at zero.",
    );
}
