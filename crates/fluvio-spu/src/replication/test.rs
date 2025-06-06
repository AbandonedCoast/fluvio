use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::env::temp_dir;

use fluvio::Isolation;
use fluvio_controlplane::replica::Replica;
use fluvio_controlplane::spu_api::update_replica::UpdateReplicaRequest;
use fluvio_storage::iterators::FileBatchIterator;
use fluvio_types::event::offsets::OffsetPublisher;
use rand::Rng;
use tracing::debug;
use derive_builder::Builder;
use once_cell::sync::Lazy;

use fluvio_storage::FileReplica;
use fluvio_future::timer::sleep;
use flv_util::fixture::ensure_clean_dir;
use fluvio_types::SpuId;
use fluvio_controlplane_metadata::spu::{IngressAddr, IngressPort, SpuSpec};
use fluvio_protocol::fixture::{create_raw_recordset, create_raw_recordset_inner};

use crate::core::{DefaultSharedGlobalContext, GlobalContext};
use crate::config::SpuConfig;
use crate::services::create_internal_server;

use super::{follower::FollowerReplicaState, leader::LeaderReplicaState};

const TOPIC: &str = "test";
const HOST: &str = "127.0.0.1";

const MAX_WAIT_LEADER: u64 = 300;
const MAX_WAIT_FOLLOWER: u64 = 100;
const WAIT_TERMINATE: u64 = 1000;
const REJECT_WAIT: u64 = 11;

const LEADER: SpuId = 5001;
const FOLLOWER1: SpuId = 5002;
const FOLLOWER2: SpuId = 5003;

static MAX_WAIT_REPLICATION: Lazy<u64> = Lazy::new(|| {
    use std::env;
    if env::var("CI").is_ok() { 5000 } else { 1000 }
});

#[derive(Builder, Debug)]
pub(crate) struct TestConfig {
    #[builder(setter(into), default = "5001")]
    base_id: SpuId,
    #[builder(setter(into), default = "1")]
    in_sync_replica: u16,
    #[builder(setter(into), default = "0")]
    followers: u16,
    #[builder(setter(into), default = "temp_dir()")]
    base_dir: PathBuf,
    #[builder(setter(into), default = "9000")]
    base_port: u16,
}

impl TestConfig {
    // isolate builder
    pub fn builder() -> TestConfigBuilder {
        TestConfigBuilder::default()
    }

    pub fn leader_config(&self) -> SpuConfig {
        let mut config = SpuConfig::default();
        config.log.base_dir.clone_from(&self.base_dir);
        config.id = self.base_id;
        config.private_endpoint = format!("{}:{}", HOST, self.base_port);
        config
    }

    pub fn follower_config(&self, follower_index: u16) -> SpuConfig {
        assert!(follower_index < self.followers);
        let mut config = SpuConfig::default();
        config.log.base_dir.clone_from(&self.base_dir);
        config.replication.min_in_sync_replicas = self.in_sync_replica;
        config.id = self.follower_id(follower_index);
        config
    }

    fn leader_port(&self) -> u16 {
        self.base_port
    }

    fn leader_public_port(&self) -> u16 {
        self.base_port + 1
    }

    fn follower_port(&self, follower_index: u16) -> u16 {
        assert!(follower_index < self.followers);
        self.base_port + 2 + follower_index
    }

    fn follower_id(&self, follower_index: u16) -> SpuId {
        assert!(follower_index < self.followers);
        self.base_id + 1 + follower_index as SpuId
    }

    fn spu_specs(&self) -> Vec<SpuSpec> {
        let mut leader_spec =
            SpuSpec::new_private_addr(self.base_id, self.leader_port(), HOST.to_owned());
        leader_spec.public_endpoint = IngressPort {
            port: self.leader_public_port(),
            ingress: vec![IngressAddr::from_host(HOST.to_owned())],
            ..Default::default()
        };
        let mut specs = vec![leader_spec];

        for i in 0..self.followers {
            specs.push(SpuSpec::new_private_addr(
                self.follower_id(i),
                self.follower_port(i),
                HOST.to_owned(),
            ));
        }
        specs
    }

    /// generate test replica with assigned SPU
    fn replica(&self) -> Replica {
        self.replica_inner(TOPIC.to_owned())
    }

    fn replica_inner(&self, topic: String) -> Replica {
        let mut followers = vec![LEADER];
        for i in 0..self.followers {
            followers.push(self.follower_id(i));
        }
        Replica::new((topic, 0), self.base_id, followers)
    }

    pub fn leader_addr(&self) -> String {
        format!("{}:{}", HOST, self.leader_port())
    }

    pub fn leader_public_addr(&self) -> String {
        format!("{}:{}", HOST, self.leader_public_port())
    }

    /// create new context with SPU populated
    pub async fn leader_ctx(&self) -> DefaultSharedGlobalContext {
        let leader_config = self.leader_config();

        let gctx = GlobalContext::new_shared_context(leader_config);
        gctx.spu_localstore().sync_all(self.spu_specs());
        gctx.sync_follower_update().await;

        gctx
    }

    /// starts new leader
    pub async fn leader_replica(
        &self,
    ) -> (DefaultSharedGlobalContext, LeaderReplicaState<FileReplica>) {
        let replica = self.replica();
        self.leader_replica_inner(replica).await
    }

    pub async fn leader_replica_inner(
        &self,
        replica: Replica,
    ) -> (DefaultSharedGlobalContext, LeaderReplicaState<FileReplica>) {
        let gctx = self.leader_ctx().await;
        gctx.replica_localstore().sync_all(vec![replica.clone()]);

        let leader_replica = gctx
            .leaders_state()
            .add_leader_replica(&gctx, replica.clone(), gctx.status_update_owned())
            .await
            .expect("leader");

        (gctx, leader_replica)
    }

    /// create new follower context witj SPU
    pub async fn follower_ctx(&self, follower_index: u16) -> DefaultSharedGlobalContext {
        let follower_config = self.follower_config(follower_index);
        let gctx = GlobalContext::new_shared_context(follower_config);
        gctx.spu_localstore().sync_all(self.spu_specs());
        gctx
    }

    pub async fn follower_replica(
        &self,
        follower_index: u16,
    ) -> (
        DefaultSharedGlobalContext,
        FollowerReplicaState<FileReplica>,
    ) {
        let replica = self.replica();
        let gctx = self.follower_ctx(follower_index).await;
        gctx.replica_localstore().sync_all(vec![replica.clone()]);

        gctx.followers_state_owned()
            .add_replica(&gctx, replica.clone())
            .await
            .expect("create");

        let follower_replica = gctx
            .followers_state()
            .get(&replica.id)
            .await
            .expect("value");

        (gctx, follower_replica)
    }
}

impl TestConfigBuilder {
    pub(crate) fn generate(&mut self, test_dir: &str) -> TestConfig {
        let mut test_config = self.build().unwrap();
        let test_path = test_config.base_dir.join(test_dir);
        ensure_clean_dir(&test_path);
        test_config.base_dir = test_path;
        test_config
    }
}

/// Test 2 replica
/// Replicating with existing records
///    
#[fluvio_future::test(ignore)]
async fn test_just_leader() {
    let port = portpicker::pick_unused_port().expect("No free ports left");
    let builder = TestConfig::builder()
        .base_port(port)
        .generate("just_leader");

    let (leader_gctx, leader_replica) = builder.leader_replica().await;

    assert_eq!(leader_replica.leo(), 0);
    assert_eq!(leader_replica.hw(), 0);

    let status = leader_gctx.status_update().remove_all().await;
    assert!(!status.is_empty());
    let lrs = &status[0];
    assert_eq!(lrs.id, (TOPIC, 0).into());
    assert_eq!(lrs.leader.spu, LEADER);
    assert_eq!(lrs.leader.hw, 0);
    assert_eq!(lrs.leader.leo, 0);

    // write records
    leader_replica
        .write_record_set(
            &mut create_raw_recordset(2),
            leader_gctx.follower_notifier(),
        )
        .await
        .expect("write");

    assert_eq!(leader_replica.leo(), 2);
    assert_eq!(leader_replica.hw(), 2);

    let status = leader_gctx.status_update().remove_all().await;
    assert!(!status.is_empty());
    let lrs = &status[0];
    assert_eq!(lrs.id, (TOPIC, 0).into());
    assert_eq!(lrs.leader.spu, LEADER);
    assert_eq!(lrs.leader.hw, 2);
    assert_eq!(lrs.leader.leo, 2);
}

/// Test 2 replica
/// Replicating with existing records
#[fluvio_future::test(ignore)]
async fn test_replication2_existing() {
    let port = portpicker::pick_unused_port().expect("No free ports left");
    let builder = TestConfig::builder()
        .followers(1_u16)
        .base_port(port)
        .generate("replication2_existing");

    let (leader_gctx, leader_replica) = builder.leader_replica().await;

    // write records
    leader_replica
        .write_record_set(
            &mut create_raw_recordset(2),
            leader_gctx.follower_notifier(),
        )
        .await
        .expect("write");

    assert_eq!(leader_replica.leo(), 2);
    assert_eq!(leader_replica.hw(), 0);
    assert!(!leader_gctx.status_update().remove_all().await.is_empty());

    let spu_server = create_internal_server(builder.leader_addr(), leader_gctx.clone()).run();

    // give leader controller time to startup
    sleep(Duration::from_millis(MAX_WAIT_LEADER)).await;

    debug!("starting follower replica controller");
    let (_, follower_replica) = builder.follower_replica(0).await;

    // at this point, follower replica should be empty since we didn't have time to sync up with leader
    assert_eq!(follower_replica.leo(), 0);
    assert_eq!(follower_replica.hw(), 0);

    // wait until follower sync up with leader
    sleep(Duration::from_millis(*MAX_WAIT_REPLICATION)).await;

    debug!("done waiting. checking result");

    // all records has been fully replicated
    assert_eq!(follower_replica.leo(), 2);

    // hw has been replicated
    assert_eq!(follower_replica.hw(), 2);
    assert_eq!(leader_replica.hw(), 2);

    let status = leader_gctx.status_update().remove_all().await;
    debug!(?status);
    assert!(!status.is_empty());
    let lrs = &status[0];
    assert_eq!(lrs.id, (TOPIC, 0).into());
    assert_eq!(lrs.leader.spu, LEADER);
    assert_eq!(lrs.leader.hw, 2);
    assert_eq!(lrs.leader.leo, 2);
    let f_status = &lrs.replicas[0];
    assert_eq!(f_status.spu, FOLLOWER1);
    assert_eq!(f_status.hw, 2);
    assert_eq!(f_status.leo, 2);

    sleep(Duration::from_millis(WAIT_TERMINATE)).await;

    spu_server.notify();
}

/// Test 2 replica
/// Replicating new records
///    
#[fluvio_future::test(ignore)]
async fn test_replication2_new_records() {
    let port = portpicker::pick_unused_port().expect("No free ports left");
    let builder = TestConfig::builder()
        .followers(1_u16)
        .base_port(port)
        .generate("replication2_new");

    let (leader_gctx, leader_replica) = builder.leader_replica().await;
    assert_eq!(leader_replica.leo(), 0);
    assert_eq!(leader_replica.hw(), 0);

    let follower_info = leader_replica.followers_info().await;
    assert_eq!(follower_info.get(&5002).unwrap().leo, -1);

    let spu_server = create_internal_server(builder.leader_addr(), leader_gctx.clone()).run();

    // give leader controller time to startup
    sleep(Duration::from_millis(MAX_WAIT_LEADER)).await;

    let (_, follower_replica) = builder.follower_replica(0).await;

    // at this point, follower replica should be empty since we didn't have time to sync up with leader
    assert_eq!(follower_replica.leo(), 0);
    assert_eq!(follower_replica.hw(), 0);

    sleep(Duration::from_millis(MAX_WAIT_FOLLOWER)).await;
    // leader should have actual follower info not just init
    let follower_info = leader_replica.followers_info().await;
    assert_eq!(follower_info.get(&5002).unwrap().leo, 0);

    // write records
    leader_replica
        .write_record_set(
            &mut create_raw_recordset(2),
            leader_gctx.follower_notifier(),
        )
        .await
        .expect("write");

    assert_eq!(leader_replica.leo(), 2);
    assert_eq!(leader_replica.hw(), 0);

    // wait until follower sync up with leader
    sleep(Duration::from_millis(*MAX_WAIT_REPLICATION)).await;

    debug!("done waiting. checking result");

    // all records has been fully replicated
    assert_eq!(follower_replica.leo(), 2);

    // hw has been replicated
    assert_eq!(follower_replica.hw(), 2);
    assert_eq!(leader_replica.hw(), 2);

    let status = leader_gctx.status_update().remove_all().await;
    debug!(?status);
    assert!(!status.is_empty());
    let lrs = &status[0];
    assert_eq!(lrs.id, (TOPIC, 0).into());
    assert_eq!(lrs.leader.spu, LEADER);
    assert_eq!(lrs.leader.hw, 2);
    assert_eq!(lrs.leader.leo, 2);
    let f_status = &lrs.replicas[0];
    assert_eq!(f_status.spu, FOLLOWER1);
    assert_eq!(f_status.hw, 2);
    assert_eq!(f_status.leo, 2);

    sleep(Duration::from_millis(WAIT_TERMINATE)).await;

    spu_server.notify();
}

/// test with 3 SPU
#[fluvio_future::test(ignore)]
async fn test_replication3_existing() {
    let port = portpicker::pick_unused_port().expect("No free ports left");
    let builder = TestConfig::builder()
        .followers(2_u16)
        .base_port(port)
        .generate("replication3_existing");

    let (leader_gctx, leader_replica) = builder.leader_replica().await;

    // write records
    leader_replica
        .write_record_set(
            &mut create_raw_recordset(2),
            leader_gctx.follower_notifier(),
        )
        .await
        .expect("write");

    assert_eq!(leader_replica.leo(), 2);
    assert_eq!(leader_replica.hw(), 0);

    let spu_server = create_internal_server(builder.leader_addr(), leader_gctx.clone()).run();

    // give leader controller time to startup
    sleep(Duration::from_millis(MAX_WAIT_LEADER)).await;

    let (_, follower_replica1) = builder.follower_replica(0).await;

    // at this point, follower replica should be empty since we didn't have time to sync up with leader
    assert_eq!(follower_replica1.leo(), 0);
    assert_eq!(follower_replica1.hw(), 0);

    let (_, follower_replica2) = builder.follower_replica(1).await;

    // at this point, follower replica should be empty since we didn't have time to sync up with leader
    assert_eq!(follower_replica2.leo(), 0);
    assert_eq!(follower_replica2.hw(), 0);

    //wait until follower sync up with leader
    sleep(Duration::from_millis(*MAX_WAIT_REPLICATION)).await;

    debug!("done waiting for first checking result");

    // all records has been fully replicated
    assert_eq!(follower_replica1.leo(), 2);
    // leader's hw is still 0
    assert_eq!(follower_replica1.hw(), 2);
    assert_eq!(leader_replica.hw(), 2);

    let (_, follower_replica2) = builder.follower_replica(1).await;
    assert_eq!(follower_replica2.leo(), 2);
    assert_eq!(follower_replica2.hw(), 2);

    sleep(Duration::from_millis(WAIT_TERMINATE)).await;

    spu_server.notify();
}

/// Test 2 replica
/// Replicating new records
///    
#[fluvio_future::test(ignore)]
async fn test_replication3_new_records() {
    let port = portpicker::pick_unused_port().expect("No free ports left");
    let builder = TestConfig::builder()
        .followers(2_u16)
        .base_port(port)
        .generate("replication3_new");

    let (leader_gctx, leader_replica) = builder.leader_replica().await;
    assert_eq!(leader_replica.leo(), 0);
    assert_eq!(leader_replica.hw(), 0);

    let follower_info = leader_replica.followers_info().await;
    assert_eq!(follower_info.get(&FOLLOWER1).unwrap().leo, -1);
    assert_eq!(follower_info.get(&FOLLOWER2).unwrap().leo, -1);

    let spu_server = create_internal_server(builder.leader_addr(), leader_gctx.clone()).run();

    // give leader controller time to startup
    sleep(Duration::from_millis(MAX_WAIT_LEADER)).await;

    let (_, follower_replica1) = builder.follower_replica(0).await;

    // at this point, follower replica should be empty since we didn't have time to sync up with leader
    assert_eq!(follower_replica1.leo(), 0);
    assert_eq!(follower_replica1.hw(), 0);

    let (_, follower_replica2) = builder.follower_replica(1).await;

    // at this point, follower replica should be empty since we didn't have time to sync up with leader
    assert_eq!(follower_replica2.leo(), 0);
    assert_eq!(follower_replica2.hw(), 0);

    // wait for followers to sync with leader
    sleep(Duration::from_millis(MAX_WAIT_FOLLOWER)).await;

    // leader should now states from follower
    let follower_info = leader_replica.followers_info().await;
    assert_eq!(follower_info.get(&FOLLOWER1).unwrap().leo, 0);
    assert_eq!(follower_info.get(&FOLLOWER2).unwrap().leo, 0);

    // write records
    leader_replica
        .write_record_set(
            &mut create_raw_recordset(2),
            leader_gctx.follower_notifier(),
        )
        .await
        .expect("write");

    assert_eq!(leader_replica.leo(), 2);
    assert_eq!(leader_replica.hw(), 0);

    // wait until follower sync up with leader
    sleep(Duration::from_millis(*MAX_WAIT_REPLICATION)).await;

    debug!("done waiting. checking result");

    // all records has been fully replicated
    assert_eq!(follower_replica1.leo(), 2);
    assert_eq!(follower_replica1.hw(), 2);
    assert_eq!(leader_replica.hw(), 2);
    assert_eq!(follower_replica2.leo(), 2);
    assert_eq!(follower_replica2.hw(), 2);

    // await while controllers terminate
    sleep(Duration::from_millis(WAIT_TERMINATE)).await;

    spu_server.notify();
}

/// Test 2 replica
/// Replicating new records
///    
#[fluvio_future::test(ignore)]
async fn test_replication2_promote() {
    let port = portpicker::pick_unused_port().expect("No free ports left");
    let builder = TestConfig::builder()
        .followers(1_u16)
        .base_port(port)
        .generate("replication2_promote");

    let (leader_gctx, leader_replica) = builder.leader_replica().await;
    assert_eq!(leader_replica.leo(), 0);

    let follower_info = leader_replica.followers_info().await;
    assert_eq!(follower_info.get(&5002).unwrap().leo, -1);

    let spu_server = create_internal_server(builder.leader_addr(), leader_gctx.clone()).run();

    // give leader controller time to startup
    sleep(Duration::from_millis(MAX_WAIT_LEADER)).await;

    let (follower_ctx, _follower_replica) = builder.follower_replica(0).await;
    let old_replica = builder.replica();

    // switch leader and follower
    let mut new_replica = old_replica.clone();
    new_replica.leader = FOLLOWER1;
    new_replica.replicas = vec![LEADER];

    sleep(Duration::from_millis(MAX_WAIT_FOLLOWER)).await;
    sleep(Duration::from_millis(*MAX_WAIT_REPLICATION)).await;

    // check follower replica exists before
    assert!(
        follower_ctx
            .followers_state()
            .get(&new_replica.id)
            .await
            .is_some()
    );

    // promote ctx
    follower_ctx.promote(&new_replica, &old_replica).await;

    // ensure follower ctx is removed
    assert!(
        follower_ctx
            .followers_state()
            .get(&new_replica.id)
            .await
            .is_none()
    );

    // ensure leader ctx is there
    assert!(
        follower_ctx
            .leaders_state()
            .get(&new_replica.id)
            .await
            .is_some()
    );

    sleep(Duration::from_millis(WAIT_TERMINATE)).await;

    spu_server.notify();
}

/// Test leader and follower starts in sequence
/// receiving request from SC
#[fluvio_future::test(ignore)]
async fn test_replication_dispatch_in_sequence() {
    let port = portpicker::pick_unused_port().expect("No free ports left");
    let builder = TestConfig::builder()
        .followers(1_u16)
        .base_port(port)
        .generate("replication_dispatch_in_sequence");

    let leader_gctx = builder.leader_ctx().await;

    let spu_server = create_internal_server(builder.leader_addr(), leader_gctx.clone()).run();

    let replica = builder.replica();

    let actions = leader_gctx
        .apply_replica_update(UpdateReplicaRequest::with_all(1, vec![replica.clone()]))
        .await;
    assert!(actions.is_empty());

    // give leader controller time to startup
    sleep(Duration::from_millis(MAX_WAIT_LEADER)).await;

    let leader = leader_gctx
        .leaders_state()
        .get(&replica.id)
        .await
        .expect("replica");
    assert!(
        leader_gctx
            .followers_state()
            .get(&replica.id)
            .await
            .is_none()
    );
    // should be new
    assert_eq!(leader.leo(), 0);
    assert_eq!(leader.hw(), 0);

    leader
        .write_record_set(
            &mut create_raw_recordset(2),
            leader_gctx.follower_notifier(),
        )
        .await
        .expect("write");

    assert_eq!(leader.leo(), 2);
    assert_eq!(leader.hw(), 0);

    let follower_gctx = builder.follower_ctx(0).await;
    let actions = follower_gctx
        .apply_replica_update(UpdateReplicaRequest::with_all(1, vec![replica.clone()]))
        .await;
    assert!(actions.is_empty());
    let follower = follower_gctx
        .followers_state()
        .get(&replica.id)
        .await
        .expect("follower");
    assert_eq!(follower.leader(), LEADER);
    assert_eq!(follower.leo(), 0);
    assert_eq!(follower.hw(), 0);

    // wait until follower sync up with leader
    sleep(Duration::from_millis(*MAX_WAIT_REPLICATION)).await;
    assert_eq!(follower.leo(), 2);

    // hw has been replicated
    assert_eq!(follower.hw(), 2);
    assert_eq!(leader.hw(), 2);

    sleep(Duration::from_millis(WAIT_TERMINATE)).await;

    spu_server.notify();
}

/// Test leader and follower starts in sequence
/// receiving request from SC
#[fluvio_future::test(ignore)]
async fn test_replication_dispatch_out_of_sequence() {
    //std::env::set_var("FLV_SHORT_RECONCILLATION", "1");
    let port = portpicker::pick_unused_port().expect("No free ports left");
    let builder = TestConfig::builder()
        .followers(1_u16)
        .base_port(port)
        .generate("replication_dispatch_out_of_sequence");

    let replica = builder.replica();

    let leader_gctx = builder.leader_ctx().await;

    let spu_server = create_internal_server(builder.leader_addr(), leader_gctx.clone()).run();

    // start the follower way before leader
    let follower_gctx = builder.follower_ctx(0).await;
    let actions = follower_gctx
        .apply_replica_update(UpdateReplicaRequest::with_all(1, vec![replica.clone()]))
        .await;
    assert!(actions.is_empty());
    let follower = follower_gctx
        .followers_state()
        .get(&replica.id)
        .await
        .expect("follower");
    assert_eq!(follower.leader(), LEADER);
    assert_eq!(follower.leo(), 0);
    assert_eq!(follower.hw(), 0);

    // delay leader's start
    sleep(Duration::from_millis(300)).await;

    let actions = leader_gctx
        .apply_replica_update(UpdateReplicaRequest::with_all(1, vec![replica.clone()]))
        .await;
    assert!(actions.is_empty());

    // give leader controller time to startup
    sleep(Duration::from_millis(MAX_WAIT_LEADER)).await;

    let leader = leader_gctx
        .leaders_state()
        .get(&replica.id)
        .await
        .expect("replica");
    assert!(
        leader_gctx
            .followers_state()
            .get(&replica.id)
            .await
            .is_none()
    );
    // should be new
    assert_eq!(leader.leo(), 0);
    assert_eq!(leader.hw(), 0);

    leader
        .write_record_set(
            &mut create_raw_recordset(2),
            leader_gctx.follower_notifier(),
        )
        .await
        .expect("write");

    // wait until follower sync up with leader
    sleep(Duration::from_secs(15)).await;
    assert_eq!(follower.leo(), 2);

    // hw has been replicated
    assert_eq!(follower.hw(), 2);
    assert_eq!(leader.hw(), 2);

    sleep(Duration::from_millis(WAIT_TERMINATE)).await;

    spu_server.notify();
}

#[fluvio_future::test()]
async fn test_replica_state_cleans_up_offset_producers() {
    let port = portpicker::pick_unused_port().expect("No free ports left");
    let builder = TestConfig::builder()
        .base_port(port)
        .generate("just_leader");

    let (_leader_gctx, leader_replica) = builder.leader_replica().await;

    //leader_replica.register_offset_publisher(offset_publisher)
    let shared_publishers = leader_replica.consumer_offset_publishers();

    {
        let publishers = shared_publishers.lock().await;
        assert!(publishers.is_empty());
    }

    // Add 10 publishers and let them drop, should correspond to replica_state::CLEANUP_FREQUENCY
    for i in 1..11 {
        let new_publisher = Arc::new(OffsetPublisher::new(0));
        leader_replica
            .register_offset_publisher(&new_publisher)
            .await;
        let publishers = shared_publishers.lock().await;
        assert!(publishers.len() == i);
    }

    // Add one final publisher and ensure dropped publishers are cleaned up
    let new_publisher = Arc::new(OffsetPublisher::new(0));
    leader_replica
        .register_offset_publisher(&new_publisher)
        .await;
    let publishers = shared_publishers.lock().await;
    assert!(publishers.len() == 1);
}

/// Test 2 replicas but one replica is rejected, and than both is sync
#[fluvio_future::test(ignore)]
async fn test_sync_2_replicas_but_one_reject() {
    let port = portpicker::pick_unused_port().expect("No free ports left");
    let builder = TestConfig::builder()
        .followers(2_u16)
        .base_port(port)
        .generate("replication_dispatch_in_sequence");
    let replica_test1 = builder.replica();
    let (leader_gctx, leader_replica) = builder.leader_replica().await;
    let spu_server = create_internal_server(builder.leader_addr(), leader_gctx.clone()).run();

    let follower_gctx = builder.follower_ctx(0).await;
    let mut replicas = vec![replica_test1.clone()];
    follower_gctx
        .replica_localstore()
        .sync_all(replicas.clone().to_vec());
    follower_gctx
        .followers_state_owned()
        .add_replica(&follower_gctx, replica_test1.clone())
        .await
        .expect("create");

    sleep(Duration::from_millis(*MAX_WAIT_REPLICATION)).await;
    let replica_test2 = builder.replica_inner("test2".to_owned());
    replicas.push(replica_test2.clone());
    follower_gctx
        .replica_localstore()
        .sync_all(replicas.clone().to_vec());
    follower_gctx
        .followers_state_owned()
        .add_replica(&follower_gctx, replica_test2.clone())
        .await
        .expect("create");

    leader_replica
        .write_record_set(
            &mut create_raw_recordset(2),
            leader_gctx.follower_notifier(),
        )
        .await
        .expect("write");

    assert_eq!(leader_replica.leo(), 2);

    sleep(Duration::from_millis(*MAX_WAIT_REPLICATION)).await;
    let actions = leader_gctx
        .apply_replica_update(UpdateReplicaRequest::with_all(1, replicas.clone()))
        .await;
    assert!(actions.is_empty());

    let (leader_gctx2, leader_replica2) = builder.leader_replica_inner(replica_test2.clone()).await;

    sleep(Duration::from_secs(REJECT_WAIT)).await;

    leader_replica2
        .write_record_set(
            &mut create_raw_recordset(2),
            leader_gctx2.follower_notifier(),
        )
        .await
        .expect("write");
    leader_replica
        .write_record_set(
            &mut create_raw_recordset(2),
            leader_gctx.follower_notifier(),
        )
        .await
        .expect("write");

    sleep(Duration::from_millis(WAIT_TERMINATE)).await;

    assert_eq!(leader_replica.leo(), 4);
    assert_eq!(leader_replica2.leo(), 2);

    sleep(Duration::from_millis(WAIT_TERMINATE)).await;

    spu_server.notify();
}

#[fluvio_future::test(ignore)]
async fn test_sync_larger_records() {
    let num_records_total = 100;
    let num_records_per_batch = 10;
    let num_batches = 10;
    let port = portpicker::pick_unused_port().expect("No free ports left");
    let builder = TestConfig::builder()
        .followers(1_u16)
        .base_port(port)
        .generate("sync_larger_records");

    let leader_gctx = builder.leader_ctx().await;

    let spu_server = create_internal_server(builder.leader_addr(), leader_gctx.clone()).run();

    let replica = builder.replica();

    let actions = leader_gctx
        .apply_replica_update(UpdateReplicaRequest::with_all(1, vec![replica.clone()]))
        .await;
    assert!(actions.is_empty());

    // give leader controller time to startup
    sleep(Duration::from_millis(MAX_WAIT_LEADER)).await;

    let leader = leader_gctx
        .leaders_state()
        .get(&replica.id)
        .await
        .expect("replica");
    assert!(
        leader_gctx
            .followers_state()
            .get(&replica.id)
            .await
            .is_none()
    );
    // should be new
    assert_eq!(leader.leo(), 0);
    assert_eq!(leader.hw(), 0);

    // create a raw record with 512kb
    let mut rng = rand::thread_rng();
    let records = Vec::from_iter(
        (0..num_batches)
            .map(|_| {
                let mut record = vec![0; 512 * 1024];
                rng.fill(&mut record[..]);
                record
            })
            .collect::<Vec<Vec<u8>>>()
            .into_iter(),
    );

    // write 10 batches with 10 records with 512kb, total of 5MB per batch and 50MB total
    for record in &records {
        leader
            .write_record_set(
                &mut create_raw_recordset_inner(num_records_per_batch, record),
                leader_gctx.follower_notifier(),
            )
            .await
            .expect("write");
    }

    assert_eq!(leader.leo(), num_records_total as i64);
    assert_eq!(leader.hw(), 0);

    let follower_gctx = builder.follower_ctx(0).await;
    let actions = follower_gctx
        .apply_replica_update(UpdateReplicaRequest::with_all(1, vec![replica.clone()]))
        .await;
    assert!(actions.is_empty());
    let follower = follower_gctx
        .followers_state()
        .get(&replica.id)
        .await
        .expect("follower");
    assert_eq!(follower.leader(), LEADER);
    assert_eq!(follower.leo(), 0);
    assert_eq!(follower.hw(), 0);

    // wait until follower sync up with leader
    sleep(Duration::from_millis(*MAX_WAIT_REPLICATION)).await;
    assert_eq!(follower.leo(), num_records_total as i64);

    // hw has been replicated
    assert_eq!(follower.hw(), num_records_total as i64);
    assert_eq!(leader.hw(), num_records_total as i64);

    // check if the records are the same
    let leader_replica = leader
        .read_records(0, num_records_total as u32, Isolation::ReadCommitted)
        .await
        .expect("read leader records");
    let follower_replica = follower
        .read_records(0, num_records_total as u32, Isolation::ReadCommitted)
        .await
        .expect("read follower records");

    assert_eq!(leader_replica.start, follower_replica.start);
    assert_eq!(leader_replica.end, follower_replica.end);
    let leader_slice = leader_replica.file_slice.expect("slice");
    let follower_slice = follower_replica.file_slice.expect("slice");

    assert_eq!(leader_slice.len(), num_records_total as u64);
    assert_eq!(follower_slice.len(), num_records_total as u64);

    follower_gctx
        .replica_localstore()
        .sync_all(vec![replica.clone()]);

    let mut batch_leader = FileBatchIterator::from_raw_slice(leader_slice);
    let mut batch_follower = FileBatchIterator::from_raw_slice(follower_slice);

    let mut leader_batches = vec![];
    let mut follower_batches = vec![];
    while let Some(Ok(record)) = batch_leader.next() {
        leader_batches.push(record);
    }
    while let Some(Ok(record)) = batch_follower.next() {
        follower_batches.push(record);
    }
    assert_eq!(leader_batches.len(), 1);
    assert_eq!(follower_batches.len(), 1);

    assert_eq!(
        leader_batches[0].batch.base_offset,
        follower_batches[0].batch.base_offset
    );
    assert_eq!(
        leader_batches[0].batch.batch_len,
        follower_batches[0].batch.batch_len
    );

    assert_eq!(leader_batches[0].records, follower_batches[0].records);
    for (record_leader, record_follower) in leader_batches[0]
        .records
        .iter()
        .zip(follower_batches[0].records.iter())
    {
        assert_eq!(record_leader, record_follower);
    }

    sleep(Duration::from_millis(WAIT_TERMINATE)).await;

    spu_server.notify();
}
