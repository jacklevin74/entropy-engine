#!/usr/bin/env python3
"""
EntropyEngine v4 — Bot Coordinator
Fixes applied vs v3:
  - L13: Bot keys randomly generated per run (not deterministic from coordinator)
  - C3:  SlotHashes sysvar used in finalize (not deprecated RecentBlockhashes)
  - H4:  close_round waits CLOSE_TIMELOCK_SLOTS (~50s) after finalize
  - M7:  SHA256-chain accumulator (handled on-chain, no bot change needed)

Usage:  python3 scripts/run_bots.py
Deps:   pip install solana anchorpy solders
"""

import asyncio
import os
import hashlib
import json
import secrets
import time
from pathlib import Path

from solana.rpc.async_api import AsyncClient
from solana.rpc.commitment import Confirmed
from solders.keypair import Keypair
from solders.pubkey import Pubkey
from solders.system_program import transfer, TransferParams
from solders.transaction import Transaction
from anchorpy import Program, Provider, Wallet, Context, Idl

RPC_URL    = "https://rpc.mainnet.x1.xyz"
PROGRAM_ID = Pubkey.from_string("FDyWtM9UBNfXNuc5oZJ1V86d3dz635WnqMfX8x5Uifbm")
WALLET_PATH = Path(os.environ.get("COORDINATOR_KEYPAIR", "~/.config/solana/id.json")).expanduser()
IDL_PATH    = Path(__file__).parent.parent / "target/idl/entropy_engine.json"

SYSTEM_PROGRAM  = Pubkey.from_string("11111111111111111111111111111111")
# C3: SlotHashes sysvar (replaces deprecated SysvarRecentBlockhashes)
SYSVAR_SLOT_HASHES = Pubkey.from_string("SysvarS1otHashes111111111111111111111111111")

N_CONTRIBUTORS   = 3
M_THRESHOLD      = 3
MIN_BOT_LAMPORTS = 25_000_000  # 0.025 XNT each
CLOSE_TIMELOCK_SLOTS = 150     # H4: match on-chain constant

def load_keypair(path) -> Keypair:
    return Keypair.from_bytes(bytes(json.loads(Path(path).read_text())))

def sha256(*parts: bytes) -> bytes:
    h = hashlib.sha256()
    for p in parts: h.update(p)
    return h.digest()

def round_pda(coordinator: Pubkey, round_id: int) -> Pubkey:
    addr, _ = Pubkey.find_program_address(
        [b"round", bytes(coordinator), round_id.to_bytes(8, "little")],
        PROGRAM_ID,
    )
    return addr

async def ensure_funded(client: AsyncClient, coordinator: Keypair, wallet: Keypair):
    bal = (await client.get_balance(wallet.pubkey())).value
    if bal < MIN_BOT_LAMPORTS:
        need = MIN_BOT_LAMPORTS - bal
        print(f"  Funding {str(wallet.pubkey())[:8]}... ({need} lamports)")
        bh = (await client.get_latest_blockhash()).value.blockhash
        ix = transfer(TransferParams(
            from_pubkey=coordinator.pubkey(),
            to_pubkey=wallet.pubkey(),
            lamports=need,
        ))
        tx = Transaction.new_signed_with_payer([ix], coordinator.pubkey(), [coordinator], bh)
        await client.send_transaction(tx)
        await asyncio.sleep(2)

async def run_round(program: Program, coordinator: Keypair, bots: list[Keypair], round_id: int):
    client = program.provider.connection
    pda    = round_pda(coordinator.pubkey(), round_id)

    print(f"\n{'═'*52}")
    print(f" Round {round_id}  |  PDA: {str(pda)[:16]}...")
    print(f"{'═'*52}")

    slot = (await client.get_slot()).value
    binding_slot = slot + 620
    print(f" Current slot: {slot}  →  binding: {binding_slot}")

    bot_secrets = [secrets.token_bytes(32) for _ in bots]
    bot_nonces  = [secrets.token_bytes(32) for _ in bots]
    commitments = [
        sha256(bot_secrets[i], bot_nonces[i], bytes(bots[i].pubkey()))
        for i in range(len(bots))
    ]

    print("\n[1] Initializing round...")
    tx = await program.rpc["initialize_round"](
        round_id, N_CONTRIBUTORS, M_THRESHOLD, binding_slot,
        ctx=Context(accounts={
            "round": pda, "coordinator": coordinator.pubkey(), "system_program": SYSTEM_PROGRAM,
        }, signers=[coordinator])
    )
    print(f"    ✅ {tx[:24]}...")

    print("\n[2] Commits...")
    for i, bot in enumerate(bots):
        tx = await program.rpc["commit"](
            list(commitments[i]),
            ctx=Context(accounts={
                "round": pda, "contributor": bot.pubkey(), "system_program": SYSTEM_PROGRAM,
            }, signers=[bot])
        )
        print(f"    ✅ bot{i+1} [{str(bot.pubkey())[:8]}]: {tx[:20]}...")

    print("\n[3] Reveals...")
    for i, bot in enumerate(bots):
        tx = await program.rpc["reveal"](
            list(bot_secrets[i]), list(bot_nonces[i]),
            ctx=Context(accounts={"round": pda, "contributor": bot.pubkey()}, signers=[bot])
        )
        print(f"    ✅ bot{i+1} [{str(bot.pubkey())[:8]}]: {tx[:20]}...")

    slot = (await client.get_slot()).value
    if slot < binding_slot:
        print(f"\n[4] Waiting for binding slot {binding_slot}...")
        while slot < binding_slot:
            await asyncio.sleep(1)
            slot = (await client.get_slot()).value
            print(f"    {slot}/{binding_slot}", end="\r")
        print(f"    ✅ Binding slot reached ({slot})")

    print("\n[5] Finalize...")
    tx = await program.rpc["finalize"](
        ctx=Context(accounts={
            "round": pda,
            "slot_hashes": SYSVAR_SLOT_HASHES,  # C3: SlotHashes sysvar
        }, signers=[])
    )
    print(f"    ✅ {tx[:24]}...")
    finalize_slot = (await client.get_slot()).value

    acc = await program.account["round"].fetch(pda)
    entropy_hex = bytes(acc.entropy_output).hex()
    print(f"\n 🎲 ENTROPY  {entropy_hex}")
    print(f"    round_id {acc.round_id}  |  status {acc.status}")

    # H4: Wait for close timelock before closing
    current_slot = (await client.get_slot()).value
    close_at = finalize_slot + CLOSE_TIMELOCK_SLOTS
    if current_slot < close_at:
        wait_slots = close_at - current_slot
        wait_secs  = wait_slots / 3  # ~3 slots/sec
        print(f"\n[6] Waiting {wait_slots} slots (~{wait_secs:.0f}s) for close timelock...")
        await asyncio.sleep(wait_secs + 2)

    print("\n[7] Closing round (reclaiming rent)...")
    tx = await program.rpc["close_round"](
        ctx=Context(accounts={"round": pda, "coordinator": coordinator.pubkey()}, signers=[coordinator])
    )
    print(f"    ✅ {tx[:24]}...")

    return entropy_hex

async def main():
    print("╔══════════════════════════════════════════════════╗")
    print("║      EntropyEngine v4 — Bot Coordinator          ║")
    print("╚══════════════════════════════════════════════════╝")
    print(f"Program : {PROGRAM_ID}")

    client      = AsyncClient(RPC_URL, commitment=Confirmed)
    coordinator = load_keypair(WALLET_PATH)
    print(f"Coord   : {coordinator.pubkey()}")

    # L13: Random bot keypairs each run — not derivable from coordinator pubkey
    bots = [Keypair() for _ in range(N_CONTRIBUTORS)]
    for i, b in enumerate(bots):
        print(f"Bot {i+1}   : {b.pubkey()} (random)")

    print("\nFunding bots...")
    for bot in bots:
        await ensure_funded(client, coordinator, bot)

    idl     = Idl.from_json(IDL_PATH.read_text())
    program = Program(idl, PROGRAM_ID, Provider(client, Wallet(coordinator)))

    round_id   = int(time.time())
    rounds_run = 0

    while True:
        try:
            await run_round(program, coordinator, bots, round_id)
            rounds_run += 1
            print(f"\n Rounds completed: {rounds_run}. Sleeping 30s...\n")
            round_id += 1
            await asyncio.sleep(30)
        except KeyboardInterrupt:
            print("\nStopped by user.")
            break
        except Exception as e:
            print(f"\n❌ Round {round_id} failed: {e}")
            print("   Retrying with new round_id in 10s...")
            round_id += 1
            await asyncio.sleep(10)

    await client.close()

if __name__ == "__main__":
    asyncio.run(main())
