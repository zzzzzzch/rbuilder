//! Config should always be deserializable, default values should be used
//!
use crate::{
    beacon_api_client::Client,
    flashbots::BlocksProcessorClient,
    live_builder::{
        bidding::DummyBiddingService,
        building::{relay_submit::RelaySubmitSinkFactory, SubmissionConfig},
        order_input::OrderInputConfig,
        LiveBuilder,
    },
    mev_boost::BLSBlockSigner,
    primitives::mev_boost::MevBoostRelay,
    telemetry::{setup_reloadable_tracing_subscriber, LoggerConfig},
    utils::{http_provider, BoxedProvider, ProviderFactoryReopener, Signer},
    validation_api_client::ValidationAPIClient,
};
use ahash::HashSet;
use alloy_chains::ChainKind;
use alloy_primitives::{utils::parse_ether, Address, FixedBytes, B256};
use ethereum_consensus::{
    builder::compute_builder_domain, crypto::SecretKey, primitives::Version,
    state_transition::Context as ContextEth,
};
use eyre::{eyre, Context};
use jsonrpsee::RpcModule;
use lazy_static::lazy_static;
use reth::tasks::pool::BlockingTaskPool;
use reth_chainspec::{Chain, ChainSpec, NamedChain};
use reth_db::DatabaseEnv;
use reth_node_core::args::utils::chain_value_parser;
use reth_primitives::{format_ether, StaticFileSegment};
use reth_provider::StaticFileProviderFactory;
use serde::{Deserialize, Deserializer};
use serde_with::{serde_as, DeserializeAs, OneOrMany};
use sqlx::PgPool;
use std::{
    env::var,
    fs::read_to_string,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tracing::{info, warn};
use url::Url;

/// From experience (Vitaly's) all generated blocks before slot_time-8sec end loosing (due to last moment orders?)
const DEFAULT_SLOT_DELTA_TO_START_SUBMITS: time::Duration = time::Duration::milliseconds(-8000);

/// Prefix for env variables in config
const ENV_PREFIX: &str = "env:";

/// Base config to be used by all builders.
/// It allows us to create a base LiveBuilder with no algorithms or custom bidding.
/// The final configuration should usually include one of this and use it to create the base LiveBuilder to then upgrade it as needed.
#[serde_as]
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct BaseConfig {
    pub telemetry_port: u16,
    pub telemetry_ip: Option<String>,
    pub log_json: bool,
    log_level: EnvOrValue<String>,
    pub log_color: bool,

    pub error_storage_path: PathBuf,

    coinbase_secret_key: EnvOrValue<String>,

    pub flashbots_db: Option<EnvOrValue<String>>,

    pub el_node_ipc_path: PathBuf,
    ///Name kept singular for backwards compatibility
    #[serde_as(deserialize_as = "OneOrMany<EnvOrValue<String>>")]
    pub cl_node_url: Vec<EnvOrValue<String>>,
    pub jsonrpc_server_port: u16,
    pub jsonrpc_server_ip: Option<String>,

    pub ignore_cancellable_orders: bool,
    pub ignore_blobs: bool,

    pub chain: String,
    pub reth_datadir: Option<PathBuf>,
    pub reth_db_path: Option<PathBuf>,
    pub reth_static_files_path: Option<PathBuf>,

    pub blocklist_file_path: Option<PathBuf>,
    pub extra_data: String,

    // Relay Submission configuration
    pub relays: Vec<RelayConfig>,
    pub dry_run: bool,
    #[serde_as(deserialize_as = "OneOrMany<_>")]
    pub dry_run_validation_url: Vec<String>,
    /// Secret key that will be used to sign normal submissions to the relay.
    relay_secret_key: EnvOrValue<String>,
    /// Secret key that will be used to sign optimistic submissions to the relay.
    optimistic_relay_secret_key: EnvOrValue<String>,
    /// When enabled builer will make optimistic submissions to optimistic relays
    /// influenced by `optimistic_max_bid_value_eth` and `optimistic_prevalidate_optimistic_blocks`
    pub optimistic_enabled: bool,
    /// Bids above this value will always be submitted in non-optimistic mode.
    pub optimistic_max_bid_value_eth: String,
    /// If true all optimistic submissions will be validated on nodes specified in `dry_run_validation_url`
    pub optimistic_prevalidate_optimistic_blocks: bool,
    pub blocks_processor_url: Option<String>,

    /// mev-share bundles coming from this address are treated in a special way(see [`ShareBundleMerger`])
    pub sbundle_mergeabe_signers: Option<Vec<Address>>,

    /// Number of threads used for incoming order simulation
    pub simulation_threads: usize,

    pub root_hash_task_pool_threads: usize,

    pub watchdog_timeout_sec: u64,

    /// List of `builders` to be used for live building
    pub live_builders: Vec<String>,

    // backtest config
    backtest_fetch_mempool_data_dir: EnvOrValue<String>,
    pub backtest_fetch_eth_rpc_url: String,
    pub backtest_fetch_eth_rpc_parallel: usize,
    pub backtest_fetch_output_file: PathBuf,
    /// List of `builders` to be used in backtest run
    pub backtest_builders: Vec<String>,
    pub backtest_results_store_path: PathBuf,

    // See [`SubmissionConfig`]
    slot_delta_to_start_submits_ms: Option<i64>,
}

lazy_static! {
    pub static ref DEFAULT_IP: Ipv4Addr = Ipv4Addr::new(0, 0, 0, 0);
}

fn parse_ip(ip: &Option<String>) -> Ipv4Addr {
    ip.as_ref().map_or(*DEFAULT_IP, |s| {
        s.parse::<Ipv4Addr>().unwrap_or(*DEFAULT_IP)
    })
}

/// Loads config from toml file, some values can be loaded from env variables with the following syntax
/// e.g. flashbots_db = "env:FLASHBOTS_DB"
///
/// variables that can be configured with env values:
/// - log_level
/// - coinbase_secret_key
/// - flashbots_db
/// - relay_secret_key
/// - optimistic_relay_secret_key
/// - backtest_fetch_mempool_data_dir
pub fn load_config_toml_and_env<T: serde::de::DeserializeOwned>(
    path: impl AsRef<Path>,
) -> eyre::Result<T> {
    let data = read_to_string(path.as_ref()).with_context(|| {
        eyre!(
            "Config file read error: {:?}",
            path.as_ref().to_string_lossy()
        )
    })?;

    let config: T = toml::from_str(&data).context("Config file parsing")?;
    Ok(config)
}

impl BaseConfig {
    pub fn setup_tracing_subsriber(&self) -> eyre::Result<()> {
        let log_level = self.log_level.value()?;
        let config = LoggerConfig {
            env_filter: log_level,
            file: None,
            log_json: self.log_json,
            log_color: self.log_color,
        };
        setup_reloadable_tracing_subscriber(config)?;
        Ok(())
    }

    pub fn telemetry_address(&self) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(self.telemetry_ip(), self.telemetry_port))
    }

    /// WARN: opens reth db
    pub async fn create_builder(
        &self,
        cancellation_token: tokio_util::sync::CancellationToken,
    ) -> eyre::Result<
        super::LiveBuilder<Arc<DatabaseEnv>, super::building::relay_submit::RelaySubmitSinkFactory>,
    > {
        let submission_config = self.submission_config()?;
        info!(
            "Builder mev boost normal relay pubkey: {:?}",
            submission_config.signer.pub_key()
        );
        info!(
            "Builder mev boost optimistic relay pubkey: {:?}",
            submission_config.optimistic_signer.pub_key()
        );
        info!(
            "Optimistic mode, enabled: {}, prevalidate: {}, max_value: {}",
            submission_config.optimistic_enabled,
            submission_config.optimistic_prevalidate_optimistic_blocks,
            format_ether(submission_config.optimistic_max_bid_value),
        );

        let provider_factory = self.provider_factory()?;

        let relays = self.relays()?;
        let sink_factory = RelaySubmitSinkFactory::new(self.submission_config()?, relays.clone());

        Ok(LiveBuilder::<Arc<DatabaseEnv>, RelaySubmitSinkFactory> {
            cls: self.beacon_clients()?,
            relays,
            watchdog_timeout: self.watchdog_timeout(),
            error_storage_path: self.error_storage_path.clone(),
            simulation_threads: self.simulation_threads,
            order_input_config: OrderInputConfig::from_config(self),

            chain_chain_spec: self.chain_spec()?,
            provider_factory,

            coinbase_signer: self.coinbase_signer()?,
            extra_data: self.extra_data()?,
            blocklist: self.blocklist()?,

            global_cancellation: cancellation_token,

            bidding_service: Box::new(DummyBiddingService {}),
            extra_rpc: RpcModule::new(()),
            sink_factory,
            builders: Vec::new(),
        })
    }

    pub fn jsonrpc_server_ip(&self) -> Ipv4Addr {
        parse_ip(&self.jsonrpc_server_ip)
    }

    pub fn telemetry_ip(&self) -> Ipv4Addr {
        parse_ip(&self.telemetry_ip)
    }

    pub fn chain_spec(&self) -> eyre::Result<Arc<ChainSpec>> {
        chain_value_parser(&self.chain)
    }

    pub fn beacon_clients(&self) -> eyre::Result<Vec<Client>> {
        self.cl_node_url
            .iter()
            .map(|url| {
                let url = Url::parse(&url.value()?)?;
                Ok(Client::new(url))
            })
            .collect()
    }

    pub fn sbundle_mergeabe_signers(&self) -> Vec<Address> {
        if self.sbundle_mergeabe_signers.is_none() {
            warn!("Defaulting sbundle_mergeabe_signers to empty. We may not comply with order flow rules.");
        }

        self.sbundle_mergeabe_signers.clone().unwrap_or_default()
    }

    pub fn relays(&self) -> eyre::Result<Vec<MevBoostRelay>> {
        let mut results = Vec::new();
        for relay in &self.relays {
            let authorization_header =
                if let Some(authorization_header) = &relay.authorization_header {
                    Some(authorization_header.value()?)
                } else {
                    None
                };

            let builder_id_header = if let Some(builder_id_header) = &relay.builder_id_header {
                Some(builder_id_header.value()?)
            } else {
                None
            };

            let api_token_header = if let Some(api_token_header) = &relay.api_token_header {
                Some(api_token_header.value()?)
            } else {
                None
            };

            let interval_between_submissions = relay
                .interval_between_submissions_ms
                .map(Duration::from_millis);

            results.push(MevBoostRelay::try_from_name_or_url(
                &relay.name,
                &relay.url,
                relay.priority,
                relay.use_ssz_for_submit,
                relay.use_gzip_for_submit,
                relay.optimistic,
                authorization_header,
                builder_id_header,
                api_token_header,
                interval_between_submissions,
            )?);
        }
        Ok(results)
    }

    /// Open reth db and DB should be opened once per process but it can be cloned and moved to different threads.
    pub fn provider_factory(&self) -> eyre::Result<ProviderFactoryReopener<Arc<DatabaseEnv>>> {
        create_provider_factory(
            self.reth_datadir.as_deref(),
            self.reth_db_path.as_deref(),
            self.reth_static_files_path.as_deref(),
            self.chain_spec()?,
        )
    }

    /// Creates threadpool for root hash calculation, should be created once per process.
    pub fn root_hash_task_pool(&self) -> eyre::Result<BlockingTaskPool> {
        Ok(BlockingTaskPool::new(
            BlockingTaskPool::builder()
                .num_threads(self.root_hash_task_pool_threads)
                .build()?,
        ))
    }

    pub fn bls_signer(&self) -> eyre::Result<BLSBlockSigner> {
        let chain_spec = self.chain_spec()?;
        let signing_domain = get_signing_domain(chain_spec.chain, self.beacon_clients()?)?;
        let secret_key = self.relay_secret_key.value()?;
        let secret_key = SecretKey::try_from(secret_key)
            .map_err(|e| eyre::eyre!("Failed to parse relay key: {:?}", e.to_string()))?;

        BLSBlockSigner::new(secret_key, signing_domain)
    }

    pub fn bls_optimistic_signer(&self) -> eyre::Result<BLSBlockSigner> {
        let chain_spec = self.chain_spec()?;
        let signing_domain = get_signing_domain(chain_spec.chain, self.beacon_clients()?)?;
        let secret_key = self.optimistic_relay_secret_key.value()?;
        let secret_key = SecretKey::try_from(secret_key).map_err(|e| {
            eyre::eyre!("Failed to parse optimistic relay key: {:?}", e.to_string())
        })?;

        BLSBlockSigner::new(secret_key, signing_domain)
    }

    pub fn coinbase_signer(&self) -> eyre::Result<Signer> {
        coinbase_signer_from_secret_key(&self.coinbase_secret_key.value()?)
    }

    pub fn extra_data(&self) -> eyre::Result<Vec<u8>> {
        let extra_data = self.extra_data.clone().into_bytes();
        if extra_data.len() > 32 {
            return Err(eyre::eyre!("Extra data is too long"));
        }
        Ok(extra_data)
    }

    pub fn blocklist(&self) -> eyre::Result<HashSet<Address>> {
        if let Some(path) = &self.blocklist_file_path {
            let blocklist_file = read_to_string(path).context("blocklist file")?;
            let blocklist: Vec<Address> =
                serde_json::from_str(&blocklist_file).context("blocklist file")?;
            return Ok(blocklist.into_iter().collect());
        }
        Ok(HashSet::default())
    }

    pub async fn flashbots_db(&self) -> eyre::Result<Option<PgPool>> {
        if let Some(url) = &self.flashbots_db {
            let url = url.value()?;
            let pool = PgPool::connect(&url).await?;
            Ok(Some(pool))
        } else {
            Ok(None)
        }
    }

    pub fn eth_rpc_provider(&self) -> eyre::Result<BoxedProvider> {
        Ok(http_provider(self.backtest_fetch_eth_rpc_url.parse()?))
    }

    pub fn watchdog_timeout(&self) -> Duration {
        Duration::from_secs(self.watchdog_timeout_sec)
    }

    pub fn submission_config(&self) -> eyre::Result<SubmissionConfig> {
        if (self.dry_run || self.optimistic_prevalidate_optimistic_blocks)
            && self.dry_run_validation_url.is_empty()
        {
            eyre::bail!(
                "Dry run or optimistic prevalidation enabled but no validation urls provided"
            );
        }
        let validation_api = {
            let urls = self
                .dry_run_validation_url
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>();

            ValidationAPIClient::new(urls.as_slice())?
        };

        let optimistic_signer = match self.bls_optimistic_signer() {
            Ok(signer) => signer,
            Err(err) => {
                if self.optimistic_enabled {
                    eyre::bail!(
                        "Optimistic mode enabled but no valid optimistic signer: {}",
                        err
                    );
                } else {
                    // we don't care about the actual value
                    self.bls_signer()?
                }
            }
        };

        Ok(SubmissionConfig {
            chain_spec: self.chain_spec()?,
            signer: self.bls_signer()?,
            dry_run: self.dry_run,
            validation_api,
            optimistic_enabled: self.optimistic_enabled,
            optimistic_signer,
            optimistic_max_bid_value: parse_ether(&self.optimistic_max_bid_value_eth)?,
            optimistic_prevalidate_optimistic_blocks: self.optimistic_prevalidate_optimistic_blocks,
            blocks_processor: if let Some(url) = &self.blocks_processor_url {
                let client = BlocksProcessorClient::try_from(url)?;
                Some(client)
            } else {
                None
            },
            slot_delta_to_start_submits: self.slot_delta_to_start_submits(),
        })
    }

    pub fn backtest_fetch_mempool_data_dir(&self) -> eyre::Result<PathBuf> {
        let path = self.backtest_fetch_mempool_data_dir.value()?;
        let path_expanded = shellexpand::tilde(&path).to_string();

        Ok(path_expanded.parse()?)
    }

    pub fn slot_delta_to_start_submits(&self) -> time::Duration {
        self.slot_delta_to_start_submits_ms
            .map(time::Duration::milliseconds)
            .unwrap_or(DEFAULT_SLOT_DELTA_TO_START_SUBMITS)
    }

    pub fn resolve_cl_node_urls(&self) -> eyre::Result<Vec<String>> {
        resolve_env_or_values::<String>(&self.cl_node_url)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvOrValue<T>(String, std::marker::PhantomData<T>);

impl<T: FromStr> EnvOrValue<T> {
    pub fn value(&self) -> eyre::Result<String> {
        let value = &self.0;
        if value.starts_with(ENV_PREFIX) {
            let var_name = value.trim_start_matches(ENV_PREFIX);
            var(var_name).map_err(|_| eyre::eyre!("Env variable: {} not set", var_name))
        } else {
            Ok(value.to_string())
        }
    }
}

impl<T> From<&str> for EnvOrValue<T> {
    fn from(s: &str) -> Self {
        Self(s.to_string(), std::marker::PhantomData)
    }
}

impl<'de, T: FromStr> Deserialize<'de> for EnvOrValue<T> {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(Self(s, std::marker::PhantomData))
    }
}

// Helper function to resolve Vec<EnvOrValue<T>> to Vec<T>
pub fn resolve_env_or_values<T: FromStr>(values: &[EnvOrValue<T>]) -> eyre::Result<Vec<T>> {
    values
        .iter()
        .try_fold(Vec::new(), |mut acc, v| -> eyre::Result<Vec<T>> {
            let value = v.value()?;
            if v.0.starts_with(ENV_PREFIX) {
                // If it's an environment variable, split by comma
                let parsed: eyre::Result<Vec<T>> = value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        T::from_str(s).map_err(|_| eyre::eyre!("Failed to parse value: {}", s))
                    })
                    .collect();
                acc.extend(parsed?);
            } else {
                // If it's not an environment variable, just return the single value
                acc.push(
                    T::from_str(&value)
                        .map_err(|_| eyre::eyre!("Failed to parse value: {}", value))?,
                );
            }
            Ok(acc)
        })
}

impl<'de, T> DeserializeAs<'de, EnvOrValue<T>> for EnvOrValue<T>
where
    T: FromStr,
    String: Deserialize<'de>,
{
    fn deserialize_as<D>(deserializer: D) -> Result<EnvOrValue<T>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(EnvOrValue(s, std::marker::PhantomData))
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RelayConfig {
    pub name: String,
    pub url: String,
    pub priority: usize,
    // true->ssz false->json
    #[serde(default)]
    pub use_ssz_for_submit: bool,
    #[serde(default)]
    pub use_gzip_for_submit: bool,
    #[serde(default)]
    pub optimistic: bool,
    #[serde(default)]
    pub authorization_header: Option<EnvOrValue<String>>,
    #[serde(default)]
    pub builder_id_header: Option<EnvOrValue<String>>,
    #[serde(default)]
    pub api_token_header: Option<EnvOrValue<String>>,
    #[serde(default)]
    pub interval_between_submissions_ms: Option<u64>,
}

pub const DEFAULT_ERROR_STORAGE_PATH: &str = "/tmp/rbuilder-error.sqlite";
pub const DEFAULT_CL_NODE_URL: &str = "http://127.0.0.1:3500";
pub const DEFAULT_EL_NODE_IPC_PATH: &str = "/tmp/reth.ipc";
pub const DEFAULT_INCOMING_BUNDLES_PORT: u16 = 8645;
pub const DEFAULT_RETH_DB_PATH: &str = "/mnt/data/reth";

impl Default for BaseConfig {
    fn default() -> Self {
        Self {
            telemetry_port: 6069,
            telemetry_ip: None,
            log_json: false,
            log_level: "info".into(),
            log_color: false,
            error_storage_path: DEFAULT_ERROR_STORAGE_PATH.parse().unwrap(),
            coinbase_secret_key: "".into(),
            relay_secret_key: "".into(),
            optimistic_relay_secret_key: "".into(),
            optimistic_enabled: false,
            optimistic_max_bid_value_eth: "0.0".to_string(),
            flashbots_db: None,
            blocks_processor_url: None,
            el_node_ipc_path: "/tmp/reth.ipc".parse().unwrap(),
            cl_node_url: vec![EnvOrValue::from("http://127.0.0.1:3500")],
            jsonrpc_server_port: DEFAULT_INCOMING_BUNDLES_PORT,
            jsonrpc_server_ip: None,
            ignore_cancellable_orders: true,
            ignore_blobs: false,
            chain: "mainnet".to_string(),
            reth_datadir: Some(DEFAULT_RETH_DB_PATH.parse().unwrap()),
            reth_db_path: None,
            reth_static_files_path: None,
            blocklist_file_path: None,
            extra_data: "extra_data_change_me".to_string(),
            relays: vec![],
            dry_run: false,
            dry_run_validation_url: vec![],
            root_hash_task_pool_threads: 1,
            watchdog_timeout_sec: 60 * 3,
            backtest_fetch_mempool_data_dir: "/mnt/data/mempool".into(),
            backtest_fetch_eth_rpc_url: "http://127.0.0.1:8545".to_string(),
            backtest_fetch_eth_rpc_parallel: 1,
            backtest_fetch_output_file: "/tmp/rbuilder-backtest.sqlite".parse().unwrap(),
            backtest_results_store_path: "/tmp/rbuilder-backtest-results.sqlite".parse().unwrap(),
            backtest_builders: Vec::new(),
            live_builders: vec!["mgp-ordering".to_string(), "mp-ordering".to_string()],
            optimistic_prevalidate_optimistic_blocks: false,
            simulation_threads: 1,
            sbundle_mergeabe_signers: None,
            slot_delta_to_start_submits_ms: None,
        }
    }
}

/// Open reth db and DB should be opened once per process but it can be cloned and moved to different threads.
pub fn create_provider_factory(
    reth_datadir: Option<&Path>,
    reth_db_path: Option<&Path>,
    reth_static_files_path: Option<&Path>,
    chain_spec: Arc<ChainSpec>,
) -> eyre::Result<ProviderFactoryReopener<Arc<DatabaseEnv>>> {
    let reth_db_path = match (reth_db_path, reth_datadir) {
        (Some(reth_db_path), _) => PathBuf::from(reth_db_path),
        (None, Some(reth_datadir)) => reth_datadir.join("db"),
        (None, None) => eyre::bail!("Either reth_db_path or reth_datadir must be provided"),
    };

    let db = open_reth_db(&reth_db_path)?;

    let reth_static_files_path = match (reth_static_files_path, reth_datadir) {
        (Some(reth_static_files_path), _) => PathBuf::from(reth_static_files_path),
        (None, Some(reth_datadir)) => reth_datadir.join("static_files"),
        (None, None) => {
            eyre::bail!("Either reth_static_files_path or reth_datadir must be provided")
        }
    };

    let provider_factory_reopener =
        ProviderFactoryReopener::new(db, chain_spec, reth_static_files_path)?;

    if provider_factory_reopener
        .provider_factory_unchecked()
        .static_file_provider()
        .get_highest_static_file_block(StaticFileSegment::Headers)
        .is_none()
    {
        eyre::bail!("No headers in static files. Check your static files path configuration.");
    }

    Ok(provider_factory_reopener)
}

/// Open reth db in read/write mode
pub fn create_provider_factory_rw(
    reth_datadir: Option<&Path>,
    reth_db_path: Option<&Path>,
    reth_static_files_path: Option<&Path>,
    chain_spec: Arc<ChainSpec>,
) -> eyre::Result<ProviderFactoryReopener<Arc<DatabaseEnv>>> {
    let reth_db_path = match (reth_db_path, reth_datadir) {
        (Some(reth_db_path), _) => PathBuf::from(reth_db_path),
        (None, Some(reth_datadir)) => reth_datadir.join("db"),
        (None, None) => eyre::bail!("Either reth_db_path or reth_datadir must be provided"),
    };

    let db = open_reth_db_rw(&reth_db_path)?;

    let reth_static_files_path = match (reth_static_files_path, reth_datadir) {
        (Some(reth_static_files_path), _) => PathBuf::from(reth_static_files_path),
        (None, Some(reth_datadir)) => reth_datadir.join("static_files"),
        (None, None) => {
            eyre::bail!("Either reth_static_files_path or reth_datadir must be provided")
        }
    };

    let provider_factory_reopener =
        ProviderFactoryReopener::new(db, chain_spec, reth_static_files_path)?;

    if provider_factory_reopener
        .provider_factory_unchecked()
        .static_file_provider()
        .get_highest_static_file_block(StaticFileSegment::Headers)
        .is_none()
    {
        eyre::bail!("No headers in static files. Check your static files path configuration.");
    }

    Ok(provider_factory_reopener)
}

fn open_reth_db(reth_db_path: &Path) -> eyre::Result<Arc<DatabaseEnv>> {
    Ok(Arc::new(
        reth_db::open_db_read_only(reth_db_path, Default::default()).context("DB open error")?,
    ))
}

fn open_reth_db_rw(reth_db_path: &Path) -> eyre::Result<Arc<DatabaseEnv>> {
    Ok(Arc::new(
        reth_db::open_db(reth_db_path, Default::default()).context("DB open error")?,
    ))
}

pub fn coinbase_signer_from_secret_key(secret_key: &str) -> eyre::Result<Signer> {
    let secret_key = B256::from_str(secret_key)?;
    Ok(Signer::try_from_secret(secret_key)?)
}

fn get_signing_domain(chain: Chain, beacon_clients: Vec<Client>) -> eyre::Result<B256> {
    let cl_context = match chain.kind() {
        ChainKind::Named(NamedChain::Mainnet) => ContextEth::for_mainnet(),
        ChainKind::Named(NamedChain::Sepolia) => ContextEth::for_sepolia(),
        ChainKind::Named(NamedChain::Goerli) => ContextEth::for_goerli(),
        ChainKind::Named(NamedChain::Holesky) => ContextEth::for_holesky(),
        _ => {
            let client = beacon_clients
                .first()
                .ok_or_else(|| eyre::eyre!("No beacon clients provided"))?;

            let spec = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(client.get_spec())
            })?;

            let genesis_fork_version = spec
                .get("GENESIS_FORK_VERSION")
                .ok_or_else(|| eyre::eyre!("GENESIS_FORK_VERSION not found in spec"))?;

            let version: FixedBytes<4> = FixedBytes::from_str(genesis_fork_version)
                .map_err(|e| eyre::eyre!("Failed to parse genesis fork version: {:?}", e))?;

            let version = Version::from(version);

            // use the mainnet one and update the genesis fork version since it is the
            // only thing required by 'compute_builder_domain'. We do this because
            // there is no default in Context.
            let mut network = ContextEth::for_mainnet();
            network.genesis_fork_version = version;

            network
        }
    };

    Ok(B256::from(&compute_builder_domain(&cl_context)?))
}

#[cfg(test)]
mod test {
    use super::*;
    use alloy_primitives::fixed_bytes;
    use reth::args::DatadirArgs;
    use reth_chainspec::{Chain, SEPOLIA};
    use reth_db::init_db;
    use reth_db_common::init::init_genesis;
    use reth_node_core::dirs::{DataDirPath, MaybePlatformPath};
    use reth_provider::{providers::StaticFileProvider, ProviderFactory};
    use tempfile::TempDir;
    use url::Url;

    #[test]
    fn test_default_config() {
        let config: BaseConfig = serde_json::from_str("{}").unwrap();
        let config_default = BaseConfig::default();

        assert_eq!(config, config_default);
    }

    #[test]
    fn test_signing_domain_known_chains() {
        let cases = [
            (
                NamedChain::Mainnet,
                fixed_bytes!("00000001f5a5fd42d16a20302798ef6ed309979b43003d2320d9f0e8ea9831a9"),
            ),
            (
                NamedChain::Sepolia,
                fixed_bytes!("00000001d3010778cd08ee514b08fe67b6c503b510987a4ce43f42306d97c67c"),
            ),
            (
                NamedChain::Goerli,
                fixed_bytes!("00000001e4be9393b074ca1f3e4aabd585ca4bea101170ccfaf71b89ce5c5c38"),
            ),
            (
                NamedChain::Holesky,
                fixed_bytes!("000000015b83a23759c560b2d0c64576e1dcfc34ea94c4988f3e0d9f77f05387"),
            ),
        ];

        for (chain, domain) in cases.iter() {
            let found = get_signing_domain(Chain::from_named(*chain), vec![]).unwrap();
            assert_eq!(found, *domain);
        }
    }

    #[ignore]
    #[test]
    fn test_signing_domain_custom_chain() {
        let client = Client::new(Url::parse("http://localhost:8000").unwrap());
        let found = get_signing_domain(Chain::from_id(12345), vec![client]).unwrap();

        assert_eq!(
            found,
            fixed_bytes!("00000001aaf2630a2874a74199f4b5d11a7d6377f363a236271bff4bf8eb4ab3")
        );
    }

    #[test]
    fn test_reth_db() {
        // Setup and initialize a temp reth db (with static files)
        let tempdir = TempDir::with_prefix_in("rbuilder-", "/tmp").unwrap();

        let data_dir = MaybePlatformPath::<DataDirPath>::from(tempdir.into_path());
        let data_dir = data_dir.unwrap_or_chain_default(Chain::mainnet(), DatadirArgs::default());

        let db = init_db(data_dir.data_dir(), Default::default()).unwrap();
        let provider_factory = ProviderFactory::new(
            db,
            SEPOLIA.clone(),
            StaticFileProvider::read_write(data_dir.static_files().as_path()).unwrap(),
        );
        init_genesis(provider_factory).unwrap();

        // Create longer-lived PathBuf values
        let data_dir_path = data_dir.data_dir();
        let db_path = data_dir.db();
        let static_files_path = data_dir.static_files();

        let test_cases = [
            // use main dir to resolve reth_db and static_files
            (Some(data_dir_path), None, None, true),
            // use main dir to resolve reth_db and provide static_files
            (
                Some(data_dir_path),
                None,
                Some(static_files_path.clone()),
                true,
            ),
            // provide both reth_db and static_files
            (
                None,
                Some(db_path.as_path()),
                Some(static_files_path.clone()),
                true,
            ),
            // fail to provide main dir to resolve empty static_files
            (None, Some(db_path.as_path()), None, false),
            // fail to provide main dir to resolve empty reth_db
            (None, None, Some(static_files_path), false),
        ];

        for (reth_datadir_path, reth_db_path, reth_static_files_path, should_succeed) in
            test_cases.iter()
        {
            let result = create_provider_factory_rw(
                reth_datadir_path.as_deref(),
                reth_db_path.as_deref(),
                reth_static_files_path.as_deref(),
                Default::default(),
            );

            if *should_succeed {
                assert!(
                    result.is_ok(),
                    "Expected success, but got error: {:?}",
                    result.err()
                );
            } else {
                assert!(result.is_err(), "Expected error, but got success");
            }
        }
    }
}
