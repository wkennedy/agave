use crossbeam_channel::{unbounded, Receiver, Sender};
use solana_runtime::bank::Bank;
use solana_runtime::bank_forks::BankForks;
use solana_sdk::clock::Slot;
use std::sync::Arc;

// Define messages for interacting with BankForks
enum BankMessage {
    Root(Sender<Slot>),
    RootBank(Sender<Arc<Bank>>),
    Insert(Bank),
    FrozenBanks(Sender<std::collections::HashMap<Slot, Arc<Bank>>>),
    Get(Sender<Option<Arc<Bank>>>, Slot),
    ActiveBankSlots(Sender<Vec<Slot>>)
    // Add more message types as needed
}

// BankManager handles the BankForks and processes messages
struct BankManager {
    bank_forks: BankForks,
    receiver: Receiver<BankMessage>,
}

impl BankManager {
    fn new(bank_forks: BankForks, receiver: Receiver<BankMessage>) -> Self {
        Self {
            bank_forks,
            receiver,
        }
    }

    fn run(&mut self) {
        while let Ok(msg) = self.receiver.recv() {
            match msg {
                BankMessage::RootBank(response_sender) => {
                    response_sender.send(self.bank_forks.root_bank()).unwrap();
                }
                BankMessage::Insert(bank) => {
                    self.bank_forks.insert(bank);
                }
                BankMessage::FrozenBanks(response_sender) => {
                    response_sender.send(self.bank_forks.frozen_banks()).unwrap();
                }
                BankMessage::Get(response_sender, slot) => {
                    response_sender.send(self.bank_forks.get(slot)).unwrap();
                }
                BankMessage::ActiveBankSlots(response_sender) => {
                    response_sender.send(self.bank_forks.active_bank_slots()).unwrap();
                }
                BankMessage::Root(response_sender) => {
                    response_sender.send(self.bank_forks.root()).unwrap();
                }
            }
        }
    }
}

// Client to interact with BankManager
pub struct BankForkClient {
    sender: Sender<BankMessage>,
}

#[allow(dead_code)]
impl BankForkClient {
    pub fn new(sender: Sender<BankMessage>) -> Self {
        Self { sender }
    }

    pub fn root(&self) -> Slot {
        let (tx, rx) = unbounded();
        self.sender.send(BankMessage::Root(tx)).unwrap();
        rx.recv().unwrap()
    }

    pub fn root_bank(&self) -> Arc<Bank> {
        let (tx, rx) = unbounded();
        self.sender.send(BankMessage::RootBank(tx)).unwrap();
        rx.recv().unwrap()
    }

    pub fn insert(&self, bank: Bank) {
        self.sender.send(BankMessage::Insert(bank)).unwrap();
    }

    pub fn frozen_banks(&self) -> std::collections::HashMap<Slot, Arc<Bank>> {
        let (tx, rx) = unbounded();
        self.sender.send(BankMessage::FrozenBanks(tx)).unwrap();
        rx.recv().unwrap()
    }

    pub fn get(&self, slot: Slot) -> Option<Arc<Bank>> {
        let (tx, rx) = unbounded();
        self.sender.send(BankMessage::Get(tx, slot)).unwrap();
        rx.recv().unwrap()
    }

    pub fn active_bank_slots(&self) -> Vec<Slot> {
        let (tx, rx) = unbounded();
        self.sender.send(BankMessage::ActiveBankSlots(tx)).unwrap();
        rx.recv().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_ledger::genesis_utils::{create_genesis_config, GenesisConfigInfo},
        solana_runtime::bank_forks::BankForks,
        solana_sdk::{pubkey::Pubkey, signature::Signer},
        std::thread
    };

    #[test]
    fn test_get_balance() {
        let (sender, receiver) = unbounded();

        //See bank_forks.rs - fn test_bank_forks_active_banks()
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new(bank);
        let bank_client = BankForkClient::new(sender);

        thread::spawn(move || {
            let mut manager = BankManager::new(bank_forks, receiver);
            manager.run();
        });

        let bank0 = bank_client.get(0).unwrap();
        let child_bank = Bank::new_from_parent(bank0, &Pubkey::default(), 1);
        bank_client.insert(child_bank);

        assert_eq!(bank_client.active_bank_slots(), vec![1]);
    }
}