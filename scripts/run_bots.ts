/**
 * EntropyEngine v4 — Bot Coordinator (TypeScript)
 * v4 fixes:
 *   L13: Random bot keypairs each run
 *   C3:  SYSVAR_SLOT_HASHES_PUBKEY (not deprecated RecentBlockhashes)
 *   H4:  wait CLOSE_TIMELOCK_SLOTS after finalize before close
 */

import * as anchor from "@coral-xyz/anchor";
import {
  Connection,
  Keypair,
  PublicKey,
  SystemProgram,
  SYSVAR_SLOT_HASHES_PUBKEY,
  sendAndConfirmTransaction,
  Transaction,
} from "@solana/web3.js";
import * as fs from "fs";
import * as path from "path";

const RPC_URL    = "https://rpc.mainnet.x1.xyz";
const PROGRAM_ID = new PublicKey("FDyWtM9UBNfXNuc5oZJ1V86d3dz635WnqMfX8x5Uifbm");
const WALLET_PATH = "/mnt/sda1/workspace/built/owl/owl-deploy-keypair.json";
const IDL_PATH    = path.join(__dirname, "../target/idl/entropy_engine.json");

const N_CONTRIBUTORS      = 3;
const M_THRESHOLD         = 3;
const MIN_BOT_LAMPORTS    = 25_000_000;
const CLOSE_TIMELOCK_SLOTS = 150; // H4: match on-chain constant

function loadKeypair(p: string): Keypair {
  return Keypair.fromSecretKey(Buffer.from(JSON.parse(fs.readFileSync(p, "utf8"))));
}

function sha256(...parts: Buffer[]): Buffer {
  const crypto = require("crypto");
  const h = crypto.createHash("sha256");
  for (const p of parts) h.update(p);
  return h.digest();
}

function roundPda(coordinator: PublicKey, roundId: bigint): PublicKey {
  const idBuf = Buffer.alloc(8);
  idBuf.writeBigUInt64LE(roundId);
  const [pda] = PublicKey.findProgramAddressSync(
    [Buffer.from("round"), coordinator.toBuffer(), idBuf],
    PROGRAM_ID
  );
  return pda;
}

async function ensureFunded(conn: Connection, coordinator: Keypair, bot: Keypair) {
  const bal = await conn.getBalance(bot.publicKey);
  if (bal < MIN_BOT_LAMPORTS) {
    const need = MIN_BOT_LAMPORTS - bal;
    console.log(`  Funding ${bot.publicKey.toBase58().slice(0, 8)}... (${need} lamports)`);
    const { blockhash } = await conn.getLatestBlockhash();
    const tx = new Transaction({ recentBlockhash: blockhash, feePayer: coordinator.publicKey });
    tx.add(anchor.web3.SystemProgram.transfer({
      fromPubkey: coordinator.publicKey,
      toPubkey:   bot.publicKey,
      lamports:   need,
    }));
    await sendAndConfirmTransaction(conn, tx, [coordinator]);
    await new Promise(r => setTimeout(r, 2000));
  }
}

async function sleep(ms: number) { return new Promise(r => setTimeout(r, ms)); }

async function runRound(
  program: anchor.Program,
  coordinator: Keypair,
  bots: Keypair[],
  roundId: bigint
) {
  const conn = program.provider.connection;
  const pda  = roundPda(coordinator.publicKey, roundId);
  const idBuf = Buffer.alloc(8); idBuf.writeBigUInt64LE(roundId);

  console.log(`\n${"═".repeat(52)}`);
  console.log(` Round ${roundId}  |  PDA: ${pda.toBase58().slice(0, 16)}...`);
  console.log(`${"═".repeat(52)}`);

  const slot0 = await conn.getSlot();
  const bindingSlot = slot0 + 620;
  console.log(` Current slot: ${slot0}  →  binding: ${bindingSlot}`);

  // Generate secrets
  const botSecrets = bots.map(() => Buffer.from(require("crypto").randomBytes(32)));
  const botNonces  = bots.map(() => Buffer.from(require("crypto").randomBytes(32)));
  const commitments = bots.map((bot, i) =>
    Array.from(sha256(botSecrets[i], botNonces[i], bot.publicKey.toBuffer()))
  );

  // [1] initialize_round
  console.log("\n[1] Initializing round...");
  let tx = await program.methods
    .initializeRound(new anchor.BN(roundId.toString()), N_CONTRIBUTORS, M_THRESHOLD, new anchor.BN(bindingSlot))
    .accounts({ round: pda, coordinator: coordinator.publicKey, systemProgram: SystemProgram.programId })
    .signers([coordinator])
    .rpc();
  console.log(`    ✅ ${tx.slice(0, 24)}...`);

  // [2] Commits
  console.log("\n[2] Commits...");
  for (let i = 0; i < bots.length; i++) {
    tx = await program.methods
      .commit(commitments[i])
      .accounts({ round: pda, contributor: bots[i].publicKey, systemProgram: SystemProgram.programId })
      .signers([bots[i]])
      .rpc();
    console.log(`    ✅ bot${i+1} [${bots[i].publicKey.toBase58().slice(0,8)}]: ${tx.slice(0,20)}...`);
  }

  // [3] Reveals
  console.log("\n[3] Reveals...");
  for (let i = 0; i < bots.length; i++) {
    tx = await program.methods
      .reveal(Array.from(botSecrets[i]), Array.from(botNonces[i]))
      .accounts({ round: pda, contributor: bots[i].publicKey })
      .signers([bots[i]])
      .rpc();
    console.log(`    ✅ bot${i+1} [${bots[i].publicKey.toBase58().slice(0,8)}]: ${tx.slice(0,20)}...`);
  }

  // [4] Wait for binding slot
  let slot = await conn.getSlot();
  if (slot < bindingSlot) {
    console.log(`\n[4] Waiting for binding slot ${bindingSlot}...`);
    while (slot < bindingSlot) {
      await sleep(1000);
      slot = await conn.getSlot();
      process.stdout.write(`    ${slot}/${bindingSlot}\r`);
    }
    console.log(`    ✅ Binding slot reached (${slot})`);
  }

  // [5] Finalize — C3: SlotHashes sysvar
  console.log("\n[5] Finalize...");
  tx = await program.methods
    .finalize()
    .accounts({ round: pda, slotHashes: SYSVAR_SLOT_HASHES_PUBKEY })
    .signers([])
    .rpc();
  console.log(`    ✅ ${tx.slice(0, 24)}...`);
  const finalizeSlot = await conn.getSlot();

  const acc: any = await program.account["round"].fetch(pda);
  const entropyHex = Buffer.from(acc.entropyOutput).toString("hex");
  console.log(`\n 🎲 ENTROPY  ${entropyHex}`);
  console.log(`    round_id ${acc.roundId}  |  status ${JSON.stringify(acc.status)}`);

  // [6] H4: Wait for close timelock
  let curSlot = await conn.getSlot();
  const closeAt = finalizeSlot + CLOSE_TIMELOCK_SLOTS;
  if (curSlot < closeAt) {
    const waitSlots = closeAt - curSlot;
    const waitMs    = (waitSlots / 3) * 1000 + 2000;
    console.log(`\n[6] Waiting ${waitSlots} slots (~${Math.ceil(waitMs/1000)}s) for close timelock...`);
    await sleep(waitMs);
  }

  // [7] Close round
  console.log("\n[7] Closing round (reclaiming rent)...");
  tx = await program.methods
    .closeRound()
    .accounts({ round: pda, coordinator: coordinator.publicKey })
    .signers([coordinator])
    .rpc();
  console.log(`    ✅ ${tx.slice(0, 24)}...`);

  return entropyHex;
}

async function main() {
  console.log("╔══════════════════════════════════════════════════╗");
  console.log("║      EntropyEngine v4 — Bot Coordinator          ║");
  console.log("╚══════════════════════════════════════════════════╝");
  console.log(`Program : ${PROGRAM_ID.toBase58()}`);

  const conn        = new Connection(RPC_URL, "confirmed");
  const coordinator = loadKeypair(WALLET_PATH);
  console.log(`Coord   : ${coordinator.publicKey.toBase58()}`);

  // L13: Random bot keypairs — not derivable from coordinator
  const bots = Array.from({ length: N_CONTRIBUTORS }, () => Keypair.generate());
  bots.forEach((b, i) => console.log(`Bot ${i+1}   : ${b.publicKey.toBase58()} (random)`));

  console.log("\nFunding bots...");
  for (const bot of bots) await ensureFunded(conn, coordinator, bot);

  const idl     = JSON.parse(fs.readFileSync(IDL_PATH, "utf8"));
  const wallet  = new anchor.Wallet(coordinator);
  const provider = new anchor.AnchorProvider(conn, wallet, { commitment: "confirmed" });
  const program  = new anchor.Program(idl, provider);

  let roundId    = BigInt(Math.floor(Date.now() / 1000));
  let roundsDone = 0;

  while (true) {
    try {
      await runRound(program, coordinator, bots, roundId);
      roundsDone++;
      console.log(`\n Rounds completed: ${roundsDone}. Sleeping 30s...\n`);
      roundId++;
      await sleep(30_000);
    } catch (e: any) {
      console.error(`\n❌ Round ${roundId} failed: ${e?.message ?? e}`);
      console.log("   Retrying in 10s...");
      roundId++;
      await sleep(10_000);
    }
  }
}

main().catch(console.error);
