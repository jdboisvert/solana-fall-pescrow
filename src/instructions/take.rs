use pinocchio::{
    cpi::{Seed, Signer},
    error::ProgramError,
    AccountView, ProgramResult,
};

use crate::state::Escrow;

// Account order: [
// 0: taker, signer, pays fees and is the recipient of A
// 1: maker, receives the B payment and rent refund
// 2: mint_a, token mint for the vaulted asset
// 3: mint_b, token mint for the maker's requested payment
// 4: escrow_account, PDA owned by the program, closed at the end
// 5: vault, ATA for escrow PDA / mint A, closed at the end
// 6: taker_ata_a, destination ATA for A owned by taker
// 7: taker_ata_b, source ATA for B owned by taker
// 8: maker_ata_b, destination ATA for B owned by maker
// 9: system_program
// 10: token_program
// 11: associated_token_program
// ]

pub fn process_take_instruction(accounts: &mut [AccountView], _data: &[u8]) -> ProgramResult {
    let [taker, maker, mint_a, mint_b, escrow_account, vault, taker_ata_a, taker_ata_b, maker_ata_b, system_program, token_program, _associated_token_program @ ..] =
        accounts
    else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    // 1 · Destructure and check the signer
    if !taker.is_signer() {
        return Err(ProgramError::MissingRequiredSignature);
    }

    // 2 · Load the escrow state, and trust it only after checking the owner
    if !escrow_account.owned_by(&crate::ID) {
        return Err(ProgramError::InvalidAccountData);
    }

    // 3 · Cross-check the passed accounts against the state
    let (amount_to_receive, bump) = {
        let escrow = Escrow::load_mut(escrow_account)?;
        if escrow.maker() != *maker.address() {
            return Err(ProgramError::InvalidAccountData);
        }
        if escrow.mint_a() != *mint_a.address() {
            return Err(ProgramError::InvalidAccountData);
        }
        if escrow.mint_b() != *mint_b.address() {
            return Err(ProgramError::InvalidAccountData);
        }
        (escrow.amount_to_receive(), escrow.bump)
    }; // ← RefMut guard dropped here

    // 4 · Re-derive the PDA
    let expected_escrow_address = pinocchio::Address::from(pinocchio_pubkey::derive_address(
        &[b"escrow", maker.address().as_ref(), &[bump]],
        None,
        &crate::ID.to_bytes(),
    ));
    if expected_escrow_address != *escrow_account.address() {
        return Err(ProgramError::InvalidSeeds);
    }

    // 5 · Validate the vault (That is how much A the taker will receive)
    let vault_state_amount = {
        let vault_state = pinocchio_token::state::Account::from_account_view(vault)?;
        if vault_state.owner() != escrow_account.address() {
            return Err(ProgramError::IllegalOwner);
        }
        if vault_state.mint() != mint_a.address() {
            return Err(ProgramError::InvalidAccountData);
        }
        vault_state.amount()
    };

    // Validate taker_ata_b the same way Make validated maker_ata: owner = taker, mint = mint_b.
    // Checked ahead of the CPIs below so a bad account fails before the taker funds any ATA rent.
    {
        let taker_ata_b_state = pinocchio_token::state::Account::from_account_view(taker_ata_b)?;
        if taker_ata_b_state.owner() != taker.address() {
            return Err(ProgramError::IllegalOwner);
        }
        if taker_ata_b_state.mint() != mint_b.address() {
            return Err(ProgramError::InvalidAccountData);
        }
    }

    // 6 · Make sure the destination ATAs exist
    pinocchio_associated_token_account::instructions::CreateIdempotent {
        funding_account: taker,
        account: taker_ata_a,
        wallet: taker,
        mint: mint_a,
        token_program,
        system_program,
    }
    .invoke()?;

    pinocchio_associated_token_account::instructions::CreateIdempotent {
        funding_account: taker,
        account: maker_ata_b,
        wallet: maker,
        mint: mint_b,
        token_program,
        system_program,
    }
    .invoke()?;

    // 7 · CPI #1, taker pays maker
    {
        let taker_ata_b_state = pinocchio_token::state::Account::from_account_view(taker_ata_b)?;
        if taker_ata_b_state.owner() != taker.address() {
            return Err(ProgramError::IllegalOwner);
        }
        if taker_ata_b_state.mint() != mint_b.address() {
            return Err(ProgramError::InvalidAccountData);
        }
    }
    pinocchio_token::instructions::Transfer {
        from: taker_ata_b,
        to: maker_ata_b,
        authority: taker,
        multisig_signers: &[] as &[&AccountView],
        amount: amount_to_receive,
    }
    .invoke()?;

    // 8 · Build the PDA signer, then CPI #2, vault pays taker
    let bump_bytes = [bump];
    let seed = [
        Seed::from(b"escrow"),
        Seed::from(maker.address().as_array()),
        Seed::from(&bump_bytes),
    ];
    let seeds = [Signer::from(&seed)];

    pinocchio_token::instructions::Transfer {
        from: vault,
        to: taker_ata_a,
        authority: escrow_account,
        multisig_signers: &[] as &[&AccountView],
        amount: vault_state_amount,
    }
    .invoke_signed(&seeds)?;

    // 9 · CPI #3, close the vault (maker made it so they get the rent)
    pinocchio_token::instructions::CloseAccount {
        account: vault,
        destination: maker,
        authority: escrow_account,
        multisig_signers: &[] as &[&AccountView],
    }
    .invoke_signed(&seeds)?;

    // 10 · Close the escrow account, by hand
    let escrow_lamports = escrow_account.lamports();
    let maker_lamports = maker
        .lamports()
        .checked_add(escrow_lamports)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    maker.set_lamports(maker_lamports);
    escrow_account.set_lamports(0);
    escrow_account.close()?;

    Ok(())
}
