use std::cmp::max;
use std::io;
use std::io::Read;
use std::ops::Deref;
use std::sync::atomic::Ordering;

use bitcoin::BlockHash;
use bitcoin::secp256k1::PublicKey;

use lightning::ln::msgs::{
	DecodeError, ErrorAction, LightningError, UnsignedChannelUpdate,
};
use lightning::routing::gossip::NetworkGraph;
use lightning::util::logger::Logger;
use lightning::util::ser::{BigSize, Readable};

use crate::error::GraphSyncError;
use crate::RapidGossipSync;

/// The purpose of this prefix is to identify the serialization format, should other rapid gossip
/// sync formats arise in the future.
///
/// The fourth byte is the protocol version in case our format gets updated.
const GOSSIP_PREFIX: [u8; 4] = [76, 68, 75, 1];

/// Maximum vector allocation capacity for distinct node IDs. This constraint is necessary to
/// avoid malicious updates being able to trigger excessive memory allocation.
const MAX_INITIAL_NODE_ID_VECTOR_CAPACITY: u32 = 50_000;

impl<NG: Deref<Target=NetworkGraph<L>>, L: Deref> RapidGossipSync<NG, L> where L::Target: Logger {
	/// Update network graph from binary data.
	/// Returns the last sync timestamp to be used the next time rapid sync data is queried.
	///
	/// `network_graph`: network graph to be updated
	///
	/// `update_data`: `&[u8]` binary stream that comprises the update data
	pub fn update_network_graph(&self, update_data: &[u8]) -> Result<u32, GraphSyncError> {
		let mut read_cursor = io::Cursor::new(update_data);
		self.update_network_graph_from_byte_stream(&mut read_cursor)
	}


	pub(crate) fn update_network_graph_from_byte_stream<R: Read>(
		&self,
		mut read_cursor: &mut R,
	) -> Result<u32, GraphSyncError> {
		let mut prefix = [0u8; 4];
		read_cursor.read_exact(&mut prefix)?;

		match prefix {
			GOSSIP_PREFIX => {}
			_ => {
				return Err(DecodeError::UnknownVersion.into());
			}
		};

		let chain_hash: BlockHash = Readable::read(read_cursor)?;
		let latest_seen_timestamp: u32 = Readable::read(read_cursor)?;
		// backdate the applied timestamp by a week
		let backdated_timestamp = latest_seen_timestamp.saturating_sub(24 * 3600 * 7);

		let node_id_count: u32 = Readable::read(read_cursor)?;
		let mut node_ids: Vec<PublicKey> = Vec::with_capacity(std::cmp::min(
			node_id_count,
			MAX_INITIAL_NODE_ID_VECTOR_CAPACITY,
		) as usize);
		for _ in 0..node_id_count {
			let current_node_id = Readable::read(read_cursor)?;
			node_ids.push(current_node_id);
		}

		let network_graph = &self.network_graph;

		let mut previous_scid: u64 = 0;
		let announcement_count: u32 = Readable::read(read_cursor)?;
		for _ in 0..announcement_count {
			let features = Readable::read(read_cursor)?;

			// handle SCID
			let scid_delta: BigSize = Readable::read(read_cursor)?;
			let short_channel_id = previous_scid
				.checked_add(scid_delta.0)
				.ok_or(DecodeError::InvalidValue)?;
			previous_scid = short_channel_id;

			let node_id_1_index: BigSize = Readable::read(read_cursor)?;
			let node_id_2_index: BigSize = Readable::read(read_cursor)?;
			if max(node_id_1_index.0, node_id_2_index.0) >= node_id_count as u64 {
				return Err(DecodeError::InvalidValue.into());
			};
			let node_id_1 = node_ids[node_id_1_index.0 as usize];
			let node_id_2 = node_ids[node_id_2_index.0 as usize];

			let announcement_result = network_graph.add_channel_from_partial_announcement(
				short_channel_id,
				backdated_timestamp as u64,
				features,
				node_id_1,
				node_id_2,
			);
			if let Err(lightning_error) = announcement_result {
				if let ErrorAction::IgnoreDuplicateGossip = lightning_error.action {
					// everything is fine, just a duplicate channel announcement
				} else {
					return Err(lightning_error.into());
				}
			}
		}

		previous_scid = 0; // updates start at a new scid

		let update_count: u32 = Readable::read(read_cursor)?;
		if update_count == 0 {
			return Ok(latest_seen_timestamp);
		}

		// obtain default values for non-incremental updates
		let default_cltv_expiry_delta: u16 = Readable::read(&mut read_cursor)?;
		let default_htlc_minimum_msat: u64 = Readable::read(&mut read_cursor)?;
		let default_fee_base_msat: u32 = Readable::read(&mut read_cursor)?;
		let default_fee_proportional_millionths: u32 = Readable::read(&mut read_cursor)?;
		let default_htlc_maximum_msat: u64 = Readable::read(&mut read_cursor)?;

		for _ in 0..update_count {
			let scid_delta: BigSize = Readable::read(read_cursor)?;
			let short_channel_id = previous_scid
				.checked_add(scid_delta.0)
				.ok_or(DecodeError::InvalidValue)?;
			previous_scid = short_channel_id;

			let channel_flags: u8 = Readable::read(read_cursor)?;

			// flags are always sent in full, and hence always need updating
			let standard_channel_flags = channel_flags & 0b_0000_0011;

			let mut synthetic_update = if channel_flags & 0b_1000_0000 == 0 {
				// full update, field flags will indicate deviations from the default
				UnsignedChannelUpdate {
					chain_hash,
					short_channel_id,
					timestamp: backdated_timestamp,
					flags: standard_channel_flags,
					cltv_expiry_delta: default_cltv_expiry_delta,
					htlc_minimum_msat: default_htlc_minimum_msat,
					htlc_maximum_msat: default_htlc_maximum_msat,
					fee_base_msat: default_fee_base_msat,
					fee_proportional_millionths: default_fee_proportional_millionths,
					excess_data: vec![],
				}
			} else {
				// incremental update, field flags will indicate mutated values
				let read_only_network_graph = network_graph.read_only();
				let channel = read_only_network_graph
					.channels()
					.get(&short_channel_id)
					.ok_or(LightningError {
						err: "Couldn't find channel for update".to_owned(),
						action: ErrorAction::IgnoreError,
					})?;

				let directional_info = channel
					.get_directional_info(channel_flags)
					.ok_or(LightningError {
						err: "Couldn't find previous directional data for update".to_owned(),
						action: ErrorAction::IgnoreError,
					})?;

				UnsignedChannelUpdate {
					chain_hash,
					short_channel_id,
					timestamp: backdated_timestamp,
					flags: standard_channel_flags,
					cltv_expiry_delta: directional_info.cltv_expiry_delta,
					htlc_minimum_msat: directional_info.htlc_minimum_msat,
					htlc_maximum_msat: directional_info.htlc_maximum_msat,
					fee_base_msat: directional_info.fees.base_msat,
					fee_proportional_millionths: directional_info.fees.proportional_millionths,
					excess_data: vec![],
				}
			};

			if channel_flags & 0b_0100_0000 > 0 {
				let cltv_expiry_delta: u16 = Readable::read(read_cursor)?;
				synthetic_update.cltv_expiry_delta = cltv_expiry_delta;
			}

			if channel_flags & 0b_0010_0000 > 0 {
				let htlc_minimum_msat: u64 = Readable::read(read_cursor)?;
				synthetic_update.htlc_minimum_msat = htlc_minimum_msat;
			}

			if channel_flags & 0b_0001_0000 > 0 {
				let fee_base_msat: u32 = Readable::read(read_cursor)?;
				synthetic_update.fee_base_msat = fee_base_msat;
			}

			if channel_flags & 0b_0000_1000 > 0 {
				let fee_proportional_millionths: u32 = Readable::read(read_cursor)?;
				synthetic_update.fee_proportional_millionths = fee_proportional_millionths;
			}

			if channel_flags & 0b_0000_0100 > 0 {
				let htlc_maximum_msat: u64 = Readable::read(read_cursor)?;
				synthetic_update.htlc_maximum_msat = htlc_maximum_msat;
			}

			network_graph.update_channel_unsigned(&synthetic_update)?;
		}

		self.network_graph.set_last_rapid_gossip_sync_timestamp(latest_seen_timestamp);
		self.is_initial_sync_complete.store(true, Ordering::Release);
		Ok(latest_seen_timestamp)
	}
}

#[cfg(test)]
mod tests {
	use bitcoin::blockdata::constants::genesis_block;
	use bitcoin::Network;

	use lightning::ln::msgs::DecodeError;
	use lightning::routing::gossip::NetworkGraph;
	use lightning::util::test_utils::TestLogger;

	use crate::error::GraphSyncError;
	use crate::RapidGossipSync;

	#[test]
	fn network_graph_fails_to_update_from_clipped_input() {
		let block_hash = genesis_block(Network::Bitcoin).block_hash();
		let logger = TestLogger::new();
		let network_graph = NetworkGraph::new(block_hash, &logger);

		let example_input = vec![
			76, 68, 75, 1, 111, 226, 140, 10, 182, 241, 179, 114, 193, 166, 162, 70, 174, 99, 247,
			79, 147, 30, 131, 101, 225, 90, 8, 156, 104, 214, 25, 0, 0, 0, 0, 0, 97, 227, 98, 218,
			0, 0, 0, 4, 2, 22, 7, 207, 206, 25, 164, 197, 231, 230, 231, 56, 102, 61, 250, 251,
			187, 172, 38, 46, 79, 247, 108, 44, 155, 48, 219, 238, 252, 53, 192, 6, 67, 2, 36, 125,
			157, 176, 223, 175, 234, 116, 94, 248, 201, 225, 97, 235, 50, 47, 115, 172, 63, 136,
			88, 216, 115, 11, 111, 217, 114, 84, 116, 124, 231, 107, 2, 158, 1, 242, 121, 152, 106,
			204, 131, 186, 35, 93, 70, 216, 10, 237, 224, 183, 89, 95, 65, 3, 83, 185, 58, 138,
			181, 64, 187, 103, 127, 68, 50, 2, 201, 19, 17, 138, 136, 149, 185, 226, 156, 137, 175,
			110, 32, 237, 0, 217, 90, 31, 100, 228, 149, 46, 219, 175, 168, 77, 4, 143, 38, 128,
			76, 97, 0, 0, 0, 2, 0, 0, 255, 8, 153, 192, 0, 2, 27, 0, 0, 0, 1, 0, 0, 255, 2, 68,
			226, 0, 6, 11, 0, 1, 2, 3, 0, 0, 0, 2, 0, 40, 0, 0, 0, 0, 0, 0, 3, 232, 0, 0, 0, 100,
			0, 0, 2, 224, 0, 0, 0, 0, 29, 129, 25, 192, 255, 8, 153, 192, 0, 2, 27, 0, 0, 36, 0, 0,
			0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 58, 85, 116, 216, 255, 2, 68, 226, 0, 6, 11, 0, 1, 24, 0,
			0, 3, 232, 0, 0, 0,
		];
		let rapid_sync = RapidGossipSync::new(&network_graph);
		let update_result = rapid_sync.update_network_graph(&example_input[..]);
		assert!(update_result.is_err());
		if let Err(GraphSyncError::DecodeError(DecodeError::ShortRead)) = update_result {
			// this is the expected error type
		} else {
			panic!("Unexpected update result: {:?}", update_result)
		}
	}

	#[test]
	fn incremental_only_update_fails_without_prior_announcements() {
		let incremental_update_input = vec![
			76, 68, 75, 1, 111, 226, 140, 10, 182, 241, 179, 114, 193, 166, 162, 70, 174, 99, 247,
			79, 147, 30, 131, 101, 225, 90, 8, 156, 104, 214, 25, 0, 0, 0, 0, 0, 97, 229, 183, 167,
			0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
			0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 8, 153, 192, 0, 2, 27, 0, 0, 136, 0, 0, 0, 221, 255, 2,
			68, 226, 0, 6, 11, 0, 1, 128,
		];

		let block_hash = genesis_block(Network::Bitcoin).block_hash();
		let logger = TestLogger::new();
		let network_graph = NetworkGraph::new(block_hash, &logger);

		assert_eq!(network_graph.read_only().channels().len(), 0);

		let rapid_sync = RapidGossipSync::new(&network_graph);
		let update_result = rapid_sync.update_network_graph(&incremental_update_input[..]);
		assert!(update_result.is_err());
		if let Err(GraphSyncError::LightningError(lightning_error)) = update_result {
			assert_eq!(lightning_error.err, "Couldn't find channel for update");
		} else {
			panic!("Unexpected update result: {:?}", update_result)
		}
	}

	#[test]
	fn incremental_only_update_fails_without_prior_updates() {
		let announced_update_input = vec![
			76, 68, 75, 1, 111, 226, 140, 10, 182, 241, 179, 114, 193, 166, 162, 70, 174, 99, 247,
			79, 147, 30, 131, 101, 225, 90, 8, 156, 104, 214, 25, 0, 0, 0, 0, 0, 97, 229, 183, 167,
			0, 0, 0, 4, 2, 22, 7, 207, 206, 25, 164, 197, 231, 230, 231, 56, 102, 61, 250, 251,
			187, 172, 38, 46, 79, 247, 108, 44, 155, 48, 219, 238, 252, 53, 192, 6, 67, 2, 36, 125,
			157, 176, 223, 175, 234, 116, 94, 248, 201, 225, 97, 235, 50, 47, 115, 172, 63, 136,
			88, 216, 115, 11, 111, 217, 114, 84, 116, 124, 231, 107, 2, 158, 1, 242, 121, 152, 106,
			204, 131, 186, 35, 93, 70, 216, 10, 237, 224, 183, 89, 95, 65, 3, 83, 185, 58, 138,
			181, 64, 187, 103, 127, 68, 50, 2, 201, 19, 17, 138, 136, 149, 185, 226, 156, 137, 175,
			110, 32, 237, 0, 217, 90, 31, 100, 228, 149, 46, 219, 175, 168, 77, 4, 143, 38, 128,
			76, 97, 0, 0, 0, 2, 0, 0, 255, 8, 153, 192, 0, 2, 27, 0, 0, 0, 1, 0, 0, 255, 2, 68,
			226, 0, 6, 11, 0, 1, 2, 3, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
			0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 8, 153, 192, 0, 2, 27, 0, 0, 136, 0, 0, 0, 221, 255,
			2, 68, 226, 0, 6, 11, 0, 1, 128,
		];

		let block_hash = genesis_block(Network::Bitcoin).block_hash();
		let logger = TestLogger::new();
		let network_graph = NetworkGraph::new(block_hash, &logger);

		assert_eq!(network_graph.read_only().channels().len(), 0);

		let rapid_sync = RapidGossipSync::new(&network_graph);
		let update_result = rapid_sync.update_network_graph(&announced_update_input[..]);
		assert!(update_result.is_err());
		if let Err(GraphSyncError::LightningError(lightning_error)) = update_result {
			assert_eq!(
				lightning_error.err,
				"Couldn't find previous directional data for update"
			);
		} else {
			panic!("Unexpected update result: {:?}", update_result)
		}
	}

	#[test]
	fn incremental_only_update_fails_without_prior_same_direction_updates() {
		let initialization_input = vec![
			76, 68, 75, 1, 111, 226, 140, 10, 182, 241, 179, 114, 193, 166, 162, 70, 174, 99, 247,
			79, 147, 30, 131, 101, 225, 90, 8, 156, 104, 214, 25, 0, 0, 0, 0, 0, 97, 227, 98, 218,
			0, 0, 0, 4, 2, 22, 7, 207, 206, 25, 164, 197, 231, 230, 231, 56, 102, 61, 250, 251,
			187, 172, 38, 46, 79, 247, 108, 44, 155, 48, 219, 238, 252, 53, 192, 6, 67, 2, 36, 125,
			157, 176, 223, 175, 234, 116, 94, 248, 201, 225, 97, 235, 50, 47, 115, 172, 63, 136,
			88, 216, 115, 11, 111, 217, 114, 84, 116, 124, 231, 107, 2, 158, 1, 242, 121, 152, 106,
			204, 131, 186, 35, 93, 70, 216, 10, 237, 224, 183, 89, 95, 65, 3, 83, 185, 58, 138,
			181, 64, 187, 103, 127, 68, 50, 2, 201, 19, 17, 138, 136, 149, 185, 226, 156, 137, 175,
			110, 32, 237, 0, 217, 90, 31, 100, 228, 149, 46, 219, 175, 168, 77, 4, 143, 38, 128,
			76, 97, 0, 0, 0, 2, 0, 0, 255, 8, 153, 192, 0, 2, 27, 0, 0, 0, 1, 0, 0, 255, 2, 68,
			226, 0, 6, 11, 0, 1, 2, 3, 0, 0, 0, 2, 0, 40, 0, 0, 0, 0, 0, 0, 3, 232, 0, 0, 3, 232,
			0, 0, 0, 1, 0, 0, 0, 0, 58, 85, 116, 216, 255, 8, 153, 192, 0, 2, 27, 0, 0, 25, 0, 0,
			0, 1, 0, 0, 0, 125, 255, 2, 68, 226, 0, 6, 11, 0, 1, 5, 0, 0, 0, 0, 29, 129, 25, 192,
		];

		let block_hash = genesis_block(Network::Bitcoin).block_hash();
		let logger = TestLogger::new();
		let network_graph = NetworkGraph::new(block_hash, &logger);

		assert_eq!(network_graph.read_only().channels().len(), 0);

		let rapid_sync = RapidGossipSync::new(&network_graph);
		let initialization_result = rapid_sync.update_network_graph(&initialization_input[..]);
		if initialization_result.is_err() {
			panic!(
				"Unexpected initialization result: {:?}",
				initialization_result
			)
		}

		assert_eq!(network_graph.read_only().channels().len(), 2);
		let initialized = network_graph.to_string();
		assert!(initialized
			.contains("021607cfce19a4c5e7e6e738663dfafbbbac262e4ff76c2c9b30dbeefc35c00643"));
		assert!(initialized
			.contains("02247d9db0dfafea745ef8c9e161eb322f73ac3f8858d8730b6fd97254747ce76b"));
		assert!(initialized
			.contains("029e01f279986acc83ba235d46d80aede0b7595f410353b93a8ab540bb677f4432"));
		assert!(initialized
			.contains("02c913118a8895b9e29c89af6e20ed00d95a1f64e4952edbafa84d048f26804c61"));
		assert!(initialized.contains("619737530008010752"));
		assert!(initialized.contains("783241506229452801"));

		let opposite_direction_incremental_update_input = vec![
			76, 68, 75, 1, 111, 226, 140, 10, 182, 241, 179, 114, 193, 166, 162, 70, 174, 99, 247,
			79, 147, 30, 131, 101, 225, 90, 8, 156, 104, 214, 25, 0, 0, 0, 0, 0, 97, 229, 183, 167,
			0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
			0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 8, 153, 192, 0, 2, 27, 0, 0, 136, 0, 0, 0, 221, 255, 2,
			68, 226, 0, 6, 11, 0, 1, 128,
		];
		let update_result = rapid_sync.update_network_graph(&opposite_direction_incremental_update_input[..]);
		assert!(update_result.is_err());
		if let Err(GraphSyncError::LightningError(lightning_error)) = update_result {
			assert_eq!(
				lightning_error.err,
				"Couldn't find previous directional data for update"
			);
		} else {
			panic!("Unexpected update result: {:?}", update_result)
		}
	}

	#[test]
	fn incremental_update_succeeds_with_prior_announcements_and_full_updates() {
		let initialization_input = vec![
			76, 68, 75, 1, 111, 226, 140, 10, 182, 241, 179, 114, 193, 166, 162, 70, 174, 99, 247,
			79, 147, 30, 131, 101, 225, 90, 8, 156, 104, 214, 25, 0, 0, 0, 0, 0, 97, 227, 98, 218,
			0, 0, 0, 4, 2, 22, 7, 207, 206, 25, 164, 197, 231, 230, 231, 56, 102, 61, 250, 251,
			187, 172, 38, 46, 79, 247, 108, 44, 155, 48, 219, 238, 252, 53, 192, 6, 67, 2, 36, 125,
			157, 176, 223, 175, 234, 116, 94, 248, 201, 225, 97, 235, 50, 47, 115, 172, 63, 136,
			88, 216, 115, 11, 111, 217, 114, 84, 116, 124, 231, 107, 2, 158, 1, 242, 121, 152, 106,
			204, 131, 186, 35, 93, 70, 216, 10, 237, 224, 183, 89, 95, 65, 3, 83, 185, 58, 138,
			181, 64, 187, 103, 127, 68, 50, 2, 201, 19, 17, 138, 136, 149, 185, 226, 156, 137, 175,
			110, 32, 237, 0, 217, 90, 31, 100, 228, 149, 46, 219, 175, 168, 77, 4, 143, 38, 128,
			76, 97, 0, 0, 0, 2, 0, 0, 255, 8, 153, 192, 0, 2, 27, 0, 0, 0, 1, 0, 0, 255, 2, 68,
			226, 0, 6, 11, 0, 1, 2, 3, 0, 0, 0, 4, 0, 40, 0, 0, 0, 0, 0, 0, 3, 232, 0, 0, 3, 232,
			0, 0, 0, 1, 0, 0, 0, 0, 58, 85, 116, 216, 255, 8, 153, 192, 0, 2, 27, 0, 0, 56, 0, 0,
			0, 0, 0, 0, 0, 1, 0, 0, 0, 100, 0, 0, 2, 224, 0, 25, 0, 0, 0, 1, 0, 0, 0, 125, 255, 2,
			68, 226, 0, 6, 11, 0, 1, 4, 0, 0, 0, 0, 29, 129, 25, 192, 0, 5, 0, 0, 0, 0, 29, 129,
			25, 192,
		];

		let block_hash = genesis_block(Network::Bitcoin).block_hash();
		let logger = TestLogger::new();
		let network_graph = NetworkGraph::new(block_hash, &logger);

		assert_eq!(network_graph.read_only().channels().len(), 0);

		let rapid_sync = RapidGossipSync::new(&network_graph);
		let initialization_result = rapid_sync.update_network_graph(&initialization_input[..]);
		assert!(initialization_result.is_ok());

		let single_direction_incremental_update_input = vec![
			76, 68, 75, 1, 111, 226, 140, 10, 182, 241, 179, 114, 193, 166, 162, 70, 174, 99, 247,
			79, 147, 30, 131, 101, 225, 90, 8, 156, 104, 214, 25, 0, 0, 0, 0, 0, 97, 229, 183, 167,
			0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
			0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 8, 153, 192, 0, 2, 27, 0, 0, 136, 0, 0, 0, 221, 255, 2,
			68, 226, 0, 6, 11, 0, 1, 128,
		];
		let update_result = rapid_sync.update_network_graph(&single_direction_incremental_update_input[..]);
		if update_result.is_err() {
			panic!("Unexpected update result: {:?}", update_result)
		}

		assert_eq!(network_graph.read_only().channels().len(), 2);
		let after = network_graph.to_string();
		assert!(
			after.contains("021607cfce19a4c5e7e6e738663dfafbbbac262e4ff76c2c9b30dbeefc35c00643")
		);
		assert!(
			after.contains("02247d9db0dfafea745ef8c9e161eb322f73ac3f8858d8730b6fd97254747ce76b")
		);
		assert!(
			after.contains("029e01f279986acc83ba235d46d80aede0b7595f410353b93a8ab540bb677f4432")
		);
		assert!(
			after.contains("02c913118a8895b9e29c89af6e20ed00d95a1f64e4952edbafa84d048f26804c61")
		);
		assert!(after.contains("619737530008010752"));
		assert!(after.contains("783241506229452801"));
	}

	#[test]
	fn full_update_succeeds() {
		let valid_input = vec![
			76, 68, 75, 1, 111, 226, 140, 10, 182, 241, 179, 114, 193, 166, 162, 70, 174, 99, 247,
			79, 147, 30, 131, 101, 225, 90, 8, 156, 104, 214, 25, 0, 0, 0, 0, 0, 97, 227, 98, 218,
			0, 0, 0, 4, 2, 22, 7, 207, 206, 25, 164, 197, 231, 230, 231, 56, 102, 61, 250, 251,
			187, 172, 38, 46, 79, 247, 108, 44, 155, 48, 219, 238, 252, 53, 192, 6, 67, 2, 36, 125,
			157, 176, 223, 175, 234, 116, 94, 248, 201, 225, 97, 235, 50, 47, 115, 172, 63, 136,
			88, 216, 115, 11, 111, 217, 114, 84, 116, 124, 231, 107, 2, 158, 1, 242, 121, 152, 106,
			204, 131, 186, 35, 93, 70, 216, 10, 237, 224, 183, 89, 95, 65, 3, 83, 185, 58, 138,
			181, 64, 187, 103, 127, 68, 50, 2, 201, 19, 17, 138, 136, 149, 185, 226, 156, 137, 175,
			110, 32, 237, 0, 217, 90, 31, 100, 228, 149, 46, 219, 175, 168, 77, 4, 143, 38, 128,
			76, 97, 0, 0, 0, 2, 0, 0, 255, 8, 153, 192, 0, 2, 27, 0, 0, 0, 1, 0, 0, 255, 2, 68,
			226, 0, 6, 11, 0, 1, 2, 3, 0, 0, 0, 4, 0, 40, 0, 0, 0, 0, 0, 0, 3, 232, 0, 0, 3, 232,
			0, 0, 0, 1, 0, 0, 0, 0, 29, 129, 25, 192, 255, 8, 153, 192, 0, 2, 27, 0, 0, 60, 0, 0,
			0, 0, 0, 0, 0, 1, 0, 0, 0, 100, 0, 0, 2, 224, 0, 0, 0, 0, 58, 85, 116, 216, 0, 29, 0,
			0, 0, 1, 0, 0, 0, 125, 0, 0, 0, 0, 58, 85, 116, 216, 255, 2, 68, 226, 0, 6, 11, 0, 1,
			0, 0, 1,
		];

		let block_hash = genesis_block(Network::Bitcoin).block_hash();
		let logger = TestLogger::new();
		let network_graph = NetworkGraph::new(block_hash, &logger);

		assert_eq!(network_graph.read_only().channels().len(), 0);

		let rapid_sync = RapidGossipSync::new(&network_graph);
		let update_result = rapid_sync.update_network_graph(&valid_input[..]);
		if update_result.is_err() {
			panic!("Unexpected update result: {:?}", update_result)
		}

		assert_eq!(network_graph.read_only().channels().len(), 2);
		let after = network_graph.to_string();
		assert!(
			after.contains("021607cfce19a4c5e7e6e738663dfafbbbac262e4ff76c2c9b30dbeefc35c00643")
		);
		assert!(
			after.contains("02247d9db0dfafea745ef8c9e161eb322f73ac3f8858d8730b6fd97254747ce76b")
		);
		assert!(
			after.contains("029e01f279986acc83ba235d46d80aede0b7595f410353b93a8ab540bb677f4432")
		);
		assert!(
			after.contains("02c913118a8895b9e29c89af6e20ed00d95a1f64e4952edbafa84d048f26804c61")
		);
		assert!(after.contains("619737530008010752"));
		assert!(after.contains("783241506229452801"));
	}
}
