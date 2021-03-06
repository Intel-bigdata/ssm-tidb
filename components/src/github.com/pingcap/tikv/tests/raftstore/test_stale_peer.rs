// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

//! A module contains test cases of stale peers gc.

use kvproto::eraftpb::MessageType;
use kvproto::raft_serverpb::{PeerState, RegionLocalState};
use tikv::raftstore::store::{keys, Peekable};
use tikv::storage::CF_RAFT;

use super::cluster::{Cluster, Simulator};
use super::node::new_node_cluster;
use super::server::new_server_cluster;
use super::transport_simulate::*;
use super::util::*;

/// A helper function for testing the behaviour of the gc of stale peer
/// which is out of region.
/// If a peer detects the leader is missing for a specified long time,
/// it should consider itself as a stale peer which is removed from the region.
/// This test case covers the following scenario:
/// At first, there are three peer A, B, C in the cluster, and A is leader.
/// Peer B gets down. And then A adds D, E, F int the cluster.
/// Peer D becomes leader of the new cluster, and then removes peer A, B, C.
/// After all these peer in and out, now the cluster has peer D, E, F.
/// If peer B goes up at this moment, it still thinks it is one of the cluster
/// and has peers A, C. However, it could not reach A, C since they are removed from
/// the cluster or probably destroyed.
/// Meantime, D, E, F would not reach B, Since it's not in the cluster anymore.
/// In this case, Peer B would notice that the leader is missing for a long time,
/// and it would check with pd to confirm whether it's still a member of the cluster.
/// If not, it should destroy itself as a stale peer which is removed out already.
fn test_stale_peer_out_of_region<T: Simulator>(cluster: &mut Cluster<T>) {
    let pd_client = cluster.pd_client.clone();
    // Disable default max peer number check.
    pd_client.disable_default_rule();

    let r1 = cluster.run_conf_change();
    pd_client.must_add_peer(r1, new_peer(2, 2));
    pd_client.must_add_peer(r1, new_peer(3, 3));
    let (key, value) = (b"k1", b"v1");
    cluster.must_put(key, value);
    assert_eq!(cluster.get(key), Some(value.to_vec()));

    let engine_2 = cluster.get_engine(2);
    must_get_equal(&engine_2, key, value);

    // Isolate peer 2 from other part of the cluster.
    cluster.add_send_filter(IsolationFilterFactory::new(2));

    // In case 2 is leader, it will fail to pass the healthy nodes check,
    // so remove isolated node first. Because 2 is isolated, so it can't remove itself.
    pd_client.must_remove_peer(r1, new_peer(2, 2));

    // Add peer [(4, 4), (5, 5), (6, 6)].
    pd_client.must_add_peer(r1, new_peer(4, 4));
    pd_client.must_add_peer(r1, new_peer(5, 5));
    pd_client.must_add_peer(r1, new_peer(6, 6));

    // Remove peer [(1, 1), (3, 3)].
    pd_client.must_remove_peer(r1, new_peer(1, 1));
    pd_client.must_remove_peer(r1, new_peer(3, 3));

    // Keep peer 2 isolated. Otherwise whether peer 3 is destroyed or not,
    // it will handle the stale raft message from peer 2 and cause peer 2 to
    // destroy itself earlier than this test case expects.

    // Wait for max_leader_missing_duration to time out.

    cluster.must_remove_region(2, r1);

    // Check whether this region is still functional properly.
    let (key2, value2) = (b"k2", b"v2");
    cluster.must_put(key2, value2);
    assert_eq!(cluster.get(key2), Some(value2.to_vec()));

    // Check whether peer(2, 2) and its data are destroyed.
    must_get_none(&engine_2, key);
    must_get_none(&engine_2, key2);
    let state_key = keys::region_state_key(1);
    let state: RegionLocalState = engine_2.get_msg_cf(CF_RAFT, &state_key).unwrap().unwrap();
    assert_eq!(state.get_state(), PeerState::Tombstone);
}

#[test]
fn test_node_stale_peer_out_of_region() {
    let count = 6;
    let mut cluster = new_node_cluster(0, count);
    test_stale_peer_out_of_region(&mut cluster);
}

#[test]
fn test_server_stale_peer_out_of_region() {
    let count = 6;
    let mut cluster = new_server_cluster(0, count);
    test_stale_peer_out_of_region(&mut cluster);
}

/// A help function for testing the behaviour of the gc of stale peer
/// which is out or region.
/// If a peer detects the leader is missing for a specified long time,
/// it should consider itself as a stale peer which is removed from the region.
/// This test case covers the following scenario:
/// A peer, B is initialized as a replicated peer without data after
/// receiving a single raft AE message. But then it goes through some process like
/// the case of `test_stale_peer_out_of_region`, it's removed out of the region
/// and wouldn't be contacted anymore.
/// In both cases, peer B would notice that the leader is missing for a long time,
/// and it's an initialized peer without any data. It would destroy itself as
/// as stale peer directly and should not impact other region data on the same store.
fn test_stale_peer_without_data<T: Simulator>(cluster: &mut Cluster<T>, right_derive: bool) {
    cluster.cfg.raft_store.right_derive_when_split = right_derive;

    let pd_client = cluster.pd_client.clone();
    // Disable default max peer number check.
    pd_client.disable_default_rule();

    let r1 = cluster.run_conf_change();
    cluster.must_put(b"k1", b"v1");
    cluster.must_put(b"k3", b"v3");
    let region = cluster.get_region(b"");
    pd_client.must_add_peer(r1, new_peer(2, 2));
    cluster.must_split(&region, b"k2");
    pd_client.must_add_peer(r1, new_peer(3, 3));

    let engine3 = cluster.get_engine(3);
    if right_derive {
        must_get_none(&engine3, b"k1");
        must_get_equal(&engine3, b"k3", b"v3");
    } else {
        must_get_equal(&engine3, b"k1", b"v1");
        must_get_none(&engine3, b"k3");
    }

    let new_region = if right_derive {
        cluster.get_region(b"k1")
    } else {
        cluster.get_region(b"k3")
    };
    let new_region_id = new_region.get_id();
    // Block peer (3, 4) at receiving snapshot, but not the heartbeat
    cluster.add_send_filter(CloneFilterFactory(
        RegionPacketFilter::new(new_region_id, 3).msg_type(MessageType::MsgSnapshot),
    ));

    pd_client.must_add_peer(new_region_id, new_peer(3, 4));

    // Wait for the heartbeat broadcasted from peer (1, 1000) to peer (3, 4).
    cluster.must_region_exist(new_region_id, 3);

    // And then isolate peer (3, 4) from peer (1, 1000).
    cluster.add_send_filter(IsolationFilterFactory::new(3));

    pd_client.must_remove_peer(new_region_id, new_peer(3, 4));

    cluster.must_remove_region(3, new_region_id);

    // There must be no data on store 3 belongs to new region
    if right_derive {
        must_get_none(&engine3, b"k1");
    } else {
        must_get_none(&engine3, b"k3");
    }

    // Check whether peer(3, 4) is destroyed.
    // Before peer 4 is destroyed, a tombstone mark will be written into the engine.
    // So we could check the tombstone mark to make sure peer 4 is destroyed.
    let state_key = keys::region_state_key(new_region_id);
    let state: RegionLocalState = engine3.get_msg_cf(CF_RAFT, &state_key).unwrap().unwrap();
    assert_eq!(state.get_state(), PeerState::Tombstone);

    // other region should not be affected.
    if right_derive {
        must_get_equal(&engine3, b"k3", b"v3");
    } else {
        must_get_equal(&engine3, b"k1", b"v1");
    }
}

#[test]
fn test_node_stale_peer_without_data_left_derive_when_split() {
    let count = 3;
    let mut cluster = new_node_cluster(0, count);
    test_stale_peer_without_data(&mut cluster, false);
}

#[test]
fn test_node_stale_peer_without_data_right_derive_when_split() {
    let count = 3;
    let mut cluster = new_node_cluster(0, count);
    test_stale_peer_without_data(&mut cluster, true);
}

#[test]
fn test_server_stale_peer_without_data_left_derive_when_split() {
    let count = 3;
    let mut cluster = new_server_cluster(0, count);
    test_stale_peer_without_data(&mut cluster, false);
}

#[test]
fn test_server_stale_peer_without_data_right_derive_when_split() {
    let count = 3;
    let mut cluster = new_server_cluster(0, count);
    test_stale_peer_without_data(&mut cluster, true);
}
