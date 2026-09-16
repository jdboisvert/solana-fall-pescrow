use pinocchio::{
    cpi::{Seed, Signer},
    error::ProgramError,
    AccountView, Address, ProgramResult,
};

use crate::state::Escrow;

// Account order: [
// 0: maker, signer, the only account allowed to cancel; receives A back plus both rent refunds
// 1: mint_a, token mint for the vaulted asset
// 2: escrow_account, PDA owned by the program, closed at the end
// 3: vault, ATA for escrow PDA / mint A, closed at the end
// 4: maker_ata_a, destination ATA for the returned A, owned by maker
// 5: token_program
// ]
//
// Authorization is three checks working together, and dropping any one of them is a hole:
//   - maker.is_signer()            without it anyone can cancel someone else's escrow
//   - escrow.maker() == maker      without it the caller can drain the vault to themselves
//   - re-derived PDA == escrow     without it a look-alike account can stand in for the escrow

pub fn process_cancel_instruction(accounts: &mut [AccountView], _data: &[u8]) -> ProgramResult {
    let [maker, mint_a, escrow_account, vault, maker_ata_a, _token_program, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    // 1 · Destructure and check the signer
    if !maker.is_signer() {
        return Err(ProgramError::MissingRequiredSignature);
    }

    // 2 · Load the escrow state, and trust it only after checking the owner
    if !escrow_account.owned_by(&crate::ID) {
        return Err(ProgramError::InvalidAccountData);
    }

    // 3 · Cross-check the passed accounts against the state
    let bump = {
        let escrow = Escrow::load_mut(escrow_account)?;
        if escrow.maker() != *maker.address() {
            return Err(ProgramError::InvalidAccountData);
        }
        if escrow.mint_a() != *mint_a.address() {
            return Err(ProgramError::InvalidAccountData);
        }
        escrow.bump
    };

    // 4 · Re-derive the PDA
    let escrow_account_pda = Address::derive_address(
        &[b"escrow", maker.address().as_ref()],
        Some(bump),
        &crate::ID,
    );
    if escrow_account_pda != *escrow_account.address() {
        return Err(ProgramError::InvalidSeeds);
    }

    // 5 · Validate the vault (that is how much A goes back to the maker)
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

    // 6 · Validate maker_ata_a before any CPI: owner = maker, mint = mint_a
    {
        let maker_ata_a_state = pinocchio_token::state::Account::from_account_view(maker_ata_a)?;
        if maker_ata_a_state.owner() != maker.address() {
            return Err(ProgramError::IllegalOwner);
        }
        if maker_ata_a_state.mint() != mint_a.address() {
            return Err(ProgramError::InvalidAccountData);
        }
    }

    // 7 · Build the PDA signer, then CPI #1, vault pays the maker back
    let bump_bytes = [bump];
    let seed = [
        Seed::from(b"escrow"),
        Seed::from(maker.address().as_array()),
        Seed::from(&bump_bytes),
    ];
    let seeds = [Signer::from(&seed)];

    // Return the deposited tokens to the maker, then close the now-empty vault and the
    // escrow account, refunding both rents to the maker.
    pinocchio_token::instructions::Transfer {
        from: vault,
        to: maker_ata_a,
        authority: escrow_account,
        multisig_signers: &[] as &[&AccountView],
        amount: vault_state_amount,
    }
    .invoke_signed(&seeds)?;

    // 8 · CPI #2, close the vault (maker funded it so they get the rent)
    pinocchio_token::instructions::CloseAccount::new(vault, maker, escrow_account)
        .invoke_signed(&seeds)?;

    // 9 · Close the escrow account, by hand
    let escrow_lamports = escrow_account.lamports();
    let maker_lamports = maker
        .lamports()
        .checked_add(escrow_lamports)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    maker.set_lamports(maker_lamports);
    escrow_account.close()?;

    Ok(())
}
