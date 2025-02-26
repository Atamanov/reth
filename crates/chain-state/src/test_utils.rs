use core::marker::PhantomData;

use crate::{
    CanonStateNotification, CanonStateNotifications, CanonStateSubscriptions,
    in_memory::ExecutedBlockWithTrieUpdates,
};
use alloy_consensus::{
    EMPTY_ROOT_HASH, Header, SignableTransaction, Transaction as _, TxEip1559, TxReceipt,
};
use alloy_eips::{
    eip1559::{ETHEREUM_BLOCK_GAS_LIMIT_30M, INITIAL_BASE_FEE},
    eip7685::Requests,
};
use alloy_primitives::{Address, B256, BlockNumber, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use rand::{Rng, thread_rng};
use reth_chainspec::{ChainSpec, EthereumHardfork, MIN_TRANSACTION_GAS};
use reth_execution_types::{Chain, ExecutionOutcome};
use reth_primitives::{
    BlockBody, EthPrimitives, NodePrimitives, Receipt, Recovered, RecoveredBlock, SealedBlock,
    SealedHeader, Transaction, TransactionSigned, transaction::SignedTransaction,
};
use reth_primitives_traits::{
    Account,
    proofs::{calculate_receipt_root, calculate_transaction_root, calculate_withdrawals_root},
};
use reth_storage_api::NodePrimitivesProvider;
use reth_trie::{HashedPostState, root::state_root_unhashed, updates::TrieUpdates};
use revm_database::BundleState;
use revm_state::AccountInfo;
use std::{
    collections::HashMap,
    ops::Range,
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast::{self, Sender};

/// Functionality to build blocks for tests and help with assertions about
/// their execution.
#[derive(Debug)]
pub struct TestBlockBuilder<N: NodePrimitives = EthPrimitives> {
    /// The account that signs all the block's transactions.
    pub signer: Address,
    /// Private key for signing.
    pub signer_pk: PrivateKeySigner,
    /// Keeps track of signer's account info after execution, will be updated in
    /// methods related to block execution.
    pub signer_execute_account_info: AccountInfo,
    /// Keeps track of signer's nonce, will be updated in methods related
    /// to block execution.
    pub signer_build_account_info: AccountInfo,
    /// Chain spec of the blocks generated by this builder
    pub chain_spec: ChainSpec,
    _prims: PhantomData<N>,
}

impl<N: NodePrimitives> Default for TestBlockBuilder<N> {
    fn default() -> Self {
        let initial_account_info = AccountInfo::from_balance(U256::from(10).pow(U256::from(18)));
        let signer_pk = PrivateKeySigner::random();
        let signer = signer_pk.address();
        Self {
            chain_spec: ChainSpec::default(),
            signer,
            signer_pk,
            signer_execute_account_info: initial_account_info.clone(),
            signer_build_account_info: initial_account_info,
            _prims: PhantomData,
        }
    }
}

impl<N: NodePrimitives> TestBlockBuilder<N> {
    /// Signer pk setter.
    pub fn with_signer_pk(mut self, signer_pk: PrivateKeySigner) -> Self {
        self.signer = signer_pk.address();
        self.signer_pk = signer_pk;

        self
    }

    /// Chainspec setter.
    pub fn with_chain_spec(mut self, chain_spec: ChainSpec) -> Self {
        self.chain_spec = chain_spec;
        self
    }

    /// Gas cost of a single transaction generated by the block builder.
    pub fn single_tx_cost() -> U256 {
        U256::from(INITIAL_BASE_FEE * MIN_TRANSACTION_GAS)
    }

    /// Generates a random [`RecoveredBlock`].
    pub fn generate_random_block(
        &mut self,
        number: BlockNumber,
        parent_hash: B256,
    ) -> RecoveredBlock<reth_primitives::Block> {
        let mut rng = thread_rng();

        let mock_tx = |nonce: u64| -> Recovered<_> {
            let tx = Transaction::Eip1559(TxEip1559 {
                chain_id: self.chain_spec.chain.id(),
                nonce,
                gas_limit: MIN_TRANSACTION_GAS,
                to: Address::random().into(),
                max_fee_per_gas: INITIAL_BASE_FEE as u128,
                max_priority_fee_per_gas: 1,
                ..Default::default()
            });
            let signature_hash = tx.signature_hash();
            let signature = self.signer_pk.sign_hash_sync(&signature_hash).unwrap();

            TransactionSigned::new_unhashed(tx, signature).with_signer(self.signer)
        };

        let num_txs = rng.gen_range(0..5);
        let signer_balance_decrease = Self::single_tx_cost() * U256::from(num_txs);
        let transactions: Vec<Recovered<_>> = (0..num_txs)
            .map(|_| {
                let tx = mock_tx(self.signer_build_account_info.nonce);
                self.signer_build_account_info.nonce += 1;
                self.signer_build_account_info.balance -= signer_balance_decrease;
                tx
            })
            .collect();

        let receipts = transactions
            .iter()
            .enumerate()
            .map(|(idx, tx)| {
                Receipt {
                    tx_type: tx.tx_type(),
                    success: true,
                    cumulative_gas_used: (idx as u64 + 1) * MIN_TRANSACTION_GAS,
                    ..Default::default()
                }
                .into_with_bloom()
            })
            .collect::<Vec<_>>();

        let initial_signer_balance = U256::from(10).pow(U256::from(18));

        let header = Header {
            number,
            parent_hash,
            gas_used: transactions.len() as u64 * MIN_TRANSACTION_GAS,
            mix_hash: B256::random(),
            gas_limit: ETHEREUM_BLOCK_GAS_LIMIT_30M,
            base_fee_per_gas: Some(INITIAL_BASE_FEE),
            transactions_root: calculate_transaction_root(
                &transactions.clone().into_iter().map(|tx| tx.into_tx()).collect::<Vec<_>>(),
            ),
            receipts_root: calculate_receipt_root(&receipts),
            beneficiary: Address::random(),
            state_root: state_root_unhashed(HashMap::from([(
                self.signer,
                Account {
                    balance: initial_signer_balance - signer_balance_decrease,
                    nonce: num_txs,
                    ..Default::default()
                }
                .into_trie_account(EMPTY_ROOT_HASH),
            )])),
            // use the number as the timestamp so it is monotonically increasing
            timestamp: number +
                EthereumHardfork::Cancun.activation_timestamp(self.chain_spec.chain).unwrap(),
            withdrawals_root: Some(calculate_withdrawals_root(&[])),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(B256::random()),
            ..Default::default()
        };

        let block = SealedBlock::from_sealed_parts(
            SealedHeader::seal_slow(header),
            BlockBody {
                transactions: transactions.into_iter().map(|tx| tx.into_tx()).collect(),
                ommers: Vec::new(),
                withdrawals: Some(vec![].into()),
            },
        );

        RecoveredBlock::try_recover_sealed_with_senders(block, vec![self.signer; num_txs as usize])
            .unwrap()
    }

    /// Creates a fork chain with the given base block.
    pub fn create_fork(
        &mut self,
        base_block: &SealedBlock,
        length: u64,
    ) -> Vec<RecoveredBlock<reth_primitives::Block>> {
        let mut fork = Vec::with_capacity(length as usize);
        let mut parent = base_block.clone();

        for _ in 0..length {
            let block = self.generate_random_block(parent.number + 1, parent.hash());
            parent = block.clone_sealed_block();
            fork.push(block);
        }

        fork
    }

    /// Gets an [`ExecutedBlockWithTrieUpdates`] with [`BlockNumber`], receipts and parent hash.
    fn get_executed_block(
        &mut self,
        block_number: BlockNumber,
        receipts: Vec<Vec<Receipt>>,
        parent_hash: B256,
    ) -> ExecutedBlockWithTrieUpdates {
        let block_with_senders = self.generate_random_block(block_number, parent_hash);

        let (block, senders) = block_with_senders.split_sealed();
        ExecutedBlockWithTrieUpdates::new(
            Arc::new(RecoveredBlock::new_sealed(block, senders)),
            Arc::new(ExecutionOutcome::new(
                BundleState::default(),
                receipts,
                block_number,
                vec![Requests::default()],
            )),
            Arc::new(HashedPostState::default()),
            Arc::new(TrieUpdates::default()),
        )
    }

    /// Generates an [`ExecutedBlockWithTrieUpdates`] that includes the given receipts.
    pub fn get_executed_block_with_receipts(
        &mut self,
        receipts: Vec<Vec<Receipt>>,
        parent_hash: B256,
    ) -> ExecutedBlockWithTrieUpdates {
        let number = rand::thread_rng().r#gen::<u64>();
        self.get_executed_block(number, receipts, parent_hash)
    }

    /// Generates an [`ExecutedBlockWithTrieUpdates`] with the given [`BlockNumber`].
    pub fn get_executed_block_with_number(
        &mut self,
        block_number: BlockNumber,
        parent_hash: B256,
    ) -> ExecutedBlockWithTrieUpdates {
        self.get_executed_block(block_number, vec![vec![]], parent_hash)
    }

    /// Generates a range of executed blocks with ascending block numbers.
    pub fn get_executed_blocks(
        &mut self,
        range: Range<u64>,
    ) -> impl Iterator<Item = ExecutedBlockWithTrieUpdates> + '_ {
        let mut parent_hash = B256::default();
        range.map(move |number| {
            let current_parent_hash = parent_hash;
            let block = self.get_executed_block_with_number(number, current_parent_hash);
            parent_hash = block.recovered_block().hash();
            block
        })
    }

    /// Returns the execution outcome for a block created with this builder.
    /// In order to properly include the bundle state, the signer balance is
    /// updated.
    pub fn get_execution_outcome(
        &mut self,
        block: RecoveredBlock<reth_primitives::Block>,
    ) -> ExecutionOutcome {
        let receipts = block
            .body()
            .transactions
            .iter()
            .enumerate()
            .map(|(idx, tx)| Receipt {
                tx_type: tx.tx_type(),
                success: true,
                cumulative_gas_used: (idx as u64 + 1) * MIN_TRANSACTION_GAS,
                ..Default::default()
            })
            .collect::<Vec<_>>();

        let mut bundle_state_builder = BundleState::builder(block.number..=block.number);

        for tx in &block.body().transactions {
            self.signer_execute_account_info.balance -= Self::single_tx_cost();
            bundle_state_builder = bundle_state_builder.state_present_account_info(
                self.signer,
                AccountInfo {
                    nonce: tx.nonce(),
                    balance: self.signer_execute_account_info.balance,
                    ..Default::default()
                },
            );
        }

        let execution_outcome = ExecutionOutcome::new(
            bundle_state_builder.build(),
            vec![vec![]],
            block.number,
            Vec::new(),
        );

        execution_outcome.with_receipts(vec![receipts])
    }
}

impl TestBlockBuilder {
    /// Creates a `TestBlockBuilder` configured for Ethereum primitives.
    pub fn eth() -> Self {
        Self::default()
    }
}
/// A test `ChainEventSubscriptions`
#[derive(Clone, Debug, Default)]
pub struct TestCanonStateSubscriptions<N: NodePrimitives = reth_primitives::EthPrimitives> {
    canon_notif_tx: Arc<Mutex<Vec<Sender<CanonStateNotification<N>>>>>,
}

impl TestCanonStateSubscriptions {
    /// Adds new block commit to the queue that can be consumed with
    /// [`TestCanonStateSubscriptions::subscribe_to_canonical_state`]
    pub fn add_next_commit(&self, new: Arc<Chain>) {
        let event = CanonStateNotification::Commit { new };
        self.canon_notif_tx.lock().as_mut().unwrap().retain(|tx| tx.send(event.clone()).is_ok())
    }

    /// Adds reorg to the queue that can be consumed with
    /// [`TestCanonStateSubscriptions::subscribe_to_canonical_state`]
    pub fn add_next_reorg(&self, old: Arc<Chain>, new: Arc<Chain>) {
        let event = CanonStateNotification::Reorg { old, new };
        self.canon_notif_tx.lock().as_mut().unwrap().retain(|tx| tx.send(event.clone()).is_ok())
    }
}

impl NodePrimitivesProvider for TestCanonStateSubscriptions {
    type Primitives = EthPrimitives;
}

impl CanonStateSubscriptions for TestCanonStateSubscriptions {
    /// Sets up a broadcast channel with a buffer size of 100.
    fn subscribe_to_canonical_state(&self) -> CanonStateNotifications {
        let (canon_notif_tx, canon_notif_rx) = broadcast::channel(100);
        self.canon_notif_tx.lock().as_mut().unwrap().push(canon_notif_tx);

        canon_notif_rx
    }
}
