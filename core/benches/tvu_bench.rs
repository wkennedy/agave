use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::sync::atomic::AtomicBool;
use criterion::{criterion_group, criterion_main, Criterion};
use crossbeam_channel::unbounded;
use rayon::iter::ParallelIterator;
use rayon::iter::{IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator};
use {
    serial_test::serial,
    solana_gossip::cluster_info::{ClusterInfo, Node},
    solana_ledger::{
        blockstore::BlockstoreSignals,
        blockstore_options::BlockstoreOptions,
        create_new_tmp_ledger,
        genesis_utils::{create_genesis_config, GenesisConfigInfo},
    },
    solana_poh::poh_recorder::create_test_recorder,
    solana_rpc::optimistically_confirmed_bank_tracker::OptimisticallyConfirmedBank,
    solana_runtime::bank::Bank,
    solana_sdk::signature::{Keypair, Signer},
    solana_streamer::socket::SocketAddrSpace,
    std::sync::atomic::{AtomicU64, Ordering},
};
use solana_client::connection_cache::ConnectionCache;
use solana_core::banking_trace::BankingTracer;
use solana_core::cluster_info_vote_listener::VoteTracker;
use solana_core::cluster_slots_service::cluster_slots::ClusterSlots;
use solana_core::consensus::heaviest_subtree_fork_choice::HeaviestSubtreeForkChoice;
use solana_core::consensus::progress_map::{ForkProgress, ProgressMap};
use solana_core::consensus::Tower;
use solana_core::consensus::tower_storage::FileTowerStorage;
use solana_core::repair::cluster_slot_state_verifier::{DuplicateConfirmedSlots, DuplicateSlotsTracker, EpochSlotsFrozenSlots};
use solana_core::repair::repair_service::OutstandingShredRepairs;
use solana_core::replay_stage::ReplayStage;
use solana_core::tvu::{Tvu, TvuConfig, TvuSockets};
use solana_core::unfrozen_gossip_verified_vote_hashes::UnfrozenGossipVerifiedVoteHashes;
use solana_ledger::blockstore::Blockstore;
use solana_ledger::leader_schedule_cache::LeaderScheduleCache;
use solana_rpc::max_slots::MaxSlots;
use solana_rpc::rpc_subscriptions::RpcSubscriptions;
use solana_runtime::accounts_background_service::AbsRequestSender;
use solana_runtime::bank_forks::BankForks;
use solana_runtime::commitment::BlockCommitmentCache;
use solana_runtime::prioritization_fee_cache::PrioritizationFeeCache;
use solana_sdk::account::{Account, ReadableAccount};
use solana_sdk::clock::Epoch;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::{system_program, system_transaction};
use solana_sdk::transaction::SanitizedTransaction;

fn test_tvu_exit(enable_wen_restart: bool) {
    solana_logger::setup();
    let leader = Node::new_localhost();
    let target1_keypair = Keypair::new();
    let target1 = Node::new_localhost_with_pubkey(&target1_keypair.pubkey());

    let starting_balance = 10_000;
    let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(starting_balance);

    let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis_config));

    let (turbine_quic_endpoint_sender, _turbine_quic_endpoint_receiver) =
        tokio::sync::mpsc::channel(/*capacity:*/ 128);
    let (_turbine_quic_endpoint_sender, turbine_quic_endpoint_receiver) = unbounded();
    let (repair_quic_endpoint_sender, _repair_quic_endpoint_receiver) =
        tokio::sync::mpsc::channel(/*buffer:*/ 128);
    //start cluster_info1
    let cluster_info1 = ClusterInfo::new(
        target1.info.clone(),
        target1_keypair.into(),
        SocketAddrSpace::Unspecified,
    );
    cluster_info1.insert_info(leader.info);
    let cref1 = Arc::new(cluster_info1);

    let (blockstore_path, _) = create_new_tmp_ledger!(&genesis_config);
    let BlockstoreSignals {
        blockstore,
        ledger_signal_receiver,
        ..
    } = Blockstore::open_with_signal(&blockstore_path, BlockstoreOptions::default())
        .expect("Expected to successfully open ledger");
    let blockstore = Arc::new(blockstore);
    let bank = bank_forks.read().unwrap().working_bank();
    create_transactions(&bank, 60000);
    let (exit, poh_recorder, poh_service, _entry_receiver) =
        create_test_recorder(bank.clone(), blockstore.clone(), None, None);
    let vote_keypair = Keypair::new();
    let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
    let block_commitment_cache = Arc::new(RwLock::new(BlockCommitmentCache::default()));
    let (retransmit_slots_sender, _retransmit_slots_receiver) = unbounded();
    let (_gossip_verified_vote_hash_sender, gossip_verified_vote_hash_receiver) = unbounded();
    let (_verified_vote_sender, verified_vote_receiver) = unbounded();
    let (replay_vote_sender, _replay_vote_receiver) = unbounded();
    let (_, gossip_confirmed_slots_receiver) = unbounded();
    let max_complete_transaction_status_slot = Arc::new(AtomicU64::default());
    let max_complete_rewards_slot = Arc::new(AtomicU64::default());
    let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
    let outstanding_repair_requests = Arc::<RwLock<OutstandingShredRepairs>>::default();
    let cluster_slots = Arc::new(ClusterSlots::default());
    let wen_restart_repair_slots = if enable_wen_restart {
        Some(Arc::new(RwLock::new(vec![])))
    } else {
        None
    };
    let tvu = Tvu::new(
        &vote_keypair.pubkey(),
        Arc::new(RwLock::new(vec![Arc::new(vote_keypair)])),
        &bank_forks,
        &cref1,
        {
            TvuSockets {
                repair: target1.sockets.repair,
                retransmit: target1.sockets.retransmit_sockets,
                fetch: target1.sockets.tvu,
                ancestor_hashes_requests: target1.sockets.ancestor_hashes_requests,
            }
        },
        blockstore,
        ledger_signal_receiver,
        &Arc::new(RpcSubscriptions::new_for_tests(
            exit.clone(),
            max_complete_transaction_status_slot,
            max_complete_rewards_slot,
            bank_forks.clone(),
            block_commitment_cache.clone(),
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks),
        )),
        &poh_recorder,
        Tower::default(),
        Arc::new(FileTowerStorage::default()),
        &leader_schedule_cache,
        exit.clone(),
        block_commitment_cache,
        Arc::<AtomicBool>::default(),
        None,
        None,
        None,
        None,
        Arc::<VoteTracker>::default(),
        retransmit_slots_sender,
        gossip_verified_vote_hash_receiver,
        verified_vote_receiver,
        replay_vote_sender,
        /*completed_data_sets_sender:*/ None,
        None,
        gossip_confirmed_slots_receiver,
        TvuConfig::default(),
        &Arc::new(MaxSlots::default()),
        None,
        None,
        AbsRequestSender::default(),
        None,
        Some(&Arc::new(ConnectionCache::new("connection_cache_test"))),
        &ignored_prioritization_fee_cache,
        BankingTracer::new_disabled(),
        turbine_quic_endpoint_sender,
        turbine_quic_endpoint_receiver,
        repair_quic_endpoint_sender,
        outstanding_repair_requests,
        cluster_slots,
        wen_restart_repair_slots,
    )
        .expect("assume success");

    exit.store(true, Ordering::Relaxed);
    tvu.join().unwrap();
    poh_service.join().unwrap();
}

fn create_accounts(num: usize) -> Vec<Keypair> {
    (0..num).into_par_iter().map(|_| Keypair::new()).collect()
}

fn create_funded_accounts(bank: &Bank, num: usize) -> Vec<Keypair> {
    assert!(
        num.is_power_of_two(),
        "must be power of 2 for parallel funding tree"
    );
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

fn create_transactions(bank: &Bank, num: usize) -> Vec<SanitizedTransaction> {
    let funded_accounts = create_funded_accounts(bank, 2 * num);
    funded_accounts
        .into_par_iter()
        .chunks(2)
        .map(|chunk| {
            let from = &chunk[0];
            let to = &chunk[1];
            system_transaction::transfer(from, &to.pubkey(), 1, bank.last_blockhash())
        })
        .map(SanitizedTransaction::from_transaction_for_tests)
        .collect()
}


fn criterion_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("my_group");
    group.sample_size(10); // Set sample size to 100
    group.bench_function("Example", |b| {

        b.iter(|| {
            test_tvu_exit(true)
        })
    });
    group.finish();
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);