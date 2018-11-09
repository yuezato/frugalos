use atomic_immut::AtomicImmut;
use cannyls;
use cannyls::device::{Device, DeviceHandle};
use cannyls_rpc::DeviceId;
use cannyls_rpc::DeviceRegistryHandle;
use fibers::sync::oneshot;
use fibers::Spawn;
use fibers_rpc::client::ClientServiceHandle as RpcServiceHandle;
use fibers_rpc::server::ServerBuilder as RpcServerBuilder;
use fibers_tasque;
use fibers_tasque::TaskQueueExt;
use frugalos_config::{DeviceGroup, Event as ConfigEvent, Service as ConfigService};
use frugalos_raft::Service as RaftService;
use frugalos_segment;
use frugalos_segment::Service as SegmentService;
use futures::future::Fuse;
use futures::{Async, Future, Poll, Stream};
use libfrugalos::entity::bucket::{Bucket as BucketConfig, BucketId};
use libfrugalos::entity::device::{
    Device as DeviceConfig, FileDevice as FileDeviceConfig, MemoryDevice as MemoryDeviceConfig,
};
use libfrugalos::entity::server::{Server, ServerId};
use prometrics::metrics::MetricBuilder;
use slog::Logger;
use std::collections::HashMap;
use std::sync::Arc;
use trackable::error::ErrorKindExt;

use bucket::Bucket;
use client::FrugalosClient;
use {Error, ErrorKind, Result};

#[derive(Debug)]
pub struct PhysicalDevice {
    id: DeviceId,
    server: ServerId,
}

// TODO: Remove `S`
pub struct Service<S> {
    logger: Logger,
    local_server: Server,
    rpc_service: RpcServiceHandle,
    raft_service: RaftService,
    frugalos_segment_service: SegmentService<S>,
    config_service: ConfigService,

    // このサーバが所有するデバイス一覧
    local_devices: HashMap<u32, LocalDevice>,

    // クラスタ全体の情報
    seqno_to_device: HashMap<u32, PhysicalDevice>,

    buckets: Arc<AtomicImmut<HashMap<BucketId, Bucket>>>,
    bucket_no_to_id: HashMap<u32, BucketId>,

    servers: HashMap<ServerId, Server>,
}
impl<S> Service<S>
where
    S: Spawn + Send + Clone + 'static,
{
    pub fn new(
        logger: Logger,
        spawner: S,
        raft_service: RaftService,
        config_service: ConfigService,
        rpc: &mut RpcServerBuilder,
        rpc_service: RpcServiceHandle,
    ) -> Result<Self> {
        let frugalos_segment_service = track!(SegmentService::new(
            logger.clone(),
            spawner,
            rpc_service.clone(),
            rpc,
            raft_service.handle(),
        ))?;
        Ok(Service {
            logger,
            local_server: config_service.local_server().clone(),
            rpc_service,
            raft_service,
            frugalos_segment_service,
            config_service,

            local_devices: HashMap::new(),
            seqno_to_device: HashMap::new(),
            buckets: Arc::new(AtomicImmut::new(HashMap::new())),
            bucket_no_to_id: HashMap::new(),
            servers: HashMap::new(),
        })
    }
    pub fn client(&self) -> FrugalosClient {
        FrugalosClient::new(self.buckets.clone())
    }
    pub fn stop(&mut self) {
        self.frugalos_segment_service.stop();
    }
    pub fn take_snapshot(&mut self) {
        self.frugalos_segment_service.take_snapshot();
    }
    fn handle_config_event(&mut self, event: ConfigEvent) -> Result<()> {
        info!(self.logger, "Configuration Event: {:?}", event);
        match event {
            ConfigEvent::PutDevice(device) => {
                if let Some(server) = device.server() {
                    let d = PhysicalDevice {
                        id: DeviceId::new(device.id().clone()),
                        server: server.clone(),
                    };
                    self.seqno_to_device.insert(device.seqno(), d);
                }
                if device
                    .server()
                    .map_or(false, |s| *s == self.local_server.id)
                {
                    // device.serverはいまPUTされんとしているdeviceの紐づけたいserverで
                    // local_serverはこのfrugalosプロセスに紐付いているサーバである。
                    // 従って、次のspawn_deviceが実行されるのは
                    // 新たにPUTしたいdeviceを紐づけたいfrugalosプロセスのみである。
                    track!(self.spawn_device(&device))?;
                }
            }
            ConfigEvent::DeleteDevice(device) => {
                self.seqno_to_device.remove(&device.seqno());
                // TODO:
                track_panic!(ErrorKind::Other, "Unsupported: {:?}", device);
            }
            ConfigEvent::PutBucket(bucket) => {
                track!(self.handle_put_bucket(&bucket))?;
            }
            ConfigEvent::DeleteBucket(bucket) => {
                track!(self.handle_delete_bucket(&bucket));
            }
            ConfigEvent::PatchSegment {
                bucket_no,
                segment_no,
                groups,
            } => {
                track_assert_eq!(groups.len(), 1, ErrorKind::Other, "Unimplemented");
                track!(self.handle_patch_segment(bucket_no, segment_no, &groups[0]))?;
            }
            ConfigEvent::PutServer(server) => {
                self.servers.insert(server.id.clone(), server);
            }
            ConfigEvent::DeleteServer(server) => {
                self.servers.remove(&server.id);
            }
        }
        Ok(())
    }
    fn handle_put_bucket(&mut self, bucket_config: &BucketConfig) -> Result<()> {
        let id = bucket_config.id().clone();
        self.bucket_no_to_id
            .insert(bucket_config.seqno(), id.clone());

        let bucket = Bucket::new(
            self.logger.clone(),
            self.rpc_service.clone(),
            &bucket_config,
        );

        let mut buckets = (&*self.buckets.load()).clone();
        buckets.insert(id, bucket);
        self.buckets.store(buckets);
        Ok(())
    }
    fn handle_delete_bucket(&mut self, bucket_config: &BucketConfig) -> Result<()> {
        use cannyls::lump::LumpId;
        use frugalos_raft::{LocalNodeId, NodeId};
        use frugalos_segment::config::ClusterMember;

        let id = bucket_config.id().clone(); // 削除するバケツのバケツ名
        let bucket_seqno = bucket_config.seqno(); // 削除するバケツのseqno

        // bucket_no_to_idテーブルからseqnoを外す
        self.bucket_no_to_id.remove(&bucket_seqno);

        // バケツテーブルからidを外す
        let mut buckets = (&*self.buckets.load()).clone();
        let bucket: Bucket = buckets[&id].clone();
        buckets.remove(&id);
        self.buckets.store(buckets);

        match bucket_config {
            BucketConfig::Metadata(ref _bucket) => {
                panic!("[metadata bucket] unimplemented");
            }
            BucketConfig::Replicated(ref _bucket) => {
                panic!("[replicated bucket] unimplemented");
            }
            BucketConfig::Dispersed(ref _bucket) => {
                use frugalos_segment::Client;

                // セグメントに相当するプロセスを全て終了させる。
                // まず、Raftノードを停止させる。
                {
                    // 1. バケツの配下にあるセグメントを全て求める
                    let segments: &[Client] = bucket.segments();
                    println!("segments.len = {}", segments.len());

                    // 2. 各セグメントに対して停止要求を流す
                    for i in 0..segments.len() {
                        let segment: &Client = &segments[i];
                        let members: &[ClusterMember] = segment.raft_segment_members().unwrap();
                        {
                            // raft_segment_membersはStorageClientから取得し
                            // raft_segment_members2はMdsClientから取得した
                            // Clientらで、二つが同じかどうかをなんとなく確かめている。
                            let members2: &[ClusterMember] = &segment.raft_segment_members2();
                            assert_eq!(members, members2);
                        }
                        // ClusterMemberからNodeIdへの変換を行う。
                        let node_ids: Vec<NodeId> =
                            members.to_vec().iter().map(|mem| mem.node).collect();

                        for n in node_ids {
                            self.frugalos_segment_service.handle().remove_node(n);
                        }
                    }
                }

                // 以下ではcannylsに入っているデータを消していく
                {
                    let mut deleting_futures = Vec::new();

                    // local_devices の中には virtual な device は出現しない
                    // これは `handle_config_event` の
                    // `PutDevice`-caseにおいて、`self.spawn_device`を呼び出すthen節に
                    // virtual deviceの場合には到達しないから。
                    // 当該のif文では、デバイスのserver値を読もうとするが
                    // virtual deviceにはserver値が無い(None)であるため。
                    for elem in self.local_devices.iter() {
                        let local_device = elem.1;

                        if let Some(ref device_handle) = local_device.handle() {
                            // Frugalos's lumpid space is the following
                            // 00000000(8bit)_[bucket_no(24bit)]_[segment_no(16bit)]_[mem_no(8bit)]_etc[72bit]
                            let target_start: u128 = u128::from(bucket_seqno) << (16 + 8 + 72);
                            let target_end: u128 = u128::from(bucket_seqno + 1) << (16 + 8 + 72);

                            let mut f = device_handle.request().delete_range(std::ops::Range {
                                start: LumpId::new(target_start),
                                end: LumpId::new(target_end),
                            });
                            deleting_futures.push(f);
                        } else {
                            // handle()がNoneを返す時には
                            // このバケツ経由ではこのデータを書き込んでいない筈であるから
                            // 何もすることはない
                        }
                    }

                    for mut f in deleting_futures {
                        while !(f.poll())?.is_ready() {}
                    }
                }

                Ok(())
            }
        }
    }
    fn handle_patch_segment(
        &mut self,
        bucket_no: u32,
        segment_no: u16,
        group: &DeviceGroup,
    ) -> Result<()> {
        // このグループに対応するRaftクラスタのメンバ群を用意
        let mut members = Vec::new();
        for (member_no, device_no) in (0..group.members.len()).zip(group.members.iter()) {
            use frugalos_raft::NodeId;
            let owner = &self.servers[&self.seqno_to_device[device_no].server];
            let node: NodeId = track!(
                format!(
                    "00{:06x}{:04x}{:02x}.{:x}@{}:{}",
                    bucket_no, segment_no, member_no, device_no, owner.host, owner.port
                ).parse()
            )?;
            members.push(node);
        }

        // バケツの更新
        if let Some(id) = self.bucket_no_to_id.get(&bucket_no) {
            // TODO: だいぶコスト高の操作なので、セグメント更新はバッチ的に行った方が良いかも
            let mut buckets = (&*self.buckets.load()).clone();
            let segment;
            {
                use frugalos_segment::config::ClusterMember;
                let bucket = buckets.get_mut(id).expect("Never fails");
                let members = members
                    .iter()
                    .zip(group.members.iter())
                    .map(|(&node, device_no)| ClusterMember {
                        node,
                        device: self.seqno_to_device[&device_no].id.clone().into_string(),
                    }).collect();
                bucket.update_segment(segment_no, members);
                segment = bucket.segments()[segment_no as usize].clone();
            }
            self.buckets.store(buckets);

            // このサーバが扱うべきRaftノードを起動
            for (node, device_no) in members.iter().zip(group.members.iter()) {
                let device_id = if let Some(id) = self.local_devices.get(&device_no).map(|d| d.id())
                {
                    id
                } else {
                    continue;
                };

                // TODO: 既に起動済みではないものだけを起動する
                info!(
                    self.logger,
                    "Add a node: {}",
                    dump!(bucket_no, segment_no, device_no, device_id, node)
                );

                let device_handle = self.local_devices.get_mut(&device_no).unwrap().watch();
                track!(
                    self.frugalos_segment_service.handle().add_node(
                        node.clone(),
                        Box::new(
                            device_handle.map_err(|e| frugalos_segment::ErrorKind::Other
                                .takes_over(e)
                                .into())
                        ),
                        segment.clone(),
                        members.iter().map(|n| n.to_raft_node_id()).collect(),
                    )
                )?;
            }
        } else {
            // 既に削除されているバケツのセグメント
            // (FIXME: タイミング的にここに来ることはなさそうなのでエラーでも良いかも)
        };

        Ok(())
    }
    fn spawn_device(&mut self, device_config: &DeviceConfig) -> Result<()> {
        let device = LocalDevice::new(
            self.logger.clone(),
            &device_config,
            self.frugalos_segment_service.device_registry().handle(),
        );
        self.local_devices.insert(device_config.seqno(), device);
        Ok(())
    }
}
impl<S> Future for Service<S>
where
    S: Spawn + Send + Clone + 'static,
{
    type Item = ();
    type Error = Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        track!(self.raft_service.poll())?;
        let ready = track!(self.frugalos_segment_service.poll())?.is_ready();
        if ready {
            info!(self.logger, "Frugalos service terminated");
            return Ok(Async::Ready(()));
        }
        while let Async::Ready(event) = track!(self.config_service.poll())? {
            let event = event.expect("Never fails");
            track!(self.handle_config_event(event))?;
        }

        for device in self.local_devices.values_mut() {
            if let Err(e) = track!(device.poll()) {
                error!(self.logger, "Device error: {}", e; "device" => device.id().as_str());
            }
        }

        Ok(Async::NotReady)
    }
}

#[derive(Debug)]
struct LocalDevice {
    logger: Logger,
    config: DeviceConfig,
    device_registry: DeviceRegistryHandle,
    handle: Option<DeviceHandle>,
    future: Fuse<fibers_tasque::AsyncCall<Result<Device>>>,
    watches: Vec<oneshot::Monitored<DeviceHandle, Error>>,
}
impl LocalDevice {
    fn new(logger: Logger, config: &DeviceConfig, device_registry: DeviceRegistryHandle) -> Self {
        info!(logger, "Starts spawning new device: {:?}", config);
        LocalDevice {
            logger,
            config: config.clone(),
            device_registry,
            handle: None,
            future: spawn_device(config).fuse(),
            watches: Vec::new(),
        }
    }
    fn id(&self) -> DeviceId {
        DeviceId::new(self.config.id().clone())
    }
    fn handle(&self) -> Option<DeviceHandle> {
        self.handle.clone()
    }
    fn watch(&mut self) -> WatchDeviceHandle {
        if let Some(ref d) = self.handle {
            WatchDeviceHandle::Ok(d.clone())
        } else {
            let (tx, rx) = oneshot::monitor();
            self.watches.push(tx);
            WatchDeviceHandle::Wait(rx)
        }
    }
}
impl Future for LocalDevice {
    type Item = ();
    type Error = Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Async::Ready(device) = track!(self.future.poll().map_err(Error::from))? {
            info!(self.logger, "New device spawned: {:?}", self.config);
            let device = track!(device.map_err(Error::from))?;
            let handle = device.handle();
            self.handle = Some(device.handle());
            let _ = self.device_registry.put_device(self.id(), device); // TODO: handle result(?)
            for w in self.watches.drain(..) {
                w.exit(Ok(handle.clone()));
            }
        }
        Ok(Async::NotReady)
    }
}

#[derive(Debug)]
pub enum WatchDeviceHandle {
    Ok(DeviceHandle),
    Wait(oneshot::Monitor<DeviceHandle, Error>),
}
impl Future for WatchDeviceHandle {
    type Item = DeviceHandle;
    type Error = Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match *self {
            WatchDeviceHandle::Ok(ref h) => Ok(Async::Ready(h.clone())),
            WatchDeviceHandle::Wait(ref mut f) => track!(f.poll().map_err(Error::from)),
        }
    }
}

fn spawn_device(device: &DeviceConfig) -> fibers_tasque::AsyncCall<Result<Device>> {
    use libfrugalos::entity::device::Device;

    match *device {
        Device::Virtual(_) => {
            fibers_tasque::DefaultIoTaskQueue.async_call(|| track_panic!(ErrorKind::Other))
        }
        Device::Memory(ref d) => spawn_memory_device(d),
        Device::File(ref d) => spawn_file_device(d),
    }
}

fn spawn_memory_device(device: &MemoryDeviceConfig) -> fibers_tasque::AsyncCall<Result<Device>> {
    let metrics = MetricBuilder::new()
        .label("device", device.id.as_ref())
        .clone();
    let nvm = cannyls::nvm::MemoryNvm::new(vec![0; device.capacity as usize]);
    let mut storage = cannyls::storage::StorageBuilder::new();
    storage.metrics(metrics.clone());
    fibers_tasque::DefaultIoTaskQueue.async_call(move || {
        let storage = track!(storage.create(nvm).map_err(Error::from))?;
        let device = cannyls::device::DeviceBuilder::new()
            .metrics(metrics)
            .spawn(|| Ok(storage)); // TODO: taskqueは止める
        Ok(device)
    })
}

fn spawn_file_device(device: &FileDeviceConfig) -> fibers_tasque::AsyncCall<Result<Device>> {
    use cannyls::nvm::FileNvmBuilder;
    let metrics = MetricBuilder::new()
        .label("device", device.id.as_ref())
        .clone();
    let filepath = device.filepath.clone();
    let capacity = device.capacity;
    let mut storage = cannyls::storage::StorageBuilder::new();
    storage.metrics(metrics.clone());
    fibers_tasque::DefaultIoTaskQueue.async_call(move || {
        let (nvm, created) = track!(
            FileNvmBuilder::new()
                .exclusive_lock(false)
                .create_if_absent(filepath, capacity)
                .map_err(Error::from)
        )?;
        let storage = if created {
            track!(storage.create(nvm).map_err(Error::from))?
        } else {
            track!(storage.open(nvm).map_err(Error::from))?
        };
        let device = cannyls::device::DeviceBuilder::new()
            .metrics(metrics)
            .spawn(|| Ok(storage)); // TODO: taskqueは止める
        Ok(device)
    })
}
