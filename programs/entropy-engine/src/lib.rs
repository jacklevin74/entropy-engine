//! EntropyEngine v4 — RANDAO + N-of-M Commit-Reveal + Stake Slashing + Block Hash Binding
//!
//! ## Fixes applied (v3 → v4):
//! 🔴 C1. cancel_round() now refunds all staked lamports inline via remaining_accounts
//! 🔴 C2. refund_contributor() removed — refund enforces dest == contributor.pubkey
//! 🔴 C3. Deprecated SysvarRecentBlockhashes replaced with SlotHashes sysvar
//!         Hard error if binding slot not found — no silent zero fallback
//! 🟠 H4. close_round() requires finalized_slot + CLOSE_TIMELOCK_SLOTS elapsed
//! 🟠 H5. slash() lamport accounting verified clean (comment added)
//! 🟡 M7. XOR accumulator replaced with SHA256-chain (prevents last-revealer bias)
//! 🟡 M10. ContributorCommitted / ContributorRevealed events added
//! 🟢 L11. Separate `refunded` bool on ContributorEntry (was reusing `slashed`)
//! 🟢 L13. Bot keys are randomly generated — see coordinator script

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    hash::hashv,
    sysvar::slot_hashes,
};

declare_id!("FDyWtM9UBNfXNuc5oZJ1V86d3dz635WnqMfX8x5Uifbm");

pub const STAKE_LAMPORTS: u64        = 10_000_000;
pub const REVEAL_DEADLINE_SLOTS: u64 = 400;
pub const COMMIT_DEADLINE_SLOTS: u64 = 200;
pub const MAX_CONTRIBUTORS: usize    = 10;
pub const MAX_BINDING_SLOTS: u64     = 100_000;
pub const CLOSE_TIMELOCK_SLOTS: u64  = 150; // H4: ~50s after finalize

const SH_ENTRY_SIZE: usize = 40;
const SH_HEADER:     usize = 8;

#[event]
pub struct EntropyProduced {
    pub round_id: u64,
    pub entropy:  [u8; 32],
    pub slot:     u64,
}

#[event]
pub struct RoundCancelled {
    pub round_id:    u64,
    pub coordinator: Pubkey,
    pub refunded:    u8,
}

#[event]
pub struct ContributorCommitted {
    pub round_id:    u64,
    pub contributor: Pubkey,
    pub count:       u8,
    pub total:       u8,
}

#[event]
pub struct ContributorRevealed {
    pub round_id:    u64,
    pub contributor: Pubkey,
    pub count:       u8,
    pub threshold:   u8,
}

#[program]
pub mod entropy_engine {
    use super::*;

    pub fn initialize_round(
        ctx: Context<InitializeRound>,
        round_id: u64,
        n_contributors: u8,
        m_threshold: u8,
        binding_slot: u64,
    ) -> Result<()> {
        let clock = Clock::get()?;
        require!(m_threshold >= 1, OracleError::InvalidParams);
        require!(n_contributors >= m_threshold, OracleError::InvalidParams);
        require!((n_contributors as usize) <= MAX_CONTRIBUTORS, OracleError::InvalidParams);
        let min_binding = clock.slot + COMMIT_DEADLINE_SLOTS + REVEAL_DEADLINE_SLOTS + 10;
        let max_binding = clock.slot + MAX_BINDING_SLOTS;
        require!(binding_slot >= min_binding, OracleError::BindingSlotTooSoon);
        require!(binding_slot <= max_binding, OracleError::BindingSlotTooFar);
        let round = &mut ctx.accounts.round;
        round.coordinator         = ctx.accounts.coordinator.key();
        round.round_id            = round_id;
        round.n_contributors      = n_contributors;
        round.m_threshold         = m_threshold;
        round.commit_deadline     = clock.slot + COMMIT_DEADLINE_SLOTS;
        round.reveal_deadline     = clock.slot + COMMIT_DEADLINE_SLOTS + REVEAL_DEADLINE_SLOTS;
        round.binding_slot        = binding_slot;
        round.commit_count        = 0;
        round.reveal_count        = 0;
        round.entropy_accumulator = [0u8; 32];
        round.entropy_output      = [0u8; 32];
        round.status              = RoundStatus::CommitPhase;
        round.slash_pool          = 0;
        round.finalized_slot      = 0;
        round.bump                = ctx.bumps.round;
        round.contributors        = std::array::from_fn(|_| ContributorEntry::default());
        msg!("round initialized: id={} N={} M={} binding_slot={}", round_id, n_contributors, m_threshold, binding_slot);
        Ok(())
    }

    pub fn commit(ctx: Context<Commit>, commitment: [u8; 32]) -> Result<()> {
        let clock = Clock::get()?;
        let contributor_key = ctx.accounts.contributor.key();
        {
            let round = &ctx.accounts.round;
            require!(round.status == RoundStatus::CommitPhase, OracleError::WrongPhase);
            require!(clock.slot <= round.commit_deadline, OracleError::CommitDeadlinePassed);
            require!(round.commit_count < round.n_contributors, OracleError::CommitSlotsFull);
            for entry in round.contributors.iter().take(round.commit_count as usize) {
                require!(entry.pubkey != contributor_key, OracleError::AlreadyCommitted);
            }
        }
        let stake_ix = anchor_lang::solana_program::system_instruction::transfer(
            &contributor_key, &ctx.accounts.round.key(), STAKE_LAMPORTS,
        );
        anchor_lang::solana_program::program::invoke(&stake_ix, &[
            ctx.accounts.contributor.to_account_info(),
            ctx.accounts.round.to_account_info(),
            ctx.accounts.system_program.to_account_info(),
        ])?;
        let round = &mut ctx.accounts.round;
        let idx = round.commit_count as usize;
        round.contributors[idx] = ContributorEntry {
            pubkey: contributor_key, commitment,
            revealed: false, slashed: false, slash_claimed: false, refunded: false,
        };
        round.commit_count += 1;
        if round.commit_count == round.n_contributors {
            round.status = RoundStatus::RevealPhase;
        }
        emit!(ContributorCommitted {
            round_id: round.round_id, contributor: contributor_key,
            count: round.commit_count, total: round.n_contributors,
        });
        msg!("committed: contributor={} count={}/{}", contributor_key, round.commit_count, round.n_contributors);
        Ok(())
    }

    pub fn reveal(ctx: Context<Reveal>, secret: [u8; 32], nonce: [u8; 32]) -> Result<()> {
        let clock = Clock::get()?;
        let contributor_key = ctx.accounts.contributor.key();
        let idx = {
            let round = &ctx.accounts.round;
            require!(round.status == RoundStatus::RevealPhase, OracleError::WrongPhase);
            require!(clock.slot <= round.reveal_deadline, OracleError::RevealDeadlinePassed);
            let idx = round.contributors.iter().take(round.commit_count as usize)
                .position(|e| e.pubkey == contributor_key)
                .ok_or(OracleError::ContributorNotFound)?;
            require!(!round.contributors[idx].revealed, OracleError::AlreadyRevealed);
            require!(!round.contributors[idx].slashed,  OracleError::AlreadySlashed);
            let computed = hashv(&[&secret, &nonce, contributor_key.as_ref()]).to_bytes();
            require!(computed == round.contributors[idx].commitment, OracleError::CommitmentMismatch);
            idx
        };
        {
            let round_info       = ctx.accounts.round.to_account_info();
            let contributor_info = ctx.accounts.contributor.to_account_info();
            **round_info.try_borrow_mut_lamports()?       -= STAKE_LAMPORTS;
            **contributor_info.try_borrow_mut_lamports()? += STAKE_LAMPORTS;
        }
        let round = &mut ctx.accounts.round;
        // M7: SHA256-chain accumulator — last revealer cannot choose output
        let new_acc = hashv(&[&round.entropy_accumulator, &secret]).to_bytes();
        round.entropy_accumulator = new_acc;
        round.contributors[idx].revealed = true;
        round.reveal_count += 1;
        emit!(ContributorRevealed {
            round_id: round.round_id, contributor: contributor_key,
            count: round.reveal_count, threshold: round.m_threshold,
        });
        msg!("revealed: contributor={} reveals={}/{}", contributor_key, round.reveal_count, round.m_threshold);
        Ok(())
    }

    pub fn finalize(ctx: Context<Finalize>) -> Result<()> {
        let clock = Clock::get()?;
        {
            let round = &ctx.accounts.round;
            require!(round.status == RoundStatus::RevealPhase, OracleError::WrongPhase);
            require!(round.reveal_count >= round.m_threshold, OracleError::InsufficientReveals);
            require!(clock.slot >= round.binding_slot, OracleError::BindingSlotNotReached);
        }
        // C3: SlotHashes sysvar — hard error if slot not found
        let block_hash_bytes: [u8; 32] = {
            let data = ctx.accounts.slot_hashes.try_borrow_data()?;
            let binding_slot = ctx.accounts.round.binding_slot;
            require!(data.len() >= SH_HEADER + SH_ENTRY_SIZE, OracleError::SlotHashNotFound);
            let count = u64::from_le_bytes(data[0..8].try_into().unwrap()) as usize;
            let max_entries = (data.len() - SH_HEADER) / SH_ENTRY_SIZE;
            let entries = count.min(max_entries);
            let mut found: Option<[u8; 32]> = None;
            for i in 0..entries {
                let offset = SH_HEADER + i * SH_ENTRY_SIZE;
                let entry_slot = u64::from_le_bytes(data[offset..offset+8].try_into().unwrap());
                if entry_slot <= binding_slot {
                    let mut h = [0u8; 32];
                    h.copy_from_slice(&data[offset+8..offset+40]);
                    found = Some(h);
                    break;
                }
            }
            require!(found.is_some(), OracleError::SlotHashNotFound);
            found.unwrap()
        };
        let round = &mut ctx.accounts.round;
        let round_id_bytes = round.round_id.to_le_bytes();
        let final_entropy = hashv(&[&round.entropy_accumulator, &block_hash_bytes, &round_id_bytes]).to_bytes();
        round.entropy_output      = final_entropy;
        round.entropy_accumulator = [0u8; 32];
        round.status              = RoundStatus::Finalized;
        round.finalized_slot      = clock.slot;
        emit!(EntropyProduced { round_id: round.round_id, entropy: final_entropy, slot: clock.slot });
        msg!("finalized: round_id={} entropy=0x{:016x}", round.round_id,
            u64::from_be_bytes(final_entropy[..8].try_into().unwrap()));
        Ok(())
    }

    pub fn slash(ctx: Context<SlashAccounts>, contributor_pubkey: Pubkey) -> Result<()> {
        let clock = Clock::get()?;
        let idx = {
            let round = &ctx.accounts.round;
            require!(
                round.status == RoundStatus::RevealPhase || round.status == RoundStatus::Finalized,
                OracleError::SlashNotAllowed
            );
            require!(clock.slot > round.reveal_deadline, OracleError::RevealDeadlineNotPassed);
            let idx = round.contributors.iter().take(round.commit_count as usize)
                .position(|e| e.pubkey == contributor_pubkey)
                .ok_or(OracleError::ContributorNotFound)?;
            require!(!round.contributors[idx].revealed, OracleError::AlreadyRevealed);
            require!(!round.contributors[idx].slashed,  OracleError::AlreadySlashed);
            idx
        };
        let round = &mut ctx.accounts.round;
        round.contributors[idx].slashed = true;
        // H5: stake was deposited at commit and NOT returned (no reveal) — safe to add to pool
        round.slash_pool += STAKE_LAMPORTS;
        msg!("slashed: contributor={} slash_pool={}", contributor_pubkey, round.slash_pool);
        Ok(())
    }

    pub fn claim_slash(ctx: Context<ClaimSlash>) -> Result<()> {
        let caller_key = ctx.accounts.claimer.key();
        let (share, idx) = {
            let round = &ctx.accounts.round;
            require!(
                round.status == RoundStatus::Finalized || round.status == RoundStatus::RevealPhase,
                OracleError::SlashNotAllowed
            );
            require!(round.slash_pool > 0, OracleError::NoSlashPool);
            let idx = round.contributors.iter().take(round.commit_count as usize)
                .position(|e| e.pubkey == caller_key && e.revealed)
                .ok_or(OracleError::ContributorNotFound)?;
            require!(!round.contributors[idx].slash_claimed, OracleError::SlashAlreadyClaimed);
            let share = round.slash_pool / round.reveal_count as u64;
            (share, idx)
        };
        {
            let round_info   = ctx.accounts.round.to_account_info();
            let claimer_info = ctx.accounts.claimer.to_account_info();
            **round_info.try_borrow_mut_lamports()?   -= share;
            **claimer_info.try_borrow_mut_lamports()? += share;
        }
        let round = &mut ctx.accounts.round;
        round.contributors[idx].slash_claimed = true;
        msg!("slash claimed: claimer={} amount={}", caller_key, share);
        Ok(())
    }

    /// C1+C2: Cancel refunds all stakes inline. remaining_accounts must be
    /// committed contributor wallets in order — destination validated against pubkey.
    pub fn cancel_round(ctx: Context<CancelRound>) -> Result<()> {
        {
            let round = &ctx.accounts.round;
            require!(round.status == RoundStatus::CommitPhase, OracleError::WrongPhase);
            require!(round.coordinator == ctx.accounts.coordinator.key(), OracleError::Unauthorized);
        }
        let commit_count = ctx.accounts.round.commit_count as usize;
        require!(ctx.remaining_accounts.len() >= commit_count, OracleError::MissingRefundAccounts);
        let round_info = ctx.accounts.round.to_account_info();
        for i in 0..commit_count {
            let entry = &ctx.accounts.round.contributors[i];
            if !entry.slashed && !entry.refunded {
                let dest = &ctx.remaining_accounts[i];
                // C2: hard validate destination == contributor pubkey
                require!(dest.key() == entry.pubkey, OracleError::RefundDestMismatch);
                **round_info.try_borrow_mut_lamports()? -= STAKE_LAMPORTS;
                **dest.try_borrow_mut_lamports()?       += STAKE_LAMPORTS;
            }
        }
        let round = &mut ctx.accounts.round;
        for i in 0..commit_count {
            if !round.contributors[i].slashed {
                round.contributors[i].refunded = true;
            }
        }
        round.status = RoundStatus::Cancelled;
        emit!(RoundCancelled { round_id: round.round_id, coordinator: round.coordinator, refunded: round.commit_count });
        msg!("round cancelled: id={} refunded={}", round.round_id, commit_count);
        Ok(())
    }

    /// H4: Finalized rounds require CLOSE_TIMELOCK_SLOTS before close.
    pub fn close_round(ctx: Context<CloseRound>) -> Result<()> {
        let clock = Clock::get()?;
        let round = &ctx.accounts.round;
        if round.status == RoundStatus::Finalized {
            require!(
                clock.slot >= round.finalized_slot + CLOSE_TIMELOCK_SLOTS,
                OracleError::CloseTooSoon
            );
        }
        msg!("round closed, rent reclaimed");
        Ok(())
    }
}

#[derive(Accounts)]
#[instruction(round_id: u64)]
pub struct InitializeRound<'info> {
    #[account(init, payer = coordinator, space = 8 + Round::LEN,
        seeds = [b"round", coordinator.key().as_ref(), &round_id.to_le_bytes()], bump)]
    pub round: Account<'info, Round>,
    #[account(mut)] pub coordinator: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Commit<'info> {
    #[account(mut, seeds = [b"round", round.coordinator.as_ref(), &round.round_id.to_le_bytes()], bump = round.bump)]
    pub round: Account<'info, Round>,
    #[account(mut)] pub contributor: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Reveal<'info> {
    #[account(mut, seeds = [b"round", round.coordinator.as_ref(), &round.round_id.to_le_bytes()], bump = round.bump)]
    pub round: Account<'info, Round>,
    #[account(mut)] pub contributor: Signer<'info>,
}

#[derive(Accounts)]
pub struct Finalize<'info> {
    #[account(mut, seeds = [b"round", round.coordinator.as_ref(), &round.round_id.to_le_bytes()], bump = round.bump)]
    pub round: Account<'info, Round>,
    /// CHECK: SlotHashes sysvar (C3 — replaces deprecated RecentBlockhashes)
    #[account(address = slot_hashes::ID)]
    pub slot_hashes: AccountInfo<'info>,
}

#[derive(Accounts)]
pub struct SlashAccounts<'info> {
    #[account(mut, seeds = [b"round", round.coordinator.as_ref(), &round.round_id.to_le_bytes()], bump = round.bump)]
    pub round: Account<'info, Round>,
    pub caller: Signer<'info>,
}

#[derive(Accounts)]
pub struct ClaimSlash<'info> {
    #[account(mut, seeds = [b"round", round.coordinator.as_ref(), &round.round_id.to_le_bytes()], bump = round.bump)]
    pub round: Account<'info, Round>,
    #[account(mut)] pub claimer: Signer<'info>,
}

#[derive(Accounts)]
pub struct CancelRound<'info> {
    #[account(mut, seeds = [b"round", round.coordinator.as_ref(), &round.round_id.to_le_bytes()], bump = round.bump)]
    pub round: Account<'info, Round>,
    pub coordinator: Signer<'info>,
    // remaining_accounts: contributor wallets in commit order
}

#[derive(Accounts)]
pub struct CloseRound<'info> {
    #[account(mut,
        seeds = [b"round", round.coordinator.as_ref(), &round.round_id.to_le_bytes()], bump = round.bump,
        close = coordinator,
        constraint = round.status == RoundStatus::Finalized || round.status == RoundStatus::Cancelled @ OracleError::WrongPhase,
        constraint = round.coordinator == coordinator.key() @ OracleError::Unauthorized,
    )]
    pub round: Account<'info, Round>,
    #[account(mut)] pub coordinator: Signer<'info>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, PartialEq, Eq, Default)]
pub enum RoundStatus { #[default] CommitPhase, RevealPhase, Finalized, Cancelled }

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default)]
pub struct ContributorEntry {
    pub pubkey:        Pubkey,
    pub commitment:    [u8; 32],
    pub revealed:      bool,
    pub slashed:       bool,
    pub slash_claimed: bool,
    pub refunded:      bool,  // L11
}
impl ContributorEntry { pub const LEN: usize = 68; }

#[account]
pub struct Round {
    pub coordinator:          Pubkey,
    pub round_id:             u64,
    pub n_contributors:       u8,
    pub m_threshold:          u8,
    pub commit_deadline:      u64,
    pub reveal_deadline:      u64,
    pub binding_slot:         u64,
    pub commit_count:         u8,
    pub reveal_count:         u8,
    pub entropy_accumulator:  [u8; 32],
    pub entropy_output:       [u8; 32],
    pub status:               RoundStatus,
    pub slash_pool:           u64,
    pub finalized_slot:       u64,  // H4
    pub bump:                 u8,
    pub contributors:         [ContributorEntry; 10],
}
impl Round {
    pub const LEN: usize = 32+8+1+1+8+8+8+1+1+32+32+1+8+8+1 + (ContributorEntry::LEN * 10);
}

#[error_code]
pub enum OracleError {
    #[msg("Invalid params")] InvalidParams,
    #[msg("Wrong phase for this instruction")] WrongPhase,
    #[msg("Commit deadline has passed")] CommitDeadlinePassed,
    #[msg("All commit slots are full")] CommitSlotsFull,
    #[msg("Already committed")] AlreadyCommitted,
    #[msg("Reveal deadline has passed")] RevealDeadlinePassed,
    #[msg("Reveal deadline has not passed yet")] RevealDeadlineNotPassed,
    #[msg("Contributor not found")] ContributorNotFound,
    #[msg("Already revealed")] AlreadyRevealed,
    #[msg("Already slashed")] AlreadySlashed,
    #[msg("Commitment mismatch")] CommitmentMismatch,
    #[msg("Not enough reveals to finalize")] InsufficientReveals,
    #[msg("Binding slot not reached yet")] BindingSlotNotReached,
    #[msg("Binding slot too soon")] BindingSlotTooSoon,
    #[msg("Binding slot too far")] BindingSlotTooFar,
    #[msg("Slash not allowed in current state")] SlashNotAllowed,
    #[msg("No slash pool to claim")] NoSlashPool,
    #[msg("Slash already claimed")] SlashAlreadyClaimed,
    #[msg("Unauthorized")] Unauthorized,
    #[msg("Binding slot hash not found in SlotHashes sysvar")] SlotHashNotFound,
    #[msg("Close too soon — wait for timelock after finalization")] CloseTooSoon,
    #[msg("Missing refund accounts in remaining_accounts")] MissingRefundAccounts,
    #[msg("Refund destination does not match contributor pubkey")] RefundDestMismatch,
}
