// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

/// This module contains drivers and types for all burn chains we support.
use std::collections::HashMap;
use std::convert::TryFrom;
use std::default::Default;
use std::error;
use std::fmt;
use std::io;
use std::marker::PhantomData;

use rusqlite::Error as sqlite_error;

use address::AddressHashMode;
use chainstate::burn::operations::leader_block_commit::OUTPUTS_PER_COMMIT;
use chainstate::burn::operations::BlockstackOperationType;
use chainstate::burn::operations::Error as op_error;
use chainstate::burn::operations::LeaderKeyRegisterOp;
use chainstate::burn::ConsensusHash;
use chainstate::stacks::StacksPublicKey;
use core::*;
use net::neighbors::MAX_NEIGHBOR_BLOCK_DELAY;
use util::db::Error as db_error;
use util::hash::Hash160;
use util::secp256k1::MessageSignature;

use crate::types::chainstate::PoxId;
use crate::types::chainstate::StacksAddress;
use crate::types::chainstate::{BlockHeaderHash, BurnchainHeaderHash, StacksBlockId};
use crate::types::proof::TrieHash;

pub mod burnchain;
pub mod db;
/// Stacks events parser used to construct the L1 hyperchain operations.
///
/// The events module processes an event stream from a Layer-1 Stacks
/// node (provided by an indexer) and produces `BurnchainTransaction`
/// and `BurnchainBlock` types. These types are fed into the sortition
/// db (again, by the indexer) in order to prepare the rest of the
/// hyperchain node to download and validate the corresponding
/// hyperchain blocks.
pub mod events;
pub mod indexer;

#[derive(Serialize, Deserialize)]
pub struct Txid(pub [u8; 32]);
impl_array_newtype!(Txid, u8, 32);
impl_array_hexstring_fmt!(Txid);
impl_byte_array_newtype!(Txid, u8, 32);
pub const TXID_ENCODED_SIZE: u32 = 32;

pub const MAGIC_BYTES_LENGTH: usize = 2;

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct MagicBytes([u8; MAGIC_BYTES_LENGTH]);
impl_array_newtype!(MagicBytes, u8, MAGIC_BYTES_LENGTH);
impl MagicBytes {
    pub fn default() -> MagicBytes {
        BLOCKSTACK_MAGIC_MAINNET
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BitcoinNetworkType {
    Mainnet,
    Testnet,
    Regtest,
}

pub const BLOCKSTACK_MAGIC_MAINNET: MagicBytes = MagicBytes([105, 100]); // 'id'

#[derive(Debug, PartialEq, Clone)]
pub struct BurnchainParameters {
    chain_name: String,
    network_name: String,
    network_id: u32,
    stable_confirmations: u32,
    consensus_hash_lifetime: u32,
    pub first_block_height: u64,
    pub first_block_hash: BurnchainHeaderHash,
    pub first_block_timestamp: u32,
    pub initial_reward_start_block: u64,
}

impl BurnchainParameters {
    pub fn from_params(chain: &str, network: &str) -> Option<BurnchainParameters> {
        match (chain, network) {
            ("bitcoin", "mainnet") => Some(BurnchainParameters::bitcoin_mainnet()),
            ("bitcoin", "testnet") => Some(BurnchainParameters::bitcoin_testnet()),
            ("bitcoin", "regtest") => Some(BurnchainParameters::bitcoin_regtest()),
            _ => None,
        }
    }

    pub fn bitcoin_mainnet() -> BurnchainParameters {
        BurnchainParameters {
            chain_name: "bitcoin".to_string(),
            network_name: "mainnet".into(),
            network_id: 0,
            stable_confirmations: 7,
            consensus_hash_lifetime: 24,
            first_block_height: BITCOIN_MAINNET_FIRST_BLOCK_HEIGHT,
            first_block_hash: BurnchainHeaderHash::from_hex(BITCOIN_MAINNET_FIRST_BLOCK_HASH)
                .unwrap(),
            first_block_timestamp: BITCOIN_MAINNET_FIRST_BLOCK_TIMESTAMP,
            initial_reward_start_block: BITCOIN_MAINNET_INITIAL_REWARD_START_BLOCK,
        }
    }

    pub fn bitcoin_testnet() -> BurnchainParameters {
        BurnchainParameters {
            chain_name: "bitcoin".to_string(),
            network_name: "testnet".into(),
            network_id: 1,
            stable_confirmations: 7,
            consensus_hash_lifetime: 24,
            first_block_height: BITCOIN_TESTNET_FIRST_BLOCK_HEIGHT,
            first_block_hash: BurnchainHeaderHash::from_hex(BITCOIN_TESTNET_FIRST_BLOCK_HASH)
                .unwrap(),
            first_block_timestamp: BITCOIN_TESTNET_FIRST_BLOCK_TIMESTAMP,
            initial_reward_start_block: BITCOIN_TESTNET_FIRST_BLOCK_HEIGHT - 10_000,
        }
    }

    pub fn bitcoin_regtest() -> BurnchainParameters {
        BurnchainParameters {
            chain_name: "bitcoin".to_string(),
            network_name: "regtest".into(),
            network_id: 2,
            stable_confirmations: 1,
            consensus_hash_lifetime: 24,
            first_block_height: BITCOIN_REGTEST_FIRST_BLOCK_HEIGHT,
            first_block_hash: BurnchainHeaderHash::from_hex(BITCOIN_REGTEST_FIRST_BLOCK_HASH)
                .unwrap(),
            first_block_timestamp: BITCOIN_REGTEST_FIRST_BLOCK_TIMESTAMP,
            initial_reward_start_block: BITCOIN_REGTEST_FIRST_BLOCK_HEIGHT,
        }
    }

    pub fn is_testnet(network_id: u32) -> bool {
        if network_id == 0 {
            false
        } else {
            true
        }
    }
}

pub trait PublicKey: Clone + fmt::Debug + serde::Serialize + serde::de::DeserializeOwned {
    fn to_bytes(&self) -> Vec<u8>;
    fn verify(&self, data_hash: &[u8], sig: &MessageSignature) -> Result<bool, &'static str>;
}

pub trait PrivateKey: Clone + fmt::Debug + serde::Serialize + serde::de::DeserializeOwned {
    fn to_bytes(&self) -> Vec<u8>;
    fn sign(&self, data_hash: &[u8]) -> Result<MessageSignature, &'static str>;
}

pub trait Address: Clone + fmt::Debug + fmt::Display {
    fn to_bytes(&self) -> Vec<u8>;
    fn from_string(from: &str) -> Option<Self>
    where
        Self: Sized;
    fn is_burn(&self) -> bool;
}

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct BurnchainSigner {
    pub hash_mode: AddressHashMode,
    pub num_sigs: usize,
    pub public_keys: Vec<StacksPublicKey>,
}

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct BurnchainRecipient {
    pub address: StacksAddress,
    pub amount: u64,
}

#[derive(Debug, PartialEq, Clone)]
/// This is the inner type of the Layer-1 Stacks event,
/// containing any operation specific data.
pub enum StacksHyperOpType {
    BlockCommit { subnet_block_hash: BlockHeaderHash },
}

#[derive(Debug, PartialEq, Clone)]
/// These operations are derived from a Layer-1 Stacks chain,
/// parsed from the `stacks-node` events API.
pub struct StacksHyperOp {
    pub txid: Txid,
    pub in_block: StacksBlockId,
    pub opcode: u8,
    pub event_index: u32,
    pub event: StacksHyperOpType,
}

#[derive(Debug, PartialEq, Clone)]
/// Enum for wrapping Layer-1 operation providers for hyperchains
pub enum BurnchainTransaction {
    StacksBase(StacksHyperOp),
}

impl BurnchainTransaction {
    pub fn txid(&self) -> Txid {
        match *self {
            BurnchainTransaction::StacksBase(ref tx) => tx.txid.clone(),
        }
    }

    pub fn vtxindex(&self) -> u32 {
        match *self {
            BurnchainTransaction::StacksBase(ref tx) => tx.event_index,
        }
    }

    pub fn opcode(&self) -> u8 {
        match *self {
            BurnchainTransaction::StacksBase(ref tx) => tx.opcode,
        }
    }

    pub fn get_burn_amount(&self) -> u64 {
        0
    }
}

#[derive(Debug, PartialEq, Clone)]
/// Represents a layer-1 Stacks block with the Hyperchain
/// relevant information parsed into th `ops` vector.
pub struct StacksHyperBlock {
    pub current_block: StacksBlockId,
    pub parent_block: StacksBlockId,
    pub block_height: u64,
    pub ops: Vec<StacksHyperOp>,
}

#[derive(Debug, PartialEq, Clone)]
/// Enum for wrapping Layer-1 blocks for hyperchains
pub enum BurnchainBlock {
    StacksHyperBlock(StacksHyperBlock),
}

#[derive(Debug, PartialEq, Clone)]
pub struct BurnchainBlockHeader {
    pub block_height: u64,
    pub block_hash: BurnchainHeaderHash,
    pub parent_block_hash: BurnchainHeaderHash,
    pub num_txs: u64,
    pub timestamp: u64,
}

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct Burnchain {
    pub peer_version: u32,
    pub network_id: u32,
    pub chain_name: String,
    pub network_name: String,
    pub working_dir: String,
    pub consensus_hash_lifetime: u32,
    pub stable_confirmations: u32,
    pub first_block_height: u64,
    pub first_block_hash: BurnchainHeaderHash,
    pub first_block_timestamp: u32,
    pub pox_constants: PoxConstants,
    pub initial_reward_start_block: u64,
}

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct PoxConstants {
    /// the length (in burn blocks) of the reward cycle
    pub reward_cycle_length: u32,
    /// the length (in burn blocks) of the prepare phase
    pub prepare_length: u32,
    /// the number of confirmations a PoX anchor block must
    ///  receive in order to become the anchor. must be at least > prepare_length/2
    pub anchor_threshold: u32,
    /// fraction of liquid STX that must vote to reject PoX for
    /// it to revert to PoB in the next reward cycle
    pub pox_rejection_fraction: u64,
    /// percentage of liquid STX that must participate for PoX
    ///  to occur
    pub pox_participation_threshold_pct: u64,
    /// last+1 block height of sunset phase
    pub sunset_end: u64,
    /// first block height of sunset phase
    pub sunset_start: u64,
    _shadow: PhantomData<()>,
}

impl PoxConstants {
    pub fn new(
        reward_cycle_length: u32,
        prepare_length: u32,
        anchor_threshold: u32,
        pox_rejection_fraction: u64,
        pox_participation_threshold_pct: u64,
        sunset_start: u64,
        sunset_end: u64,
    ) -> PoxConstants {
        assert!(anchor_threshold > (prepare_length / 2));
        assert!(prepare_length < reward_cycle_length);
        assert!(sunset_start <= sunset_end);

        PoxConstants {
            reward_cycle_length,
            prepare_length,
            anchor_threshold,
            pox_rejection_fraction,
            pox_participation_threshold_pct,
            sunset_start,
            sunset_end,
            _shadow: PhantomData,
        }
    }
    #[cfg(test)]
    pub fn test_default() -> PoxConstants {
        // 20 reward slots; 10 prepare-phase slots
        PoxConstants::new(10, 5, 3, 25, 5, 5000, 10000)
    }

    pub fn reward_slots(&self) -> u32 {
        (self.reward_cycle_length - self.prepare_length) * (OUTPUTS_PER_COMMIT as u32)
    }

    /// is participating_ustx enough to engage in PoX in the next reward cycle?
    pub fn enough_participation(&self, participating_ustx: u128, liquid_ustx: u128) -> bool {
        participating_ustx
            .checked_mul(100)
            .expect("OVERFLOW: uSTX overflowed u128")
            > liquid_ustx
                .checked_mul(self.pox_participation_threshold_pct as u128)
                .expect("OVERFLOW: uSTX overflowed u128")
    }

    pub fn mainnet_default() -> PoxConstants {
        PoxConstants::new(
            POX_REWARD_CYCLE_LENGTH,
            POX_PREPARE_WINDOW_LENGTH,
            80,
            25,
            5,
            BITCOIN_MAINNET_FIRST_BLOCK_HEIGHT + POX_SUNSET_START,
            BITCOIN_MAINNET_FIRST_BLOCK_HEIGHT + POX_SUNSET_END,
        )
    }

    pub fn testnet_default() -> PoxConstants {
        PoxConstants::new(
            POX_REWARD_CYCLE_LENGTH / 2,   // 1050
            POX_PREPARE_WINDOW_LENGTH / 2, // 50
            40,
            12,
            2,
            BITCOIN_TESTNET_FIRST_BLOCK_HEIGHT + POX_SUNSET_START,
            BITCOIN_TESTNET_FIRST_BLOCK_HEIGHT + POX_SUNSET_END,
        ) // total liquid supply is 40000000000000000 µSTX
    }

    pub fn regtest_default() -> PoxConstants {
        PoxConstants::new(
            5,
            1,
            1,
            3333333333333333,
            1,
            BITCOIN_REGTEST_FIRST_BLOCK_HEIGHT + POX_SUNSET_START,
            BITCOIN_REGTEST_FIRST_BLOCK_HEIGHT + POX_SUNSET_END,
        )
    }
}

/// Structure for encoding our view of the network
#[derive(Debug, PartialEq, Clone)]
pub struct BurnchainView {
    pub burn_block_height: u64, // last-seen block height (at chain tip)
    pub burn_block_hash: BurnchainHeaderHash, // last-seen burn block hash
    pub burn_stable_block_height: u64, // latest stable block height (e.g. chain tip minus 7)
    pub burn_stable_block_hash: BurnchainHeaderHash, // latest stable burn block hash
    pub last_burn_block_hashes: HashMap<u64, BurnchainHeaderHash>, // map all block heights from burn_block_height back to the oldest one we'll take for considering the peer a neighbor
}

/// The burnchain block's encoded state transition:
/// -- the new burn distribution
/// -- the sequence of valid blockstack operations that went into it
/// -- the set of previously-accepted leader VRF keys consumed
#[derive(Debug, Clone)]
pub struct BurnchainStateTransition {
    pub accepted_ops: Vec<BlockstackOperationType>,
}

#[derive(Debug)]
pub enum Error {
    /// Unsupported burn chain
    UnsupportedBurnchain,
    /// Bitcoin-related error
    Bitcoin(String),
    /// burn database error
    DBError(db_error),
    /// Download error
    DownloadError(String),
    /// Parse error
    ParseError,
    /// Thread channel error
    ThreadChannelError,
    /// Missing headers
    MissingHeaders,
    /// Missing parent block
    MissingParentBlock,
    /// Remote burnchain peer has misbehaved
    BurnchainPeerBroken,
    /// filesystem error
    FSError(io::Error),
    /// Operation processing error
    OpError(op_error),
    /// Try again error
    TrySyncAgain,
    UnknownBlock(BurnchainHeaderHash),
    NonCanonicalPoxId(PoxId, PoxId),
    CoordinatorClosed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::UnsupportedBurnchain => write!(f, "Unsupported burnchain"),
            Error::Bitcoin(ref btce) => fmt::Display::fmt(btce, f),
            Error::DBError(ref dbe) => fmt::Display::fmt(dbe, f),
            Error::DownloadError(ref btce) => fmt::Display::fmt(btce, f),
            Error::ParseError => write!(f, "Parse error"),
            Error::MissingHeaders => write!(f, "Missing block headers"),
            Error::MissingParentBlock => write!(f, "Missing parent block"),
            Error::ThreadChannelError => write!(f, "Error in thread channel"),
            Error::BurnchainPeerBroken => write!(f, "Remote burnchain peer has misbehaved"),
            Error::FSError(ref e) => fmt::Display::fmt(e, f),
            Error::OpError(ref e) => fmt::Display::fmt(e, f),
            Error::TrySyncAgain => write!(f, "Try synchronizing again"),
            Error::UnknownBlock(block) => write!(f, "Unknown burnchain block {}", block),
            Error::NonCanonicalPoxId(parent, child) => write!(
                f,
                "{} is not a descendant of the canonical parent PoXId: {}",
                parent, child
            ),
            Error::CoordinatorClosed => write!(f, "ChainsCoordinator channel hung up"),
        }
    }
}

impl error::Error for Error {
    fn cause(&self) -> Option<&dyn error::Error> {
        match *self {
            Error::UnsupportedBurnchain => None,
            Error::Bitcoin(ref _e) => None,
            Error::DBError(ref e) => Some(e),
            Error::DownloadError(ref _e) => None,
            Error::ParseError => None,
            Error::MissingHeaders => None,
            Error::MissingParentBlock => None,
            Error::ThreadChannelError => None,
            Error::BurnchainPeerBroken => None,
            Error::FSError(ref e) => Some(e),
            Error::OpError(ref e) => Some(e),
            Error::TrySyncAgain => None,
            Error::UnknownBlock(_) => None,
            Error::NonCanonicalPoxId(_, _) => None,
            Error::CoordinatorClosed => None,
        }
    }
}

impl From<db_error> for Error {
    fn from(e: db_error) -> Error {
        Error::DBError(e)
    }
}

impl From<sqlite_error> for Error {
    fn from(e: sqlite_error) -> Error {
        Error::DBError(db_error::SqliteError(e))
    }
}

impl BurnchainView {
    #[cfg(test)]
    pub fn make_test_data(&mut self) {
        let oldest_height = if self.burn_stable_block_height < MAX_NEIGHBOR_BLOCK_DELAY {
            0
        } else {
            self.burn_stable_block_height - MAX_NEIGHBOR_BLOCK_DELAY
        };

        let mut ret = HashMap::new();
        for i in oldest_height..self.burn_block_height + 1 {
            if i == self.burn_stable_block_height {
                ret.insert(i, self.burn_stable_block_hash.clone());
            } else if i == self.burn_block_height {
                ret.insert(i, self.burn_block_hash.clone());
            } else {
                let data = {
                    use sha2::Digest;
                    use sha2::Sha256;
                    let mut hasher = Sha256::new();
                    hasher.input(&i.to_le_bytes());
                    hasher.result()
                };
                let mut data_32 = [0x00; 32];
                data_32.copy_from_slice(&data[0..32]);
                ret.insert(i, BurnchainHeaderHash(data_32));
            }
        }
        self.last_burn_block_hashes = ret;
    }
}

#[cfg(test)]
pub mod test;
