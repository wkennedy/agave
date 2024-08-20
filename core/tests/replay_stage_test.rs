use bytes::Bytes;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use crossbeam_channel::unbounded;
use futures::stream::iter;
use log::info;
use rand::Rng;
use rayon::prelude::*;
use solana_core::banking_trace::BankingTracer;
use solana_core::cluster_info_vote_listener::VoteTracker;
use solana_core::cluster_slots_service::cluster_slots::ClusterSlots;
use solana_core::consensus::heaviest_subtree_fork_choice::HeaviestSubtreeForkChoice;
use solana_core::consensus::progress_map::{ForkProgress, ProgressMap};
use solana_core::consensus::tower_storage::FileTowerStorage;
use solana_core::consensus::{Tower, VOTE_THRESHOLD_DEPTH};
use solana_core::repair::ancestor_hashes_service::{AncestorHashesReplayUpdate, AncestorHashesReplayUpdateSender};
use solana_core::repair::cluster_slot_state_verifier::{DuplicateConfirmedSlots, DuplicateSlotsTracker, EpochSlotsFrozenSlots};
use solana_core::repair::repair_service::OutstandingShredRepairs;
use solana_core::replay_stage::{ReplayStage, ReplayStageConfig};
use solana_core::unfrozen_gossip_verified_vote_hashes::UnfrozenGossipVerifiedVoteHashes;
use solana_entry::entry::{create_ticks, Entry};
use solana_gossip::cluster_info::{ClusterInfo, Node};
use solana_ledger::blockstore::{Blockstore, BlockstoreError, BlockstoreSignals};
use solana_ledger::blockstore_options::BlockstoreOptions;
use solana_ledger::genesis_utils::create_genesis_config;
use solana_ledger::leader_schedule_cache::LeaderScheduleCache;
use solana_ledger::shred::{ProcessShredsStats, ReedSolomonCache, Shredder};
use solana_ledger::{create_new_tmp_ledger, create_new_tmp_ledger_auto_delete};
use solana_poh::poh_recorder::create_test_recorder;
use solana_rpc::optimistically_confirmed_bank_tracker::OptimisticallyConfirmedBank;
use solana_rpc::rpc::{create_test_transaction_entries, populate_blockstore_for_tests};
use solana_rpc::rpc_subscriptions::RpcSubscriptions;
use solana_runtime::accounts_background_service::AbsRequestSender;
use solana_runtime::bank::Bank;
use solana_runtime::bank_client::BankClient;
use solana_runtime::bank_forks::BankForks;
use solana_runtime::commitment::{BlockCommitmentCache, VOTE_THRESHOLD_SIZE};
use solana_runtime::genesis_utils::{create_genesis_config_with_leader, GenesisConfigInfo};
use solana_runtime::installed_scheduler_pool::BankWithScheduler;
use solana_runtime::loader_utils::create_invoke_instruction;
use solana_runtime::prioritization_fee_cache::PrioritizationFeeCache;
use solana_sdk::account::{Account, ReadableAccount};
use solana_sdk::clock::{Epoch, Slot};
use solana_sdk::hash::Hash;
use solana_sdk::message::Message;
use solana_sdk::poh_config::PohConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::{SanitizedTransaction, Transaction};
use solana_sdk::{system_program, system_transaction};
use solana_streamer::socket::SocketAddrSpace;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

fn create_replay_stage_config(
    bank_forks: Arc<RwLock<BankForks>>,
    exit: Arc<AtomicBool>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
    ancestor_hashes_replay_update_sender: AncestorHashesReplayUpdateSender,
    replay_forks_threads: NonZeroUsize,
    replay_transactions_threads: NonZeroUsize,
    vote_keypair: Keypair,
) -> ReplayStageConfig {
    let rpc_subscriptions = Arc::new(RpcSubscriptions::new_for_tests(
        exit.clone(),
        Arc::new(AtomicU64::default()), // max_complete_transaction_status_slot
        Arc::new(AtomicU64::default()), // max_complete_rewards_slot
        bank_forks.clone(),
        block_commitment_cache.clone(),
        OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks),
    ));

    let tower_storage = Arc::new(FileTowerStorage::default());
    let authorized_voter_keypairs = Arc::new(RwLock::new(vec![Arc::new(vote_keypair.insecure_clone())]));

    ReplayStageConfig {
        vote_account: vote_keypair.pubkey().clone(),
        authorized_voter_keypairs,
        exit,
        rpc_subscriptions,
        leader_schedule_cache,
        accounts_background_request_sender: AbsRequestSender::default(),
        block_commitment_cache,
        transaction_status_sender: None,
        rewards_recorder_sender: None,
        cache_block_meta_sender: None,
        entry_notification_sender: None,
        bank_notification_sender: None,
        wait_for_vote_to_start_leader: false, // default value
        ancestor_hashes_replay_update_sender,
        tower_storage,
        wait_to_vote_slot: None,
        replay_forks_threads,
        replay_transactions_threads,
    }
}

#[test]
fn run_replay_stage() -> () {
    solana_logger::setup();

    //Setup nodes and cluster
    let leader = Node::new_localhost();
    let non_leader_node_keypair = Keypair::new();
    let non_leader_node = Node::new_localhost_with_pubkey(&non_leader_node_keypair.pubkey());

    let cluster_info = ClusterInfo::new(
        non_leader_node.info.clone(),
        non_leader_node_keypair.into(),
        SocketAddrSpace::Unspecified,
    );

    let cluster_keypair = &cluster_info.keypair().clone();
    let cluster_pubkey = &cluster_keypair.pubkey().clone();
    cluster_info.insert_info(leader.info.clone());
    let cluster_info_arc = Arc::new(cluster_info);
    //End setup nodes and cluster

    //Setup GenesisConfig
    let starting_balance = 10_000;
    let GenesisConfigInfo { mut genesis_config, .. } = create_genesis_config_with_leader(starting_balance, leader.info.pubkey(), 3);
    genesis_config.ticks_per_slot = 4;

    info!("GenesisConfig ticks_per_slot: {:?}", &genesis_config.ticks_per_slot);
    info!("GenesisConfig poh_config: {:?}", &genesis_config.poh_config);
    //End setup GenesisConfig

    //Bank setup
    let bank0 = Bank::new_for_tests(&genesis_config);
    for _ in 0..genesis_config.ticks_per_slot {
        bank0.register_default_tick_for_test();
    }
    bank0.freeze();

    let bank_forks = BankForks::new_rw_arc(bank0);

    let bank_1_slot = 1;
    let bank1 = Bank::new_from_parent(
        bank_forks.read().unwrap().get(0).unwrap(),
        &Pubkey::default(),
        bank_1_slot,
    );
    bank_forks.write().unwrap().insert(bank1);
    //End bank setup

    //Setup blockstore
    let (blockstore_path, _blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
    let BlockstoreSignals {
        blockstore,
        ledger_signal_receiver,
        ..
    } = Blockstore::open_with_signal(&blockstore_path.path(), BlockstoreOptions::default())
        .expect("Expected to successfully open ledger");
    let blockstore = Arc::new(blockstore);

    let bank = bank_forks.read().unwrap().working_bank();

    populate_blockstore_for_tests(
        vec![],
        bank.clone(),
        blockstore.clone(),
        Arc::new(AtomicU64::default()),
    );
    //End setup blockstore

    let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));

    let vote_keypair = Keypair::new();

    let tower = Tower::new_from_bankforks(&bank_forks.read().unwrap(), &cluster_pubkey, &vote_keypair.pubkey().clone());

    //Poh Setup
    let (exit, poh_recorder, poh_service, _entry_receiver) = create_test_recorder(bank.clone(), blockstore.clone(), None, Some(leader_schedule_cache.clone()));

    //Setup caches
    let block_commitment_cache = Arc::new(RwLock::new(BlockCommitmentCache::default()));
    let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));

    let cluster_slots = Arc::new(ClusterSlots::default());

    //Setup threads
    let replay_forks_threads = NonZeroUsize::new(2).unwrap(); //tvu_config default - 1 serial mode, > 1 parallel
    let replay_transactions_threads = NonZeroUsize::new(2).unwrap(); //tvu_config default - 1 serial mode, > 1 parallel

    //Setup channels
    let (retransmit_slots_sender, _retransmit_slots_receiver) = unbounded();
    let (_gossip_verified_vote_hash_sender, gossip_verified_vote_hash_receiver) = unbounded();
    let (replay_vote_sender, _replay_vote_receiver) = unbounded();
    let (ancestor_hashes_replay_update_sender, _ancestor_hashes_replay_update_receiver) = unbounded();
    let (cost_update_sender, _cost_update_receiver) = unbounded();
    let (_duplicate_slots_sender, duplicate_slots_receiver) = unbounded();
    let (_ancestor_duplicate_slots_sender, ancestor_duplicate_slots_receiver) = unbounded();
    let (_, gossip_confirmed_slots_receiver) = unbounded(); // gossip_confirmed_slots_receiver
    let (cluster_slots_update_sender, _cluster_slots_update_receiver) = unbounded();
    let (voting_sender, _voting_receiver) = unbounded();
    let (drop_bank_sender, _drop_bank_receiver) = unbounded();
    let (dumped_slots_sender, _dumped_slots_receiver) = unbounded();
    let (_popular_pruned_forks_sender, popular_pruned_forks_receiver) = unbounded();


    //Create replay stage config
    let replay_stage_config = create_replay_stage_config(bank_forks.clone(), exit.clone(), leader_schedule_cache.clone(), block_commitment_cache.clone(), ancestor_hashes_replay_update_sender.clone(), replay_forks_threads, replay_transactions_threads, vote_keypair.insecure_clone());

    let stage = ReplayStage::new(
        replay_stage_config,
        blockstore,
        bank_forks,
        cluster_info_arc,
        ledger_signal_receiver,
        duplicate_slots_receiver,
        poh_recorder,
        tower,
        Arc::<VoteTracker>::default(),
        cluster_slots,
        retransmit_slots_sender,
        ancestor_duplicate_slots_receiver,
        replay_vote_sender,
        gossip_confirmed_slots_receiver,
        gossip_verified_vote_hash_receiver,
        cluster_slots_update_sender,
        cost_update_sender,
        voting_sender,
        drop_bank_sender,
        None,
        None,
        ignored_prioritization_fee_cache,
        dumped_slots_sender,
        BankingTracer::new_disabled(),
        popular_pruned_forks_receiver,
    ).expect("Panic in ReplayStage::new");

    thread::sleep(Duration::from_millis(500));
    exit.store(true, Ordering::Relaxed);
    stage.join().unwrap();
    poh_service.join().unwrap();
}

// TODO Create test for multiple banks
// TODO Create test with actual transactions
// for i in 1..=3 {
//     let prev_bank = bank_forks.read().unwrap().get(i - 1).unwrap();
//     let slot = prev_bank.slot() + 1;
//     let bank = new_bank_from_parent_with_bank_forks(
//         bank_forks.as_ref(),
//         prev_bank,
//         &Pubkey::default(),
//         slot,
//     );
//     let _res = bank.transfer(
//         10,
//         &mint_keypair,
//         &solana_sdk::pubkey::new_rand(),
//     );
//     for _ in 0..genesis_config.ticks_per_slot {
//         bank.register_default_tick_for_test();
//     }
//
// }

//BEGIN HELPER FUNCTIONS
//These were pieced together from other areas of the code
#[allow(clippy::too_many_arguments)]
pub fn fill_blockstore_slot_with_ticks(
    blockstore: &Blockstore,
    ticks_per_slot: u64,
    slot: u64,
    parent_slot: u64,
    last_entry_hash: Hash,
) -> Hash {
    // Only slot 0 can be equal to the parent_slot
    assert!(slot.saturating_sub(1) >= parent_slot);
    let num_slots = (slot - parent_slot).max(1);
    let entries = create_ticks(num_slots * ticks_per_slot, 0, last_entry_hash);
    let last_entry_hash = entries.last().unwrap().hash;

    write_entries(
        blockstore,
        slot,
        0,
        0,
        ticks_per_slot,
        Some(parent_slot),
        true,
        &Arc::new(Keypair::new()),
        entries,
        0,
    ).unwrap();

    last_entry_hash
}

//Copied from blockstore.rs
#[allow(clippy::too_many_arguments)]
fn write_entries(
    blockstore: &Blockstore,
    start_slot: Slot,
    num_ticks_in_start_slot: u64,
    start_index: u32,
    ticks_per_slot: u64,
    parent: Option<u64>,
    is_full_slot: bool,
    keypair: &Keypair,
    entries: Vec<Entry>,
    version: u16,
) -> Result<usize, BlockstoreError/*num of data shreds*/> {
    let mut parent_slot = parent.map_or(start_slot.saturating_sub(1), |v| v);
    let num_slots = (start_slot - parent_slot).max(1); // Note: slot 0 has parent slot 0
    assert!(num_ticks_in_start_slot < num_slots * ticks_per_slot);
    let mut remaining_ticks_in_slot = num_slots * ticks_per_slot - num_ticks_in_start_slot;

    let mut current_slot = start_slot;
    let mut shredder = Shredder::new(current_slot, parent_slot, 0, version).unwrap();
    let mut all_shreds = vec![];
    let mut slot_entries = vec![];
    let reed_solomon_cache = ReedSolomonCache::default();
    let mut chained_merkle_root = Some(Hash::new_from_array(rand::thread_rng().gen()));
    // Find all the entries for start_slot
    for entry in entries.into_iter() {
        if remaining_ticks_in_slot == 0 {
            current_slot += 1;
            parent_slot = current_slot - 1;
            remaining_ticks_in_slot = ticks_per_slot;
            let current_entries = std::mem::take(&mut slot_entries);
            let start_index = {
                if all_shreds.is_empty() {
                    start_index
                } else {
                    0
                }
            };
            let (mut data_shreds, mut coding_shreds) = shredder.entries_to_shreds(
                keypair,
                &current_entries,
                true, // is_last_in_slot
                chained_merkle_root,
                start_index, // next_shred_index
                start_index, // next_code_index
                true,        // merkle_variant
                &reed_solomon_cache,
                &mut ProcessShredsStats::default(),
            );
            all_shreds.append(&mut data_shreds);
            all_shreds.append(&mut coding_shreds);
            chained_merkle_root = Some(coding_shreds.last().unwrap().merkle_root().unwrap());
            shredder = Shredder::new(
                current_slot,
                parent_slot,
                (ticks_per_slot - remaining_ticks_in_slot) as u8,
                version,
            )
                .unwrap();
        }

        if entry.is_tick() {
            remaining_ticks_in_slot -= 1;
        }
        slot_entries.push(entry);
    }

    if !slot_entries.is_empty() {
        let (mut data_shreds, mut coding_shreds) = shredder.entries_to_shreds(
            keypair,
            &slot_entries,
            is_full_slot,
            chained_merkle_root,
            0,    // next_shred_index
            0,    // next_code_index
            true, // merkle_variant
            &reed_solomon_cache,
            &mut ProcessShredsStats::default(),
        );
        all_shreds.append(&mut data_shreds);
        all_shreds.append(&mut coding_shreds);
    }
    let num_data = all_shreds.iter().filter(|shred| shred.is_data()).count();
    blockstore.insert_shreds(all_shreds, None, false)?;
    Ok(num_data)
}

#[allow(dead_code)]
fn new_bank_from_parent_with_bank_forks(
    bank_forks: &RwLock<BankForks>,
    parent: Arc<Bank>,
    collector_id: &Pubkey,
    slot: Slot,
) -> Arc<Bank> {
    let bank = Bank::new_from_parent(parent, collector_id, slot);
    bank_forks
        .write()
        .unwrap()
        .insert(bank)
        .clone_without_scheduler()
}

pub fn create_transaction_entries(
    bank: &Bank, num: usize,
) -> (Vec<Entry>, Vec<Signature>) {
    let transactions = create_transactions(bank, num);

    let mut blockhash = bank.confirmed_last_blockhash(); //confirmed_last_blockhash
    let rent_exempt_amount = bank.get_minimum_balance_for_rent_exemption(0);
    let mut entries = Vec::new();
    let mut signatures = Vec::new();


    for transaction in transactions {
        signatures.push(transaction.signatures[0]);
        let entry = Entry::new(&blockhash, 1, vec![transaction]);
        blockhash = entry.hash;
        entries.push(entry);
    }
    // let final_tick = Entry::new_tick(0, &blockhash);
    let final_tick = solana_entry::entry::next_entry(&blockhash, 1, vec![]);
    entries.push(final_tick);

    (entries, signatures)
}

fn create_transactions(bank: &Bank, num: usize) -> Vec<Transaction> {
    let funded_accounts = create_funded_accounts(bank, 2 * num);
    funded_accounts
        .into_par_iter()
        .chunks(2)
        .map(|chunk| {
            let from = &chunk[0];
            let to = &chunk[1];
            system_transaction::transfer(from, &to.pubkey(), 1, bank.last_blockhash())
        })
        .collect()
}

fn create_accounts(num: usize) -> Vec<Keypair> {
    (0..num).into_par_iter().map(|_| Keypair::new()).collect()
}

fn create_funded_accounts(bank: &Bank, num: usize) -> Vec<Keypair> {
    assert_eq!(num % 2, 0, "must be multiple of 2 for parallel funding tree");
    let accounts = create_accounts(num);

    accounts.par_iter().for_each(|account| {
        bank.store_account(
            &account.pubkey(),
            &Account {
                lamports: 5100,
                data: vec![],
                owner: system_program::id(),
                executable: false,
                rent_epoch: Epoch::MAX,
            }
                .to_account_shared_data(),
        );
    });

    accounts
}
//(run `cargo fix --test "replay_stage_test"`

