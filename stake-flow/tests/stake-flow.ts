import * as anchor from "@coral-xyz/anchor";
import { Program, web3 } from "@coral-xyz/anchor";
import { StakeFlow } from "../target/types/stake_flow";
import {
  TOKEN_PROGRAM_ID,
  createMint,
  createAccount,
  mintTo,
  getAccount,
  Account,
} from "@solana/spl-token";
import { assert, expect, use } from "chai";
import {
  PublicKey,
  Keypair,
  SystemProgram,
  SYSVAR_RENT_PUBKEY,
  LAMPORTS_PER_SOL,
} from "@solana/web3.js";
import { BN } from "bn.js";

// this airdrops sol to an address
async function airdropSol(publicKey, amount) {
  let airdropTx = await anchor.getProvider().connection.requestAirdrop(publicKey, amount * anchor.web3.LAMPORTS_PER_SOL);
  await confirmTransaction(airdropTx);
}

async function confirmTransaction(tx) {
  const latestBlockHash = await anchor.getProvider().connection.getLatestBlockhash();
  await anchor.getProvider().connection.confirmTransaction({
    blockhash: latestBlockHash.blockhash,
    lastValidBlockHeight: latestBlockHash.lastValidBlockHeight,
    signature: tx,
  });
}

describe("stake-flow", () => {
  // Configure the client to use the local cluster.
  anchor.setProvider(anchor.AnchorProvider.env());

  const program = anchor.workspace.stakeFlow as Program<StakeFlow>;

  let config: any;
  const owner = anchor.web3.Keypair.generate();
  const user = anchor.web3.Keypair.generate();

  let stakeTokenMint: PublicKey;
  let userTokenAccount: PublicKey;
  let userStxAccount: PublicKey; // for liquid stake receipt tokens
  let stakeLockedPda: PublicKey; // for locked stake

  it("Is initialized!", async () => {
    await airdropSol(owner.publicKey, 10); // 10 SOL

    // create token mint for staking
   stakeTokenMint = await createMint(
      anchor.getProvider().connection,
      owner,
      owner.publicKey,
      null,
      9,
   );

   // create user token account for staking source
   userTokenAccount = await createAccount(
      anchor.getProvider().connection,
      owner,
      stakeTokenMint,
      user.publicKey,
      undefined,
      undefined,
      TOKEN_PROGRAM_ID,
    );

    // mint user some stake tokens
    await mintTo(
      anchor.getProvider().connection,
      owner,
      stakeTokenMint,
      userTokenAccount,
      owner,
      2000 * 10 ** 9, // 2000 tokens with 9 decimals
    );

    const tx = await program.methods.initialize(
       new anchor.BN(500), // 5%
       new anchor.BN(1000), // 10 %
  )
    .accounts({
      admin: owner.publicKey,
      stakeTokenMint: stakeTokenMint,
      tokenProgram: TOKEN_PROGRAM_ID,
    })
    .signers([owner])
    .rpc();

    console.log("Owner key is", owner.publicKey.toString());

    let [protocolConfig] = PublicKey.findProgramAddressSync(
      [Buffer.from("protocol_config")],
      program.programId,
    );
    config = await program.account.protocolConfig.fetch(protocolConfig);
    console.log("Protocol config admin is", config.admin.toString());
    assert.equal(config.admin.toString(), owner.publicKey.toString(), "Admin should be the owner who initialized the protocol");
  });

  it("Stake-liquid test", async () => {
    await airdropSol(user.publicKey, 10); // 10 SOL

    // create user stx token account
    userStxAccount = await createAccount(
      anchor.getProvider().connection,
      user,
      config.stxMint,
      user.publicKey,
      undefined,
      undefined,
      TOKEN_PROGRAM_ID,
    );

    const solBalance = await anchor.getProvider().connection.getBalance(userStxAccount);
    console.log("User stx account sol balance before is", solBalance);

    const stakeAmount = new anchor.BN(10 * 10 ** 9); // 10 tokens with 9 decimals
    try { 
      await program.methods.stakeLiquid(stakeAmount)
    .accounts({
      stakeTokenMint: stakeTokenMint,
      userTokenAccount: userTokenAccount,
      userStxAccount: userStxAccount,
      user: user.publicKey,
      tokenProgram: TOKEN_PROGRAM_ID,
    })
    .signers([user])
    .rpc();
  } catch (err) {
    console.error("Error during staking:", err);
  }

  // read user stx token receipt. It should be 1 : 1 since first depositor
    const balance = await anchor.getProvider().connection.getTokenAccountBalance(userStxAccount);
    console.log("User stx account token balance is", balance.value.amount.toString());

    assert.equal(balance.value.amount.toString(), stakeAmount.toString(), "User should receive stx tokens equal to the amount staked for the first depositor");
  });

  it("Stake-locked test", async () => {
    const amount = new anchor.BN(10 * 10 ** 9); // 10 tokens with 9 decimals
    await program.methods.stakeLocked(amount)
    .accounts({
      stakeTokenMint: stakeTokenMint,
      userTokenAccount: userTokenAccount,
      user: user.publicKey,
      tokenProgram: TOKEN_PROGRAM_ID,
    })
    .signers([user])
    .rpc();

    stakeLockedPda = PublicKey.findProgramAddressSync(
      [Buffer.from("user_stake"), user.publicKey.toBuffer()],
      program.programId,
    )[0];
    const userStakeAccount = await program.account.userStake.fetch(stakeLockedPda);
    
    console.log("User stake token amount", userStakeAccount.amount.toString());
    assert.equal(userStakeAccount.amount.toString(), amount.toString(), "User stake account should reflect the locked stake amount");
  });
  
});



