/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::ops::DerefMut;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use cached_config::ConfigHandle;
use cached_config::ConfigStore;
use fbinit::FacebookInit;
use futures::try_join;
use metaconfig_types::BlobConfig;
use metaconfig_types::DatabaseConfig;
use metaconfig_types::ShardableRemoteDatabaseConfig;
use metaconfig_types::StorageConfig;
use replication_lag_config::ReplicationLagBlobstoreConfig;
use replication_lag_config::ReplicationLagTableConfig;
use slog::info;
use slog::Logger;
#[cfg(fbcode_build)]
use sql_ext::facebook::MyAdmin;
use sql_ext::replication::NoReplicaLagMonitor;
use sql_ext::replication::ReplicaLagMonitor;
use sql_ext::replication::WaitForReplicationConfig;
use tokio::sync::Mutex;

#[derive(Default)]
struct State {
    last_sync_queue_lag: Option<(Instant, Duration)>,
    last_xdb_blobstore_lag: Option<(Instant, Duration)>,
}

#[derive(Clone)]
pub struct WaitForReplication {
    config_handle: ConfigHandle<ReplicationLagBlobstoreConfig>,
    sync_queue_monitor: Arc<dyn ReplicaLagMonitor>,
    xdb_blobstore_monitor: Arc<dyn ReplicaLagMonitor>,
    state: Arc<Mutex<State>>,
}

const CONFIGS_PATH: &str = "scm/mononoke/mysql/replication_lag/config";

impl WaitForReplication {
    pub fn new(
        fb: FacebookInit,
        config_store: &ConfigStore,
        storage_config: StorageConfig,
        config_name: &'static str,
    ) -> Result<Self> {
        let config_handle =
            config_store.get_config_handle(format!("{}/{}", CONFIGS_PATH, config_name))?;
        let (sync_queue_monitor, xdb_blobstore_monitor) = match storage_config.blobstore {
            BlobConfig::Multiplexed {
                blobstores,
                queue_db: DatabaseConfig::Remote(remote),
                ..
            } => {
                #[cfg(fbcode_build)]
                {
                    let my_admin = MyAdmin::new(fb)?;
                    let sync_queue = Arc::new(my_admin.single_shard_lag_monitor(remote.db_address))
                        as Arc<dyn ReplicaLagMonitor>;
                    let xdb_blobstore = blobstores
                        .into_iter()
                        .find_map(|(_, _, config)| match config {
                            BlobConfig::Mysql {
                                remote: ShardableRemoteDatabaseConfig::Unsharded(remote),
                            } => Some(
                                Arc::new(my_admin.single_shard_lag_monitor(remote.db_address))
                                    as Arc<dyn ReplicaLagMonitor>,
                            ),
                            BlobConfig::Mysql {
                                remote: ShardableRemoteDatabaseConfig::Sharded(remote),
                            } => Some(Arc::new(my_admin.shardmap_lag_monitor(remote.shard_map))),
                            _ => None,
                        })
                        .unwrap_or_else(|| Arc::new(NoReplicaLagMonitor()));
                    (sync_queue, xdb_blobstore)
                }
                #[cfg(not(fbcode_build))]
                {
                    let _ = (remote, blobstores);
                    unimplemented!()
                }
            }
            _ => (
                Arc::new(NoReplicaLagMonitor()) as Arc<dyn ReplicaLagMonitor>,
                Arc::new(NoReplicaLagMonitor()) as Arc<dyn ReplicaLagMonitor>,
            ),
        };
        Ok(Self {
            config_handle,
            sync_queue_monitor,
            xdb_blobstore_monitor,
            state: Arc::new(Mutex::new(State::default())),
        })
    }

    pub async fn wait_for_replication(&self, logger: &Logger) -> Result<()> {
        let config = self.config_handle.get();
        let mut state_lock = self.state.lock().await;
        let State {
            last_sync_queue_lag,
            last_xdb_blobstore_lag,
        } = state_lock.deref_mut();
        try_join!(
            self.wait_for_table(
                logger,
                "sync queue",
                last_sync_queue_lag,
                &self.sync_queue_monitor,
                config.sync_queue.as_ref()
            ),
            self.wait_for_table(
                logger,
                "XDB blobstore",
                last_xdb_blobstore_lag,
                &self.xdb_blobstore_monitor,
                config.xdb_blobstore.as_ref()
            ),
        )?;
        Ok(())
    }

    async fn wait_for_table<'a>(
        &'a self,
        logger: &'a Logger,
        name: &'static str,
        last_lag: &'a mut Option<(Instant, Duration)>,
        monitor: &'a Arc<dyn ReplicaLagMonitor>,
        config: Option<&'a ReplicationLagTableConfig>,
    ) -> Result<()> {
        if let Some(raw_config) = config {
            let max_replication_lag_allowed =
                Duration::from_millis(raw_config.max_replication_lag_allowed_ms.try_into()?);
            let poll_interval = Duration::from_millis(raw_config.poll_interval_ms.try_into()?);
            match last_lag.as_mut() {
                // If queried too recently, just assume it's all ok.
                Some((instant, duration))
                    if instant.elapsed() < poll_interval
                        && *duration < max_replication_lag_allowed =>
                {
                    return Ok(());
                }
                // If impossible to have surpassed replication_lag, don't query
                Some((instant, duration))
                    if *duration + instant.elapsed() < max_replication_lag_allowed =>
                {
                    return Ok(());
                }
                _ => {}
            }
            info!(
                logger,
                "Waiting for replication lag on {} to drop below {:?}",
                name,
                max_replication_lag_allowed
            );
            let config =
                WaitForReplicationConfig::new(max_replication_lag_allowed, poll_interval, logger);
            let new_last_lag = monitor.wait_for_replication(&config).await?;
            *last_lag = Some((Instant::now(), new_last_lag.delay));
        }
        Ok(())
    }
}
