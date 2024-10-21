use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, RwLock};
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use crossbeam_channel::unbounded;
use rayon::prelude::*;
use solana_core::banking_trace::BankingTracer;
use solana_core::cluster_info_vote_listener::VoteTracker;
use solana_core::cluster_slots_service::cluster_slots::ClusterSlots;
use solana_core::consensus::heaviest_subtree_fork_choice::HeaviestSubtreeForkChoice;
use solana_core::consensus::progress_map::{ForkProgress, ProgressMap};
use solana_core::consensus::Tower;
use solana_core::consensus::tower_storage::FileTowerStorage;
use solana_core::repair::cluster_slot_state_verifier::{DuplicateConfirmedSlots, DuplicateSlotsTracker, EpochSlotsFrozenSlots};
use solana_core::repair::repair_service::OutstandingShredRepairs;
use solana_core::replay_stage::{ReplayStage, ReplayStageConfig};
use solana_core::unfrozen_gossip_verified_vote_hashes::UnfrozenGossipVerifiedVoteHashes;
use solana_gossip::cluster_info::{ClusterInfo, Node};
use solana_ledger::blockstore::{Blockstore, BlockstoreSignals};
use solana_ledger::blockstore_options::BlockstoreOptions;
use solana_ledger::create_new_tmp_ledger;
use solana_ledger::genesis_utils::create_genesis_config;
use solana_ledger::leader_schedule_cache::LeaderScheduleCache;
use solana_poh::poh_recorder::create_test_recorder;
use solana_rpc::optimistically_confirmed_bank_tracker::OptimisticallyConfirmedBank;
use solana_rpc::rpc_subscriptions::RpcSubscriptions;
use solana_runtime::accounts_background_service::AbsRequestSender;
use solana_runtime::bank::Bank;
use solana_runtime::bank_forks::BankForks;
use solana_runtime::commitment::BlockCommitmentCache;
use solana_runtime::genesis_utils::GenesisConfigInfo;
use solana_runtime::installed_scheduler_pool::BankWithScheduler;
use solana_runtime::prioritization_fee_cache::PrioritizationFeeCache;
use solana_sdk::clock::Slot;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_streamer::socket::SocketAddrSpace;

fn criterion_benchmark(c: &mut Criterion) {
    c.bench_function("Example", |b| {
        let genesis_config = create_genesis_config(10_000).genesis_config;
        let bank0 = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank0);

        let root = 3;
        let root_bank = Bank::new_from_parent(
            bank_forks.read().unwrap().get(0).unwrap(),
            &Pubkey::default(),
            root,
        );
        root_bank.freeze();
        let root_hash = root_bank.hash();
        bank_forks.write().unwrap().insert(root_bank);

        let mut heaviest_subtree_fork_choice = HeaviestSubtreeForkChoice::new((root, root_hash));

        let mut progress = ProgressMap::default();
        for i in 0..=root {
            progress.insert(i, ForkProgress::new(Hash::default(), None, None, 0, 0));
        }

        let mut duplicate_slots_tracker: DuplicateSlotsTracker =
            vec![root - 1, root, root + 1].into_iter().collect();
        let mut duplicate_confirmed_slots: DuplicateConfirmedSlots = vec![root - 1, root, root + 1]
            .into_iter()
            .map(|s| (s, Hash::default()))
            .collect();
        let mut unfrozen_gossip_verified_vote_hashes: UnfrozenGossipVerifiedVoteHashes =
            UnfrozenGossipVerifiedVoteHashes {
                votes_per_slot: vec![root - 1, root, root + 1]
                    .into_iter()
                    .map(|s| (s, HashMap::new()))
                    .collect(),
            };
        let mut epoch_slots_frozen_slots: EpochSlotsFrozenSlots = vec![root - 1, root, root + 1]
            .into_iter()
            .map(|slot| (slot, Hash::default()))
            .collect();
        let (drop_bank_sender, _drop_bank_receiver) = unbounded();

        b.iter(|| {
            for _i in 0..100 {
                ReplayStage::handle_new_root(
                    root,
                    &bank_forks,
                    &mut progress,
                    &AbsRequestSender::default(),
                    None,
                    &mut heaviest_subtree_fork_choice,
                    &mut duplicate_slots_tracker,
                    &mut duplicate_confirmed_slots,
                    &mut unfrozen_gossip_verified_vote_hashes,
                    &mut true,
                    &mut Vec::new(),
                    &mut epoch_slots_frozen_slots,
                    &drop_bank_sender,
                )
                    .unwrap();
            }
        })
    });
}


fn setup_test_environment() -> (
    Slot,
    Arc<RwLock<BankForks>>,
    ProgressMap,
    AbsRequestSender,
    Option<Slot>,
    HeaviestSubtreeForkChoice,
    DuplicateSlotsTracker,
    DuplicateConfirmedSlots,
    UnfrozenGossipVerifiedVoteHashes,
    Vec<Signature>,
    EpochSlotsFrozenSlots,
    crossbeam_channel::Sender<Vec<BankWithScheduler>>,
) {
    // Initialize all necessary structs and data
    let genesis_config = create_genesis_config(10_000).genesis_config;
    let bank0 = Bank::new_for_tests(&genesis_config);
    let bank_forks = BankForks::new_rw_arc(bank0);

    let root = 3;
    let root_bank = Bank::new_from_parent(
        bank_forks.read().unwrap().get(0).unwrap(),
        &Pubkey::default(),
        root,
    );
    root_bank.freeze();
    let root_hash = root_bank.hash();
    bank_forks.write().unwrap().insert(root_bank);

    let mut heaviest_subtree_fork_choice = HeaviestSubtreeForkChoice::new((root, root_hash));

    let mut progress = ProgressMap::default();
    for i in 0..=root {
        progress.insert(i, ForkProgress::new(Hash::default(), None, None, 0, 0));
    }

    let mut duplicate_slots_tracker: DuplicateSlotsTracker =
        vec![root - 1, root, root + 1].into_iter().collect();
    let mut duplicate_confirmed_slots: DuplicateConfirmedSlots = vec![root - 1, root, root + 1]
        .into_iter()
        .map(|s| (s, Hash::default()))
        .collect();
    let mut unfrozen_gossip_verified_vote_hashes: UnfrozenGossipVerifiedVoteHashes =
        UnfrozenGossipVerifiedVoteHashes {
            votes_per_slot: vec![root - 1, root, root + 1]
                .into_iter()
                .map(|s| (s, HashMap::new()))
                .collect(),
        };
    let mut epoch_slots_frozen_slots: EpochSlotsFrozenSlots = vec![root - 1, root, root + 1]
        .into_iter()
        .map(|slot| (slot, Hash::default()))
        .collect();
    let (drop_bank_sender, _drop_bank_receiver) = unbounded();
    (root, bank_forks, progress, AbsRequestSender::default(), None, heaviest_subtree_fork_choice, duplicate_slots_tracker, duplicate_confirmed_slots, unfrozen_gossip_verified_vote_hashes, Vec::new(), epoch_slots_frozen_slots, drop_bank_sender)
}

fn benchmark_concurrent(c: &mut Criterion) {
    let mut group = c.benchmark_group("Concurrent Function Calls");
    group.measurement_time(Duration::from_secs(100));

    let (
        root,
        bank_forks,
        mut progress,
        accounts_background_request_sender,
        highest_super_majority_root,
        heaviest_subtree_fork_choice,
        duplicate_slots_tracker,
        duplicate_confirmed_slots,
        mut unfrozen_gossip_verified_vote_hashes,
        voted_signatures,
        epoch_slots_frozen_slots,
        drop_bank_sender
    ) = setup_test_environment();


    group.bench_function("parallel", |b| {
        b.iter(|| {
            (0..1).into_par_iter().for_each(|i| {
                let mut progress = ProgressMap::default();
                for i in 0..=root {
                    progress.insert(i, ForkProgress::new(Hash::default(), None, None, 0, 0));
                }

                let mut unfrozen_gossip_verified_vote_hashes: UnfrozenGossipVerifiedVoteHashes =
                    UnfrozenGossipVerifiedVoteHashes {
                        votes_per_slot: vec![root - 1, root, root + 1]
                            .into_iter()
                            .map(|s| (s, HashMap::new()))
                            .collect(),
                    };

                black_box(ReplayStage::handle_new_root(
                    root,
                    &bank_forks,
                    &mut progress,
                    &accounts_background_request_sender,
                    highest_super_majority_root,
                    &mut heaviest_subtree_fork_choice.clone(),
                    &mut duplicate_slots_tracker.clone(),
                    &mut duplicate_confirmed_slots.clone(),
                    &mut unfrozen_gossip_verified_vote_hashes,
                    &mut true,
                    &mut voted_signatures.clone(),
                    &mut epoch_slots_frozen_slots.clone(),
                    &drop_bank_sender,
                ).unwrap());
            });
        })
    });

    group.finish();
}

criterion_group!(benches, criterion_benchmark, benchmark_concurrent);
criterion_main!(benches);