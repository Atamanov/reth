//! Database access for `eth_` block RPC methods. Loads block and receipt data w.r.t. network.

use super::{LoadPendingBlock, LoadReceipt, SpawnBlocking};
use crate::{
    node::RpcNodeCoreExt, EthApiTypes, FromEthApiError, FullEthApiTypes, RpcBlock, RpcNodeCore,
    RpcReceipt,
};
use alloy_eips::BlockId;
use alloy_primitives::Sealable;
use alloy_rlp::Encodable;
use alloy_rpc_types_eth::{Block, BlockTransactions, Header, Index};
use futures::Future;
use reth_node_api::BlockBody;
use reth_primitives::{RecoveredBlock, SealedBlock};
use reth_provider::{
    BlockIdReader, BlockReader, BlockReaderIdExt, ProviderHeader, ProviderReceipt,
};
use reth_rpc_types_compat::block::from_block;
use revm_primitives::U256;
use std::sync::Arc;

/// Result type of the fetched block receipts.
pub type BlockReceiptsResult<N, E> = Result<Option<Vec<RpcReceipt<N>>>, E>;
/// Result type of the fetched block and its receipts.
pub type BlockAndReceiptsResult<Eth> = Result<
    Option<(
        SealedBlock<<<Eth as RpcNodeCore>::Provider as BlockReader>::Block>,
        Arc<Vec<ProviderReceipt<<Eth as RpcNodeCore>::Provider>>>,
    )>,
    <Eth as EthApiTypes>::Error,
>;

/// Block related functions for the [`EthApiServer`](crate::EthApiServer) trait in the
/// `eth_` namespace.
pub trait EthBlocks: LoadBlock {
    /// Returns the block header for the given block id.
    #[expect(clippy::type_complexity)]
    fn rpc_block_header(
        &self,
        block_id: BlockId,
    ) -> impl Future<Output = Result<Option<Header<ProviderHeader<Self::Provider>>>, Self::Error>> + Send
    where
        Self: FullEthApiTypes,
    {
        async move { Ok(self.rpc_block(block_id, false).await?.map(|block| block.header)) }
    }

    /// Returns the populated rpc block object for the given block id.
    ///
    /// If `full` is true, the block object will contain all transaction objects, otherwise it will
    /// only contain the transaction hashes.
    fn rpc_block(
        &self,
        block_id: BlockId,
        full: bool,
    ) -> impl Future<Output = Result<Option<RpcBlock<Self::NetworkTypes>>, Self::Error>> + Send
    where
        Self: FullEthApiTypes,
    {
        async move {
            let Some(block) = self.block_with_senders(block_id).await? else { return Ok(None) };

            let block = from_block((*block).clone(), full.into(), self.tx_resp_builder())?;
            Ok(Some(block))
        }
    }

    /// Returns the number transactions in the given block.
    ///
    /// Returns `None` if the block does not exist
    fn block_transaction_count(
        &self,
        block_id: BlockId,
    ) -> impl Future<Output = Result<Option<usize>, Self::Error>> + Send {
        async move {
            if block_id.is_pending() {
                // Pending block can be fetched directly without need for caching
                return Ok(self
                    .provider()
                    .pending_block()
                    .map_err(Self::Error::from_eth_err)?
                    .map(|block| block.body().transactions().len()))
            }

            let block_hash = match self
                .provider()
                .block_hash_for_id(block_id)
                .map_err(Self::Error::from_eth_err)?
            {
                Some(block_hash) => block_hash,
                None => return Ok(None),
            };

            Ok(self
                .cache()
                .get_recovered_block(block_hash)
                .await
                .map_err(Self::Error::from_eth_err)?
                .map(|b| b.body().transaction_count()))
        }
    }

    /// Helper function for `eth_getBlockReceipts`.
    ///
    /// Returns all transaction receipts in block, or `None` if block wasn't found.
    #[allow(clippy::type_complexity)]
    fn block_receipts(
        &self,
        block_id: BlockId,
    ) -> impl Future<Output = BlockReceiptsResult<Self::NetworkTypes, Self::Error>> + Send
    where
        Self: LoadReceipt;

    /// Helper method that loads a block and all its receipts.
    #[allow(clippy::type_complexity)]
    fn load_block_and_receipts(
        &self,
        block_id: BlockId,
    ) -> impl Future<Output = BlockAndReceiptsResult<Self>> + Send
    where
        Self: LoadReceipt,
    {
        async move {
            if block_id.is_pending() {
                // First, try to get the pending block from the provider, in case we already
                // received the actual pending block from the CL.
                if let Some((block, receipts)) = self
                    .provider()
                    .pending_block_and_receipts()
                    .map_err(Self::Error::from_eth_err)?
                {
                    return Ok(Some((block, Arc::new(receipts))));
                }

                // If no pending block from provider, build the pending block locally.
                if let Some((block, receipts)) = self.local_pending_block().await? {
                    return Ok(Some((block.into_sealed_block(), Arc::new(receipts))));
                }
            }

            if let Some(block_hash) =
                self.provider().block_hash_for_id(block_id).map_err(Self::Error::from_eth_err)?
            {
                return self
                    .cache()
                    .get_block_and_receipts(block_hash)
                    .await
                    .map_err(Self::Error::from_eth_err)
                    .map(|b| b.map(|(b, r)| (b.clone_sealed_block(), r)))
            }

            Ok(None)
        }
    }

    /// Returns uncle headers of given block.
    ///
    /// Returns an empty vec if there are none.
    #[expect(clippy::type_complexity)]
    fn ommers(
        &self,
        block_id: BlockId,
    ) -> Result<Option<Vec<ProviderHeader<Self::Provider>>>, Self::Error> {
        self.provider().ommers_by_id(block_id).map_err(Self::Error::from_eth_err)
    }

    /// Returns uncle block at given index in given block.
    ///
    /// Returns `None` if index out of range.
    fn ommer_by_block_and_index(
        &self,
        block_id: BlockId,
        index: Index,
    ) -> impl Future<Output = Result<Option<RpcBlock<Self::NetworkTypes>>, Self::Error>> + Send
    {
        async move {
            let uncles = if block_id.is_pending() {
                // Pending block can be fetched directly without need for caching
                self.provider()
                    .pending_block()
                    .map_err(Self::Error::from_eth_err)?
                    .and_then(|block| block.body().ommers().map(|o| o.to_vec()))
            } else {
                self.provider().ommers_by_id(block_id).map_err(Self::Error::from_eth_err)?
            }
            .unwrap_or_default();

            Ok(uncles.into_iter().nth(index.into()).map(|header| {
                let block = alloy_consensus::Block::<alloy_consensus::TxEnvelope, _>::uncle(header);
                let size = U256::from(block.length());
                Block {
                    uncles: vec![],
                    header: Header::from_consensus(block.header.seal_slow(), None, Some(size)),
                    transactions: BlockTransactions::Uncle,
                    withdrawals: None,
                }
            }))
        }
    }
}

/// Loads a block from database.
///
/// Behaviour shared by several `eth_` RPC methods, not exclusive to `eth_` blocks RPC methods.
pub trait LoadBlock: LoadPendingBlock + SpawnBlocking + RpcNodeCoreExt {
    /// Returns the block object for the given block id.
    #[expect(clippy::type_complexity)]
    fn block_with_senders(
        &self,
        block_id: BlockId,
    ) -> impl Future<
        Output = Result<
            Option<Arc<RecoveredBlock<<Self::Provider as BlockReader>::Block>>>,
            Self::Error,
        >,
    > + Send {
        async move {
            if block_id.is_pending() {
                // Pending block can be fetched directly without need for caching
                if let Some(pending_block) = self
                    .provider()
                    .pending_block_with_senders()
                    .map_err(Self::Error::from_eth_err)?
                {
                    return Ok(Some(Arc::new(pending_block)));
                }

                // If no pending block from provider, try to get local pending block
                return match self.local_pending_block().await? {
                    Some((block, _)) => Ok(Some(Arc::new(block))),
                    None => Ok(None),
                };
            }

            let block_hash = match self
                .provider()
                .block_hash_for_id(block_id)
                .map_err(Self::Error::from_eth_err)?
            {
                Some(block_hash) => block_hash,
                None => return Ok(None),
            };

            self.cache().get_recovered_block(block_hash).await.map_err(Self::Error::from_eth_err)
        }
    }
}
