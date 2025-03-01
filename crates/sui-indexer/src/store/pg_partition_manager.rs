// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use diesel::r2d2::R2D2Connection;
use diesel::sql_types::{BigInt, VarChar};
use diesel::{QueryableByName, RunQueryDsl};
use std::collections::BTreeMap;
use std::time::Duration;
use tracing::{error, info};

use crate::db::ConnectionPool;
use crate::errors::IndexerError;
use crate::handlers::EpochToCommit;
use crate::models::epoch::StoredEpochInfo;
use crate::store::diesel_macro::*;
use downcast::Any;

const GET_PARTITION_SQL: &str = r"
SELECT parent.relname                                            AS table_name,
       MIN(CAST(SUBSTRING(child.relname FROM '\d+$') AS BIGINT)) AS first_partition,
       MAX(CAST(SUBSTRING(child.relname FROM '\d+$') AS BIGINT)) AS last_partition
FROM pg_inherits
         JOIN pg_class parent ON pg_inherits.inhparent = parent.oid
         JOIN pg_class child ON pg_inherits.inhrelid = child.oid
         JOIN pg_namespace nmsp_parent ON nmsp_parent.oid = parent.relnamespace
         JOIN pg_namespace nmsp_child ON nmsp_child.oid = child.relnamespace
WHERE parent.relkind = 'p'
GROUP BY table_name;
";

pub struct PgPartitionManager<T: R2D2Connection + 'static> {
    cp: ConnectionPool<T>,
}

impl<T: R2D2Connection> Clone for PgPartitionManager<T> {
    fn clone(&self) -> PgPartitionManager<T> {
        Self {
            cp: self.cp.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct EpochPartitionData {
    last_epoch: u64,
    next_epoch: u64,
    last_epoch_start_cp: u64,
    next_epoch_start_cp: u64,
}

impl EpochPartitionData {
    pub fn compose_data(epoch: EpochToCommit, last_db_epoch: StoredEpochInfo) -> Self {
        let last_epoch = last_db_epoch.epoch as u64;
        let last_epoch_start_cp = last_db_epoch.first_checkpoint_id as u64;
        let next_epoch = epoch.new_epoch.epoch;
        let next_epoch_start_cp = epoch.new_epoch.first_checkpoint_id;
        Self {
            last_epoch,
            next_epoch,
            last_epoch_start_cp,
            next_epoch_start_cp,
        }
    }
}

impl<T: R2D2Connection> PgPartitionManager<T> {
    pub fn new(cp: ConnectionPool<T>) -> Result<Self, IndexerError> {
        let manager = Self { cp };
        let tables = manager.get_table_partitions()?;
        info!(
            "Found {} tables with partitions : [{:?}]",
            tables.len(),
            tables
        );
        Ok(manager)
    }

    pub fn get_table_partitions(&self) -> Result<BTreeMap<String, (u64, u64)>, IndexerError> {
        #[derive(QueryableByName, Debug, Clone)]
        struct PartitionedTable {
            #[diesel(sql_type = VarChar)]
            table_name: String,
            #[diesel(sql_type = BigInt)]
            first_partition: i64,
            #[diesel(sql_type = BigInt)]
            last_partition: i64,
        }

        Ok(
            read_only_blocking!(&self.cp, |conn| diesel::RunQueryDsl::load(
                diesel::sql_query(GET_PARTITION_SQL),
                conn
            ))?
            .into_iter()
            .map(|table: PartitionedTable| {
                (
                    table.table_name,
                    (table.first_partition as u64, table.last_partition as u64),
                )
            })
            .collect(),
        )
    }

    pub fn advance_and_prune_epoch_partition(
        &self,
        table: String,
        first_partition: u64,
        last_partition: u64,
        data: &EpochPartitionData,
        epochs_to_keep: Option<u64>,
    ) -> Result<(), IndexerError> {
        if data.next_epoch == 0 {
            tracing::info!("Epoch 0 partition has been created in the initial setup.");
            return Ok(());
        }
        if last_partition == data.last_epoch {
            transactional_blocking_with_retry!(
                &self.cp,
                |conn| {
                    RunQueryDsl::execute(
                        diesel::sql_query("CALL advance_partition($1, $2, $3, $4, $5)")
                            .bind::<diesel::sql_types::Text, _>(table.clone())
                            .bind::<diesel::sql_types::BigInt, _>(data.last_epoch as i64)
                            .bind::<diesel::sql_types::BigInt, _>(data.next_epoch as i64)
                            .bind::<diesel::sql_types::BigInt, _>(data.last_epoch_start_cp as i64)
                            .bind::<diesel::sql_types::BigInt, _>(data.next_epoch_start_cp as i64),
                        conn,
                    )
                },
                Duration::from_secs(10)
            )?;
            info!(
                "Advanced epoch partition for table {} from {} to {}",
                table, last_partition, data.next_epoch
            );

            // prune old partitions beyond the retention period
            if let Some(epochs_to_keep) = epochs_to_keep {
                for epoch in first_partition..(data.next_epoch - epochs_to_keep + 1) {
                    transactional_blocking_with_retry!(
                        &self.cp,
                        |conn| {
                            RunQueryDsl::execute(
                                diesel::sql_query("CALL drop_partition($1, $2)")
                                    .bind::<diesel::sql_types::Text, _>(table.clone())
                                    .bind::<diesel::sql_types::BigInt, _>(epoch as i64),
                                conn,
                            )
                        },
                        Duration::from_secs(10)
                    )?;
                    info!("Dropped epoch partition {} for table {}", epoch, table);
                }
            }
        } else if last_partition != data.next_epoch {
            // skip when the partition is already advanced once, which is possible when indexer
            // crashes and restarts; error otherwise.
            error!(
                "Epoch partition for table {} is not in sync with the last epoch {}.",
                table, data.last_epoch
            );
        }
        Ok(())
    }
}
