#[cfg(test)]
mod tests {

    use std::path::PathBuf;

    use litesvm::LiteSVM;
    use litesvm_token::{
        spl_token::{self},
        CreateAssociatedTokenAccount, CreateMint, MintTo,
    };

    use solana_instruction::{AccountMeta, Instruction};
    use solana_keypair::Keypair;
    use solana_message::Message;
    use solana_native_token::LAMPORTS_PER_SOL;
    use solana_program_pack::Pack;
    use solana_pubkey::Pubkey;
    use solana_signer::Signer;
    use solana_transaction::Transaction;

    const PROGRAM_ID: &str = "4ibrEMW5F6hKnkW4jVedswYv6H6VtwPN6ar6dvXDN1nT";
    const TOKEN_PROGRAM_ID: Pubkey = spl_token::ID;
    const ASSOCIATED_TOKEN_PROGRAM_ID: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

    /// Everything Make mints and derives, so Take and Cancel tests can start from a live escrow.
    struct MadeEscrow {
        mint_a: Pubkey,
        mint_b: Pubkey,
        escrow: Pubkey,
        bump: u8,
        vault: Pubkey,
        maker_ata_a: Pubkey,
        amount_to_receive: u64,
        amount_to_give: u64,
    }

    /// The maker starts with this much A; Make moves `amount_to_give` of it into the vault.
    const MAKER_STARTING_A: u64 = 1_000_000_000; // 1,000 tokens at 6 decimals

    fn program_id() -> Pubkey {
        Pubkey::from(crate::ID)
    }

    fn associated_token_program() -> Pubkey {
        ASSOCIATED_TOKEN_PROGRAM_ID.parse::<Pubkey>().unwrap()
    }

    fn setup() -> (LiteSVM, Keypair) {
        let mut svm = LiteSVM::new();
        let payer = Keypair::new();

        // LiteSVM 0.9 still ships the pre-SIMD-0194 Rent sysvar (3480 lamports/byte-year,
        // 2-year exemption threshold). Mainnet has activated SIMD-0194, which folds the
        // threshold into the rate (6960 lamports/byte, threshold 1.0), and pinocchio 0.11
        // computes rent exemption that way. Set the sysvar to match the live cluster.
        #[allow(deprecated)]
        svm.set_sysvar(&solana_rent::Rent {
            lamports_per_byte_year: 6960,
            exemption_threshold: 1.0,
            burn_percent: 50,
        });

        svm.airdrop(&payer.pubkey(), 10 * LAMPORTS_PER_SOL)
            .expect("Airdrop failed");

        // Load program SO file (produced by `cargo build-sbf`)
        let so_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/deploy/escrow.so");

        let program_data = std::fs::read(&so_path).unwrap_or_else(|e| {
            panic!(
                "Failed to read program SO file at {}: {e}. Run `cargo build-sbf` first.",
                so_path.display()
            )
        });

        svm.add_program(program_id(), &program_data)
            .expect("Failed to add program");

        (svm, payer)
    }

    /// Mint A and B, fund the maker with A, then run `Make` so an escrow and a funded vault exist.
    ///
    /// `maker` is both the mint authority and the fee payer, so later tests can mint B to a taker
    /// with the same keypair.
    fn make_escrow(svm: &mut LiteSVM, maker: &Keypair) -> MadeEscrow {
        let program_id = program_id();

        let mint_a = CreateMint::new(svm, maker)
            .decimals(6)
            .authority(&maker.pubkey())
            .send()
            .unwrap();

        let mint_b = CreateMint::new(svm, maker)
            .decimals(6)
            .authority(&maker.pubkey())
            .send()
            .unwrap();

        let maker_ata_a = CreateAssociatedTokenAccount::new(svm, maker, &mint_a)
            .owner(&maker.pubkey())
            .send()
            .unwrap();

        let (escrow, bump) = Pubkey::find_program_address(
            &[b"escrow".as_ref(), maker.pubkey().as_ref()],
            &PROGRAM_ID.parse().unwrap(),
        );

        let vault = spl_associated_token_account::get_associated_token_address(&escrow, &mint_a);

        MintTo::new(svm, maker, &mint_a, &maker_ata_a, MAKER_STARTING_A)
            .send()
            .unwrap();

        let amount_to_receive: u64 = 100_000_000; // 100 B
        let amount_to_give: u64 = 500_000_000; // 500 A

        let make_data = [
            vec![0u8], // Discriminator for "Make"
            amount_to_receive.to_le_bytes().to_vec(),
            amount_to_give.to_le_bytes().to_vec(),
        ]
        .concat();

        let make_ix = Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new(maker.pubkey(), true),
                AccountMeta::new(mint_a, false),
                AccountMeta::new(mint_b, false),
                AccountMeta::new(escrow, false),
                AccountMeta::new(maker_ata_a, false),
                AccountMeta::new(vault, false),
                AccountMeta::new(solana_sdk_ids::system_program::ID, false),
                AccountMeta::new(TOKEN_PROGRAM_ID, false),
                AccountMeta::new(associated_token_program(), false),
            ],
            data: make_data,
        };

        let message = Message::new(&[make_ix], Some(&maker.pubkey()));
        let recent_blockhash = svm.latest_blockhash();
        let tx = svm
            .send_transaction(Transaction::new(&[maker], message, recent_blockhash))
            .unwrap();
        println!("Make CUs consumed: {}", tx.compute_units_consumed);

        MadeEscrow {
            mint_a,
            mint_b,
            escrow,
            bump,
            vault,
            maker_ata_a,
            amount_to_receive,
            amount_to_give,
        }
    }

    /// The twelve accounts `Take` expects, in challenge order.
    fn take_accounts(
        taker: &Pubkey,
        maker: &Pubkey,
        e: &MadeEscrow,
        taker_ata_a: &Pubkey,
        taker_ata_b: &Pubkey,
        maker_ata_b: &Pubkey,
    ) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new(*taker, true),
            AccountMeta::new(*maker, false),
            AccountMeta::new(e.mint_a, false),
            AccountMeta::new(e.mint_b, false),
            AccountMeta::new(e.escrow, false),
            AccountMeta::new(e.vault, false),
            AccountMeta::new(*taker_ata_a, false),
            AccountMeta::new(*taker_ata_b, false),
            AccountMeta::new(*maker_ata_b, false),
            AccountMeta::new(solana_sdk_ids::system_program::ID, false),
            AccountMeta::new(TOKEN_PROGRAM_ID, false),
            AccountMeta::new(associated_token_program(), false),
        ]
    }

    /// The six accounts `Cancel` expects, in challenge order.
    fn cancel_accounts(maker: &Pubkey, e: &MadeEscrow) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new(*maker, true),
            AccountMeta::new(e.mint_a, false),
            AccountMeta::new(e.escrow, false),
            AccountMeta::new(e.vault, false),
            AccountMeta::new(e.maker_ata_a, false),
            AccountMeta::new(TOKEN_PROGRAM_ID, false),
        ]
    }

    fn token_balance(svm: &LiteSVM, ata: &Pubkey) -> u64 {
        let account = svm
            .get_account(ata)
            .unwrap_or_else(|| panic!("token account {ata} does not exist"));
        spl_token_2022::state::Account::unpack(&account.data)
            .unwrap()
            .amount
    }

    /// True once the runtime has zeroed the account: either dropped entirely or left with no
    /// lamports. `close()` zeroes the lamports, so a surviving record with a balance means the
    /// account was never actually closed.
    fn is_closed(svm: &LiteSVM, address: &Pubkey) -> bool {
        svm.get_account(address)
            .map(|a| a.lamports == 0)
            .unwrap_or(true)
    }

    #[test]
    pub fn test_make_instruction() {
        let (mut svm, maker) = setup();
        let program_id = program_id();

        assert_eq!(program_id.to_string(), PROGRAM_ID);

        let e = make_escrow(&mut svm, &maker);

        // --- read the state back, rather than trusting that the transaction succeeded ---
        let vault_acc = svm.get_account(&e.vault).unwrap();
        let vault_state = spl_token_2022::state::Account::unpack(&vault_acc.data).unwrap();
        println!(
            "Vault owner: {} (escrow PDA? {})",
            vault_state.owner,
            vault_state.owner == e.escrow
        );
        println!("Vault balance: {}", vault_state.amount);
        assert_eq!(vault_state.owner, e.escrow);
        assert_eq!(vault_state.amount, e.amount_to_give);

        println!("Maker ATA balance: {}", token_balance(&svm, &e.maker_ata_a));
        assert_eq!(
            token_balance(&svm, &e.maker_ata_a),
            MAKER_STARTING_A - e.amount_to_give
        );

        let esc = svm.get_account(&e.escrow).unwrap();
        println!(
            "Escrow account owner: {} (program? {})",
            esc.owner,
            esc.owner == program_id
        );
        println!("Escrow data len: {}", esc.data.len());
        let d = &esc.data;
        println!(
            "  maker   = {}",
            Pubkey::new_from_array(d[0..32].try_into().unwrap())
        );
        println!(
            "  mint_a  = {}",
            Pubkey::new_from_array(d[32..64].try_into().unwrap())
        );
        println!(
            "  mint_b  = {}",
            Pubkey::new_from_array(d[64..96].try_into().unwrap())
        );
        println!(
            "  receive = {}",
            u64::from_le_bytes(d[96..104].try_into().unwrap())
        );
        println!(
            "  give    = {}",
            u64::from_le_bytes(d[104..112].try_into().unwrap())
        );
        println!("  bump    = {}", d[112]);
        assert_eq!(&d[0..32], maker.pubkey().as_ref());
        assert_eq!(&d[32..64], e.mint_a.as_ref());
        assert_eq!(&d[64..96], e.mint_b.as_ref());
        assert_eq!(
            u64::from_le_bytes(d[96..104].try_into().unwrap()),
            e.amount_to_receive
        );
        assert_eq!(
            u64::from_le_bytes(d[104..112].try_into().unwrap()),
            e.amount_to_give
        );
        assert_eq!(d[112], e.bump);
    }

    /// Happy path: the taker pays B, receives A, and both PDA accounts are closed to the maker.
    #[test]
    pub fn test_take_instruction() {
        let (mut svm, maker) = setup();
        let program_id = program_id();
        let e = make_escrow(&mut svm, &maker);

        let taker = Keypair::new();
        svm.airdrop(&taker.pubkey(), 10 * LAMPORTS_PER_SOL)
            .expect("Airdrop failed");

        let taker_ata_b = CreateAssociatedTokenAccount::new(&mut svm, &taker, &e.mint_b)
            .owner(&taker.pubkey())
            .send()
            .unwrap();
        MintTo::new(
            &mut svm,
            &maker,
            &e.mint_b,
            &taker_ata_b,
            e.amount_to_receive,
        )
        .send()
        .unwrap();

        // Derived but deliberately not created: the program must create both via CreateIdempotent.
        let taker_ata_a =
            spl_associated_token_account::get_associated_token_address(&taker.pubkey(), &e.mint_a);
        let maker_ata_b =
            spl_associated_token_account::get_associated_token_address(&maker.pubkey(), &e.mint_b);
        assert!(
            svm.get_account(&taker_ata_a).is_none(),
            "taker_ata_a must not exist before Take"
        );
        assert!(
            svm.get_account(&maker_ata_b).is_none(),
            "maker_ata_b must not exist before Take"
        );

        // The taker pays the fees, so the maker's balance moves only by the two rent refunds.
        let maker_before = svm.get_balance(&maker.pubkey()).unwrap();
        let vault_rent = svm.get_account(&e.vault).unwrap().lamports;
        let escrow_rent = svm.get_account(&e.escrow).unwrap().lamports;

        let take_ix = Instruction {
            program_id,
            accounts: take_accounts(
                &taker.pubkey(),
                &maker.pubkey(),
                &e,
                &taker_ata_a,
                &taker_ata_b,
                &maker_ata_b,
            ),
            data: vec![1u8],
        };

        let message = Message::new(&[take_ix], Some(&taker.pubkey()));
        let recent_blockhash = svm.latest_blockhash();
        let tx = svm
            .send_transaction(Transaction::new(&[&taker], message, recent_blockhash))
            .unwrap();
        println!("Take CUs consumed: {}", tx.compute_units_consumed);

        // The taker got the vaulted A.
        assert_eq!(token_balance(&svm, &taker_ata_a), e.amount_to_give);
        // The maker got the B they asked for.
        assert_eq!(token_balance(&svm, &maker_ata_b), e.amount_to_receive);
        // Both PDA accounts are gone.
        assert!(is_closed(&svm, &e.vault), "vault must be closed after Take");
        assert!(
            is_closed(&svm, &e.escrow),
            "escrow must be closed after Take"
        );
        // Both rents landed on the maker, exactly.
        let maker_after = svm.get_balance(&maker.pubkey()).unwrap();
        assert_eq!(
            maker_after - maker_before,
            vault_rent + escrow_rent,
            "the maker should receive the rent of both closed accounts"
        );
    }

    /// Happy path: the maker pulls their own A back out and both PDA accounts are closed.
    #[test]
    pub fn test_cancel_instruction() {
        let (mut svm, maker) = setup();
        let program_id = program_id();
        let e = make_escrow(&mut svm, &maker);

        assert_eq!(
            token_balance(&svm, &e.maker_ata_a),
            MAKER_STARTING_A - e.amount_to_give
        );

        let cancel_ix = Instruction {
            program_id,
            accounts: cancel_accounts(&maker.pubkey(), &e),
            data: vec![2u8],
        };

        let message = Message::new(&[cancel_ix], Some(&maker.pubkey()));
        let recent_blockhash = svm.latest_blockhash();
        let tx = svm
            .send_transaction(Transaction::new(&[&maker], message, recent_blockhash))
            .unwrap();
        println!("Cancel CUs consumed: {}", tx.compute_units_consumed);

        // Every A the maker started with is back.
        assert_eq!(token_balance(&svm, &e.maker_ata_a), MAKER_STARTING_A);
        assert!(
            is_closed(&svm, &e.vault),
            "vault must be closed after Cancel"
        );
        assert!(
            is_closed(&svm, &e.escrow),
            "escrow must be closed after Cancel"
        );
    }

    /// A taker who cannot cover `amount_to_receive` must fail, and the vault must be untouched.
    #[test]
    pub fn test_take_fails_with_insufficient_taker_balance() {
        let (mut svm, maker) = setup();
        let program_id = program_id();
        let e = make_escrow(&mut svm, &maker);

        let taker = Keypair::new();
        svm.airdrop(&taker.pubkey(), 10 * LAMPORTS_PER_SOL)
            .expect("Airdrop failed");

        let taker_ata_b = CreateAssociatedTokenAccount::new(&mut svm, &taker, &e.mint_b)
            .owner(&taker.pubkey())
            .send()
            .unwrap();
        // Half of what Take will try to move.
        MintTo::new(
            &mut svm,
            &maker,
            &e.mint_b,
            &taker_ata_b,
            e.amount_to_receive / 2,
        )
        .send()
        .unwrap();

        let taker_ata_a =
            spl_associated_token_account::get_associated_token_address(&taker.pubkey(), &e.mint_a);
        let maker_ata_b =
            spl_associated_token_account::get_associated_token_address(&maker.pubkey(), &e.mint_b);

        let take_ix = Instruction {
            program_id,
            accounts: take_accounts(
                &taker.pubkey(),
                &maker.pubkey(),
                &e,
                &taker_ata_a,
                &taker_ata_b,
                &maker_ata_b,
            ),
            data: vec![1u8],
        };

        let message = Message::new(&[take_ix], Some(&taker.pubkey()));
        let recent_blockhash = svm.latest_blockhash();
        let result = svm.send_transaction(Transaction::new(&[&taker], message, recent_blockhash));
        assert!(
            result.is_err(),
            "an underfunded taker must not be able to complete Take"
        );
        let failure = result.unwrap_err();
        println!(
            "Underfunded Take rejected: {:?} (CUs consumed: {})",
            failure.err, failure.meta.compute_units_consumed
        );

        // Nothing moved: the vault still holds every token, and the escrow is still open.
        assert_eq!(token_balance(&svm, &e.vault), e.amount_to_give);
        assert!(
            !is_closed(&svm, &e.escrow),
            "a failed Take must leave the escrow open"
        );
        assert_eq!(token_balance(&svm, &taker_ata_b), e.amount_to_receive / 2);
    }

    /// The critical one: a stranger signs their own Cancel and must be stopped by the
    /// stored-maker check. If this ever passes, anyone can drain any escrow.
    #[test]
    pub fn test_cancel_fails_for_stranger() {
        let (mut svm, maker) = setup();
        let program_id = program_id();
        let e = make_escrow(&mut svm, &maker);

        let stranger = Keypair::new();
        svm.airdrop(&stranger.pubkey(), 10 * LAMPORTS_PER_SOL)
            .expect("Airdrop failed");

        // Signed by the stranger, but the escrow PDA and vault still belong to the real maker.
        let mut accounts = cancel_accounts(&maker.pubkey(), &e);
        accounts[0] = AccountMeta::new(stranger.pubkey(), true);

        let cancel_ix = Instruction {
            program_id,
            accounts,
            data: vec![2u8],
        };

        let message = Message::new(&[cancel_ix], Some(&stranger.pubkey()));
        let recent_blockhash = svm.latest_blockhash();
        let result =
            svm.send_transaction(Transaction::new(&[&stranger], message, recent_blockhash));
        assert!(
            result.is_err(),
            "a stranger must not be able to cancel someone else's escrow"
        );
        let failure = result.unwrap_err();
        println!(
            "Stranger Cancel rejected: {:?} (CUs consumed: {})",
            failure.err, failure.meta.compute_units_consumed
        );

        // The escrow is untouched: same balance, still open.
        assert_eq!(token_balance(&svm, &e.vault), e.amount_to_give);
        assert!(
            !is_closed(&svm, &e.escrow),
            "a failed Cancel must leave the escrow open"
        );
        assert_eq!(
            token_balance(&svm, &e.maker_ata_a),
            MAKER_STARTING_A - e.amount_to_give
        );
    }

    /// Optional third negative case: Take with a `maker` account that is not the one stored in
    /// the escrow. The cross-check in step 3 must reject it before any token moves.
    #[test]
    pub fn test_take_fails_with_mismatched_maker() {
        let (mut svm, maker) = setup();
        let program_id = program_id();
        let e = make_escrow(&mut svm, &maker);

        let taker = Keypair::new();
        svm.airdrop(&taker.pubkey(), 10 * LAMPORTS_PER_SOL)
            .expect("Airdrop failed");

        let impostor = Keypair::new();
        svm.airdrop(&impostor.pubkey(), LAMPORTS_PER_SOL)
            .expect("Airdrop failed");

        // The taker is fully funded, so the only thing wrong here is the maker account.
        let taker_ata_b = CreateAssociatedTokenAccount::new(&mut svm, &taker, &e.mint_b)
            .owner(&taker.pubkey())
            .send()
            .unwrap();
        MintTo::new(
            &mut svm,
            &maker,
            &e.mint_b,
            &taker_ata_b,
            e.amount_to_receive,
        )
        .send()
        .unwrap();

        let taker_ata_a =
            spl_associated_token_account::get_associated_token_address(&taker.pubkey(), &e.mint_a);
        let impostor_ata_b = spl_associated_token_account::get_associated_token_address(
            &impostor.pubkey(),
            &e.mint_b,
        );

        let take_ix = Instruction {
            program_id,
            accounts: take_accounts(
                &taker.pubkey(),
                &impostor.pubkey(),
                &e,
                &taker_ata_a,
                &taker_ata_b,
                &impostor_ata_b,
            ),
            data: vec![1u8],
        };

        let message = Message::new(&[take_ix], Some(&taker.pubkey()));
        let recent_blockhash = svm.latest_blockhash();
        let result = svm.send_transaction(Transaction::new(&[&taker], message, recent_blockhash));
        assert!(
            result.is_err(),
            "Take must reject a maker that is not the one stored in the escrow"
        );
        let failure = result.unwrap_err();
        println!(
            "Mismatched-maker Take rejected: {:?} (CUs consumed: {})",
            failure.err, failure.meta.compute_units_consumed
        );

        // No B left the taker and no A left the vault.
        assert_eq!(token_balance(&svm, &taker_ata_b), e.amount_to_receive);
        assert_eq!(token_balance(&svm, &e.vault), e.amount_to_give);
        assert!(
            !is_closed(&svm, &e.escrow),
            "a failed Take must leave the escrow open"
        );
    }
}
