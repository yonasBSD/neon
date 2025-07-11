//! Code to manage pageservers
//!
//! In the local test environment, the data for each pageserver is stored in
//!
//! ```text
//!   .neon/pageserver_<pageserver_id>
//! ```
//!
use std::collections::HashMap;
use std::io;
use std::io::Write;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, bail};
use camino::Utf8PathBuf;
use pageserver_api::config::{DEFAULT_GRPC_LISTEN_PORT, DEFAULT_HTTP_LISTEN_PORT};
use pageserver_api::models::{self, TenantInfo, TimelineInfo};
use pageserver_api::shard::TenantShardId;
use pageserver_client::mgmt_api;
use postgres_backend::AuthType;
use postgres_connection::{PgConnectionConfig, parse_host_port};
use safekeeper_api::PgMajorVersion;
use utils::auth::{Claims, Scope};
use utils::id::{NodeId, TenantId, TimelineId};
use utils::lsn::Lsn;

use crate::background_process;
use crate::local_env::{LocalEnv, NeonLocalInitPageserverConf, PageServerConf};

/// Directory within .neon which will be used by default for LocalFs remote storage.
pub const PAGESERVER_REMOTE_STORAGE_DIR: &str = "local_fs_remote_storage/pageserver";

//
// Control routines for pageserver.
//
// Used in CLI and tests.
//
#[derive(Debug)]
pub struct PageServerNode {
    pub pg_connection_config: PgConnectionConfig,
    pub conf: PageServerConf,
    pub env: LocalEnv,
    pub http_client: mgmt_api::Client,
}

impl PageServerNode {
    pub fn from_env(env: &LocalEnv, conf: &PageServerConf) -> PageServerNode {
        let (host, port) =
            parse_host_port(&conf.listen_pg_addr).expect("Unable to parse listen_pg_addr");
        let port = port.unwrap_or(5432);

        let endpoint = if env.storage_controller.use_https_pageserver_api {
            format!(
                "https://{}",
                conf.listen_https_addr.as_ref().expect(
                    "listen https address should be specified if use_https_pageserver_api is on"
                )
            )
        } else {
            format!("http://{}", conf.listen_http_addr)
        };

        Self {
            pg_connection_config: PgConnectionConfig::new_host_port(host, port),
            conf: conf.clone(),
            env: env.clone(),
            http_client: mgmt_api::Client::new(
                env.create_http_client(),
                endpoint,
                {
                    match conf.http_auth_type {
                        AuthType::Trust => None,
                        AuthType::NeonJWT => Some(
                            env.generate_auth_token(&Claims::new(None, Scope::PageServerApi))
                                .unwrap(),
                        ),
                    }
                }
                .as_deref(),
            ),
        }
    }

    fn pageserver_make_identity_toml(&self, node_id: NodeId) -> toml_edit::DocumentMut {
        toml_edit::DocumentMut::from_str(&format!("id={node_id}")).unwrap()
    }

    fn pageserver_init_make_toml(
        &self,
        conf: NeonLocalInitPageserverConf,
    ) -> anyhow::Result<toml_edit::DocumentMut> {
        assert_eq!(
            &PageServerConf::from(&conf),
            &self.conf,
            "during neon_local init, we derive the runtime state of ps conf (self.conf) from the --config flag fully"
        );

        // TODO(christian): instead of what we do here, create a pageserver_api::config::ConfigToml (PR #7656)

        // FIXME: the paths should be shell-escaped to handle paths with spaces, quotas etc.
        let pg_distrib_dir_param = format!(
            "pg_distrib_dir='{}'",
            self.env.pg_distrib_dir_raw().display()
        );

        let broker_endpoint_param = format!("broker_endpoint='{}'", self.env.broker.client_url());

        let mut overrides = vec![pg_distrib_dir_param, broker_endpoint_param];

        overrides.push(format!(
            "control_plane_api='{}'",
            self.env.control_plane_api.as_str()
        ));

        // Storage controller uses the same auth as pageserver: if JWT is enabled
        // for us, we will also need it to talk to them.
        if matches!(conf.http_auth_type, AuthType::NeonJWT) {
            let jwt_token = self
                .env
                .generate_auth_token(&Claims::new(None, Scope::GenerationsApi))
                .unwrap();
            overrides.push(format!("control_plane_api_token='{jwt_token}'"));
        }

        if !conf.other.contains_key("remote_storage") {
            overrides.push(format!(
                "remote_storage={{local_path='../{PAGESERVER_REMOTE_STORAGE_DIR}'}}"
            ));
        }

        if [conf.http_auth_type, conf.pg_auth_type, conf.grpc_auth_type]
            .contains(&AuthType::NeonJWT)
        {
            // Keys are generated in the toplevel repo dir, pageservers' workdirs
            // are one level below that, so refer to keys with ../
            overrides.push("auth_validation_public_key_path='../auth_public_key.pem'".to_owned());
        }

        if let Some(ssl_ca_file) = self.env.ssl_ca_cert_path() {
            overrides.push(format!("ssl_ca_file='{}'", ssl_ca_file.to_str().unwrap()));
        }

        // Apply the user-provided overrides
        overrides.push({
            let mut doc =
                toml_edit::ser::to_document(&conf).expect("we deserialized this from toml earlier");
            // `id` is written out to `identity.toml` instead of `pageserver.toml`
            doc.remove("id").expect("it's part of the struct");
            doc.to_string()
        });

        // Turn `overrides` into a toml document.
        // TODO: above code is legacy code, it should be refactored to use toml_edit directly.
        let mut config_toml = toml_edit::DocumentMut::new();
        for fragment_str in overrides {
            let fragment = toml_edit::DocumentMut::from_str(&fragment_str)
                .expect("all fragments in `overrides` are valid toml documents, this function controls that");
            for (key, item) in fragment.iter() {
                config_toml.insert(key, item.clone());
            }
        }
        Ok(config_toml)
    }

    /// Initializes a pageserver node by creating its config with the overrides provided.
    pub fn initialize(&self, conf: NeonLocalInitPageserverConf) -> anyhow::Result<()> {
        self.pageserver_init(conf)
            .with_context(|| format!("Failed to run init for pageserver node {}", self.conf.id))
    }

    pub fn repo_path(&self) -> PathBuf {
        self.env.pageserver_data_dir(self.conf.id)
    }

    /// The pid file is created by the pageserver process, with its pid stored inside.
    /// Other pageservers cannot lock the same file and overwrite it for as long as the current
    /// pageserver runs. (Unless someone removes the file manually; never do that!)
    fn pid_file(&self) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(self.repo_path().join("pageserver.pid"))
            .expect("non-Unicode path")
    }

    pub async fn start(&self, retry_timeout: &Duration) -> anyhow::Result<()> {
        self.start_node(retry_timeout).await
    }

    fn pageserver_init(&self, conf: NeonLocalInitPageserverConf) -> anyhow::Result<()> {
        let datadir = self.repo_path();
        let node_id = self.conf.id;
        println!(
            "Initializing pageserver node {} at '{}' in {:?}",
            node_id,
            self.pg_connection_config.raw_address(),
            datadir
        );
        io::stdout().flush()?;

        // If the config file we got as a CLI argument includes the `availability_zone`
        // config, then use that to populate the `metadata.json` file for the pageserver.
        // In production the deployment orchestrator does this for us.
        let az_id = conf
            .other
            .get("availability_zone")
            .map(|toml| {
                let az_str = toml.to_string();
                // Trim the (") chars from the toml representation
                if az_str.starts_with('"') && az_str.ends_with('"') {
                    az_str[1..az_str.len() - 1].to_string()
                } else {
                    az_str
                }
            })
            .unwrap_or("local".to_string());

        let config = self
            .pageserver_init_make_toml(conf)
            .context("make pageserver toml")?;
        let config_file_path = datadir.join("pageserver.toml");
        let mut config_file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&config_file_path)
            .with_context(|| format!("open pageserver toml for write: {config_file_path:?}"))?;
        config_file
            .write_all(config.to_string().as_bytes())
            .context("write pageserver toml")?;
        drop(config_file);

        let identity_file_path = datadir.join("identity.toml");
        let mut identity_file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(identity_file_path)
            .with_context(|| format!("open identity toml for write: {config_file_path:?}"))?;
        let identity_toml = self.pageserver_make_identity_toml(node_id);
        identity_file
            .write_all(identity_toml.to_string().as_bytes())
            .context("write identity toml")?;
        drop(identity_toml);

        if self.env.generate_local_ssl_certs {
            self.env.generate_ssl_cert(
                datadir.join("server.crt").as_path(),
                datadir.join("server.key").as_path(),
            )?;
        }

        // TODO: invoke a TBD config-check command to validate that pageserver will start with the written config

        // Write metadata file, used by pageserver on startup to register itself with
        // the storage controller
        let metadata_path = datadir.join("metadata.json");

        let http_host = "localhost".to_string();
        let (_, http_port) =
            parse_host_port(&self.conf.listen_http_addr).expect("Unable to parse listen_http_addr");
        let http_port = http_port.unwrap_or(DEFAULT_HTTP_LISTEN_PORT);

        let https_port = match self.conf.listen_https_addr.as_ref() {
            Some(https_addr) => {
                let (_https_host, https_port) =
                    parse_host_port(https_addr).expect("Unable to parse listen_https_addr");
                Some(https_port.unwrap_or(9899))
            }
            None => None,
        };

        let (mut grpc_host, mut grpc_port) = (None, None);
        if let Some(grpc_addr) = &self.conf.listen_grpc_addr {
            let (_, port) = parse_host_port(grpc_addr).expect("Unable to parse listen_grpc_addr");
            grpc_host = Some("localhost".to_string());
            grpc_port = Some(port.unwrap_or(DEFAULT_GRPC_LISTEN_PORT));
        }

        // Intentionally hand-craft JSON: this acts as an implicit format compat test
        // in case the pageserver-side structure is edited, and reflects the real life
        // situation: the metadata is written by some other script.
        std::fs::write(
            metadata_path,
            serde_json::to_vec(&pageserver_api::config::NodeMetadata {
                postgres_host: "localhost".to_string(),
                postgres_port: self.pg_connection_config.port(),
                grpc_host,
                grpc_port,
                http_host,
                http_port,
                https_port,
                other: HashMap::from([(
                    "availability_zone_id".to_string(),
                    serde_json::json!(az_id),
                )]),
            })
            .unwrap(),
        )
        .expect("Failed to write metadata file");

        Ok(())
    }

    async fn start_node(&self, retry_timeout: &Duration) -> anyhow::Result<()> {
        // TODO: using a thread here because start_process() is not async but we need to call check_status()
        let datadir = self.repo_path();
        println!(
            "Starting pageserver node {} at '{}' in {:?}, retrying for {:?}",
            self.conf.id,
            self.pg_connection_config.raw_address(),
            datadir,
            retry_timeout
        );
        io::stdout().flush().context("flush stdout")?;

        let datadir_path_str = datadir.to_str().with_context(|| {
            format!(
                "Cannot start pageserver node {} in path that has no string representation: {:?}",
                self.conf.id, datadir,
            )
        })?;
        let args = vec!["-D", datadir_path_str];

        background_process::start_process(
            "pageserver",
            &datadir,
            &self.env.pageserver_bin(),
            args,
            self.pageserver_env_variables()?,
            background_process::InitialPidFile::Expect(self.pid_file()),
            retry_timeout,
            || async {
                let st = self.check_status().await;
                match st {
                    Ok(()) => Ok(true),
                    Err(mgmt_api::Error::ReceiveBody(_)) => Ok(false),
                    Err(e) => Err(anyhow::anyhow!("Failed to check node status: {e}")),
                }
            },
        )
        .await?;

        Ok(())
    }

    fn pageserver_env_variables(&self) -> anyhow::Result<Vec<(String, String)>> {
        // FIXME: why is this tied to pageserver's auth type? Whether or not the safekeeper
        // needs a token, and how to generate that token, seems independent to whether
        // the pageserver requires a token in incoming requests.
        Ok(if self.conf.http_auth_type != AuthType::Trust {
            // Generate a token to connect from the pageserver to a safekeeper
            let token = self
                .env
                .generate_auth_token(&Claims::new(None, Scope::SafekeeperData))?;
            vec![("NEON_AUTH_TOKEN".to_owned(), token)]
        } else {
            Vec::new()
        })
    }

    ///
    /// Stop the server.
    ///
    /// If 'immediate' is true, we use SIGQUIT, killing the process immediately.
    /// Otherwise we use SIGTERM, triggering a clean shutdown
    ///
    /// If the server is not running, returns success
    ///
    pub fn stop(&self, immediate: bool) -> anyhow::Result<()> {
        background_process::stop_process(immediate, "pageserver", &self.pid_file())
    }

    pub async fn check_status(&self) -> mgmt_api::Result<()> {
        self.http_client.status().await
    }

    pub async fn tenant_list(&self) -> mgmt_api::Result<Vec<TenantInfo>> {
        self.http_client.list_tenants().await
    }
    pub fn parse_config(mut settings: HashMap<&str, &str>) -> anyhow::Result<models::TenantConfig> {
        let result = models::TenantConfig {
            checkpoint_distance: settings
                .remove("checkpoint_distance")
                .map(|x| x.parse::<u64>())
                .transpose()
                .context("Failed to parse 'checkpoint_distance' as an integer")?,
            checkpoint_timeout: settings
                .remove("checkpoint_timeout")
                .map(humantime::parse_duration)
                .transpose()
                .context("Failed to parse 'checkpoint_timeout' as duration")?,
            compaction_target_size: settings
                .remove("compaction_target_size")
                .map(|x| x.parse::<u64>())
                .transpose()
                .context("Failed to parse 'compaction_target_size' as an integer")?,
            compaction_period: settings
                .remove("compaction_period")
                .map(humantime::parse_duration)
                .transpose()
                .context("Failed to parse 'compaction_period' as duration")?,
            compaction_threshold: settings
                .remove("compaction_threshold")
                .map(|x| x.parse::<usize>())
                .transpose()
                .context("Failed to parse 'compaction_threshold' as an integer")?,
            compaction_upper_limit: settings
                .remove("compaction_upper_limit")
                .map(|x| x.parse::<usize>())
                .transpose()
                .context("Failed to parse 'compaction_upper_limit' as an integer")?,
            compaction_algorithm: settings
                .remove("compaction_algorithm")
                .map(serde_json::from_str)
                .transpose()
                .context("Failed to parse 'compaction_algorithm' json")?,
            compaction_shard_ancestor: settings
                .remove("compaction_shard_ancestor")
                .map(|x| x.parse::<bool>())
                .transpose()
                .context("Failed to parse 'compaction_shard_ancestor' as a bool")?,
            compaction_l0_first: settings
                .remove("compaction_l0_first")
                .map(|x| x.parse::<bool>())
                .transpose()
                .context("Failed to parse 'compaction_l0_first' as a bool")?,
            compaction_l0_semaphore: settings
                .remove("compaction_l0_semaphore")
                .map(|x| x.parse::<bool>())
                .transpose()
                .context("Failed to parse 'compaction_l0_semaphore' as a bool")?,
            l0_flush_delay_threshold: settings
                .remove("l0_flush_delay_threshold")
                .map(|x| x.parse::<usize>())
                .transpose()
                .context("Failed to parse 'l0_flush_delay_threshold' as an integer")?,
            l0_flush_stall_threshold: settings
                .remove("l0_flush_stall_threshold")
                .map(|x| x.parse::<usize>())
                .transpose()
                .context("Failed to parse 'l0_flush_stall_threshold' as an integer")?,
            gc_horizon: settings
                .remove("gc_horizon")
                .map(|x| x.parse::<u64>())
                .transpose()
                .context("Failed to parse 'gc_horizon' as an integer")?,
            gc_period: settings.remove("gc_period")
                .map(humantime::parse_duration)
                .transpose()
                .context("Failed to parse 'gc_period' as duration")?,
            image_creation_threshold: settings
                .remove("image_creation_threshold")
                .map(|x| x.parse::<usize>())
                .transpose()
                .context("Failed to parse 'image_creation_threshold' as non zero integer")?,
            // HADRON
            image_layer_force_creation_period: settings
                .remove("image_layer_force_creation_period")
                .map(humantime::parse_duration)
                .transpose()
                .context("Failed to parse 'image_layer_force_creation_period' as duration")?,
            image_layer_creation_check_threshold: settings
                .remove("image_layer_creation_check_threshold")
                .map(|x| x.parse::<u8>())
                .transpose()
                .context("Failed to parse 'image_creation_check_threshold' as integer")?,
            image_creation_preempt_threshold: settings
                .remove("image_creation_preempt_threshold")
                .map(|x| x.parse::<usize>())
                .transpose()
                .context("Failed to parse 'image_creation_preempt_threshold' as integer")?,
            pitr_interval: settings.remove("pitr_interval")
                .map(humantime::parse_duration)
                .transpose()
                .context("Failed to parse 'pitr_interval' as duration")?,
            walreceiver_connect_timeout: settings
                .remove("walreceiver_connect_timeout")
                .map(humantime::parse_duration)
                .transpose()
                .context("Failed to parse 'walreceiver_connect_timeout' as duration")?,
            lagging_wal_timeout: settings
                .remove("lagging_wal_timeout")
                .map(humantime::parse_duration)
                .transpose()
                .context("Failed to parse 'lagging_wal_timeout' as duration")?,
            max_lsn_wal_lag: settings
                .remove("max_lsn_wal_lag")
                .map(|x| x.parse::<NonZeroU64>())
                .transpose()
                .context("Failed to parse 'max_lsn_wal_lag' as non zero integer")?,
            eviction_policy: settings
                .remove("eviction_policy")
                .map(serde_json::from_str)
                .transpose()
                .context("Failed to parse 'eviction_policy' json")?,
            min_resident_size_override: settings
                .remove("min_resident_size_override")
                .map(|x| x.parse::<u64>())
                .transpose()
                .context("Failed to parse 'min_resident_size_override' as integer")?,
            evictions_low_residence_duration_metric_threshold: settings
                .remove("evictions_low_residence_duration_metric_threshold")
                .map(humantime::parse_duration)
                .transpose()
                .context("Failed to parse 'evictions_low_residence_duration_metric_threshold' as duration")?,
            heatmap_period: settings
                .remove("heatmap_period")
                .map(humantime::parse_duration)
                .transpose()
                .context("Failed to parse 'heatmap_period' as duration")?,
            lazy_slru_download: settings
                .remove("lazy_slru_download")
                .map(|x| x.parse::<bool>())
                .transpose()
                .context("Failed to parse 'lazy_slru_download' as bool")?,
            timeline_get_throttle: settings
                .remove("timeline_get_throttle")
                .map(serde_json::from_str)
                .transpose()
                .context("parse `timeline_get_throttle` from json")?,
            lsn_lease_length: settings.remove("lsn_lease_length")
                .map(humantime::parse_duration)
                .transpose()
                .context("Failed to parse 'lsn_lease_length' as duration")?,
            lsn_lease_length_for_ts: settings
                .remove("lsn_lease_length_for_ts")
                .map(humantime::parse_duration)
                .transpose()
                .context("Failed to parse 'lsn_lease_length_for_ts' as duration")?,
            timeline_offloading: settings
                .remove("timeline_offloading")
                .map(|x| x.parse::<bool>())
                .transpose()
                .context("Failed to parse 'timeline_offloading' as bool")?,
            rel_size_v2_enabled: settings
                .remove("rel_size_v2_enabled")
                .map(|x| x.parse::<bool>())
                .transpose()
                .context("Failed to parse 'rel_size_v2_enabled' as bool")?,
            gc_compaction_enabled: settings
                .remove("gc_compaction_enabled")
                .map(|x| x.parse::<bool>())
                .transpose()
                .context("Failed to parse 'gc_compaction_enabled' as bool")?,
            gc_compaction_verification: settings
                .remove("gc_compaction_verification")
                .map(|x| x.parse::<bool>())
                .transpose()
                .context("Failed to parse 'gc_compaction_verification' as bool")?,
            gc_compaction_initial_threshold_kb: settings
                .remove("gc_compaction_initial_threshold_kb")
                .map(|x| x.parse::<u64>())
                .transpose()
                .context("Failed to parse 'gc_compaction_initial_threshold_kb' as integer")?,
            gc_compaction_ratio_percent: settings
                .remove("gc_compaction_ratio_percent")
                .map(|x| x.parse::<u64>())
                .transpose()
                .context("Failed to parse 'gc_compaction_ratio_percent' as integer")?,
            sampling_ratio: settings
                .remove("sampling_ratio")
                .map(serde_json::from_str)
                .transpose()
                .context("Falied to parse 'sampling_ratio'")?,
            relsize_snapshot_cache_capacity: settings
                .remove("relsize snapshot cache capacity")
                .map(|x| x.parse::<usize>())
                .transpose()
                .context("Falied to parse 'relsize_snapshot_cache_capacity' as integer")?,
            basebackup_cache_enabled: settings
                .remove("basebackup_cache_enabled")
                .map(|x| x.parse::<bool>())
                .transpose()
                .context("Failed to parse 'basebackup_cache_enabled' as bool")?,
        };
        if !settings.is_empty() {
            bail!("Unrecognized tenant settings: {settings:?}")
        } else {
            Ok(result)
        }
    }

    pub async fn tenant_config(
        &self,
        tenant_id: TenantId,
        settings: HashMap<&str, &str>,
    ) -> anyhow::Result<()> {
        let config = Self::parse_config(settings)?;
        self.http_client
            .set_tenant_config(&models::TenantConfigRequest { tenant_id, config })
            .await?;

        Ok(())
    }

    pub async fn timeline_list(
        &self,
        tenant_shard_id: &TenantShardId,
    ) -> anyhow::Result<Vec<TimelineInfo>> {
        Ok(self.http_client.list_timelines(*tenant_shard_id).await?)
    }

    /// Import a basebackup prepared using either:
    /// a) `pg_basebackup -F tar`, or
    /// b) The `fullbackup` pageserver endpoint
    ///
    /// # Arguments
    /// * `tenant_id` - tenant to import into. Created if not exists
    /// * `timeline_id` - id to assign to imported timeline
    /// * `base` - (start lsn of basebackup, path to `base.tar` file)
    /// * `pg_wal` - if there's any wal to import: (end lsn, path to `pg_wal.tar`)
    pub async fn timeline_import(
        &self,
        tenant_id: TenantId,
        timeline_id: TimelineId,
        base: (Lsn, PathBuf),
        pg_wal: Option<(Lsn, PathBuf)>,
        pg_version: PgMajorVersion,
    ) -> anyhow::Result<()> {
        // Init base reader
        let (start_lsn, base_tarfile_path) = base;
        let base_tarfile = tokio::fs::File::open(base_tarfile_path).await?;
        let base_tarfile =
            mgmt_api::ReqwestBody::wrap_stream(tokio_util::io::ReaderStream::new(base_tarfile));

        // Init wal reader if necessary
        let (end_lsn, wal_reader) = if let Some((end_lsn, wal_tarfile_path)) = pg_wal {
            let wal_tarfile = tokio::fs::File::open(wal_tarfile_path).await?;
            let wal_reader =
                mgmt_api::ReqwestBody::wrap_stream(tokio_util::io::ReaderStream::new(wal_tarfile));
            (end_lsn, Some(wal_reader))
        } else {
            (start_lsn, None)
        };

        // Import base
        self.http_client
            .import_basebackup(
                tenant_id,
                timeline_id,
                start_lsn,
                end_lsn,
                pg_version,
                base_tarfile,
            )
            .await?;

        // Import wal if necessary
        if let Some(wal_reader) = wal_reader {
            self.http_client
                .import_wal(tenant_id, timeline_id, start_lsn, end_lsn, wal_reader)
                .await?;
        }

        Ok(())
    }
    pub async fn timeline_info(
        &self,
        tenant_shard_id: TenantShardId,
        timeline_id: TimelineId,
        force_await_logical_size: mgmt_api::ForceAwaitLogicalSize,
    ) -> anyhow::Result<TimelineInfo> {
        let timeline_info = self
            .http_client
            .timeline_info(tenant_shard_id, timeline_id, force_await_logical_size)
            .await?;
        Ok(timeline_info)
    }
}
