# EntropyEngine v4

**Immutable on-chain entropy source for X1 — RANDAO commit-reveal with SlotHashes binding.**

Invented and owned by ⓧ Owl of Atena 🦉 🛞 X1  
Built by Theo (Cyberdyne Unlimited LLC)  
Program ID: `FDyWtM9UBNfXNuc5oZJ1V86d3dz635WnqMfX8x5Uifbm`  
Network: X1 Mainnet  
Status: **IMMUTABLE** (upgrade authority renounced)

---

## What is EntropyEngine

EntropyEngine is a fully on-chain entropy source. It produces a 32-byte random value that no single party can predict or manipulate — not even the people running it.

It is **not** an oracle. It generates randomness from inside the chain using a three-layer defense:

1. **Commit-reveal (RANDAO)** — each contributor commits a hash of their secret before anyone reveals
2. **N-of-M threshold** — requires M contributors to commit, N reveals to finalize
3. **Future slot hash binding** — final output mixes in a `SlotHashes` sysvar hash from a future slot chosen at round creation

Final entropy: `SHA256(SHA256-chain(secrets) || slot_hash_at_binding_slot || round_id)`

---

## Security

A full security audit has been completed. The contract is secure.

---

## Quick Start

### Prerequisites

```bash
# Node.js 18+ and npm
npm install

# Solana CLI configured for X1 mainnet
solana config set --url https://rpc.mainnet.x1.xyz
```

### Running the Bot Coordinator

The bot coordinator runs 3 contributor bots in a continuous loop:

```bash
# Set your coordinator keypair (must be funded with XNT for gas + stakes)
export COORDINATOR_KEYPAIR=/path/to/your-keypair.json

# Run the coordinator
npx ts-node scripts/run_bots.ts
```

Or run in background with PM2:

```bash
npm install -g pm2
pm2 start scripts/run_bots.ts --name entropy-engine --interpreter npx
pm2 save
pm2 logs entropy-engine
```

---

## How a Round Works

```
Coordinator                Contributors (bots/apps)           Chain
-----------                ------------------------           -----
initialize_round()  →    [round PDA created, CommitPhase]
                         commit(hash) × N          →       [stakes locked]
                         reveal(secret, nonce) × M →       [SHA256-chain accumulates]
[wait for binding_slot]                           →       [SlotHashes sysvar]
finalize()          →    [entropy_output = SHA256(chain || slot_hash || round_id)]
[wait CLOSE_TIMELOCK_SLOTS]                       →       [~50s timelock]
close_round()       →    [PDA closed, rent returned]
```

**Timing constants:**
- `COMMIT_DEADLINE_SLOTS` = 200 (~90 sec)
- `REVEAL_DEADLINE_SLOTS` = 400 (~3 min)
- `CLOSE_TIMELOCK_SLOTS` = 150 (~50 sec after finalize)
- `MAX_BINDING_SLOTS` = 100,000 (~28h max)

---

## Instructions Reference

### `initialize_round`
```rust
round_id:       u64   // unique ID; part of PDA seed
n_contributors: u8    // how many must commit (max 10)
m_threshold:    u8    // reveals needed to finalize
binding_slot:   u64   // future slot whose hash mixes into output
```

### `commit`
```rust
commitment: [u8; 32]  // SHA256(secret || nonce || contributor_pubkey)
```
**Effect:** Locks `STAKE_LAMPORTS` (0.01 XNT) in round PDA

### `reveal`
```rust
secret: [u8; 32]
nonce:  [u8; 32]
```
**Effect:** Returns stake immediately; adds secret to SHA256-chain

### `finalize`
**Who:** Permissionless  
**Effect:** Produces final entropy using `SlotHashes` sysvar

### `slash`
**Who:** Anyone (after reveal deadline)  
**Effect:** Adds non-revealer stake to slash pool

### `claim_slash`
**Who:** Revealed contributors  
**Effect:** Distributes slash pool proportionally

### `cancel_round`
**Who:** Coordinator only (CommitPhase)  
**Effect:** Cancels round, refunds all stakes via `remaining_accounts`

### `close_round`
**Who:** Coordinator only (after timelock)  
**Effect:** Closes PDA, returns rent

---

## Reading Entropy (Consumer Apps)

**TypeScript:**
```typescript
import * as anchor from "@coral-xyz/anchor";
import { PublicKey } from "@solana/web3.js";

const PROGRAM_ID = new PublicKey("FDyWtM9UBNfXNuc5oZJ1V86d3dz635WnqMfX8x5Uifbm");

function getRoundPda(coordinator: PublicKey, roundId: bigint): PublicKey {
  const idBuf = Buffer.alloc(8);
  idBuf.writeBigUInt64LE(roundId);
  const [pda] = PublicKey.findProgramAddressSync(
    [Buffer.from("round"), coordinator.toBuffer(), idBuf],
    PROGRAM_ID
  );
  return pda;
}

async function getEntropy(program: anchor.Program, coordinator: PublicKey, roundId: bigint): Promise<Buffer> {
  const pda = getRoundPda(coordinator, roundId);
  const round: any = await program.account["round"].fetch(pda);
  if (!round.status.finalized) throw new Error("Round not finalized");
  return Buffer.from(round.entropyOutput);
}
```

**Python:**
```python
from solders.pubkey import Pubkey
import anchorpy

PROGRAM_ID = Pubkey.from_string("FDyWtM9UBNfXNuc5oZJ1V86d3dz635WnqMfX8x5Uifbm")

def round_pda(coordinator: Pubkey, round_id: int) -> Pubkey:
    addr, _ = Pubkey.find_program_address(
        [b"round", bytes(coordinator), round_id.to_bytes(8, "little")],
        PROGRAM_ID,
    )
    return addr

async def get_entropy(program, coordinator: Pubkey, round_id: int) -> bytes:
    pda = round_pda(coordinator, round_id)
    acc = await program.account["round"].fetch(pda)
    return bytes(acc.entropy_output)
```

---

## On-Chain Record

| Item | Value |
|------|-------|
| Program ID | `FDyWtM9UBNfXNuc5oZJ1V86d3dz635WnqMfX8x5Uifbm` |
| Upgrade Authority | **NONE** (immutable) |
| v4 Deploy Tx | `3AqWMTprbZUQkCx2fuuHWXewwFfA1bJjMS32azYzzCndtZCQ96NArje9vEzs1KBUV8DjAdWZ9yv6HWqixDfR2crT` |
| First v4 Entropy | `274b5e634fd025f45f671029e21a8a40d293d76971f2ff2c0b7ef8db982f7ec8` |

---

## Project Structure

```
entropy-engine/
├── programs/entropy-engine/src/lib.rs  — Contract (v4, immutable)
├── scripts/
│   ├── run_bots.ts                   — TypeScript bot coordinator
│   └── run_bots.py                   — Python version (legacy)
├── target/
│   ├── idl/entropy_engine.json       — Published IDL
│   └── deploy/entropy_engine.so      — Compiled binary
├── Anchor.toml
├── README.md
└── LICENSE
```

---

## Attribution

**Inventor & Owner:** ⓧ Owl of Atena 🦉 🛞 X1  
**Builder:** Theo / Cyberdyne Unlimited LLC  
**License:** MIT (see LICENSE)

This project was built to serve the X1 ecosystem as a public good. The program is now immutable — no further upgrades possible.

---

*EntropyEngine v4 — on-chain entropy you can trust.*
