//! Best transaction and filter testing

use reth_transaction_pool::{BestTransactions, TransactionPool, noop::NoopTransactionPool};

#[test]
fn test_best_transactions() {
    let noop = NoopTransactionPool::default();
    let mut best =
        noop.best_transactions().filter_transactions(|_| true).without_blobs().without_updates();
    assert!(best.next().is_none());
}
