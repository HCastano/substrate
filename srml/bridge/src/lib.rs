// Copyright 2017-2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! # Bridge Module
//!
//! This will eventually have some useful documentation.
//! For now though, enjoy this cow's wisdom.
//!
//!```ignore
//!________________________________________
//! / You are only young once, but you can  \
//! \ stay immature indefinitely.           /
//! ----------------------------------------
//!        \   ^__^
//!         \  (oo)\_______
//!            (__)\       )\/\
//!                ||----w |
//!                ||     ||
//!```

// Ensure we're `no_std` when compiling for Wasm.
#![cfg_attr(not(feature = "std"), no_std)]

mod error;
mod storage_proof;

use crate::storage_proof::StorageProofChecker;
use codec::{Encode, Decode};
use sr_primitives::traits::{Header, Member};
use support::{
	decl_error, decl_module, decl_storage,
	Parameter,
};
use system::{ensure_signed};

#[derive(Encode, Decode, Clone, PartialEq)]
#[cfg_attr(feature = "std", derive(Debug))]
pub struct BridgeInfo<T: Trait> {
	last_finalized_block_number: T::BlockNumber,
	last_finalized_block_hash: T::Hash,
	last_finalized_state_root: T::Hash,
	current_validator_set: Vec<(T::ValidatorId, ValidatorWeight)>,
}

impl<T: Trait> BridgeInfo<T> {
	pub fn new(
			block_number: &T::BlockNumber,
			block_hash: &T::Hash,
			state_root: &T::Hash,
			validator_set: Vec<(T::ValidatorId, ValidatorWeight)>,
		) -> Self
	{
		// I don't like how this is done, should come back to...
		BridgeInfo {
			last_finalized_block_number: *block_number,
			last_finalized_block_hash: *block_hash,
			last_finalized_state_root: *state_root,
			current_validator_set: validator_set,
		}
	}
}

type BridgeId = u64;
type ValidatorWeight = u64;

pub trait Trait: system::Trait {
	/// A stable ID for a validator.
	type ValidatorId: Member + Parameter;
}

decl_storage! {
	trait Store for Module<T: Trait> as Bridge {
		/// The number of current bridges managed by the module.
		pub NumBridges get(num_bridges) config(): BridgeId;

		/// Maps a bridge id to a bridge struct. Allows a single
		/// `bridge` module to manage multiple bridges.
		pub TrackedBridges get(tracked_bridges): map BridgeId => Option<BridgeInfo<T>>;
	}
}

decl_module! {
	pub struct Module<T: Trait> for enum Call where origin: T::Origin {
		// TODO: Change proof type to StorageProof once #3834 is merged
		fn initialize_bridge(
			origin,
			block_header: T::Header,
			validator_set: Vec<(T::ValidatorId, ValidatorWeight)>,
			validator_set_proof: Vec<Vec<u8>>,
		) {
			// NOTE: Will want to make this a governance issued call
			let _sender = ensure_signed(origin)?;

			let block_number = block_header.number();
			let block_hash = block_header.hash();
			let state_root = block_header.state_root();

			Self::check_validator_set_proof(state_root, validator_set_proof, &validator_set)?;

			let bridge_info = BridgeInfo::new(block_number, &block_hash, state_root, validator_set);

			let new_bridge_id = NumBridges::get() + 1;
			<TrackedBridges<T>>::insert(new_bridge_id, bridge_info);

			NumBridges::put(new_bridge_id);
		}

		fn submit_finalized_headers(origin) {
			let _sender = ensure_signed(origin)?;
		}
	}
}

decl_error! {
	// Error for the Bridge module
	pub enum Error {
		InvalidStorageProof,
		InvalidValidatorSetProof,
		ValidatorSetMismatch,
		AncestorNotFound,
	}
}

impl<T: Trait> Module<T> {
	fn check_validator_set_proof(
		state_root: &T::Hash,
		proof: Vec<Vec<u8>>,
		validator_set: &Vec<(T::ValidatorId, ValidatorWeight)>,
	) -> std::result::Result<(), Error> {

		// pub const GRANDPA_AUTHORITIES_KEY: &'static [u8] = b":grandpa_authorities";
		// pub type AuthorityList = Vec<(AuthorityId, AuthorityWeight)>;
		let checker = <StorageProofChecker<<T::Hashing as sr_primitives::traits::Hash>::Hasher>>::new(
			*state_root,
			proof.clone()
		).unwrap();

		// By encoding the given set we should have an easy way to compare
		// with the stuff we get out of storage via `read_value`
		let encoded_validator_set = validator_set.encode();
		let actual_validator_set = checker.read_value(b":grandpa_authorities").unwrap().unwrap();

		if encoded_validator_set == actual_validator_set {
			Ok(())
		} else {
			Err(Error::ValidatorSetMismatch)
		}
	}
}

// A naive way to check whether a `child` header is an ancestor
// of an `ancestor` header. For this it requires a proof which
// is a header chain, This could be updated to use something like
// Log2 Ancestors (#2053) in the future.
fn verify_ancestry<H>(proof: Vec<H>, ancestor: H, child: H) -> std::result::Result<(), Error>
where
	H: Header
{
	let mut curr_header = &proof[0];
	if curr_header.hash() != child.hash() {
		return Err(Error::AncestorNotFound);
	}

	let mut parent_hash = curr_header.parent_hash();

	// If we find that the header's parent hash matches our ancestor's hash we're done
	for i in 1..proof.len() {
		curr_header = &proof[i];

		// Need to check that blocks are actually related
		if curr_header.hash() != *parent_hash {
			break;
		}

		parent_hash = curr_header.parent_hash();
		if *parent_hash == ancestor.hash() {
			return Ok(())
		}
	}

	Err(Error::AncestorNotFound)
}

#[cfg(test)]
mod tests {
	use super::*;

	use primitives::{Blake2Hasher, H256};
	use sr_primitives::{
		Perbill, traits::{Header as HeaderT, IdentityLookup}, testing::Header, generic::Digest,
	};
	use support::{assert_ok, assert_err, impl_outer_origin, parameter_types};

	// NOTE: What's this for?
	impl_outer_origin! {
		pub enum Origin for Test {}
	}

	#[derive(Clone, PartialEq, Eq, Debug)]
	pub struct Test;

	type _System = system::Module<Test>;
	type MockBridge = Module<Test>;

	// TODO: Figure out what I actually need from here
	parameter_types! {
		pub const BlockHashCount: u64 = 250;
		pub const MaximumBlockWeight: u32 = 1024;
		pub const MaximumBlockLength: u32 = 2 * 1024;
		pub const MinimumPeriod: u64 = 5;
		pub const AvailableBlockRatio: Perbill = Perbill::one();
	}

	type DummyValidatorId = u64;

	impl system::Trait for Test {
		type Origin = Origin;
		type Index = u64;
		type BlockNumber = u64;
		type Call = ();
		type Hash = H256;
		type Hashing = sr_primitives::traits::BlakeTwo256;
		type AccountId = DummyValidatorId;
		type Lookup = IdentityLookup<Self::AccountId>;
		type Header = Header;
		type Event = ();
		type BlockHashCount = ();
		type MaximumBlockWeight = ();
		type AvailableBlockRatio = ();
		type MaximumBlockLength = ();
		type Version = ();
	}

	impl Trait for Test {
		type ValidatorId = DummyValidatorId;
	}

	fn new_test_ext() -> runtime_io::TestExternalities {
		let mut t = system::GenesisConfig::default().build_storage::<Test>().unwrap();
		GenesisConfig {
			num_bridges: 0,
		}.assimilate_storage(&mut t).unwrap();
		t.into()
	}

	#[test]
	fn it_works_for_default_value() {
		new_test_ext().execute_with(|| {
			assert_eq!(MockBridge::num_bridges(), 0);
		});
	}

	fn create_dummy_validator_proof(validator_set: Vec<(DummyValidatorId, ValidatorWeight)>) -> (H256, Vec<Vec<u8>>) {
		use state_machine::{prove_read, backend::{Backend, InMemory}};

		let encoded_set = validator_set.encode();

		// construct storage proof
		let backend = <InMemory<Blake2Hasher>>::from(vec![
			(None, b":grandpa_authorities".to_vec(), Some(encoded_set)),
		]);
		let root = backend.storage_root(std::iter::empty()).0;

		// Generates a storage read proof
		let proof = prove_read(backend, &[&b":grandpa_authorities"[..]]).unwrap();

		(root, proof)
	}

	#[test]
	fn it_can_validate_validator_sets() {
		let validators = vec![(1, 1), (2, 1), (3, 1)];
		let (root, proof) = create_dummy_validator_proof(validators.clone());

		assert_ok!(MockBridge::check_validator_set_proof(&root, proof, &validators));
	}

	#[test]
	fn it_rejects_invalid_validator_sets() {
		let validators = vec![(1, 1), (2, 1), (3, 1)];
		let (root, proof) = create_dummy_validator_proof(validators.clone());

		let invalid_validators = vec![(3, 1), (2, 1), (1, 1)];
		assert_err!(
			MockBridge::check_validator_set_proof(&root, proof, &invalid_validators),
			Error::ValidatorSetMismatch
		);
	}

	#[test]
	fn it_creates_a_new_bridge() {
		let validators = vec![(1, 1), (2, 1), (3, 1)];
		let (root, proof) = create_dummy_validator_proof(validators.clone());

		let test_header = Header {
			parent_hash: H256::default(),
			number: 42,
			state_root: root,
			extrinsics_root: H256::default(),
			digest: Digest::default(),
		};
		let test_hash = test_header.hash();

		new_test_ext().execute_with(|| {
			assert_eq!(MockBridge::num_bridges(), 0);
			dbg!(&test_header);
			assert_ok!(
				MockBridge::initialize_bridge(
					Origin::signed(1),
					test_header,
					validators.clone(),
					proof,
			));

			assert_eq!(
				MockBridge::tracked_bridges(1),
				Some(BridgeInfo {
					last_finalized_block_number: 42,
					last_finalized_block_hash: test_hash,
					last_finalized_state_root: root,
					current_validator_set: validators.clone(),
				}));

			assert_eq!(MockBridge::num_bridges(), 1);
		});
	}

	fn get_related_block_headers() -> (Header, Header, Header) {
		let grandparent = Header {
			parent_hash: H256::default(),
			number: 1,
			state_root: H256::default(),
			extrinsics_root: H256::default(),
			digest: Digest::default(),
		};

		let parent = Header {
			parent_hash: grandparent.hash(),
			number: grandparent.number() + 1,
			state_root: H256::default(),
			extrinsics_root: H256::default(),
			digest: Digest::default(),
		};

		let child = Header {
			parent_hash: parent.hash(),
			number: parent.number() + 1,
			state_root: H256::default(),
			extrinsics_root: H256::default(),
			digest: Digest::default(),
		};

		(grandparent, parent, child)
	}

	#[test]
	fn check_that_child_is_ancestor_of_grandparent() {
		let (grandparent, parent, child) = get_related_block_headers();

		let mut proof = Vec::new();
		proof.push(child.clone());
		proof.push(parent);
		proof.push(grandparent.clone());

		assert_ok!(verify_ancestry(proof, grandparent, child));
	}

	#[test]
	fn check_that_child_ancestor_is_not_correct() {
		let (grandparent, parent, child) = get_related_block_headers();

		let mut proof = Vec::new();
		proof.push(child.clone());
		proof.push(parent);
		proof.push(grandparent.clone());

		let fake_grandparent = Header {
			parent_hash: H256::from_slice(&[1u8; 32]),
			number: 42,
			state_root: H256::default(),
			extrinsics_root: H256::default(),
			digest: Digest::default(),
		};

		assert_err!(
			verify_ancestry(proof, fake_grandparent, child),
			Error::AncestorNotFound
		);
	}

	#[test]
	fn checker_fails_if_given_invalid_proof() {
		let (grandparent, parent, child) = get_related_block_headers();
		let fake_ancestor = Header {
			parent_hash: H256::from_slice(&[1u8; 32]),
			number: 42,
			state_root: H256::default(),
			extrinsics_root: H256::default(),
			digest: Digest::default(),
		};

		let mut invalid_proof = Vec::new();
		invalid_proof.push(child.clone());
		invalid_proof.push(fake_ancestor);
		invalid_proof.push(parent);
		invalid_proof.push(grandparent.clone());

		assert_err!(
			verify_ancestry(invalid_proof, grandparent, child),
			Error::AncestorNotFound
		);
	}
}
