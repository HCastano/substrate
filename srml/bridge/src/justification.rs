// Copyright 2018-2019 Parity Technologies (UK) Ltd.
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

use client::{CallExecutor, Client};
use client::backend::Backend;
use client::error::Error as ClientError;
use codec::{Encode, Decode};
use grandpa::voter_set::VoterSet;
use grandpa::{Error as GrandpaError};

// Might be able to get this from primitives re-export
use rstd::collections::{
	btree_map::BTreeMap,
	btree_set::BTreeSet,
};

use sr_primitives::app_crypto::RuntimeAppPublic;
use sr_primitives::generic::BlockId;
use sr_primitives::traits::{NumberFor, Block as BlockT, Header as HeaderT};
use primitives::{H256, Blake2Hasher};

use fg_primitives::{AuthorityId, RoundNumber, SetId as SetIdNumber, AuthoritySignature};

// Should I make this a part of fg_primitives?
use fg::{Commit, Error, Message};

/// A GRANDPA justification for block finality, it includes a commit message and
/// an ancestry proof including all headers routing all precommit target blocks
/// to the commit target block. Due to the current voting strategy the precommit
/// targets should be the same as the commit target, since honest voters don't
/// vote past authority set change blocks.
///
/// This is meant to be stored in the db and passed around the network to other
/// nodes, and are used by syncing nodes to prove authority set handoffs.
#[derive(Encode, Decode)]
pub struct GrandpaJustification<Block: BlockT> {
	round: u64,
	pub(crate) commit: Commit<Block>,
	votes_ancestries: Vec<Block::Header>,
}

impl<Block: BlockT<Hash=H256>> GrandpaJustification<Block> {
	/// Create a GRANDPA justification from the given commit. This method
	/// assumes the commit is valid and well-formed.
	pub(crate) fn from_commit<B, E, RA>(
		client: &Client<B, E, Block, RA>,
		round: u64,
		commit: Commit<Block>,
	) -> Result<GrandpaJustification<Block>, Error> where
		B: Backend<Block, Blake2Hasher>,
		E: CallExecutor<Block, Blake2Hasher> + Send + Sync,
		RA: Send + Sync,
	{
		// Can't use this HashSet
		let mut votes_ancestries_hashes = BTreeSet::new();
		let mut votes_ancestries = Vec::new();

		let error = || {
			let msg = "invalid precommits for target commit".to_string();
			Err(Error::Client(ClientError::BadJustification(msg)))
		};

		for signed in commit.precommits.iter() {
			let mut current_hash = signed.precommit.target_hash.clone();
			loop {
				if current_hash == commit.target_hash { break; }

				match client.header(&BlockId::Hash(current_hash))? {
					Some(current_header) => {
						if *current_header.number() <= commit.target_number {
							return error();
						}

						let parent_hash = current_header.parent_hash().clone();
						if votes_ancestries_hashes.insert(current_hash) {
							votes_ancestries.push(current_header);
						}
						current_hash = parent_hash;
					},
					_ => return error(),
				}
			}
		}

		Ok(GrandpaJustification { round, commit, votes_ancestries })
	}

	/// Decode a GRANDPA justification and validate the commit and the votes'
	/// ancestry proofs finalize the given block.
	pub(crate) fn decode_and_verify_finalizes(
		encoded: &[u8],
		finalized_target: (Block::Hash, NumberFor<Block>),
		set_id: u64,
		voters: &VoterSet<AuthorityId>,
	) -> Result<GrandpaJustification<Block>, ClientError> where
		NumberFor<Block>: grandpa::BlockNumberOps,
	{

		let justification = GrandpaJustification::<Block>::decode(&mut &*encoded)
			.map_err(|_| ClientError::JustificationDecode)?;

		if (justification.commit.target_hash, justification.commit.target_number) != finalized_target {
			let msg = "invalid commit target in grandpa justification".to_string();
			Err(ClientError::BadJustification(msg))
		} else {
			justification.verify(set_id, voters).map(|_| justification)
		}
	}

	/// Validate the commit and the votes' ancestry proofs.
	pub(crate) fn verify(&self, set_id: u64, voters: &VoterSet<AuthorityId>) -> Result<(), ClientError>
	where
		NumberFor<Block>: grandpa::BlockNumberOps,
	{
		use grandpa::Chain;

		let ancestry_chain = AncestryChain::<Block>::new(&self.votes_ancestries);

		match grandpa::validate_commit(
			&self.commit,
			voters,
			&ancestry_chain,
		) {
			Ok(ref result) if result.ghost().is_some() => {},
			_ => {
				let msg = "invalid commit in grandpa justification".to_string();
				return Err(ClientError::BadJustification(msg));
			}
		}

		// Jim says he would skip the stuff with `visited_hashes`
		let mut visited_hashes = BTreeSet::new();
		for signed in self.commit.precommits.iter() {
			// NOTE: Rip this out, use sr_io primitives instead
			if let Err(_) = check_message_sig::<Block>(
				&grandpa::Message::Precommit(signed.precommit.clone()),
				&signed.id,
				&signed.signature,
				self.round,
				set_id,
			) {
				return Err(ClientError::BadJustification(
					"invalid signature for precommit in grandpa justification".to_string()).into());
			}

			if self.commit.target_hash == signed.precommit.target_hash {
				continue;
			}

			match ancestry_chain.ancestry(self.commit.target_hash, signed.precommit.target_hash) {
				Ok(route) => {
					// ancestry starts from parent hash but the precommit target hash has been visited
					visited_hashes.insert(signed.precommit.target_hash);
					for hash in route {
						visited_hashes.insert(hash);
					}
				},
				_ => {
					return Err(ClientError::BadJustification(
						"invalid precommit ancestry proof in grandpa justification".to_string()).into());
				},
			}
		}

		let ancestry_hashes = self.votes_ancestries
			.iter()
			.map(|h: &Block::Header| h.hash())
			.collect();

		if visited_hashes != ancestry_hashes {
			return Err(ClientError::BadJustification(
				"invalid precommit ancestries in grandpa justification with unused headers".to_string()).into());
		}

		Ok(())
	}
}

use core::cmp::{Ord, Ordering};

#[derive(Eq)]
struct BlockHashKey<Block: BlockT>(Block::Hash);

impl<Block: BlockT> BlockHashKey<Block> {
	fn new(hash: Block::Hash) -> Self {
		Self(hash)
	}
}

impl<Block: BlockT> Ord for BlockHashKey<Block> {
	fn cmp(&self, other: &Self) -> Ordering {
		self.0.as_ref().cmp(other.0.as_ref())
	}
}

impl<Block: BlockT> PartialOrd for BlockHashKey<Block> {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		Some(self.0.as_ref().cmp(other.0.as_ref()))
	}
}

impl<Block: BlockT> PartialEq for BlockHashKey<Block> {
	fn eq(&self, other: &Self) -> bool {
		self.0.as_ref() == other.0.as_ref()
	}
}

/// A utility trait implementing `grandpa::Chain` using a given set of headers.
/// This is useful when validating commits, using the given set of headers to
/// verify a valid ancestry route to the target commit block.
// Since keys in a BTreeMap need to implement `Ord` we can't use Block::Hash directly.
// We need to turn the Hash into a slice of u8, which does implement Ord.
struct AncestryChain<Block: BlockT> {
	ancestry: BTreeMap<BlockHashKey<Block>, Block::Header>,
}

impl<Block: BlockT> AncestryChain<Block> {
	fn new(ancestry: &[Block::Header]) -> AncestryChain<Block> {
		let ancestry: BTreeMap<_, _> = ancestry
			.iter()
			.cloned()
			.map(|h: Block::Header| (BlockHashKey::new(h.hash()), h))
			.collect();

		AncestryChain { ancestry }
	}
}

impl<Block: BlockT> grandpa::Chain<Block::Hash, NumberFor<Block>> for AncestryChain<Block> where
	NumberFor<Block>: grandpa::BlockNumberOps
{
	fn ancestry(&self, base: Block::Hash, block: Block::Hash) -> Result<Vec<Block::Hash>, GrandpaError> {
		let mut route = Vec::new();
		let mut current_hash = block;
		loop {
			if current_hash == base { break; }

			let key = BlockHashKey::new(current_hash);
			match self.ancestry.get(&key) {
				Some(current_header) => {
					current_hash = *current_header.parent_hash();
					route.push(current_hash);
				},
				_ => return Err(GrandpaError::NotDescendent),
			}
		}
		route.pop(); // remove the base

		Ok(route)
	}

	fn best_chain_containing(&self, _block: Block::Hash) -> Option<(Block::Hash, NumberFor<Block>)> {
		None
	}
}

pub(crate) fn localized_payload<E: Encode>(round: RoundNumber, set_id: SetIdNumber, message: &E) -> Vec<u8> {
	(message, round, set_id).encode()
}

// NOTE: Stolen from `communication/mod.rs`
// check a message.
fn check_message_sig<Block: BlockT>(
	message: &Message<Block>,
	id: &AuthorityId,
	signature: &AuthoritySignature,
	round: RoundNumber,
	set_id: SetIdNumber,
) -> Result<(), ()> {
	let as_public = id.clone();
	let encoded_raw = localized_payload(round, set_id, message);
	// Since `app::Public` implements `RuntimeAppPublic` we can call `verify()`
	if as_public.verify(&encoded_raw, signature) {
		Ok(())
	} else {
		// debug!(target: "afg", "Bad signature on message from {:?}", id);
		Err(())
	}
}
