pub(crate) mod split_state;
use std::collections::HashMap;
use std::io::Write;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diesel::deserialize::{FromSql, FromSqlRow};
use diesel::expression::AsExpression;
use diesel::pg::Pg;
use diesel::prelude::*;
use diesel::serialize::{IsNull, ToSql};
use diesel_async::async_connection_wrapper::AsyncConnectionWrapper;
use diesel_async::pooled_connection::bb8::Pool;
use diesel_async::pooled_connection::{AsyncDieselConnectionManager, ManagerConfig};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use diesel_migrations::{EmbeddedMigrations, embed_migrations};
use futures::FutureExt;
use futures::future::BoxFuture;
use itertools::Itertools;
use pageserver_api::controller_api::{
    AvailabilityZone, MetadataHealthRecord, NodeLifecycle, NodeSchedulingPolicy, PlacementPolicy,
    SafekeeperDescribeResponse, ShardSchedulingPolicy, SkSchedulingPolicy,
};
use pageserver_api::models::{ShardImportStatus, TenantConfig};
use pageserver_api::shard::{
    ShardConfigError, ShardCount, ShardIdentity, ShardNumber, ShardStripeSize, TenantShardId,
};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};
use rustls::crypto::ring;
use safekeeper_api::membership::SafekeeperGeneration;
use scoped_futures::ScopedBoxFuture;
use serde::{Deserialize, Serialize};
use utils::generation::Generation;
use utils::id::{NodeId, TenantId, TimelineId};
use utils::lsn::Lsn;

use self::split_state::SplitState;
use crate::metrics::{
    DatabaseQueryErrorLabelGroup, DatabaseQueryLatencyLabelGroup, METRICS_REGISTRY,
};
use crate::node::Node;
use crate::timeline_import::{
    TimelineImport, TimelineImportUpdateError, TimelineImportUpdateFollowUp,
};
const MIGRATIONS: EmbeddedMigrations = embed_migrations!("./migrations");

/// ## What do we store?
///
/// The storage controller service does not store most of its state durably.
///
/// The essential things to store durably are:
/// - generation numbers, as these must always advance monotonically to ensure data safety.
/// - Tenant's PlacementPolicy and TenantConfig, as the source of truth for these is something external.
/// - Node's scheduling policies, as the source of truth for these is something external.
///
/// Other things we store durably as an implementation detail:
/// - Node's host/port: this could be avoided it we made nodes emit a self-registering heartbeat,
///   but it is operationally simpler to make this service the authority for which nodes
///   it talks to.
///
/// ## Performance/efficiency
///
/// The storage controller service does not go via the database for most things: there are
/// a couple of places where we must, and where efficiency matters:
/// - Incrementing generation numbers: the Reconciler has to wait for this to complete
///   before it can attach a tenant, so this acts as a bound on how fast things like
///   failover can happen.
/// - Pageserver re-attach: we will increment many shards' generations when this happens,
///   so it is important to avoid e.g. issuing O(N) queries.
///
/// Database calls relating to nodes have low performance requirements, as they are very rarely
/// updated, and reads of nodes are always from memory, not the database.  We only require that
/// we can UPDATE a node's scheduling mode reasonably quickly to mark a bad node offline.
pub struct Persistence {
    connection_pool: Pool<AsyncPgConnection>,
}

/// Legacy format, for use in JSON compat objects in test environment
#[derive(Serialize, Deserialize)]
struct JsonPersistence {
    tenants: HashMap<TenantShardId, TenantShardPersistence>,
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum DatabaseError {
    #[error(transparent)]
    Query(#[from] diesel::result::Error),
    #[error(transparent)]
    Connection(#[from] diesel::result::ConnectionError),
    #[error(transparent)]
    ConnectionPool(#[from] diesel_async::pooled_connection::bb8::RunError),
    #[error("Logical error: {0}")]
    Logical(String),
    #[error("Migration error: {0}")]
    Migration(String),
    #[error("CAS error: {0}")]
    Cas(String),
}

#[derive(measured::FixedCardinalityLabel, Copy, Clone)]
pub(crate) enum DatabaseOperation {
    InsertNode,
    UpdateNode,
    DeleteNode,
    ListNodes,
    ListTombstones,
    BeginShardSplit,
    CompleteShardSplit,
    AbortShardSplit,
    Detach,
    ReAttach,
    IncrementGeneration,
    TenantGenerations,
    ShardGenerations,
    ListTenantShards,
    LoadTenant,
    InsertTenantShards,
    UpdateTenantShard,
    DeleteTenant,
    UpdateTenantConfig,
    UpdateMetadataHealth,
    ListMetadataHealth,
    ListMetadataHealthUnhealthy,
    ListMetadataHealthOutdated,
    ListSafekeepers,
    GetLeader,
    UpdateLeader,
    SetPreferredAzs,
    InsertTimeline,
    UpdateTimeline,
    UpdateTimelineMembership,
    UpdateCplaneNotifiedGeneration,
    UpdateSkSetNotifiedGeneration,
    GetTimeline,
    InsertTimelineReconcile,
    RemoveTimelineReconcile,
    ListTimelineReconcile,
    ListTimelineReconcileStartup,
    InsertTimelineImport,
    UpdateTimelineImport,
    DeleteTimelineImport,
    ListTimelineImports,
    IsTenantImportingTimeline,
}

#[must_use]
pub(crate) enum AbortShardSplitStatus {
    /// We aborted the split in the database by reverting to the parent shards
    Aborted,
    /// The split had already been persisted.
    Complete,
}

pub(crate) type DatabaseResult<T> = Result<T, DatabaseError>;

/// Some methods can operate on either a whole tenant or a single shard
#[derive(Clone)]
pub(crate) enum TenantFilter {
    Tenant(TenantId),
    Shard(TenantShardId),
}

/// Represents the results of looking up generation+pageserver for the shards of a tenant
pub(crate) struct ShardGenerationState {
    pub(crate) tenant_shard_id: TenantShardId,
    pub(crate) generation: Option<Generation>,
    pub(crate) generation_pageserver: Option<NodeId>,
}

// A generous allowance for how many times we may retry serializable transactions
// before giving up.  This is not expected to be hit: it is a defensive measure in case we
// somehow engineer a situation where duelling transactions might otherwise live-lock.
const MAX_RETRIES: usize = 128;

impl Persistence {
    // The default postgres connection limit is 100.  We use up to 99, to leave one free for a human admin under
    // normal circumstances.  This assumes we have exclusive use of the database cluster to which we connect.
    pub const MAX_CONNECTIONS: u32 = 99;

    // We don't want to keep a lot of connections alive: close them down promptly if they aren't being used.
    const IDLE_CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);
    const MAX_CONNECTION_LIFETIME: Duration = Duration::from_secs(60);

    pub async fn new(database_url: String) -> Self {
        let mut mgr_config = ManagerConfig::default();
        mgr_config.custom_setup = Box::new(establish_connection_rustls);

        let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new_with_config(
            database_url,
            mgr_config,
        );

        // We will use a connection pool: this is primarily to _limit_ our connection count, rather than to optimize time
        // to execute queries (database queries are not generally on latency-sensitive paths).
        let connection_pool = Pool::builder()
            .max_size(Self::MAX_CONNECTIONS)
            .max_lifetime(Some(Self::MAX_CONNECTION_LIFETIME))
            .idle_timeout(Some(Self::IDLE_CONNECTION_TIMEOUT))
            // Always keep at least one connection ready to go
            .min_idle(Some(1))
            .test_on_check_out(true)
            .build(manager)
            .await
            .expect("Could not build connection pool");

        Self { connection_pool }
    }

    /// A helper for use during startup, where we would like to tolerate concurrent restarts of the
    /// database and the storage controller, therefore the database might not be available right away
    pub async fn await_connection(
        database_url: &str,
        timeout: Duration,
    ) -> Result<(), diesel::ConnectionError> {
        let started_at = Instant::now();
        log_postgres_connstr_info(database_url)
            .map_err(|e| diesel::ConnectionError::InvalidConnectionUrl(e.to_string()))?;
        loop {
            match establish_connection_rustls(database_url).await {
                Ok(_) => {
                    tracing::info!("Connected to database.");
                    return Ok(());
                }
                Err(e) => {
                    if started_at.elapsed() > timeout {
                        return Err(e);
                    } else {
                        tracing::info!("Database not yet available, waiting... ({e})");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }
    }

    /// Execute the diesel migrations that are built into this binary
    pub(crate) async fn migration_run(&self) -> DatabaseResult<()> {
        use diesel_migrations::{HarnessWithOutput, MigrationHarness};

        // Can't use self.with_conn here as we do spawn_blocking which requires static.
        let conn = self
            .connection_pool
            .dedicated_connection()
            .await
            .map_err(|e| DatabaseError::Migration(e.to_string()))?;
        let mut async_wrapper: AsyncConnectionWrapper<AsyncPgConnection> =
            AsyncConnectionWrapper::from(conn);
        tokio::task::spawn_blocking(move || {
            let mut retry_count = 0;
            loop {
                let result = HarnessWithOutput::write_to_stdout(&mut async_wrapper)
                    .run_pending_migrations(MIGRATIONS)
                    .map(|_| ())
                    .map_err(|e| DatabaseError::Migration(e.to_string()));
                match result {
                    Ok(r) => break Ok(r),
                    Err(
                        err @ DatabaseError::Query(diesel::result::Error::DatabaseError(
                            diesel::result::DatabaseErrorKind::SerializationFailure,
                            _,
                        )),
                    ) => {
                        retry_count += 1;
                        if retry_count > MAX_RETRIES {
                            tracing::error!(
                                "Exceeded max retries on SerializationFailure errors: {err:?}"
                            );
                            break Err(err);
                        } else {
                            // Retry on serialization errors: these are expected, because even though our
                            // transactions don't fight for the same rows, they will occasionally collide
                            // on index pages (e.g. increment_generation for unrelated shards can collide)
                            tracing::debug!(
                                "Retrying transaction on serialization failure {err:?}"
                            );
                            continue;
                        }
                    }
                    Err(e) => break Err(e),
                }
            }
        })
        .await
        .map_err(|e| DatabaseError::Migration(e.to_string()))??;
        Ok(())
    }

    /// Wraps `with_conn` in order to collect latency and error metrics
    async fn with_measured_conn<'a, 'b, F, R>(
        &self,
        op: DatabaseOperation,
        func: F,
    ) -> DatabaseResult<R>
    where
        F: for<'r> Fn(&'r mut AsyncPgConnection) -> ScopedBoxFuture<'b, 'r, DatabaseResult<R>>
            + Send
            + std::marker::Sync
            + 'a,
        R: Send + 'b,
    {
        let latency = &METRICS_REGISTRY
            .metrics_group
            .storage_controller_database_query_latency;
        let _timer = latency.start_timer(DatabaseQueryLatencyLabelGroup { operation: op });

        let res = self.with_conn(func).await;

        if let Err(err) = &res {
            let error_counter = &METRICS_REGISTRY
                .metrics_group
                .storage_controller_database_query_error;
            error_counter.inc(DatabaseQueryErrorLabelGroup {
                error_type: err.error_label(),
                operation: op,
            })
        }

        res
    }

    /// Call the provided function with a Diesel database connection in a retry loop
    async fn with_conn<'a, 'b, F, R>(&self, func: F) -> DatabaseResult<R>
    where
        F: for<'r> Fn(&'r mut AsyncPgConnection) -> ScopedBoxFuture<'b, 'r, DatabaseResult<R>>
            + Send
            + std::marker::Sync
            + 'a,
        R: Send + 'b,
    {
        let mut retry_count = 0;
        loop {
            let mut conn = self.connection_pool.get().await?;
            match conn
                .build_transaction()
                .serializable()
                .run(|c| func(c))
                .await
            {
                Ok(r) => break Ok(r),
                Err(
                    err @ DatabaseError::Query(diesel::result::Error::DatabaseError(
                        diesel::result::DatabaseErrorKind::SerializationFailure,
                        _,
                    )),
                ) => {
                    retry_count += 1;
                    if retry_count > MAX_RETRIES {
                        tracing::error!(
                            "Exceeded max retries on SerializationFailure errors: {err:?}"
                        );
                        break Err(err);
                    } else {
                        // Retry on serialization errors: these are expected, because even though our
                        // transactions don't fight for the same rows, they will occasionally collide
                        // on index pages (e.g. increment_generation for unrelated shards can collide)
                        tracing::debug!("Retrying transaction on serialization failure {err:?}");
                        continue;
                    }
                }
                Err(e) => break Err(e),
            }
        }
    }

    /// When a node is first registered, persist it before using it for anything
    /// If the provided node_id already exists, it will be error.
    /// The common case is when a node marked for deletion wants to register.
    pub(crate) async fn insert_node(&self, node: &Node) -> DatabaseResult<()> {
        let np = &node.to_persistent();
        self.with_measured_conn(DatabaseOperation::InsertNode, move |conn| {
            Box::pin(async move {
                diesel::insert_into(crate::schema::nodes::table)
                    .values(np)
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .await
    }

    /// At startup, populate the list of nodes which our shards may be placed on
    pub(crate) async fn list_nodes(&self) -> DatabaseResult<Vec<NodePersistence>> {
        use crate::schema::nodes::dsl::*;

        let result: Vec<NodePersistence> = self
            .with_measured_conn(DatabaseOperation::ListNodes, move |conn| {
                Box::pin(async move {
                    Ok(crate::schema::nodes::table
                        .filter(lifecycle.ne(String::from(NodeLifecycle::Deleted)))
                        .load::<NodePersistence>(conn)
                        .await?)
                })
            })
            .await?;

        tracing::info!("list_nodes: loaded {} nodes", result.len());

        Ok(result)
    }

    pub(crate) async fn list_tombstones(&self) -> DatabaseResult<Vec<NodePersistence>> {
        use crate::schema::nodes::dsl::*;

        let result: Vec<NodePersistence> = self
            .with_measured_conn(DatabaseOperation::ListTombstones, move |conn| {
                Box::pin(async move {
                    Ok(crate::schema::nodes::table
                        .filter(lifecycle.eq(String::from(NodeLifecycle::Deleted)))
                        .load::<NodePersistence>(conn)
                        .await?)
                })
            })
            .await?;

        tracing::info!("list_tombstones: loaded {} nodes", result.len());

        Ok(result)
    }

    pub(crate) async fn update_node<V>(
        &self,
        input_node_id: NodeId,
        values: V,
    ) -> DatabaseResult<()>
    where
        V: diesel::AsChangeset<Target = crate::schema::nodes::table> + Clone + Send + Sync,
        V::Changeset: diesel::query_builder::QueryFragment<diesel::pg::Pg> + Send, // valid Postgres SQL
    {
        use crate::schema::nodes::dsl::*;
        let updated = self
            .with_measured_conn(DatabaseOperation::UpdateNode, move |conn| {
                let values = values.clone();
                Box::pin(async move {
                    let updated = diesel::update(nodes)
                        .filter(node_id.eq(input_node_id.0 as i64))
                        .filter(lifecycle.ne(String::from(NodeLifecycle::Deleted)))
                        .set(values)
                        .execute(conn)
                        .await?;
                    Ok(updated)
                })
            })
            .await?;

        if updated != 1 {
            Err(DatabaseError::Logical(format!(
                "Node {node_id:?} not found for update",
            )))
        } else {
            Ok(())
        }
    }

    pub(crate) async fn update_node_scheduling_policy(
        &self,
        input_node_id: NodeId,
        input_scheduling: NodeSchedulingPolicy,
    ) -> DatabaseResult<()> {
        use crate::schema::nodes::dsl::*;
        self.update_node(
            input_node_id,
            scheduling_policy.eq(String::from(input_scheduling)),
        )
        .await
    }

    pub(crate) async fn update_node_on_registration(
        &self,
        input_node_id: NodeId,
        input_https_port: Option<u16>,
        input_grpc_addr: Option<String>,
        input_grpc_port: Option<u16>,
    ) -> DatabaseResult<()> {
        use crate::schema::nodes::dsl::*;
        self.update_node(
            input_node_id,
            (
                listen_https_port.eq(input_https_port.map(|x| x as i32)),
                listen_grpc_addr.eq(input_grpc_addr),
                listen_grpc_port.eq(input_grpc_port.map(|x| x as i32)),
            ),
        )
        .await
    }

    /// Tombstone is a special state where the node is not deleted from the database,
    /// but it is not available for usage.
    /// The main reason for it is to prevent the flaky node to register.
    pub(crate) async fn set_tombstone(&self, del_node_id: NodeId) -> DatabaseResult<()> {
        use crate::schema::nodes::dsl::*;
        self.update_node(
            del_node_id,
            lifecycle.eq(String::from(NodeLifecycle::Deleted)),
        )
        .await
    }

    pub(crate) async fn delete_node(&self, del_node_id: NodeId) -> DatabaseResult<()> {
        use crate::schema::nodes::dsl::*;
        self.with_measured_conn(DatabaseOperation::DeleteNode, move |conn| {
            Box::pin(async move {
                // You can hard delete a node only if it has a tombstone.
                // So we need to check if the node has lifecycle set to deleted.
                let node_to_delete = nodes
                    .filter(node_id.eq(del_node_id.0 as i64))
                    .first::<NodePersistence>(conn)
                    .await
                    .optional()?;

                if let Some(np) = node_to_delete {
                    let lc = NodeLifecycle::from_str(&np.lifecycle).map_err(|e| {
                        DatabaseError::Logical(format!(
                            "Node {del_node_id} has invalid lifecycle: {e}"
                        ))
                    })?;

                    if lc != NodeLifecycle::Deleted {
                        return Err(DatabaseError::Logical(format!(
                            "Node {del_node_id} was not soft deleted before, cannot hard delete it"
                        )));
                    }

                    diesel::delete(nodes)
                        .filter(node_id.eq(del_node_id.0 as i64))
                        .execute(conn)
                        .await?;
                }

                Ok(())
            })
        })
        .await
    }

    /// At startup, load the high level state for shards, such as their config + policy.  This will
    /// be enriched at runtime with state discovered on pageservers.
    ///
    /// We exclude shards configured to be detached.  During startup, if we see any attached locations
    /// for such shards, they will automatically be detached as 'orphans'.
    pub(crate) async fn load_active_tenant_shards(
        &self,
    ) -> DatabaseResult<Vec<TenantShardPersistence>> {
        use crate::schema::tenant_shards::dsl::*;
        self.with_measured_conn(DatabaseOperation::ListTenantShards, move |conn| {
            Box::pin(async move {
                let query = tenant_shards.filter(
                    placement_policy.ne(serde_json::to_string(&PlacementPolicy::Detached).unwrap()),
                );
                let result = query.load::<TenantShardPersistence>(conn).await?;

                Ok(result)
            })
        })
        .await
    }

    /// When restoring a previously detached tenant into memory, load it from the database
    pub(crate) async fn load_tenant(
        &self,
        filter_tenant_id: TenantId,
    ) -> DatabaseResult<Vec<TenantShardPersistence>> {
        use crate::schema::tenant_shards::dsl::*;
        self.with_measured_conn(DatabaseOperation::LoadTenant, move |conn| {
            Box::pin(async move {
                let query = tenant_shards.filter(tenant_id.eq(filter_tenant_id.to_string()));
                let result = query.load::<TenantShardPersistence>(conn).await?;

                Ok(result)
            })
        })
        .await
    }

    /// Tenants must be persisted before we schedule them for the first time.  This enables us
    /// to correctly retain generation monotonicity, and the externally provided placement policy & config.
    pub(crate) async fn insert_tenant_shards(
        &self,
        shards: Vec<TenantShardPersistence>,
    ) -> DatabaseResult<()> {
        use crate::schema::{metadata_health, tenant_shards};

        let now = chrono::Utc::now();

        let metadata_health_records = shards
            .iter()
            .map(|t| MetadataHealthPersistence {
                tenant_id: t.tenant_id.clone(),
                shard_number: t.shard_number,
                shard_count: t.shard_count,
                healthy: true,
                last_scrubbed_at: now,
            })
            .collect::<Vec<_>>();

        let shards = &shards;
        let metadata_health_records = &metadata_health_records;
        self.with_measured_conn(DatabaseOperation::InsertTenantShards, move |conn| {
            Box::pin(async move {
                diesel::insert_into(tenant_shards::table)
                    .values(shards)
                    .execute(conn)
                    .await?;

                diesel::insert_into(metadata_health::table)
                    .values(metadata_health_records)
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .await
    }

    /// Ordering: call this _after_ deleting the tenant on pageservers, but _before_ dropping state for
    /// the tenant from memory on this server.
    pub(crate) async fn delete_tenant(&self, del_tenant_id: TenantId) -> DatabaseResult<()> {
        use crate::schema::tenant_shards::dsl::*;
        self.with_measured_conn(DatabaseOperation::DeleteTenant, move |conn| {
            Box::pin(async move {
                // `metadata_health` status (if exists) is also deleted based on the cascade behavior.
                diesel::delete(tenant_shards)
                    .filter(tenant_id.eq(del_tenant_id.to_string()))
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .await
    }

    /// When a tenant invokes the /re-attach API, this function is responsible for doing an efficient
    /// batched increment of the generations of all tenants whose generation_pageserver is equal to
    /// the node that called /re-attach.
    #[tracing::instrument(skip_all, fields(node_id))]
    pub(crate) async fn re_attach(
        &self,
        input_node_id: NodeId,
    ) -> DatabaseResult<HashMap<TenantShardId, Generation>> {
        use crate::schema::nodes::dsl::{scheduling_policy, *};
        use crate::schema::tenant_shards::dsl::*;
        let updated = self
            .with_measured_conn(DatabaseOperation::ReAttach, move |conn| {
                Box::pin(async move {
                    let node: Option<NodePersistence> = nodes
                        .filter(node_id.eq(input_node_id.0 as i64))
                        .first::<NodePersistence>(conn)
                        .await
                        .optional()?;

                    // Check if the node is not marked as deleted
                    match node {
                        Some(node) if matches!(NodeLifecycle::from_str(&node.lifecycle), Ok(NodeLifecycle::Deleted)) => {
                            return Err(DatabaseError::Logical(format!(
                                "Node {input_node_id} is marked as deleted, re-attach is not allowed"
                            )));
                        }
                        _ => {
                            // go through
                        }
                    };

                    let rows_updated = diesel::update(tenant_shards)
                        .filter(generation_pageserver.eq(input_node_id.0 as i64))
                        .set(generation.eq(generation + 1))
                        .execute(conn)
                        .await?;

                    tracing::info!("Incremented {} tenants' generations", rows_updated);

                    // TODO: UPDATE+SELECT in one query

                    let updated = tenant_shards
                        .filter(generation_pageserver.eq(input_node_id.0 as i64))
                        .select(TenantShardPersistence::as_select())
                        .load(conn)
                        .await?;

                    if let Some(node) = node {
                        let old_scheduling_policy =
                            NodeSchedulingPolicy::from_str(&node.scheduling_policy).unwrap();
                        let new_scheduling_policy = match old_scheduling_policy {
                            NodeSchedulingPolicy::Active => NodeSchedulingPolicy::Active,
                            NodeSchedulingPolicy::PauseForRestart => NodeSchedulingPolicy::Active,
                            NodeSchedulingPolicy::Draining => NodeSchedulingPolicy::Active,
                            NodeSchedulingPolicy::Filling => NodeSchedulingPolicy::Active,
                            NodeSchedulingPolicy::Pause => NodeSchedulingPolicy::Pause,
                            NodeSchedulingPolicy::Deleting => NodeSchedulingPolicy::Pause,
                        };
                        diesel::update(nodes)
                            .filter(node_id.eq(input_node_id.0 as i64))
                            .set(scheduling_policy.eq(String::from(new_scheduling_policy)))
                            .execute(conn)
                            .await?;
                    }

                    Ok(updated)
                })
            })
            .await?;

        let mut result = HashMap::new();
        for tsp in updated {
            let tenant_shard_id = TenantShardId {
                tenant_id: TenantId::from_str(tsp.tenant_id.as_str())
                    .map_err(|e| DatabaseError::Logical(format!("Malformed tenant id: {e}")))?,
                shard_number: ShardNumber(tsp.shard_number as u8),
                shard_count: ShardCount::new(tsp.shard_count as u8),
            };

            let Some(g) = tsp.generation else {
                // If the generation_pageserver column was non-NULL, then the generation column should also be non-NULL:
                // we only set generation_pageserver when setting generation.
                return Err(DatabaseError::Logical(
                    "Generation should always be set after incrementing".to_string(),
                ));
            };
            result.insert(tenant_shard_id, Generation::new(g as u32));
        }

        Ok(result)
    }

    /// Reconciler calls this immediately before attaching to a new pageserver, to acquire a unique, monotonically
    /// advancing generation number.  We also store the NodeId for which the generation was issued, so that in
    /// [`Self::re_attach`] we can do a bulk UPDATE on the generations for that node.
    pub(crate) async fn increment_generation(
        &self,
        tenant_shard_id: TenantShardId,
        node_id: NodeId,
    ) -> anyhow::Result<Generation> {
        use crate::schema::tenant_shards::dsl::*;
        let updated = self
            .with_measured_conn(DatabaseOperation::IncrementGeneration, move |conn| {
                Box::pin(async move {
                    let updated = diesel::update(tenant_shards)
                        .filter(tenant_id.eq(tenant_shard_id.tenant_id.to_string()))
                        .filter(shard_number.eq(tenant_shard_id.shard_number.0 as i32))
                        .filter(shard_count.eq(tenant_shard_id.shard_count.literal() as i32))
                        .set((
                            generation.eq(generation + 1),
                            generation_pageserver.eq(node_id.0 as i64),
                        ))
                        // TODO: only returning() the generation column
                        .returning(TenantShardPersistence::as_returning())
                        .get_result(conn)
                        .await?;

                    Ok(updated)
                })
            })
            .await?;

        // Generation is always non-null in the rseult: if the generation column had been NULL, then we
        // should have experienced an SQL Confilict error while executing a query that tries to increment it.
        debug_assert!(updated.generation.is_some());
        let Some(g) = updated.generation else {
            return Err(DatabaseError::Logical(
                "Generation should always be set after incrementing".to_string(),
            )
            .into());
        };

        Ok(Generation::new(g as u32))
    }

    /// When we want to call out to the running shards for a tenant, e.g. during timeline CRUD operations,
    /// we need to know where the shard is attached, _and_ the generation, so that we can re-check the generation
    /// afterwards to confirm that our timeline CRUD operation is truly persistent (it must have happened in the
    /// latest generation)
    ///
    /// If the tenant doesn't exist, an empty vector is returned.
    ///
    /// Output is sorted by shard number
    pub(crate) async fn tenant_generations(
        &self,
        filter_tenant_id: TenantId,
    ) -> Result<Vec<ShardGenerationState>, DatabaseError> {
        use crate::schema::tenant_shards::dsl::*;
        let rows = self
            .with_measured_conn(DatabaseOperation::TenantGenerations, move |conn| {
                Box::pin(async move {
                    let result = tenant_shards
                        .filter(tenant_id.eq(filter_tenant_id.to_string()))
                        .select(TenantShardPersistence::as_select())
                        .order(shard_number)
                        .load(conn)
                        .await?;
                    Ok(result)
                })
            })
            .await?;

        Ok(rows
            .into_iter()
            .map(|p| ShardGenerationState {
                tenant_shard_id: p
                    .get_tenant_shard_id()
                    .expect("Corrupt tenant shard id in database"),
                generation: p.generation.map(|g| Generation::new(g as u32)),
                generation_pageserver: p.generation_pageserver.map(|n| NodeId(n as u64)),
            })
            .collect())
    }

    /// Read the generation number of specific tenant shards
    ///
    /// Output is unsorted.  Output may not include values for all inputs, if they are missing in the database.
    pub(crate) async fn shard_generations(
        &self,
        mut tenant_shard_ids: impl Iterator<Item = &TenantShardId>,
    ) -> Result<Vec<(TenantShardId, Option<Generation>)>, DatabaseError> {
        let mut rows = Vec::with_capacity(tenant_shard_ids.size_hint().0);

        // We will chunk our input to avoid composing arbitrarily long `IN` clauses.  Typically we are
        // called with a single digit number of IDs, but in principle we could be called with tens
        // of thousands (all the shards on one pageserver) from the generation validation API.
        loop {
            // A modest hardcoded chunk size to handle typical cases in a single query but never generate particularly
            // large query strings.
            let chunk_ids = tenant_shard_ids.by_ref().take(32);

            // Compose a comma separated list of tuples for matching on (tenant_id, shard_number, shard_count)
            let in_clause = chunk_ids
                .map(|tsid| {
                    format!(
                        "('{}', {}, {})",
                        tsid.tenant_id, tsid.shard_number.0, tsid.shard_count.0
                    )
                })
                .join(",");

            // We are done when our iterator gives us nothing to filter on
            if in_clause.is_empty() {
                break;
            }

            let in_clause = &in_clause;
            let chunk_rows = self
                .with_measured_conn(DatabaseOperation::ShardGenerations, move |conn| {
                    Box::pin(async move {
                        // diesel doesn't support multi-column IN queries, so we compose raw SQL.  No escaping is required because
                        // the inputs are strongly typed and cannot carry any user-supplied raw string content.
                        let result : Vec<TenantShardPersistence> = diesel::sql_query(
                            format!("SELECT * from tenant_shards where (tenant_id, shard_number, shard_count) in ({in_clause});").as_str()
                        ).load(conn).await?;

                        Ok(result)
                    })
                })
                .await?;
            rows.extend(chunk_rows.into_iter())
        }

        Ok(rows
            .into_iter()
            .map(|tsp| {
                (
                    tsp.get_tenant_shard_id()
                        .expect("Bad tenant ID in database"),
                    tsp.generation.map(|g| Generation::new(g as u32)),
                )
            })
            .collect())
    }

    #[allow(non_local_definitions)]
    /// For use when updating a persistent property of a tenant, such as its config or placement_policy.
    ///
    /// Do not use this for settting generation, unless in the special onboarding code path (/location_config)
    /// API: use [`Self::increment_generation`] instead.  Setting the generation via this route is a one-time thing
    /// that we only do the first time a tenant is set to an attached policy via /location_config.
    pub(crate) async fn update_tenant_shard(
        &self,
        tenant: TenantFilter,
        input_placement_policy: Option<PlacementPolicy>,
        input_config: Option<TenantConfig>,
        input_generation: Option<Generation>,
        input_scheduling_policy: Option<ShardSchedulingPolicy>,
    ) -> DatabaseResult<()> {
        use crate::schema::tenant_shards::dsl::*;

        let tenant = &tenant;
        let input_placement_policy = &input_placement_policy;
        let input_config = &input_config;
        let input_generation = &input_generation;
        let input_scheduling_policy = &input_scheduling_policy;
        self.with_measured_conn(DatabaseOperation::UpdateTenantShard, move |conn| {
            Box::pin(async move {
                let query = match tenant {
                    TenantFilter::Shard(tenant_shard_id) => diesel::update(tenant_shards)
                        .filter(tenant_id.eq(tenant_shard_id.tenant_id.to_string()))
                        .filter(shard_number.eq(tenant_shard_id.shard_number.0 as i32))
                        .filter(shard_count.eq(tenant_shard_id.shard_count.literal() as i32))
                        .into_boxed(),
                    TenantFilter::Tenant(input_tenant_id) => diesel::update(tenant_shards)
                        .filter(tenant_id.eq(input_tenant_id.to_string()))
                        .into_boxed(),
                };

                // Clear generation_pageserver if we are moving into a state where we won't have
                // any attached pageservers.
                let input_generation_pageserver = match input_placement_policy {
                    None | Some(PlacementPolicy::Attached(_)) => None,
                    Some(PlacementPolicy::Detached | PlacementPolicy::Secondary) => Some(None),
                };

                #[derive(AsChangeset)]
                #[diesel(table_name = crate::schema::tenant_shards)]
                struct ShardUpdate {
                    generation: Option<i32>,
                    placement_policy: Option<String>,
                    config: Option<String>,
                    scheduling_policy: Option<String>,
                    generation_pageserver: Option<Option<i64>>,
                }

                let update = ShardUpdate {
                    generation: input_generation.map(|g| g.into().unwrap() as i32),
                    placement_policy: input_placement_policy
                        .as_ref()
                        .map(|p| serde_json::to_string(&p).unwrap()),
                    config: input_config
                        .as_ref()
                        .map(|c| serde_json::to_string(&c).unwrap()),
                    scheduling_policy: input_scheduling_policy
                        .map(|p| serde_json::to_string(&p).unwrap()),
                    generation_pageserver: input_generation_pageserver,
                };

                query.set(update).execute(conn).await?;

                Ok(())
            })
        })
        .await?;

        Ok(())
    }

    /// Note that passing None for a shard clears the preferred AZ (rather than leaving it unmodified)
    pub(crate) async fn set_tenant_shard_preferred_azs(
        &self,
        preferred_azs: Vec<(TenantShardId, Option<AvailabilityZone>)>,
    ) -> DatabaseResult<Vec<(TenantShardId, Option<AvailabilityZone>)>> {
        use crate::schema::tenant_shards::dsl::*;

        let preferred_azs = preferred_azs.as_slice();
        self.with_measured_conn(DatabaseOperation::SetPreferredAzs, move |conn| {
            Box::pin(async move {
                let mut shards_updated = Vec::default();

                for (tenant_shard_id, preferred_az) in preferred_azs.iter() {
                    let updated = diesel::update(tenant_shards)
                        .filter(tenant_id.eq(tenant_shard_id.tenant_id.to_string()))
                        .filter(shard_number.eq(tenant_shard_id.shard_number.0 as i32))
                        .filter(shard_count.eq(tenant_shard_id.shard_count.literal() as i32))
                        .set(preferred_az_id.eq(preferred_az.as_ref().map(|az| az.0.clone())))
                        .execute(conn)
                        .await?;

                    if updated == 1 {
                        shards_updated.push((*tenant_shard_id, preferred_az.clone()));
                    }
                }

                Ok(shards_updated)
            })
        })
        .await
    }

    pub(crate) async fn detach(&self, tenant_shard_id: TenantShardId) -> anyhow::Result<()> {
        use crate::schema::tenant_shards::dsl::*;
        self.with_measured_conn(DatabaseOperation::Detach, move |conn| {
            Box::pin(async move {
                let updated = diesel::update(tenant_shards)
                    .filter(tenant_id.eq(tenant_shard_id.tenant_id.to_string()))
                    .filter(shard_number.eq(tenant_shard_id.shard_number.0 as i32))
                    .filter(shard_count.eq(tenant_shard_id.shard_count.literal() as i32))
                    .set((
                        generation_pageserver.eq(Option::<i64>::None),
                        placement_policy
                            .eq(serde_json::to_string(&PlacementPolicy::Detached).unwrap()),
                    ))
                    .execute(conn)
                    .await?;

                Ok(updated)
            })
        })
        .await?;

        Ok(())
    }

    // When we start shard splitting, we must durably mark the tenant so that
    // on restart, we know that we must go through recovery.
    //
    // We create the child shards here, so that they will be available for increment_generation calls
    // if some pageserver holding a child shard needs to restart before the overall tenant split is complete.
    pub(crate) async fn begin_shard_split(
        &self,
        old_shard_count: ShardCount,
        split_tenant_id: TenantId,
        parent_to_children: Vec<(TenantShardId, Vec<TenantShardPersistence>)>,
    ) -> DatabaseResult<()> {
        use crate::schema::tenant_shards::dsl::*;
        let parent_to_children = parent_to_children.as_slice();
        self.with_measured_conn(DatabaseOperation::BeginShardSplit, move |conn| {
            Box::pin(async move {
            // Mark parent shards as splitting

            let updated = diesel::update(tenant_shards)
                .filter(tenant_id.eq(split_tenant_id.to_string()))
                .filter(shard_count.eq(old_shard_count.literal() as i32))
                .set((splitting.eq(1),))
                .execute(conn).await?;
            if u8::try_from(updated)
                .map_err(|_| DatabaseError::Logical(
                    format!("Overflow existing shard count {updated} while splitting"))
                )? != old_shard_count.count() {
                // Perhaps a deletion or another split raced with this attempt to split, mutating
                // the parent shards that we intend to split. In this case the split request should fail.
                return Err(DatabaseError::Logical(
                    format!("Unexpected existing shard count {updated} when preparing tenant for split (expected {})", old_shard_count.count())
                ));
            }

            // FIXME: spurious clone to sidestep closure move rules
            let parent_to_children = parent_to_children.to_vec();

            // Insert child shards
            for (parent_shard_id, children) in parent_to_children {
                let mut parent = crate::schema::tenant_shards::table
                    .filter(tenant_id.eq(parent_shard_id.tenant_id.to_string()))
                    .filter(shard_number.eq(parent_shard_id.shard_number.0 as i32))
                    .filter(shard_count.eq(parent_shard_id.shard_count.literal() as i32))
                    .load::<TenantShardPersistence>(conn).await?;
                let parent = if parent.len() != 1 {
                    return Err(DatabaseError::Logical(format!(
                        "Parent shard {parent_shard_id} not found"
                    )));
                } else {
                    parent.pop().unwrap()
                };
                for mut shard in children {
                    // Carry the parent's generation into the child
                    shard.generation = parent.generation;

                    debug_assert!(shard.splitting == SplitState::Splitting);
                    diesel::insert_into(tenant_shards)
                        .values(shard)
                        .execute(conn).await?;
                }
            }

            Ok(())
        })
        })
        .await
    }

    // When we finish shard splitting, we must atomically clean up the old shards
    // and insert the new shards, and clear the splitting marker.
    pub(crate) async fn complete_shard_split(
        &self,
        split_tenant_id: TenantId,
        old_shard_count: ShardCount,
        new_shard_count: ShardCount,
    ) -> DatabaseResult<()> {
        use crate::schema::tenant_shards::dsl::*;
        self.with_measured_conn(DatabaseOperation::CompleteShardSplit, move |conn| {
            Box::pin(async move {
                // Sanity: child shards must still exist, as we're deleting parent shards
                let child_shards_query = tenant_shards
                    .filter(tenant_id.eq(split_tenant_id.to_string()))
                    .filter(shard_count.eq(new_shard_count.literal() as i32));
                let child_shards = child_shards_query
                    .load::<TenantShardPersistence>(conn)
                    .await?;
                if child_shards.len() != new_shard_count.count() as usize {
                    return Err(DatabaseError::Logical(format!(
                        "Unexpected child shard count {} while completing split to \
                            count {new_shard_count:?} on tenant {split_tenant_id}",
                        child_shards.len()
                    )));
                }

                // Drop parent shards
                diesel::delete(tenant_shards)
                    .filter(tenant_id.eq(split_tenant_id.to_string()))
                    .filter(shard_count.eq(old_shard_count.literal() as i32))
                    .execute(conn)
                    .await?;

                // Clear sharding flag
                let updated = diesel::update(tenant_shards)
                    .filter(tenant_id.eq(split_tenant_id.to_string()))
                    .filter(shard_count.eq(new_shard_count.literal() as i32))
                    .set((splitting.eq(0),))
                    .execute(conn)
                    .await?;
                assert!(updated == new_shard_count.count() as usize);

                Ok(())
            })
        })
        .await
    }

    /// Used when the remote part of a shard split failed: we will revert the database state to have only
    /// the parent shards, with SplitState::Idle.
    pub(crate) async fn abort_shard_split(
        &self,
        split_tenant_id: TenantId,
        new_shard_count: ShardCount,
    ) -> DatabaseResult<AbortShardSplitStatus> {
        use crate::schema::tenant_shards::dsl::*;
        self.with_measured_conn(DatabaseOperation::AbortShardSplit, move |conn| {
            Box::pin(async move {
                // Clear the splitting state on parent shards
                let updated = diesel::update(tenant_shards)
                    .filter(tenant_id.eq(split_tenant_id.to_string()))
                    .filter(shard_count.ne(new_shard_count.literal() as i32))
                    .set((splitting.eq(0),))
                    .execute(conn)
                    .await?;

                // Parent shards are already gone: we cannot abort.
                if updated == 0 {
                    return Ok(AbortShardSplitStatus::Complete);
                }

                // Sanity check: if parent shards were present, their cardinality should
                // be less than the number of child shards.
                if updated >= new_shard_count.count() as usize {
                    return Err(DatabaseError::Logical(format!(
                        "Unexpected parent shard count {updated} while aborting split to \
                            count {new_shard_count:?} on tenant {split_tenant_id}"
                    )));
                }

                // Erase child shards
                diesel::delete(tenant_shards)
                    .filter(tenant_id.eq(split_tenant_id.to_string()))
                    .filter(shard_count.eq(new_shard_count.literal() as i32))
                    .execute(conn)
                    .await?;

                Ok(AbortShardSplitStatus::Aborted)
            })
        })
        .await
    }

    /// Stores all the latest metadata health updates durably. Updates existing entry on conflict.
    ///
    /// **Correctness:** `metadata_health_updates` should all belong the tenant shards managed by the storage controller.
    #[allow(dead_code)]
    pub(crate) async fn update_metadata_health_records(
        &self,
        healthy_records: Vec<MetadataHealthPersistence>,
        unhealthy_records: Vec<MetadataHealthPersistence>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> DatabaseResult<()> {
        use crate::schema::metadata_health::dsl::*;

        let healthy_records = healthy_records.as_slice();
        let unhealthy_records = unhealthy_records.as_slice();
        self.with_measured_conn(DatabaseOperation::UpdateMetadataHealth, move |conn| {
            Box::pin(async move {
                diesel::insert_into(metadata_health)
                    .values(healthy_records)
                    .on_conflict((tenant_id, shard_number, shard_count))
                    .do_update()
                    .set((healthy.eq(true), last_scrubbed_at.eq(now)))
                    .execute(conn)
                    .await?;

                diesel::insert_into(metadata_health)
                    .values(unhealthy_records)
                    .on_conflict((tenant_id, shard_number, shard_count))
                    .do_update()
                    .set((healthy.eq(false), last_scrubbed_at.eq(now)))
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .await
    }

    /// Lists all the metadata health records.
    #[allow(dead_code)]
    pub(crate) async fn list_metadata_health_records(
        &self,
    ) -> DatabaseResult<Vec<MetadataHealthPersistence>> {
        self.with_measured_conn(DatabaseOperation::ListMetadataHealth, move |conn| {
            Box::pin(async {
                Ok(crate::schema::metadata_health::table
                    .load::<MetadataHealthPersistence>(conn)
                    .await?)
            })
        })
        .await
    }

    /// Lists all the metadata health records that is unhealthy.
    #[allow(dead_code)]
    pub(crate) async fn list_unhealthy_metadata_health_records(
        &self,
    ) -> DatabaseResult<Vec<MetadataHealthPersistence>> {
        use crate::schema::metadata_health::dsl::*;
        self.with_measured_conn(
            DatabaseOperation::ListMetadataHealthUnhealthy,
            move |conn| {
                Box::pin(async {
                    DatabaseResult::Ok(
                        crate::schema::metadata_health::table
                            .filter(healthy.eq(false))
                            .load::<MetadataHealthPersistence>(conn)
                            .await?,
                    )
                })
            },
        )
        .await
    }

    /// Lists all the metadata health records that have not been updated since an `earlier` time.
    #[allow(dead_code)]
    pub(crate) async fn list_outdated_metadata_health_records(
        &self,
        earlier: chrono::DateTime<chrono::Utc>,
    ) -> DatabaseResult<Vec<MetadataHealthPersistence>> {
        use crate::schema::metadata_health::dsl::*;

        self.with_measured_conn(DatabaseOperation::ListMetadataHealthOutdated, move |conn| {
            Box::pin(async move {
                let query = metadata_health.filter(last_scrubbed_at.lt(earlier));
                let res = query.load::<MetadataHealthPersistence>(conn).await?;

                Ok(res)
            })
        })
        .await
    }

    /// Get the current entry from the `leader` table if one exists.
    /// It is an error for the table to contain more than one entry.
    pub(crate) async fn get_leader(&self) -> DatabaseResult<Option<ControllerPersistence>> {
        let mut leader: Vec<ControllerPersistence> = self
            .with_measured_conn(DatabaseOperation::GetLeader, move |conn| {
                Box::pin(async move {
                    Ok(crate::schema::controllers::table
                        .load::<ControllerPersistence>(conn)
                        .await?)
                })
            })
            .await?;

        if leader.len() > 1 {
            return Err(DatabaseError::Logical(format!(
                "More than one entry present in the leader table: {leader:?}"
            )));
        }

        Ok(leader.pop())
    }

    /// Update the new leader with compare-exchange semantics. If `prev` does not
    /// match the current leader entry, then the update is treated as a failure.
    /// When `prev` is not specified, the update is forced.
    pub(crate) async fn update_leader(
        &self,
        prev: Option<ControllerPersistence>,
        new: ControllerPersistence,
    ) -> DatabaseResult<()> {
        use crate::schema::controllers::dsl::*;

        let updated = self
            .with_measured_conn(DatabaseOperation::UpdateLeader, move |conn| {
                let prev = prev.clone();
                let new = new.clone();
                Box::pin(async move {
                    let updated = match &prev {
                        Some(prev) => {
                            diesel::update(controllers)
                                .filter(address.eq(prev.address.clone()))
                                .filter(started_at.eq(prev.started_at))
                                .set((
                                    address.eq(new.address.clone()),
                                    started_at.eq(new.started_at),
                                ))
                                .execute(conn)
                                .await?
                        }
                        None => {
                            diesel::insert_into(controllers)
                                .values(new.clone())
                                .execute(conn)
                                .await?
                        }
                    };

                    Ok(updated)
                })
            })
            .await?;

        if updated == 0 {
            return Err(DatabaseError::Logical(
                "Leader table update failed".to_string(),
            ));
        }

        Ok(())
    }

    /// At startup, populate the list of nodes which our shards may be placed on
    pub(crate) async fn list_safekeepers(&self) -> DatabaseResult<Vec<SafekeeperPersistence>> {
        let safekeepers: Vec<SafekeeperPersistence> = self
            .with_measured_conn(DatabaseOperation::ListNodes, move |conn| {
                Box::pin(async move {
                    Ok(crate::schema::safekeepers::table
                        .load::<SafekeeperPersistence>(conn)
                        .await?)
                })
            })
            .await?;

        tracing::info!("list_safekeepers: loaded {} nodes", safekeepers.len());

        Ok(safekeepers)
    }

    pub(crate) async fn safekeeper_upsert(
        &self,
        record: SafekeeperUpsert,
    ) -> Result<(), DatabaseError> {
        use crate::schema::safekeepers::dsl::*;

        self.with_conn(move |conn| {
            let record = record.clone();
            Box::pin(async move {
                let bind = record
                    .as_insert_or_update()
                    .map_err(|e| DatabaseError::Logical(format!("{e}")))?;

                let inserted_updated = diesel::insert_into(safekeepers)
                    .values(&bind)
                    .on_conflict(id)
                    .do_update()
                    .set(&bind)
                    .execute(conn)
                    .await?;

                if inserted_updated != 1 {
                    return Err(DatabaseError::Logical(format!(
                        "unexpected number of rows ({inserted_updated})"
                    )));
                }

                Ok(())
            })
        })
        .await
    }

    pub(crate) async fn set_safekeeper_scheduling_policy(
        &self,
        id_: i64,
        scheduling_policy_: SkSchedulingPolicy,
    ) -> Result<(), DatabaseError> {
        use crate::schema::safekeepers::dsl::*;

        self.with_conn(move |conn| {
            Box::pin(async move {
                #[derive(Insertable, AsChangeset)]
                #[diesel(table_name = crate::schema::safekeepers)]
                struct UpdateSkSchedulingPolicy<'a> {
                    id: i64,
                    scheduling_policy: &'a str,
                }
                let scheduling_policy_ = String::from(scheduling_policy_);

                let rows_affected = diesel::update(safekeepers.filter(id.eq(id_)))
                    .set(scheduling_policy.eq(scheduling_policy_))
                    .execute(conn)
                    .await?;

                if rows_affected != 1 {
                    return Err(DatabaseError::Logical(format!(
                        "unexpected number of rows ({rows_affected})",
                    )));
                }

                Ok(())
            })
        })
        .await
    }

    /// Activate the given safekeeper, ensuring that there is no TOCTOU.
    /// Returns `Some` if the safekeeper has indeed been activating (or already active). Other states return `None`.
    pub(crate) async fn activate_safekeeper(&self, id_: i64) -> Result<Option<()>, DatabaseError> {
        use crate::schema::safekeepers::dsl::*;

        self.with_conn(move |conn| {
            Box::pin(async move {
                #[derive(Insertable, AsChangeset)]
                #[diesel(table_name = crate::schema::safekeepers)]
                struct UpdateSkSchedulingPolicy<'a> {
                    id: i64,
                    scheduling_policy: &'a str,
                }
                let scheduling_policy_active = String::from(SkSchedulingPolicy::Active);
                let scheduling_policy_activating = String::from(SkSchedulingPolicy::Activating);

                let rows_affected = diesel::update(
                    safekeepers.filter(id.eq(id_)).filter(
                        scheduling_policy
                            .eq(scheduling_policy_activating)
                            .or(scheduling_policy.eq(&scheduling_policy_active)),
                    ),
                )
                .set(scheduling_policy.eq(&scheduling_policy_active))
                .execute(conn)
                .await?;

                if rows_affected == 0 {
                    return Ok(Some(()));
                }
                if rows_affected != 1 {
                    return Err(DatabaseError::Logical(format!(
                        "unexpected number of rows ({rows_affected})",
                    )));
                }

                Ok(Some(()))
            })
        })
        .await
    }

    /// Persist timeline. Returns if the timeline was newly inserted. If it wasn't, we haven't done any writes.
    pub(crate) async fn insert_timeline(&self, entry: TimelinePersistence) -> DatabaseResult<bool> {
        use crate::schema::timelines;

        let entry = &entry;
        self.with_measured_conn(DatabaseOperation::InsertTimeline, move |conn| {
            Box::pin(async move {
                let inserted_updated = diesel::insert_into(timelines::table)
                    .values(entry)
                    .on_conflict((timelines::tenant_id, timelines::timeline_id))
                    .do_nothing()
                    .execute(conn)
                    .await?;

                match inserted_updated {
                    0 => Ok(false),
                    1 => Ok(true),
                    _ => Err(DatabaseError::Logical(format!(
                        "unexpected number of rows ({inserted_updated})"
                    ))),
                }
            })
        })
        .await
    }

    /// Update an already present timeline.
    /// VERY UNSAFE FUNCTION: this overrides in-progress migrations. Don't use this unless neccessary.
    pub(crate) async fn update_timeline_unsafe(
        &self,
        entry: TimelineUpdate,
    ) -> DatabaseResult<bool> {
        use crate::schema::timelines;

        let entry = &entry;
        self.with_measured_conn(DatabaseOperation::UpdateTimeline, move |conn| {
            Box::pin(async move {
                let inserted_updated = diesel::update(timelines::table)
                    .filter(timelines::tenant_id.eq(&entry.tenant_id))
                    .filter(timelines::timeline_id.eq(&entry.timeline_id))
                    .set(entry)
                    .execute(conn)
                    .await?;

                match inserted_updated {
                    0 => Ok(false),
                    1 => Ok(true),
                    _ => Err(DatabaseError::Logical(format!(
                        "unexpected number of rows ({inserted_updated})"
                    ))),
                }
            })
        })
        .await
    }

    /// Update timeline membership configuration in the database.
    /// Perform a compare-and-swap (CAS) operation on the timeline's generation.
    /// The `new_generation` must be the next (+1) generation after the one in the database.
    /// Also inserts reconcile_requests to safekeeper_timeline_pending_ops table in the same
    /// transaction.
    pub(crate) async fn update_timeline_membership(
        &self,
        tenant_id: TenantId,
        timeline_id: TimelineId,
        new_generation: SafekeeperGeneration,
        sk_set: &[NodeId],
        new_sk_set: Option<&[NodeId]>,
        reconcile_requests: &[TimelinePendingOpPersistence],
    ) -> DatabaseResult<()> {
        use crate::schema::safekeeper_timeline_pending_ops as stpo;
        use crate::schema::timelines;
        use diesel::query_dsl::methods::FilterDsl;

        let prev_generation = new_generation.previous().unwrap();

        let tenant_id = &tenant_id;
        let timeline_id = &timeline_id;
        self.with_measured_conn(DatabaseOperation::UpdateTimelineMembership, move |conn| {
            Box::pin(async move {
                let updated = diesel::update(timelines::table)
                    .filter(timelines::tenant_id.eq(&tenant_id.to_string()))
                    .filter(timelines::timeline_id.eq(&timeline_id.to_string()))
                    .filter(timelines::generation.eq(prev_generation.into_inner() as i32))
                    .set((
                        timelines::generation.eq(new_generation.into_inner() as i32),
                        timelines::sk_set
                            .eq(sk_set.iter().map(|id| id.0 as i64).collect::<Vec<_>>()),
                        timelines::new_sk_set.eq(new_sk_set
                            .map(|set| set.iter().map(|id| id.0 as i64).collect::<Vec<_>>())),
                    ))
                    .execute(conn)
                    .await?;

                match updated {
                    0 => {
                        // TODO(diko): It makes sense to select the current generation
                        // and include it in the error message for better debuggability.
                        return Err(DatabaseError::Cas(
                            "Failed to update membership configuration".to_string(),
                        ));
                    }
                    1 => {}
                    _ => {
                        return Err(DatabaseError::Logical(format!(
                            "unexpected number of rows ({updated})"
                        )));
                    }
                };

                for req in reconcile_requests {
                    let inserted_updated = diesel::insert_into(stpo::table)
                        .values(req)
                        .on_conflict((stpo::tenant_id, stpo::timeline_id, stpo::sk_id))
                        .do_update()
                        .set(req)
                        .filter(stpo::generation.lt(req.generation))
                        .execute(conn)
                        .await?;

                    if inserted_updated > 1 {
                        return Err(DatabaseError::Logical(format!(
                            "unexpected number of rows ({inserted_updated})"
                        )));
                    }
                }

                Ok(())
            })
        })
        .await
    }

    /// Update the cplane notified generation for a timeline.
    /// Perform a compare-and-swap (CAS) operation on the timeline's cplane notified generation.
    /// The update will fail if the specified generation is less than the cplane notified generation
    /// in the database.
    pub(crate) async fn update_cplane_notified_generation(
        &self,
        tenant_id: TenantId,
        timeline_id: TimelineId,
        generation: SafekeeperGeneration,
    ) -> DatabaseResult<()> {
        use crate::schema::timelines::dsl;

        let tenant_id = &tenant_id;
        let timeline_id = &timeline_id;
        self.with_measured_conn(
            DatabaseOperation::UpdateCplaneNotifiedGeneration,
            move |conn| {
                Box::pin(async move {
                    let updated = diesel::update(dsl::timelines)
                        .filter(dsl::tenant_id.eq(&tenant_id.to_string()))
                        .filter(dsl::timeline_id.eq(&timeline_id.to_string()))
                        .filter(dsl::cplane_notified_generation.le(generation.into_inner() as i32))
                        .set(dsl::cplane_notified_generation.eq(generation.into_inner() as i32))
                        .execute(conn)
                        .await?;

                    match updated {
                        0 => Err(DatabaseError::Cas(
                            "Failed to update cplane notified generation".to_string(),
                        )),
                        1 => Ok(()),
                        _ => Err(DatabaseError::Logical(format!(
                            "unexpected number of rows ({updated})"
                        ))),
                    }
                })
            },
        )
        .await
    }

    /// Update the sk set notified generation for a timeline.
    /// Perform a compare-and-swap (CAS) operation on the timeline's sk set notified generation.
    /// The update will fail if the specified generation is less than the sk set notified generation
    /// in the database.
    pub(crate) async fn update_sk_set_notified_generation(
        &self,
        tenant_id: TenantId,
        timeline_id: TimelineId,
        generation: SafekeeperGeneration,
    ) -> DatabaseResult<()> {
        use crate::schema::timelines::dsl;

        let tenant_id = &tenant_id;
        let timeline_id = &timeline_id;
        self.with_measured_conn(
            DatabaseOperation::UpdateSkSetNotifiedGeneration,
            move |conn| {
                Box::pin(async move {
                    let updated = diesel::update(dsl::timelines)
                        .filter(dsl::tenant_id.eq(&tenant_id.to_string()))
                        .filter(dsl::timeline_id.eq(&timeline_id.to_string()))
                        .filter(dsl::sk_set_notified_generation.le(generation.into_inner() as i32))
                        .set(dsl::sk_set_notified_generation.eq(generation.into_inner() as i32))
                        .execute(conn)
                        .await?;

                    match updated {
                        0 => Err(DatabaseError::Cas(
                            "Failed to update sk set notified generation".to_string(),
                        )),
                        1 => Ok(()),
                        _ => Err(DatabaseError::Logical(format!(
                            "unexpected number of rows ({updated})"
                        ))),
                    }
                })
            },
        )
        .await
    }

    /// Load timeline from db. Returns `None` if not present.
    pub(crate) async fn get_timeline(
        &self,
        tenant_id: TenantId,
        timeline_id: TimelineId,
    ) -> DatabaseResult<Option<TimelinePersistence>> {
        use crate::schema::timelines::dsl;

        let tenant_id = &tenant_id;
        let timeline_id = &timeline_id;
        let timeline_from_db = self
            .with_measured_conn(DatabaseOperation::GetTimeline, move |conn| {
                Box::pin(async move {
                    let mut from_db: Vec<TimelineFromDb> = dsl::timelines
                        .filter(
                            dsl::tenant_id
                                .eq(&tenant_id.to_string())
                                .and(dsl::timeline_id.eq(&timeline_id.to_string())),
                        )
                        .load(conn)
                        .await?;
                    if from_db.is_empty() {
                        return Ok(None);
                    }
                    if from_db.len() != 1 {
                        return Err(DatabaseError::Logical(format!(
                            "unexpected number of rows ({})",
                            from_db.len()
                        )));
                    }

                    Ok(Some(from_db.pop().unwrap().into_persistence()))
                })
            })
            .await?;

        Ok(timeline_from_db)
    }

    /// Set `delete_at` for the given timeline
    pub(crate) async fn timeline_set_deleted_at(
        &self,
        tenant_id: TenantId,
        timeline_id: TimelineId,
    ) -> DatabaseResult<()> {
        use crate::schema::timelines;

        let deletion_time = chrono::Local::now().to_utc();
        self.with_measured_conn(DatabaseOperation::InsertTimeline, move |conn| {
            Box::pin(async move {
                let updated = diesel::update(timelines::table)
                    .filter(timelines::tenant_id.eq(tenant_id.to_string()))
                    .filter(timelines::timeline_id.eq(timeline_id.to_string()))
                    .set(timelines::deleted_at.eq(Some(deletion_time)))
                    .execute(conn)
                    .await?;

                match updated {
                    0 => Ok(()),
                    1 => Ok(()),
                    _ => Err(DatabaseError::Logical(format!(
                        "unexpected number of rows ({updated})"
                    ))),
                }
            })
        })
        .await
    }

    /// Load timeline from db. Returns `None` if not present.
    ///
    /// Only works if `deleted_at` is set, so you should call [`Self::timeline_set_deleted_at`] before.
    pub(crate) async fn delete_timeline(
        &self,
        tenant_id: TenantId,
        timeline_id: TimelineId,
    ) -> DatabaseResult<()> {
        use crate::schema::timelines::dsl;

        let tenant_id = &tenant_id;
        let timeline_id = &timeline_id;
        self.with_measured_conn(DatabaseOperation::GetTimeline, move |conn| {
            Box::pin(async move {
                diesel::delete(dsl::timelines)
                    .filter(dsl::tenant_id.eq(&tenant_id.to_string()))
                    .filter(dsl::timeline_id.eq(&timeline_id.to_string()))
                    .filter(dsl::deleted_at.is_not_null())
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .await?;

        Ok(())
    }

    /// Loads a list of all timelines from database.
    pub(crate) async fn list_timelines_for_tenant(
        &self,
        tenant_id: TenantId,
    ) -> DatabaseResult<Vec<TimelinePersistence>> {
        use crate::schema::timelines::dsl;

        let tenant_id = &tenant_id;
        let timelines = self
            .with_measured_conn(DatabaseOperation::GetTimeline, move |conn| {
                Box::pin(async move {
                    let timelines: Vec<TimelineFromDb> = dsl::timelines
                        .filter(dsl::tenant_id.eq(&tenant_id.to_string()))
                        .load(conn)
                        .await?;
                    Ok(timelines)
                })
            })
            .await?;

        let timelines = timelines
            .into_iter()
            .map(TimelineFromDb::into_persistence)
            .collect();
        Ok(timelines)
    }

    /// Persist pending op. Returns if it was newly inserted. If it wasn't, we haven't done any writes.
    pub(crate) async fn insert_pending_op(
        &self,
        entry: TimelinePendingOpPersistence,
    ) -> DatabaseResult<bool> {
        use crate::schema::safekeeper_timeline_pending_ops as skpo;
        // This overrides the `filter` fn used in other functions, so contain the mayhem via a function-local use
        use diesel::query_dsl::methods::FilterDsl;

        let entry = &entry;
        self.with_measured_conn(DatabaseOperation::InsertTimelineReconcile, move |conn| {
            Box::pin(async move {
                // For simplicity it makes sense to keep only the last operation
                // per (tenant, timeline, sk) tuple: if we migrated a timeline
                // from node and adding it back it is not necessary to remove
                // data on it. Hence, generation is not part of primary key and
                // we override any rows with lower generations here.
                let inserted_updated = diesel::insert_into(skpo::table)
                    .values(entry)
                    .on_conflict((skpo::tenant_id, skpo::timeline_id, skpo::sk_id))
                    .do_update()
                    .set(entry)
                    .filter(skpo::generation.lt(entry.generation))
                    .execute(conn)
                    .await?;

                match inserted_updated {
                    0 => Ok(false),
                    1 => Ok(true),
                    _ => Err(DatabaseError::Logical(format!(
                        "unexpected number of rows ({inserted_updated})"
                    ))),
                }
            })
        })
        .await
    }
    /// Remove persisted pending op.
    pub(crate) async fn remove_pending_op(
        &self,
        tenant_id: TenantId,
        timeline_id: Option<TimelineId>,
        sk_id: NodeId,
        generation: u32,
    ) -> DatabaseResult<()> {
        use crate::schema::safekeeper_timeline_pending_ops::dsl;

        let tenant_id = &tenant_id;
        let timeline_id = &timeline_id;
        self.with_measured_conn(DatabaseOperation::RemoveTimelineReconcile, move |conn| {
            let timeline_id_str = timeline_id.map(|tid| tid.to_string()).unwrap_or_default();
            Box::pin(async move {
                diesel::delete(dsl::safekeeper_timeline_pending_ops)
                    .filter(dsl::tenant_id.eq(tenant_id.to_string()))
                    .filter(dsl::timeline_id.eq(timeline_id_str))
                    .filter(dsl::sk_id.eq(sk_id.0 as i64))
                    .filter(dsl::generation.eq(generation as i32))
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .await
    }

    /// Load pending operations from db, joined together with timeline data.
    pub(crate) async fn list_pending_ops_with_timelines(
        &self,
    ) -> DatabaseResult<Vec<(TimelinePendingOpPersistence, Option<TimelinePersistence>)>> {
        use crate::schema::safekeeper_timeline_pending_ops::dsl;
        use crate::schema::timelines;

        let timeline_from_db = self
            .with_measured_conn(
                DatabaseOperation::ListTimelineReconcileStartup,
                move |conn| {
                    Box::pin(async move {
                        let from_db: Vec<(TimelinePendingOpPersistence, Option<TimelineFromDb>)> =
                            dsl::safekeeper_timeline_pending_ops
                                .left_join(
                                    timelines::table.on(timelines::tenant_id
                                        .eq(dsl::tenant_id)
                                        .and(timelines::timeline_id.eq(dsl::timeline_id))),
                                )
                                .select((
                                    TimelinePendingOpPersistence::as_select(),
                                    Option::<TimelineFromDb>::as_select(),
                                ))
                                .load(conn)
                                .await?;
                        Ok(from_db)
                    })
                },
            )
            .await?;

        Ok(timeline_from_db
            .into_iter()
            .map(|(op, tl_opt)| (op, tl_opt.map(|tl_opt| tl_opt.into_persistence())))
            .collect())
    }
    /// List pending operations for a given timeline (including tenant-global ones)
    pub(crate) async fn list_pending_ops_for_timeline(
        &self,
        tenant_id: TenantId,
        timeline_id: TimelineId,
    ) -> DatabaseResult<Vec<TimelinePendingOpPersistence>> {
        use crate::schema::safekeeper_timeline_pending_ops::dsl;

        let timelines_from_db = self
            .with_measured_conn(DatabaseOperation::ListTimelineReconcile, move |conn| {
                Box::pin(async move {
                    let from_db: Vec<TimelinePendingOpPersistence> =
                        dsl::safekeeper_timeline_pending_ops
                            .filter(dsl::tenant_id.eq(tenant_id.to_string()))
                            .filter(
                                dsl::timeline_id
                                    .eq(timeline_id.to_string())
                                    .or(dsl::timeline_id.eq("")),
                            )
                            .load(conn)
                            .await?;
                    Ok(from_db)
                })
            })
            .await?;

        Ok(timelines_from_db)
    }

    /// Delete all pending ops for the given timeline.
    ///
    /// Use this only at timeline deletion, otherwise use generation based APIs
    pub(crate) async fn remove_pending_ops_for_timeline(
        &self,
        tenant_id: TenantId,
        timeline_id: Option<TimelineId>,
    ) -> DatabaseResult<()> {
        use crate::schema::safekeeper_timeline_pending_ops::dsl;

        let tenant_id = &tenant_id;
        let timeline_id = &timeline_id;
        self.with_measured_conn(DatabaseOperation::RemoveTimelineReconcile, move |conn| {
            let timeline_id_str = timeline_id.map(|tid| tid.to_string()).unwrap_or_default();
            Box::pin(async move {
                diesel::delete(dsl::safekeeper_timeline_pending_ops)
                    .filter(dsl::tenant_id.eq(tenant_id.to_string()))
                    .filter(dsl::timeline_id.eq(timeline_id_str))
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .await?;

        Ok(())
    }

    pub(crate) async fn insert_timeline_import(
        &self,
        import: TimelineImportPersistence,
    ) -> DatabaseResult<bool> {
        self.with_measured_conn(DatabaseOperation::InsertTimelineImport, move |conn| {
            Box::pin({
                let import = import.clone();
                async move {
                    let inserted = diesel::insert_into(crate::schema::timeline_imports::table)
                        .values(import)
                        .execute(conn)
                        .await?;
                    Ok(inserted == 1)
                }
            })
        })
        .await
    }

    pub(crate) async fn list_timeline_imports(&self) -> DatabaseResult<Vec<TimelineImport>> {
        use crate::schema::timeline_imports::dsl;
        let persistent = self
            .with_measured_conn(DatabaseOperation::ListTimelineImports, move |conn| {
                Box::pin(async move {
                    let from_db: Vec<TimelineImportPersistence> =
                        dsl::timeline_imports.load(conn).await?;
                    Ok(from_db)
                })
            })
            .await?;

        let imports: Result<Vec<TimelineImport>, _> = persistent
            .into_iter()
            .map(TimelineImport::from_persistent)
            .collect();
        match imports {
            Ok(ok) => Ok(ok.into_iter().collect()),
            Err(err) => Err(DatabaseError::Logical(format!(
                "failed to deserialize import: {err}"
            ))),
        }
    }

    pub(crate) async fn get_timeline_import(
        &self,
        tenant_id: TenantId,
        timeline_id: TimelineId,
    ) -> DatabaseResult<Option<TimelineImport>> {
        use crate::schema::timeline_imports::dsl;
        let persistent_import = self
            .with_measured_conn(DatabaseOperation::ListTimelineImports, move |conn| {
                Box::pin(async move {
                    let mut from_db: Vec<TimelineImportPersistence> = dsl::timeline_imports
                        .filter(dsl::tenant_id.eq(tenant_id.to_string()))
                        .filter(dsl::timeline_id.eq(timeline_id.to_string()))
                        .load(conn)
                        .await?;

                    if from_db.len() > 1 {
                        return Err(DatabaseError::Logical(format!(
                            "unexpected number of rows ({})",
                            from_db.len()
                        )));
                    }

                    Ok(from_db.pop())
                })
            })
            .await?;

        persistent_import
            .map(TimelineImport::from_persistent)
            .transpose()
            .map_err(|err| DatabaseError::Logical(format!("failed to deserialize import: {err}")))
    }

    pub(crate) async fn delete_timeline_import(
        &self,
        tenant_id: TenantId,
        timeline_id: TimelineId,
    ) -> DatabaseResult<()> {
        use crate::schema::timeline_imports::dsl;

        self.with_measured_conn(DatabaseOperation::DeleteTimelineImport, move |conn| {
            Box::pin(async move {
                diesel::delete(crate::schema::timeline_imports::table)
                    .filter(
                        dsl::tenant_id
                            .eq(tenant_id.to_string())
                            .and(dsl::timeline_id.eq(timeline_id.to_string())),
                    )
                    .execute(conn)
                    .await?;

                Ok(())
            })
        })
        .await
    }

    /// Idempotently update the status of one shard for an ongoing timeline import
    ///
    /// If the update was persisted to the database, then the current state of the
    /// import is returned to the caller. In case of logical errors a bespoke
    /// [`TimelineImportUpdateError`] instance is returned. Other database errors
    /// are covered by the outer [`DatabaseError`].
    pub(crate) async fn update_timeline_import(
        &self,
        tenant_shard_id: TenantShardId,
        timeline_id: TimelineId,
        shard_status: ShardImportStatus,
    ) -> DatabaseResult<Result<Option<TimelineImport>, TimelineImportUpdateError>> {
        use crate::schema::timeline_imports::dsl;

        self.with_measured_conn(DatabaseOperation::UpdateTimelineImport, move |conn| {
            Box::pin({
                let shard_status = shard_status.clone();
                async move {
                    // Load the current state from the database
                    let mut from_db: Vec<TimelineImportPersistence> = dsl::timeline_imports
                        .filter(
                            dsl::tenant_id
                                .eq(tenant_shard_id.tenant_id.to_string())
                                .and(dsl::timeline_id.eq(timeline_id.to_string())),
                        )
                        .load(conn)
                        .await?;

                    assert!(from_db.len() <= 1);

                    let mut status = match from_db.pop() {
                        Some(some) => TimelineImport::from_persistent(some).unwrap(),
                        None => {
                            return Ok(Err(TimelineImportUpdateError::ImportNotFound {
                                tenant_id: tenant_shard_id.tenant_id,
                                timeline_id,
                            }));
                        }
                    };

                    // Perform the update in-memory
                    let follow_up = match status.update(tenant_shard_id.to_index(), shard_status) {
                        Ok(ok) => ok,
                        Err(err) => {
                            return Ok(Err(err));
                        }
                    };

                    let new_persistent = status.to_persistent();

                    // Write back if required (in the same transaction)
                    match follow_up {
                        TimelineImportUpdateFollowUp::Persist => {
                            let updated = diesel::update(dsl::timeline_imports)
                                .filter(
                                    dsl::tenant_id
                                        .eq(tenant_shard_id.tenant_id.to_string())
                                        .and(dsl::timeline_id.eq(timeline_id.to_string())),
                                )
                                .set(dsl::shard_statuses.eq(new_persistent.shard_statuses))
                                .execute(conn)
                                .await?;

                            if updated != 1 {
                                return Ok(Err(TimelineImportUpdateError::ImportNotFound {
                                    tenant_id: tenant_shard_id.tenant_id,
                                    timeline_id,
                                }));
                            }

                            Ok(Ok(Some(status)))
                        }
                        TimelineImportUpdateFollowUp::None => Ok(Ok(None)),
                    }
                }
            })
        })
        .await
    }

    pub(crate) async fn is_tenant_importing_timeline(
        &self,
        tenant_id: TenantId,
    ) -> DatabaseResult<bool> {
        use crate::schema::timeline_imports::dsl;
        self.with_measured_conn(DatabaseOperation::IsTenantImportingTimeline, move |conn| {
            Box::pin(async move {
                let imports: i64 = dsl::timeline_imports
                    .filter(dsl::tenant_id.eq(tenant_id.to_string()))
                    .count()
                    .get_result(conn)
                    .await?;

                Ok(imports > 0)
            })
        })
        .await
    }
}

pub(crate) fn load_certs() -> anyhow::Result<Arc<rustls::RootCertStore>> {
    let der_certs = rustls_native_certs::load_native_certs();

    if !der_certs.errors.is_empty() {
        anyhow::bail!("could not parse certificates: {:?}", der_certs.errors);
    }

    let mut store = rustls::RootCertStore::empty();
    store.add_parsable_certificates(der_certs.certs);
    Ok(Arc::new(store))
}

#[derive(Debug)]
/// A verifier that accepts all certificates (but logs an error still)
struct AcceptAll(Arc<WebPkiServerVerifier>);
impl ServerCertVerifier for AcceptAll {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let r =
            self.0
                .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now);
        if let Err(err) = r {
            tracing::info!(
                ?server_name,
                "ignoring db connection TLS validation error: {err:?}"
            );
            return Ok(ServerCertVerified::assertion());
        }
        r
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// Loads the root certificates and constructs a client config suitable for connecting.
/// This function is blocking.
fn client_config_with_root_certs() -> anyhow::Result<rustls::ClientConfig> {
    let client_config =
        rustls::ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("ring should support the default protocol versions");
    static DO_CERT_CHECKS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let do_cert_checks =
        DO_CERT_CHECKS.get_or_init(|| std::env::var("STORCON_DB_CERT_CHECKS").is_ok());
    Ok(if *do_cert_checks {
        client_config
            .with_root_certificates(load_certs()?)
            .with_no_client_auth()
    } else {
        let verifier = AcceptAll(
            WebPkiServerVerifier::builder_with_provider(
                load_certs()?,
                Arc::new(ring::default_provider()),
            )
            .build()?,
        );
        client_config
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth()
    })
}

fn establish_connection_rustls(config: &str) -> BoxFuture<ConnectionResult<AsyncPgConnection>> {
    let fut = async {
        // We first set up the way we want rustls to work.
        let rustls_config = client_config_with_root_certs()
            .map_err(|err| ConnectionError::BadConnection(format!("{err:?}")))?;
        let tls = tokio_postgres_rustls::MakeRustlsConnect::new(rustls_config);
        let (client, conn) = tokio_postgres::connect(config, tls)
            .await
            .map_err(|e| ConnectionError::BadConnection(e.to_string()))?;

        AsyncPgConnection::try_from_client_and_connection(client, conn).await
    };
    fut.boxed()
}

#[cfg_attr(test, test)]
fn test_config_debug_censors_password() {
    let has_pw =
        "host=/var/lib/postgresql,localhost port=1234 user=specialuser password='NOT ALLOWED TAG'";
    let has_pw_cfg = has_pw.parse::<tokio_postgres::Config>().unwrap();
    assert!(format!("{has_pw_cfg:?}").contains("specialuser"));
    // Ensure that the password is not leaked by the debug impl
    assert!(!format!("{has_pw_cfg:?}").contains("NOT ALLOWED TAG"));
}

fn log_postgres_connstr_info(config_str: &str) -> anyhow::Result<()> {
    let config = config_str
        .parse::<tokio_postgres::Config>()
        .map_err(|_e| anyhow::anyhow!("Couldn't parse config str"))?;
    // We use debug formatting here, and use a unit test to ensure that we don't leak the password.
    // To make extra sure the test gets ran, run it every time the function is called
    // (this is rather cold code, we can afford it).
    #[cfg(not(test))]
    test_config_debug_censors_password();
    tracing::info!("database connection config: {config:?}");
    Ok(())
}

/// Parts of [`crate::tenant_shard::TenantShard`] that are stored durably
#[derive(
    QueryableByName, Queryable, Selectable, Insertable, Serialize, Deserialize, Clone, Eq, PartialEq,
)]
#[diesel(table_name = crate::schema::tenant_shards)]
pub(crate) struct TenantShardPersistence {
    #[serde(default)]
    pub(crate) tenant_id: String,
    #[serde(default)]
    pub(crate) shard_number: i32,
    #[serde(default)]
    pub(crate) shard_count: i32,
    #[serde(default)]
    pub(crate) shard_stripe_size: i32,

    // Latest generation number: next time we attach, increment this
    // and use the incremented number when attaching.
    //
    // Generation is only None when first onboarding a tenant, where it may
    // be in PlacementPolicy::Secondary and therefore have no valid generation state.
    pub(crate) generation: Option<i32>,

    // Currently attached pageserver
    #[serde(rename = "pageserver")]
    pub(crate) generation_pageserver: Option<i64>,

    #[serde(default)]
    pub(crate) placement_policy: String,
    #[serde(default)]
    pub(crate) splitting: SplitState,
    #[serde(default)]
    pub(crate) config: String,
    #[serde(default)]
    pub(crate) scheduling_policy: String,

    // Hint that we should attempt to schedule this tenant shard the given
    // availability zone in order to minimise the chances of cross-AZ communication
    // with compute.
    pub(crate) preferred_az_id: Option<String>,
}

impl TenantShardPersistence {
    fn get_shard_count(&self) -> Result<ShardCount, ShardConfigError> {
        self.shard_count
            .try_into()
            .map(ShardCount)
            .map_err(|_| ShardConfigError::InvalidCount)
    }

    fn get_shard_number(&self) -> Result<ShardNumber, ShardConfigError> {
        self.shard_number
            .try_into()
            .map(ShardNumber)
            .map_err(|_| ShardConfigError::InvalidNumber)
    }

    fn get_stripe_size(&self) -> Result<ShardStripeSize, ShardConfigError> {
        self.shard_stripe_size
            .try_into()
            .map(ShardStripeSize)
            .map_err(|_| ShardConfigError::InvalidStripeSize)
    }

    pub(crate) fn get_shard_identity(&self) -> Result<ShardIdentity, ShardConfigError> {
        if self.shard_count == 0 {
            // NB: carry over the stripe size from the persisted record, to avoid consistency check
            // failures if the persisted value differs from the default stripe size. The stripe size
            // doesn't really matter for unsharded tenants anyway.
            Ok(ShardIdentity::unsharded_with_stripe_size(
                self.get_stripe_size()?,
            ))
        } else {
            Ok(ShardIdentity::new(
                self.get_shard_number()?,
                self.get_shard_count()?,
                self.get_stripe_size()?,
            )?)
        }
    }

    pub(crate) fn get_tenant_shard_id(&self) -> anyhow::Result<TenantShardId> {
        Ok(TenantShardId {
            tenant_id: TenantId::from_str(self.tenant_id.as_str())?,
            shard_number: self.get_shard_number()?,
            shard_count: self.get_shard_count()?,
        })
    }
}

/// Parts of [`crate::node::Node`] that are stored durably
#[derive(Serialize, Deserialize, Queryable, Selectable, Insertable, Eq, PartialEq)]
#[diesel(table_name = crate::schema::nodes)]
pub(crate) struct NodePersistence {
    pub(crate) node_id: i64,
    pub(crate) scheduling_policy: String,
    pub(crate) listen_http_addr: String,
    pub(crate) listen_http_port: i32,
    pub(crate) listen_pg_addr: String,
    pub(crate) listen_pg_port: i32,
    pub(crate) availability_zone_id: String,
    pub(crate) listen_https_port: Option<i32>,
    pub(crate) lifecycle: String,
    pub(crate) listen_grpc_addr: Option<String>,
    pub(crate) listen_grpc_port: Option<i32>,
}

/// Tenant metadata health status that are stored durably.
#[derive(Queryable, Selectable, Insertable, Serialize, Deserialize, Clone, Eq, PartialEq)]
#[diesel(table_name = crate::schema::metadata_health)]
pub(crate) struct MetadataHealthPersistence {
    #[serde(default)]
    pub(crate) tenant_id: String,
    #[serde(default)]
    pub(crate) shard_number: i32,
    #[serde(default)]
    pub(crate) shard_count: i32,

    pub(crate) healthy: bool,
    pub(crate) last_scrubbed_at: chrono::DateTime<chrono::Utc>,
}

impl MetadataHealthPersistence {
    pub fn new(
        tenant_shard_id: TenantShardId,
        healthy: bool,
        last_scrubbed_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        let tenant_id = tenant_shard_id.tenant_id.to_string();
        let shard_number = tenant_shard_id.shard_number.0 as i32;
        let shard_count = tenant_shard_id.shard_count.literal() as i32;

        MetadataHealthPersistence {
            tenant_id,
            shard_number,
            shard_count,
            healthy,
            last_scrubbed_at,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn get_tenant_shard_id(&self) -> Result<TenantShardId, hex::FromHexError> {
        Ok(TenantShardId {
            tenant_id: TenantId::from_str(self.tenant_id.as_str())?,
            shard_number: ShardNumber(self.shard_number as u8),
            shard_count: ShardCount::new(self.shard_count as u8),
        })
    }
}

impl From<MetadataHealthPersistence> for MetadataHealthRecord {
    fn from(value: MetadataHealthPersistence) -> Self {
        MetadataHealthRecord {
            tenant_shard_id: value
                .get_tenant_shard_id()
                .expect("stored tenant id should be valid"),
            healthy: value.healthy,
            last_scrubbed_at: value.last_scrubbed_at,
        }
    }
}

#[derive(
    Serialize, Deserialize, Queryable, Selectable, Insertable, Eq, PartialEq, Debug, Clone,
)]
#[diesel(table_name = crate::schema::controllers)]
pub(crate) struct ControllerPersistence {
    pub(crate) address: String,
    pub(crate) started_at: chrono::DateTime<chrono::Utc>,
}

// What we store in the database
#[derive(Serialize, Deserialize, Queryable, Selectable, Eq, PartialEq, Debug, Clone)]
#[diesel(table_name = crate::schema::safekeepers)]
pub(crate) struct SafekeeperPersistence {
    pub(crate) id: i64,
    pub(crate) region_id: String,
    /// 1 is special, it means just created (not currently posted to storcon).
    /// Zero or negative is not really expected.
    /// Otherwise the number from `release-$(number_of_commits_on_branch)` tag.
    pub(crate) version: i64,
    pub(crate) host: String,
    pub(crate) port: i32,
    pub(crate) http_port: i32,
    pub(crate) availability_zone_id: String,
    pub(crate) scheduling_policy: SkSchedulingPolicyFromSql,
    pub(crate) https_port: Option<i32>,
}

/// Wrapper struct around [`SkSchedulingPolicy`] because both it and [`FromSql`] are from foreign crates,
/// and we don't want to make [`safekeeper_api`] depend on [`diesel`].
#[derive(Serialize, Deserialize, FromSqlRow, Eq, PartialEq, Debug, Copy, Clone)]
pub(crate) struct SkSchedulingPolicyFromSql(pub(crate) SkSchedulingPolicy);

impl From<SkSchedulingPolicy> for SkSchedulingPolicyFromSql {
    fn from(value: SkSchedulingPolicy) -> Self {
        SkSchedulingPolicyFromSql(value)
    }
}

impl FromSql<diesel::sql_types::VarChar, Pg> for SkSchedulingPolicyFromSql {
    fn from_sql(
        bytes: <Pg as diesel::backend::Backend>::RawValue<'_>,
    ) -> diesel::deserialize::Result<Self> {
        let bytes = bytes.as_bytes();
        match core::str::from_utf8(bytes) {
            Ok(s) => match SkSchedulingPolicy::from_str(s) {
                Ok(policy) => Ok(SkSchedulingPolicyFromSql(policy)),
                Err(e) => Err(format!("can't parse: {e}").into()),
            },
            Err(e) => Err(format!("invalid UTF-8 for scheduling policy: {e}").into()),
        }
    }
}

impl SafekeeperPersistence {
    pub(crate) fn from_upsert(
        upsert: SafekeeperUpsert,
        scheduling_policy: SkSchedulingPolicy,
    ) -> Self {
        crate::persistence::SafekeeperPersistence {
            id: upsert.id,
            region_id: upsert.region_id,
            version: upsert.version,
            host: upsert.host,
            port: upsert.port,
            http_port: upsert.http_port,
            https_port: upsert.https_port,
            availability_zone_id: upsert.availability_zone_id,
            scheduling_policy: SkSchedulingPolicyFromSql(scheduling_policy),
        }
    }
    pub(crate) fn as_describe_response(&self) -> Result<SafekeeperDescribeResponse, DatabaseError> {
        Ok(SafekeeperDescribeResponse {
            id: NodeId(self.id as u64),
            region_id: self.region_id.clone(),
            version: self.version,
            host: self.host.clone(),
            port: self.port,
            http_port: self.http_port,
            https_port: self.https_port,
            availability_zone_id: self.availability_zone_id.clone(),
            scheduling_policy: self.scheduling_policy.0,
        })
    }
}

/// What we expect from the upsert http api
#[derive(Serialize, Deserialize, Eq, PartialEq, Debug, Clone)]
pub(crate) struct SafekeeperUpsert {
    pub(crate) id: i64,
    pub(crate) region_id: String,
    /// 1 is special, it means just created (not currently posted to storcon).
    /// Zero or negative is not really expected.
    /// Otherwise the number from `release-$(number_of_commits_on_branch)` tag.
    pub(crate) version: i64,
    pub(crate) host: String,
    pub(crate) port: i32,
    /// The active flag will not be stored in the database and will be ignored.
    pub(crate) active: Option<bool>,
    pub(crate) http_port: i32,
    pub(crate) https_port: Option<i32>,
    pub(crate) availability_zone_id: String,
}

impl SafekeeperUpsert {
    fn as_insert_or_update(&self) -> anyhow::Result<InsertUpdateSafekeeper<'_>> {
        if self.version < 0 {
            anyhow::bail!("negative version: {}", self.version);
        }
        Ok(InsertUpdateSafekeeper {
            id: self.id,
            region_id: &self.region_id,
            version: self.version,
            host: &self.host,
            port: self.port,
            http_port: self.http_port,
            https_port: self.https_port,
            availability_zone_id: &self.availability_zone_id,
            // None means a wish to not update this column. We expose abilities to update it via other means.
            scheduling_policy: None,
        })
    }
}

#[derive(Insertable, AsChangeset)]
#[diesel(table_name = crate::schema::safekeepers)]
struct InsertUpdateSafekeeper<'a> {
    id: i64,
    region_id: &'a str,
    version: i64,
    host: &'a str,
    port: i32,
    http_port: i32,
    https_port: Option<i32>,
    availability_zone_id: &'a str,
    scheduling_policy: Option<&'a str>,
}

#[derive(Serialize, Deserialize, FromSqlRow, AsExpression, Eq, PartialEq, Debug, Copy, Clone)]
#[diesel(sql_type = crate::schema::sql_types::PgLsn)]
pub(crate) struct LsnWrapper(pub(crate) Lsn);

impl From<Lsn> for LsnWrapper {
    fn from(value: Lsn) -> Self {
        LsnWrapper(value)
    }
}

impl FromSql<crate::schema::sql_types::PgLsn, Pg> for LsnWrapper {
    fn from_sql(
        bytes: <Pg as diesel::backend::Backend>::RawValue<'_>,
    ) -> diesel::deserialize::Result<Self> {
        let byte_arr: diesel::deserialize::Result<[u8; 8]> = bytes
            .as_bytes()
            .try_into()
            .map_err(|_| "Can't obtain lsn from sql".into());
        Ok(LsnWrapper(Lsn(u64::from_be_bytes(byte_arr?))))
    }
}

impl ToSql<crate::schema::sql_types::PgLsn, Pg> for LsnWrapper {
    fn to_sql<'b>(
        &'b self,
        out: &mut diesel::serialize::Output<'b, '_, Pg>,
    ) -> diesel::serialize::Result {
        out.write_all(&u64::to_be_bytes(self.0.0))
            .map(|_| IsNull::No)
            .map_err(Into::into)
    }
}

#[derive(Insertable, AsChangeset, Clone)]
#[diesel(table_name = crate::schema::timelines)]
pub(crate) struct TimelinePersistence {
    pub(crate) tenant_id: String,
    pub(crate) timeline_id: String,
    pub(crate) start_lsn: LsnWrapper,
    pub(crate) generation: i32,
    pub(crate) sk_set: Vec<i64>,
    pub(crate) new_sk_set: Option<Vec<i64>>,
    pub(crate) cplane_notified_generation: i32,
    pub(crate) deleted_at: Option<chrono::DateTime<chrono::Utc>>,
    pub(crate) sk_set_notified_generation: i32,
}

/// This is separate from [TimelinePersistence] only because postgres allows NULLs
/// in arrays and there is no way to forbid that at schema level. Hence diesel
/// wants `sk_set` to be `Vec<Option<i64>>` instead of `Vec<i64>` for
/// Queryable/Selectable. It does however allow insertions without redundant
/// Option(s), so [TimelinePersistence] doesn't have them.
#[derive(Queryable, Selectable)]
#[diesel(table_name = crate::schema::timelines)]
pub(crate) struct TimelineFromDb {
    pub(crate) tenant_id: String,
    pub(crate) timeline_id: String,
    pub(crate) start_lsn: LsnWrapper,
    pub(crate) generation: i32,
    pub(crate) sk_set: Vec<Option<i64>>,
    pub(crate) new_sk_set: Option<Vec<Option<i64>>>,
    pub(crate) cplane_notified_generation: i32,
    pub(crate) deleted_at: Option<chrono::DateTime<chrono::Utc>>,
    pub(crate) sk_set_notified_generation: i32,
}

impl TimelineFromDb {
    fn into_persistence(self) -> TimelinePersistence {
        // We should never encounter null entries in the sets, but we need to filter them out.
        // There is no way to forbid this in the schema that diesel recognizes (to our knowledge).
        let sk_set = self.sk_set.into_iter().flatten().collect::<Vec<_>>();
        let new_sk_set = self
            .new_sk_set
            .map(|s| s.into_iter().flatten().collect::<Vec<_>>());
        TimelinePersistence {
            tenant_id: self.tenant_id,
            timeline_id: self.timeline_id,
            start_lsn: self.start_lsn,
            generation: self.generation,
            sk_set,
            new_sk_set,
            cplane_notified_generation: self.cplane_notified_generation,
            deleted_at: self.deleted_at,
            sk_set_notified_generation: self.sk_set_notified_generation,
        }
    }
}

// This is separate from TimelinePersistence because we don't want to touch generation and deleted_at values for the update.
#[derive(AsChangeset)]
#[diesel(table_name = crate::schema::timelines)]
#[diesel(treat_none_as_null = true)]
pub(crate) struct TimelineUpdate {
    pub(crate) tenant_id: String,
    pub(crate) timeline_id: String,
    pub(crate) start_lsn: LsnWrapper,
    pub(crate) sk_set: Vec<i64>,
    pub(crate) new_sk_set: Option<Vec<i64>>,
}

#[derive(Insertable, AsChangeset, Queryable, Selectable, Clone)]
#[diesel(table_name = crate::schema::safekeeper_timeline_pending_ops)]
pub(crate) struct TimelinePendingOpPersistence {
    pub(crate) sk_id: i64,
    pub(crate) tenant_id: String,
    pub(crate) timeline_id: String,
    pub(crate) generation: i32,
    pub(crate) op_kind: SafekeeperTimelineOpKind,
}

#[derive(Serialize, Deserialize, FromSqlRow, AsExpression, Eq, PartialEq, Debug, Copy, Clone)]
#[diesel(sql_type = diesel::sql_types::VarChar)]
pub(crate) enum SafekeeperTimelineOpKind {
    Pull,
    Exclude,
    Delete,
}

impl FromSql<diesel::sql_types::VarChar, Pg> for SafekeeperTimelineOpKind {
    fn from_sql(
        bytes: <Pg as diesel::backend::Backend>::RawValue<'_>,
    ) -> diesel::deserialize::Result<Self> {
        let bytes = bytes.as_bytes();
        match core::str::from_utf8(bytes) {
            Ok(s) => match s {
                "pull" => Ok(SafekeeperTimelineOpKind::Pull),
                "exclude" => Ok(SafekeeperTimelineOpKind::Exclude),
                "delete" => Ok(SafekeeperTimelineOpKind::Delete),
                _ => Err(format!("can't parse: {s}").into()),
            },
            Err(e) => Err(format!("invalid UTF-8 for op_kind: {e}").into()),
        }
    }
}

impl ToSql<diesel::sql_types::VarChar, Pg> for SafekeeperTimelineOpKind {
    fn to_sql<'b>(
        &'b self,
        out: &mut diesel::serialize::Output<'b, '_, Pg>,
    ) -> diesel::serialize::Result {
        let kind_str = match self {
            SafekeeperTimelineOpKind::Pull => "pull",
            SafekeeperTimelineOpKind::Exclude => "exclude",
            SafekeeperTimelineOpKind::Delete => "delete",
        };
        out.write_all(kind_str.as_bytes())
            .map(|_| IsNull::No)
            .map_err(Into::into)
    }
}

#[derive(Serialize, Deserialize, Queryable, Selectable, Insertable, Eq, PartialEq, Clone)]
#[diesel(table_name = crate::schema::timeline_imports)]
pub(crate) struct TimelineImportPersistence {
    pub(crate) tenant_id: String,
    pub(crate) timeline_id: String,
    pub(crate) shard_statuses: serde_json::Value,
}
