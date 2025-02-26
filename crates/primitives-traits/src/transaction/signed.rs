//! API of a signed transaction.

use crate::{
    InMemorySize, MaybeCompact, MaybeSerde, MaybeSerdeBincodeCompat,
    crypto::secp256k1::{recover_signer, recover_signer_unchecked},
};
use alloc::{fmt, vec::Vec};
use alloy_consensus::{
    SignableTransaction,
    transaction::{PooledTransaction, Recovered},
};
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, B256, PrimitiveSignature as Signature, TxHash, keccak256};
use core::hash::Hash;

/// Helper trait that unifies all behaviour required by block to support full node operations.
pub trait FullSignedTx: SignedTransaction + MaybeCompact + MaybeSerdeBincodeCompat {}
impl<T> FullSignedTx for T where T: SignedTransaction + MaybeCompact + MaybeSerdeBincodeCompat {}

/// A signed transaction.
#[auto_impl::auto_impl(&, Arc)]
pub trait SignedTransaction:
    Send
    + Sync
    + Unpin
    + Clone
    + fmt::Debug
    + PartialEq
    + Eq
    + Hash
    + alloy_rlp::Encodable
    + alloy_rlp::Decodable
    + Encodable2718
    + Decodable2718
    + alloy_consensus::Transaction
    + MaybeSerde
    + InMemorySize
{
    /// Returns reference to transaction hash.
    fn tx_hash(&self) -> &TxHash;

    /// Returns reference to signature.
    fn signature(&self) -> &Signature;

    /// Returns whether this transaction type can be __broadcasted__ as full transaction over the
    /// network.
    ///
    /// Some transactions are not broadcastable as objects and only allowed to be broadcasted as
    /// hashes, e.g. because they missing context (e.g. blob sidecar).
    fn is_broadcastable_in_full(&self) -> bool {
        // EIP-4844 transactions are not broadcastable in full, only hashes are allowed.
        !self.is_eip4844()
    }

    /// Recover signer from signature and hash.
    ///
    /// Returns `None` if the transaction's signature is invalid following [EIP-2](https://eips.ethereum.org/EIPS/eip-2), see also `reth_primitives::transaction::recover_signer`.
    ///
    /// Note:
    ///
    /// This can fail for some early ethereum mainnet transactions pre EIP-2, use
    /// [`Self::recover_signer_unchecked`] if you want to recover the signer without ensuring that
    /// the signature has a low `s` value.
    fn recover_signer(&self) -> Result<Address, RecoveryError>;

    /// Recover signer from signature and hash.
    ///
    /// Returns an error if the transaction's signature is invalid.
    fn try_recover(&self) -> Result<Address, RecoveryError> {
        self.recover_signer().map_err(|_| RecoveryError)
    }

    /// Recover signer from signature and hash _without ensuring that the signature has a low `s`
    /// value_.
    ///
    /// Returns `None` if the transaction's signature is invalid, see also
    /// `reth_primitives::transaction::recover_signer_unchecked`.
    fn recover_signer_unchecked(&self) -> Result<Address, RecoveryError> {
        self.recover_signer_unchecked_with_buf(&mut Vec::new()).map_err(|_| RecoveryError)
    }

    /// Recover signer from signature and hash _without ensuring that the signature has a low `s`
    /// value_.
    ///
    /// Returns an error if the transaction's signature is invalid.
    fn try_recover_unchecked(&self) -> Result<Address, RecoveryError> {
        self.recover_signer_unchecked()
    }

    /// Same as [`Self::recover_signer_unchecked`] but receives a buffer to operate on. This is used
    /// during batch recovery to avoid allocating a new buffer for each transaction.
    fn recover_signer_unchecked_with_buf(
        &self,
        buf: &mut Vec<u8>,
    ) -> Result<Address, RecoveryError>;

    /// Calculate transaction hash, eip2728 transaction does not contain rlp header and start with
    /// tx type.
    fn recalculate_hash(&self) -> B256 {
        keccak256(self.encoded_2718())
    }

    /// Tries to recover signer and return [`Recovered`] by cloning the type.
    #[auto_impl(keep_default_for(&, Arc))]
    fn try_clone_into_recovered(&self) -> Result<Recovered<Self>, RecoveryError> {
        self.recover_signer().map(|signer| Recovered::new_unchecked(self.clone(), signer))
    }

    /// Tries to recover signer and return [`Recovered`].
    ///
    /// Returns `Err(Self)` if the transaction's signature is invalid, see also
    /// [`SignedTransaction::recover_signer`].
    #[auto_impl(keep_default_for(&, Arc))]
    fn try_into_recovered(self) -> Result<Recovered<Self>, Self> {
        match self.recover_signer() {
            Ok(signer) => Ok(Recovered::new_unchecked(self, signer)),
            Err(_) => Err(self),
        }
    }

    /// Consumes the type, recover signer and return [`Recovered`] _without
    /// ensuring that the signature has a low `s` value_ (EIP-2).
    ///
    /// Returns `None` if the transaction's signature is invalid.
    #[auto_impl(keep_default_for(&, Arc))]
    fn into_recovered_unchecked(self) -> Result<Recovered<Self>, RecoveryError> {
        self.recover_signer_unchecked().map(|signer| Recovered::new_unchecked(self, signer))
    }

    /// Returns the [`Recovered`] transaction with the given sender.
    ///
    /// Note: assumes the given signer is the signer of this transaction.
    #[auto_impl(keep_default_for(&, Arc))]
    fn with_signer(self, signer: Address) -> Recovered<Self> {
        Recovered::new_unchecked(self, signer)
    }
}

impl SignedTransaction for PooledTransaction {
    fn tx_hash(&self) -> &TxHash {
        match self {
            Self::Legacy(tx) => tx.hash(),
            Self::Eip2930(tx) => tx.hash(),
            Self::Eip1559(tx) => tx.hash(),
            Self::Eip7702(tx) => tx.hash(),
            Self::Eip4844(tx) => tx.hash(),
        }
    }

    fn signature(&self) -> &Signature {
        match self {
            Self::Legacy(tx) => tx.signature(),
            Self::Eip2930(tx) => tx.signature(),
            Self::Eip1559(tx) => tx.signature(),
            Self::Eip7702(tx) => tx.signature(),
            Self::Eip4844(tx) => tx.signature(),
        }
    }

    fn recover_signer(&self) -> Result<Address, RecoveryError> {
        let signature_hash = self.signature_hash();
        recover_signer(self.signature(), signature_hash)
    }

    fn recover_signer_unchecked_with_buf(
        &self,
        buf: &mut Vec<u8>,
    ) -> Result<Address, RecoveryError> {
        match self {
            Self::Legacy(tx) => tx.tx().encode_for_signing(buf),
            Self::Eip2930(tx) => tx.tx().encode_for_signing(buf),
            Self::Eip1559(tx) => tx.tx().encode_for_signing(buf),
            Self::Eip7702(tx) => tx.tx().encode_for_signing(buf),
            Self::Eip4844(tx) => tx.tx().encode_for_signing(buf),
        }
        let signature_hash = keccak256(buf);
        recover_signer_unchecked(self.signature(), signature_hash)
    }
}

#[cfg(feature = "op")]
impl SignedTransaction for op_alloy_consensus::OpPooledTransaction {
    fn tx_hash(&self) -> &TxHash {
        match self {
            Self::Legacy(tx) => tx.hash(),
            Self::Eip2930(tx) => tx.hash(),
            Self::Eip1559(tx) => tx.hash(),
            Self::Eip7702(tx) => tx.hash(),
        }
    }

    fn signature(&self) -> &Signature {
        match self {
            Self::Legacy(tx) => tx.signature(),
            Self::Eip2930(tx) => tx.signature(),
            Self::Eip1559(tx) => tx.signature(),
            Self::Eip7702(tx) => tx.signature(),
        }
    }

    fn recover_signer(&self) -> Result<Address, RecoveryError> {
        let signature_hash = self.signature_hash();
        recover_signer(self.signature(), signature_hash)
    }

    fn recover_signer_unchecked_with_buf(
        &self,
        buf: &mut Vec<u8>,
    ) -> Result<Address, RecoveryError> {
        match self {
            Self::Legacy(tx) => tx.tx().encode_for_signing(buf),
            Self::Eip2930(tx) => tx.tx().encode_for_signing(buf),
            Self::Eip1559(tx) => tx.tx().encode_for_signing(buf),
            Self::Eip7702(tx) => tx.tx().encode_for_signing(buf),
        }
        let signature_hash = keccak256(buf);
        recover_signer_unchecked(self.signature(), signature_hash)
    }
}

/// Opaque error type for sender recovery.
#[derive(Debug, Default, thiserror::Error)]
#[error("Failed to recover the signer")]
pub struct RecoveryError;
