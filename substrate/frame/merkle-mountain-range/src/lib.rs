// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! # Merkle Mountain Range
//!
//! ## Overview
//!
//! Details on Merkle Mountain Ranges (MMRs) can be found here:
//! <https://github.com/mimblewimble/grin/blob/master/doc/mmr.md>
//!
//! The MMR pallet constructs an MMR from leaf data obtained on every block from
//! `LeafDataProvider`. MMR nodes are stored both in:
//! - on-chain storage - hashes only; not full leaf content;
//! - off-chain storage - via Indexing API we push full leaf content (and all internal nodes as
//! well) to the Off-chain DB, so that the data is available for Off-chain workers.
//! Hashing used for MMR is configurable independently from the rest of the runtime (i.e. not using
//! `frame_system::Hashing`) so something compatible with external chains can be used (like
//! Keccak256 for Ethereum compatibility).
//!
//! Depending on the usage context (off-chain vs on-chain) the pallet is able to:
//! - verify MMR leaf proofs (on-chain)
//! - generate leaf proofs (off-chain)
//!
//! See [primitives::Compact] documentation for how you can optimize proof size for leafs that are
//! composed from multiple elements.
//!
//! ## What for?
//!
//! Primary use case for this pallet is to generate MMR root hashes, that can latter on be used by
//! BEEFY protocol (see <https://github.com/paritytech/grandpa-bridge-gadget>).
//! MMR root hashes along with BEEFY will make it possible to build Super Light Clients (SLC) of
//! Substrate-based chains. The SLC will be able to follow finality and can be shown proofs of more
//! details that happened on the source chain.
//! In that case the chain which contains the pallet generates the Root Hashes and Proofs, which
//! are then presented to another chain acting as a light client which can verify them.
//!
//! Secondary use case is to archive historical data, but still be able to retrieve them on-demand
//! if needed. For instance if parent block hashes are stored in the MMR it's possible at any point
//! in time to provide an MMR proof about some past block hash, while this data can be safely pruned
//! from on-chain storage.
//!
//! NOTE This pallet is experimental and not proven to work in production.
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::vec::Vec;
use log;

use frame::prelude::*;

pub use sp_mmr_primitives::{
	self as primitives, utils, utils::NodesUtils, AncestryProof, Error, FullLeaf, LeafDataProvider,
	LeafIndex, LeafProof, NodeIndex, OnNewRoot,
};

pub use pallet::*;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
mod default_weights;
mod mmr;
#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

/// The most common use case for MMRs is to store historical block hashes,
/// so that any point in time in the future we can receive a proof about some past
/// blocks without using excessive on-chain storage.
///
/// Hence we implement the [LeafDataProvider] for [ParentNumberAndHash] which is a
/// crate-local wrapper over [frame_system::Pallet]. Since the current block hash
/// is not available (since the block is not finished yet),
/// we use the `parent_hash` here along with parent block number.
pub struct ParentNumberAndHash<T: Config> {
	_phantom: PhantomData<T>,
}

impl<T: Config> LeafDataProvider for ParentNumberAndHash<T> {
	type LeafData = (BlockNumberFor<T>, <T as frame_system::Config>::Hash);

	fn leaf_data() -> Self::LeafData {
		(
			frame_system::Pallet::<T>::block_number().saturating_sub(One::one()),
			frame_system::Pallet::<T>::parent_hash(),
		)
	}
}

/// Block hash provider for a given block number.
pub trait BlockHashProvider<BlockNumber, BlockHash> {
	fn block_hash(block_number: BlockNumber) -> BlockHash;
}

/// Default implementation of BlockHashProvider using frame_system.
pub struct DefaultBlockHashProvider<T: Config> {
	_phantom: core::marker::PhantomData<T>,
}

impl<T: Config> BlockHashProvider<BlockNumberFor<T>, T::Hash> for DefaultBlockHashProvider<T> {
	fn block_hash(block_number: BlockNumberFor<T>) -> T::Hash {
		frame_system::Pallet::<T>::block_hash(block_number)
	}
}

pub trait WeightInfo {
	fn on_initialize(peaks: u32) -> Weight;
}

/// This trait decoples dependencies on pallets needed for benchmarking.
#[cfg(feature = "runtime-benchmarks")]
pub trait BenchmarkHelper {
	fn setup();
}

#[cfg(feature = "runtime-benchmarks")]
impl BenchmarkHelper for () {
	fn setup() {}
}

/// An MMR specific to the pallet.
type ModuleMmr<StorageType, T, I> = mmr::Mmr<StorageType, T, I, LeafOf<T, I>>;

/// Leaf data.
type LeafOf<T, I> = <<T as Config<I>>::LeafData as LeafDataProvider>::LeafData;

/// Hashing used for the pallet.
pub(crate) type HashingOf<T, I> = <T as Config<I>>::Hashing;
/// Hash type used for the pallet.
pub(crate) type HashOf<T, I> = <<T as Config<I>>::Hashing as Hash>::Output;

#[frame::pallet]
pub mod pallet {
	use super::*;

	#[pallet::pallet]
	pub struct Pallet<T, I = ()>(PhantomData<(T, I)>);

	/// This pallet's configuration trait
	#[pallet::config]
	pub trait Config<I: 'static = ()>: frame_system::Config {
		/// Prefix for elements stored in the Off-chain DB via Indexing API.
		///
		/// Each node of the MMR is inserted both on-chain and off-chain via Indexing API.
		/// The former does not store full leaf content, just its compact version (hash),
		/// and some of the inner mmr nodes might be pruned from on-chain storage.
		/// The latter will contain all the entries in their full form.
		///
		/// Each node is stored in the Off-chain DB under key derived from the
		/// [`Self::INDEXING_PREFIX`] and its in-tree index (MMR position).
		const INDEXING_PREFIX: &'static [u8];

		/// A hasher type for MMR.
		///
		/// To construct trie nodes that result in merging (bagging) two peaks, depending on the
		/// node kind we take either:
		/// - The node (hash) itself if it's an inner node.
		/// - The hash of SCALE-encoding of the leaf data if it's a leaf node.
		///
		/// Then we create a tuple of these two hashes, SCALE-encode it (concatenate) and
		/// hash, to obtain a new MMR inner node - the new peak.
		type Hashing: Hash;

		/// Data stored in the leaf nodes.
		///
		/// The [LeafData](primitives::LeafDataProvider) is responsible for returning the entire
		/// leaf data that will be inserted to the MMR.
		/// [LeafDataProvider](primitives::LeafDataProvider)s can be composed into tuples to put
		/// multiple elements into the tree. In such a case it might be worth using
		/// [primitives::Compact] to make MMR proof for one element of the tuple leaner.
		///
		/// Note that the leaf at each block MUST be unique. You may want to include a block hash or
		/// block number as an easiest way to ensure that.
		/// Also note that the leaf added by each block is expected to only reference data coming
		/// from ancestor blocks (leaves are saved offchain using `(pos, parent_hash)` key to be
		/// fork-resistant, as such conflicts could only happen on 1-block deep forks, which means
		/// two forks with identical line of ancestors compete to write the same offchain key, but
		/// that's fine as long as leaves only contain data coming from ancestors - conflicting
		/// writes are identical).
		type LeafData: LeafDataProvider;

		/// A hook to act on the new MMR root.
		///
		/// For some applications it might be beneficial to make the MMR root available externally
		/// apart from having it in the storage. For instance you might output it in the header
		/// digest (see [`frame_system::Pallet::deposit_log`]) to make it available for Light
		/// Clients. Hook complexity should be `O(1)`.
		type OnNewRoot: OnNewRoot<HashOf<Self, I>>;

		/// Block hash provider for a given block number.
		type BlockHashProvider: BlockHashProvider<
			BlockNumberFor<Self>,
			<Self as frame_system::Config>::Hash,
		>;

		/// Weights for this pallet.
		type WeightInfo: WeightInfo;

		/// Benchmarking setup helper trait.
		#[cfg(feature = "runtime-benchmarks")]
		type BenchmarkHelper: BenchmarkHelper;
	}

	/// Latest MMR Root hash.
	#[pallet::storage]
	pub type RootHash<T: Config<I>, I: 'static = ()> = StorageValue<_, HashOf<T, I>, ValueQuery>;

	/// Current size of the MMR (number of leaves).
	#[pallet::storage]
	#[pallet::getter(fn mmr_leaves)]
	pub type NumberOfLeaves<T, I = ()> = StorageValue<_, LeafIndex, ValueQuery>;

	/// Hashes of the nodes in the MMR.
	///
	/// Note this collection only contains MMR peaks, the inner nodes (and leaves)
	/// are pruned and only stored in the Offchain DB.
	#[pallet::storage]
	#[pallet::getter(fn mmr_peak)]
	pub type Nodes<T: Config<I>, I: 'static = ()> =
		StorageMap<_, Identity, NodeIndex, HashOf<T, I>, OptionQuery>;

	/// Helper flag used in the runtime benchmarks for the initial setup.
	#[cfg(feature = "runtime-benchmarks")]
	#[pallet::storage]
	pub type UseLocalStorage<T, I = ()> = StorageValue<_, bool, ValueQuery>;

	#[pallet::hooks]
	impl<T: Config<I>, I: 'static> Hooks<BlockNumberFor<T>> for Pallet<T, I> {
		fn on_initialize(_n: BlockNumberFor<T>) -> Weight {
			let leaves = NumberOfLeaves::<T, I>::get();
			let peaks_before = NodesUtils::new(leaves).number_of_peaks();
			let data = T::LeafData::leaf_data();

			// append new leaf to MMR
			let mut mmr: ModuleMmr<mmr::storage::RuntimeStorage, T, I> = mmr::Mmr::new(leaves);
			// MMR push never fails, but better safe than sorry.
			if mmr.push(data).is_none() {
				log::error!(target: "runtime::mmr", "MMR push failed");
				return T::WeightInfo::on_initialize(peaks_before as u32)
			}
			// Update the size, `mmr.finalize()` should also never fail.
			let (leaves, root) = match mmr.finalize() {
				Ok((leaves, root)) => (leaves, root),
				Err(e) => {
					log::error!(target: "runtime::mmr", "MMR finalize failed: {:?}", e);
					return T::WeightInfo::on_initialize(peaks_before as u32)
				},
			};
			<T::OnNewRoot as OnNewRoot<_>>::on_new_root(&root);

			NumberOfLeaves::<T, I>::put(leaves);
			RootHash::<T, I>::put(root);

			let peaks_after = NodesUtils::new(leaves).number_of_peaks();

			T::WeightInfo::on_initialize(peaks_before.max(peaks_after) as u32)
		}
	}
}

/// Stateless MMR proof verification for batch of leaves.
///
/// This function can be used to verify received MMR [primitives::LeafProof] (`proof`)
/// for given leaves set (`leaves`) against a known MMR root hash (`root`).
/// Note, the leaves should be sorted such that corresponding leaves and leaf indices have the
/// same position in both the `leaves` vector and the `leaf_indices` vector contained in the
/// [primitives::LeafProof].
pub fn verify_leaves_proof<H, L>(
	root: H::Output,
	leaves: Vec<mmr::Node<H, L>>,
	proof: LeafProof<H::Output>,
) -> Result<(), Error>
where
	H: Hash,
	L: FullLeaf,
{
	let is_valid = mmr::verify_leaves_proof::<H, L>(root, leaves, proof)?;
	if is_valid {
		Ok(())
	} else {
		Err(Error::Verify.log_debug(("The proof is incorrect.", root)))
	}
}

/// Stateless ancestry proof verification.
pub fn verify_ancestry_proof<H, L>(
	root: H::Output,
	ancestry_proof: AncestryProof<H::Output>,
) -> Result<H::Output, Error>
where
	H: Hash,
	L: FullLeaf,
{
	mmr::verify_ancestry_proof::<H, L>(root, ancestry_proof)
		.map_err(|_| Error::Verify.log_debug(("The ancestry proof is incorrect.", root)))
}

impl<T: Config<I>, I: 'static> Pallet<T, I> {
	/// Build offchain key from `parent_hash` of block that originally added node `pos` to MMR.
	///
	/// This combination makes the offchain (key,value) entry resilient to chain forks.
	fn node_temp_offchain_key(
		pos: NodeIndex,
		parent_hash: <T as frame_system::Config>::Hash,
	) -> Vec<u8> {
		NodesUtils::node_temp_offchain_key::<HeaderFor<T>>(&T::INDEXING_PREFIX, pos, parent_hash)
	}

	/// Build canonical offchain key for node `pos` in MMR.
	///
	/// Used for nodes added by now finalized blocks.
	/// Never read keys using `node_canon_offchain_key` unless you sure that
	/// there's no `node_offchain_key` key in the storage.
	fn node_canon_offchain_key(pos: NodeIndex) -> Vec<u8> {
		NodesUtils::node_canon_offchain_key(&T::INDEXING_PREFIX, pos)
	}

	/// Provide the parent number for the block that added `leaf_index` to the MMR.
	fn leaf_index_to_parent_block_num(leaf_index: LeafIndex) -> BlockNumberFor<T> {
		// leaves are zero-indexed and were added one per block since pallet activation,
		// while block numbers are one-indexed, so block number that added `leaf_idx` is:
		// `block_num = block_num_when_pallet_activated + leaf_idx + 1`
		// `block_num = (current_block_num - leaves_count) + leaf_idx + 1`
		// `parent_block_num = current_block_num - leaves_count + leaf_idx`.
		<frame_system::Pallet<T>>::block_number()
			.saturating_sub(Self::mmr_leaves().saturated_into())
			.saturating_add(leaf_index.saturated_into())
	}

	/// Convert a block number into a leaf index.
	fn block_num_to_leaf_index(block_num: BlockNumberFor<T>) -> Result<LeafIndex, Error>
	where
		T: frame_system::Config,
	{
		let first_mmr_block = utils::first_mmr_block_num::<HeaderFor<T>>(
			<frame_system::Pallet<T>>::block_number(),
			NumberOfLeaves::<T, I>::get(),
		)?;

		utils::block_num_to_leaf_index::<HeaderFor<T>>(block_num, first_mmr_block)
	}

	/// Convert a block number into a leaf index.
	pub fn block_num_to_leaf_count(block_num: BlockNumberFor<T>) -> Result<LeafIndex, Error>
	where
		T: frame_system::Config,
	{
		let leaf_index = Self::block_num_to_leaf_index(block_num)?;
		Ok(leaf_index.saturating_add(1))
	}

	/// Generate an MMR proof for the given `block_numbers`.
	/// If `best_known_block_number = Some(n)`, this generates a historical proof for
	/// the chain with head at height `n`.
	/// Else it generates a proof for the MMR at the current block height.
	///
	/// Note this method can only be used from an off-chain context
	/// (Offchain Worker or Runtime API call), since it requires
	/// all the leaves to be present.
	/// It may return an error or panic if used incorrectly.
	pub fn generate_proof(
		block_numbers: Vec<BlockNumberFor<T>>,
		best_known_block_number: Option<BlockNumberFor<T>>,
	) -> Result<(Vec<LeafOf<T, I>>, LeafProof<HashOf<T, I>>), Error> {
		// check whether best_known_block_number provided, else use current best block
		let best_known_block_number =
			best_known_block_number.unwrap_or_else(|| <frame_system::Pallet<T>>::block_number());

		let leaf_count = Self::block_num_to_leaf_count(best_known_block_number)?;

		// we need to translate the block_numbers into leaf indices.
		let leaf_indices = block_numbers
			.iter()
			.map(|block_num| -> Result<LeafIndex, Error> {
				Self::block_num_to_leaf_index(*block_num)
			})
			.collect::<Result<Vec<LeafIndex>, _>>()?;

		let mmr: ModuleMmr<mmr::storage::OffchainStorage, T, I> = mmr::Mmr::new(leaf_count);
		mmr.generate_proof(leaf_indices)
	}

	/// Verify MMR proof for given `leaves`.
	///
	/// This method is safe to use within the runtime code.
	/// It will return `Ok(())` if the proof is valid
	/// and an `Err(..)` if MMR is inconsistent (some leaves are missing)
	/// or the proof is invalid.
	pub fn verify_leaves(
		leaves: Vec<LeafOf<T, I>>,
		proof: LeafProof<HashOf<T, I>>,
	) -> Result<(), Error> {
		if proof.leaf_count > NumberOfLeaves::<T, I>::get() ||
			proof.leaf_count == 0 ||
			proof.items.len().saturating_add(leaves.len()) as u64 > proof.leaf_count
		{
			return Err(
				Error::Verify.log_debug("The proof has incorrect number of leaves or proof items.")
			)
		}

		let mmr: ModuleMmr<mmr::storage::OffchainStorage, T, I> = mmr::Mmr::new(proof.leaf_count);
		let is_valid = mmr.verify_leaves_proof(leaves, proof)?;
		if is_valid {
			Ok(())
		} else {
			Err(Error::Verify.log_debug("The proof is incorrect."))
		}
	}

	pub fn generate_ancestry_proof(
		prev_block_number: BlockNumberFor<T>,
		best_known_block_number: Option<BlockNumberFor<T>>,
	) -> Result<AncestryProof<HashOf<T, I>>, Error> {
		// check whether best_known_block_number provided, else use current best block
		let best_known_block_number =
			best_known_block_number.unwrap_or_else(|| <frame_system::Pallet<T>>::block_number());

		let leaf_count = Self::block_num_to_leaf_count(best_known_block_number)?;
		let prev_leaf_count = Self::block_num_to_leaf_count(prev_block_number)?;

		let mmr: ModuleMmr<mmr::storage::OffchainStorage, T, I> = mmr::Mmr::new(leaf_count);
		mmr.generate_ancestry_proof(prev_leaf_count)
	}

	#[cfg(feature = "runtime-benchmarks")]
	pub fn generate_mock_ancestry_proof() -> Result<AncestryProof<HashOf<T, I>>, Error> {
		let leaf_count = Self::block_num_to_leaf_count(<frame_system::Pallet<T>>::block_number())?;
		let mmr: ModuleMmr<mmr::storage::OffchainStorage, T, I> = mmr::Mmr::new(leaf_count);
		mmr.generate_mock_ancestry_proof()
	}

	pub fn verify_ancestry_proof(
		root: HashOf<T, I>,
		ancestry_proof: AncestryProof<HashOf<T, I>>,
	) -> Result<HashOf<T, I>, Error> {
		verify_ancestry_proof::<HashingOf<T, I>, LeafOf<T, I>>(root, ancestry_proof)
	}

	/// Return the on-chain MMR root hash.
	pub fn mmr_root() -> HashOf<T, I> {
		RootHash::<T, I>::get()
	}
}
