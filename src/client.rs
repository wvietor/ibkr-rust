use anyhow::Context;
use crossbeam::queue::SegQueue;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::net::tcp::OwnedReadHalf;
use tokio::task::JoinHandle;
use tokio::{io::AsyncReadExt, net::TcpStream, sync::mpsc};
use tokio_util::sync::CancellationToken;

use crate::contract::{ContractId, Security};
use crate::decode::Decoder;
use crate::market_data::{
    histogram, historical_bar, historical_ticks, live_bar, live_data, live_ticks,
    updating_historical_bar,
};
use crate::message::{In, Out, ToClient, ToWrapper};
use crate::wrapper::{
    indicators::{LocalMarker, RemoteMarker},
    Initializer, Local, Remote,
};
use crate::{
    account::Tag,
    comm::Writer,
    constants, decode,
    execution::Filter,
    order::{Executable, Order},
    payload::ExchangeId,
    reader::Reader,
};

// ======================================
// === Types for Handling Config File ===
// ======================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
struct Ports {
    tws_live: u16,
    tws_paper: u16,
    gateway_live: u16,
    gateway_paper: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
struct Config {
    address: std::net::Ipv4Addr,
    #[serde(alias = "Ports")]
    ports: Ports,
}

impl Config {
    #[inline]
    fn new(path: &str) -> anyhow::Result<Self> {
        toml::from_str(
            std::fs::read_to_string(path)
                .with_context(|| format!("Invalid config file at path {path}"))?
                .as_str(),
        )
        .with_context(|| {
            format!(
                "Invalid TOML file at path {path}.\n
        # =========================\n
        # === config.toml Usage ===\n
        # =========================\n
        address: std::net::Ipv4Addr\n
        \n
        [Ports]\n
        tws_live: u16\n
        tws_paper: u16\n
        \n
        gateway_live: u16\n
        gateway_paper: u16\n"
            )
        })
    }
}

// =======================================
// === Client Builder and Helper Types ===
// =======================================

//noinspection SpellCheckingInspection
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Represents the two types of connections to IBKR's trading systems.
pub enum Mode {
    /// A live trading connection with real money.
    Live,
    /// A paper (simulated) trading connection with fake money.
    Paper,
}

/// For safety, the default [`Mode`] is a paper trading environment
///
/// # Examples
/// ```
/// # use ibapi::client::Mode;
/// let m = Mode::default();
/// assert_eq!(m, Mode::Paper);
/// ```
impl Default for Mode {
    #[inline]
    fn default() -> Self {
        Self::Paper
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Represents the two platforms that facilitate trading with IBKR's systems. The two hosts are
/// indistinguishable from the perspective of an API application.
pub enum Host {
    /// IBKR's flagship Trader Workstation desktop application.
    Tws,
    /// A leaner GUI that requires less performance overhead but has no monitoring of sophisticated
    /// graphical capabilities.
    Gateway,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Inner {
    ConfigFile {
        mode: Mode,
        host: Host,
        config: Config,
    },
    Manual {
        port: u16,
        address: std::net::Ipv4Addr,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Facilitates the creation of a new connection to IBKR's trading systems.
///
/// Each connection requires a TCP port and address with which to connect to the appropriate IBKR
/// platform. This information is communicated by either: 1) Manually specifying the parameters in
/// [`Builder::manual`] or 2) Automatically looking them up in the config.toml file by specifying a
///  [`Mode`] and [`Host`] in [`Builder::from_config_file`].
pub struct Builder(Inner);

impl Builder {
    #[inline]
    /// Creates a new [`Builder`] from a mode, host, and (optionally) a path to "config.toml"
    ///
    /// # Arguments
    /// * `mode` - Specifies whether the builder will create a live (real money) or paper (fake
    /// money) trading environment.
    /// * `host` - Specifies the platform used for communication with IBKR's trading systems.
    /// * `path` - An optional string slice that overrides the default location of "./config.toml".
    ///
    /// # Errors
    /// Returns any error encountered while reading and parsing the config file.
    pub fn from_config_file(mode: Mode, host: Host, path: Option<&str>) -> anyhow::Result<Self> {
        let config = Config::new(path.unwrap_or("./config.toml"))?;
        Ok(Self(Inner::ConfigFile { mode, host, config }))
    }

    #[must_use]
    #[inline]
    /// Creates a new [`Builder`] from a TCP port and address.
    ///
    /// # Arguments
    /// * `port` - The TCP port with which to connect to IBKR's trading systems.
    /// * `address` - The IP address with which to connect to IBKR's trading systems.
    pub fn manual(port: u16, address: Option<std::net::Ipv4Addr>) -> Self {
        Self(Inner::Manual {
            port,
            address: address.unwrap_or(std::net::Ipv4Addr::LOCALHOST),
        })
    }

    /// Initiates a connection to IBKR's trading systems and returns a [`Client`].
    ///
    /// # Arguments
    /// * `client_id` - A unique ID for IBKR's systems to distinguish between clients
    ///
    /// # Errors
    /// This function will error if any of the following occurs:
    /// 1) An error occurs while initiating a TCP connection on the port and address specified in
    /// either [`Builder::manual`] or in the "config.toml" file specified in
    /// [`Builder::from_config_file`].
    /// 2) An error occurs while reading or writing the handshake message that initiates a
    /// connection with IBKR's trading systems.
    ///
    /// # Returns
    /// An inactive [`Client`] that will become active upon calling [`Client::local`] or
    /// [`Client::remote`].
    pub async fn connect(&self, client_id: i64) -> anyhow::Result<Client<indicators::Inactive>> {
        let (mode, host, port, address) = match self.0 {
            Inner::ConfigFile { mode, host, config } => (
                Some(mode),
                Some(host),
                match (mode, host) {
                    (Mode::Live, Host::Tws) => config.ports.tws_live,
                    (Mode::Live, Host::Gateway) => config.ports.gateway_live,
                    (Mode::Paper, Host::Tws) => config.ports.tws_paper,
                    (Mode::Paper, Host::Gateway) => config.ports.gateway_paper,
                },
                config.address,
            ),
            Inner::Manual { port, address } => (None, None, port, address),
        };

        let (mut reader, writer) = TcpStream::connect((address, port)).await?.into_split();

        let mut writer = Writer::new(writer);
        writer.add_prefix("API\0")?;
        writer.add_body(format!(
            "v{}..{}",
            constants::MIN_CLIENT_VERSION,
            constants::MAX_CLIENT_VERSION
        ))?;
        writer.send().await?;

        let mut buf = bytes::BytesMut::with_capacity(usize::try_from(reader.read_u32().await?)?);
        reader.read_buf(&mut buf).await?;
        let resp = buf.into_iter().map(char::from).collect::<String>();
        let mut params = resp.split('\0');

        let server_version = params
            .next()
            .ok_or_else(|| anyhow::Error::msg("Missing server version in IBKR handshake response"))?
            .parse()
            .with_context(|| "Failed to parse server version")?;
        let conn_time = chrono::NaiveDateTime::parse_and_remainder(
            params
                .next()
                .ok_or_else(|| {
                    anyhow::Error::msg("Missing connection time in IBKR handshake response")
                })?
                .trim_end_matches(|c: char| !c.is_numeric()),
            "%Y%m%d %X",
        )
        .with_context(|| "Failed to parse connection time")?
        .0;

        let (client_tx, wrapper_rx) =
            mpsc::channel::<ToWrapper>(constants::TO_WRAPPER_CHANNEL_SIZE);
        let (wrapper_tx, client_rx) = mpsc::channel::<ToClient>(constants::TO_CLIENT_CHANNEL_SIZE);

        let mut client = Client {
            mode,
            host,
            port,
            address,
            client_id,
            server_version,
            conn_time,
            writer,
            status: indicators::Inactive {
                reader,
                client_tx,
                client_rx,
                wrapper_tx,
                wrapper_rx,
            },
        };
        client.start_api().await?;

        Ok(client)
    }
}

// ===============================
// === Status Trait Definition ===
// ===============================

#[allow(clippy::module_name_repetitions)]
/// An active client, which can request information from IBKR trading systems.
pub type ActiveClient = Client<indicators::Active>;

type IntoActive = (
    Client<indicators::Active>,
    mpsc::Sender<ToClient>,
    mpsc::Receiver<ToWrapper>,
    Arc<SegQueue<Vec<String>>>,
);

#[inline]
#[allow(clippy::too_many_lines)]
async fn decode_msg_remote<W>(
    fields: Vec<String>,
    local: &mut Decoder<RemoteMarker<W>>,
    tx: &mut mpsc::Sender<ToClient>,
    rx: &mut mpsc::Receiver<ToWrapper>,
) where
    W: Remote,
{
    let status = match fields.first() {
        None => Err(anyhow::Error::msg("Empty fields received from reader")),
        Some(s) => match s.parse() {
            Ok(In::TickPrice) => Decoder::<RemoteMarker<W>>::tick_price_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "tick price msg"),
            Ok(In::TickSize) => Decoder::<RemoteMarker<W>>::tick_size_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "tick size msg"),
            Ok(In::OrderStatus) => Decoder::<RemoteMarker<W>>::order_status_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "order status msg"),
            Ok(In::ErrMsg) => Decoder::<RemoteMarker<W>>::err_msg_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "err msg msg"),
            Ok(In::OpenOrder) => Decoder::<RemoteMarker<W>>::open_order_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "open order msg"),
            Ok(In::AcctValue) => Decoder::<RemoteMarker<W>>::acct_value_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "acct value msg"),
            Ok(In::PortfolioValue) => Decoder::<RemoteMarker<W>>::portfolio_value_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "portfolio value msg"),
            Ok(In::AcctUpdateTime) => Decoder::<RemoteMarker<W>>::acct_update_time_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "acct update time msg"),
            Ok(In::NextValidId) => Decoder::<RemoteMarker<W>>::next_valid_id_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
                tx,
                rx,
            )
            .await
            .with_context(|| "next valid id msg"),
            Ok(In::ContractData) => Decoder::<RemoteMarker<W>>::contract_data_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
                tx,
                rx,
            )
            .await
            .with_context(|| "contract data msg"),
            Ok(In::ExecutionData) => Decoder::<RemoteMarker<W>>::execution_data_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "execution data msg"),
            Ok(In::MarketDepth) => Decoder::<RemoteMarker<W>>::market_depth_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "market depth msg"),
            Ok(In::MarketDepthL2) => Decoder::<RemoteMarker<W>>::market_depth_l2_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "market depth l2 msg"),
            Ok(In::NewsBulletins) => Decoder::<RemoteMarker<W>>::news_bulletins_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "news bulletins msg"),
            Ok(In::ManagedAccts) => Decoder::<RemoteMarker<W>>::managed_accts_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
                tx,
                rx,
            )
            .await
            .with_context(|| "managed accounts msg"),
            Ok(In::ReceiveFa) => Decoder::<RemoteMarker<W>>::receive_fa_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "receive fa msg"),
            Ok(In::HistoricalData) => Decoder::<RemoteMarker<W>>::historical_data_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "historical data msg"),
            Ok(In::BondContractData) => Decoder::<RemoteMarker<W>>::bond_contract_data_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "bond contract data msg"),
            Ok(In::ScannerParameters) => Decoder::<RemoteMarker<W>>::scanner_parameters_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "scanner parameters msg"),
            Ok(In::ScannerData) => Decoder::<RemoteMarker<W>>::scanner_data_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "scanner data msg"),
            Ok(In::TickOptionComputation) => {
                Decoder::<RemoteMarker<W>>::tick_option_computation_msg(
                    &mut fields.into_iter(),
                    &mut local.0.wrapper,
                )
                .await
                .with_context(|| "tick option computation msg")
            }
            Ok(In::TickGeneric) => Decoder::<RemoteMarker<W>>::tick_generic_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "tick generic msg"),
            Ok(In::TickString) => Decoder::<RemoteMarker<W>>::tick_string_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "tick string msg"),
            Ok(In::TickEfp) => Decoder::<RemoteMarker<W>>::tick_efp_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "tick efp msg"),
            Ok(In::CurrentTime) => Decoder::<RemoteMarker<W>>::current_time_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "current time msg"),
            Ok(In::RealTimeBars) => Decoder::<RemoteMarker<W>>::real_time_bars_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "real time bars msg"),
            Ok(In::FundamentalData) => Decoder::<RemoteMarker<W>>::fundamental_data_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "fundamental data msg"),
            Ok(In::ContractDataEnd) => Decoder::<RemoteMarker<W>>::contract_data_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "contract data end msg"),
            Ok(In::OpenOrderEnd) => Decoder::<RemoteMarker<W>>::open_order_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "open order end msg"),
            Ok(In::AcctDownloadEnd) => Decoder::<RemoteMarker<W>>::acct_download_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "acct download end msg"),
            Ok(In::ExecutionDataEnd) => Decoder::<RemoteMarker<W>>::execution_data_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "execution data end msg"),
            Ok(In::DeltaNeutralValidation) => {
                Decoder::<RemoteMarker<W>>::delta_neutral_validation_msg(
                    &mut fields.into_iter(),
                    &mut local.0.wrapper,
                )
                .await
                .with_context(|| "delta neutral validation msg")
            }
            Ok(In::TickSnapshotEnd) => Decoder::<RemoteMarker<W>>::tick_snapshot_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "tick snapshot end msg"),
            Ok(In::MarketDataType) => Decoder::<RemoteMarker<W>>::market_data_type_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "market data type msg"),
            Ok(In::CommissionReport) => Decoder::<RemoteMarker<W>>::commission_report_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "commission report msg"),
            Ok(In::PositionData) => Decoder::<RemoteMarker<W>>::position_data_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "position data msg"),
            Ok(In::PositionEnd) => Decoder::<RemoteMarker<W>>::position_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "position end msg"),
            Ok(In::AccountSummary) => Decoder::<RemoteMarker<W>>::account_summary_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "account summary msg"),
            Ok(In::AccountSummaryEnd) => Decoder::<RemoteMarker<W>>::account_summary_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "account summary end msg"),
            Ok(In::VerifyMessageApi) => Decoder::<RemoteMarker<W>>::verify_message_api_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "verify message api msg"),
            Ok(In::VerifyCompleted) => Decoder::<RemoteMarker<W>>::verify_completed_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "verify completed msg"),
            Ok(In::DisplayGroupList) => Decoder::<RemoteMarker<W>>::display_group_list_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "display group list msg"),
            Ok(In::DisplayGroupUpdated) => Decoder::<RemoteMarker<W>>::display_group_updated_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "display group updated msg"),
            Ok(In::VerifyAndAuthMessageApi) => {
                Decoder::<RemoteMarker<W>>::verify_and_auth_message_api_msg(
                    &mut fields.into_iter(),
                    &mut local.0.wrapper,
                )
                .await
                .with_context(|| "verify and auth message api msg")
            }
            Ok(In::VerifyAndAuthCompleted) => {
                Decoder::<RemoteMarker<W>>::verify_and_auth_completed_msg(
                    &mut fields.into_iter(),
                    &mut local.0.wrapper,
                )
                .await
                .with_context(|| "verify and auth completed msg")
            }
            Ok(In::PositionMulti) => Decoder::<RemoteMarker<W>>::position_multi_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "position multi msg"),
            Ok(In::PositionMultiEnd) => Decoder::<RemoteMarker<W>>::position_multi_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "position multi end msg"),
            Ok(In::AccountUpdateMulti) => Decoder::<RemoteMarker<W>>::account_update_multi_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "account update multi msg"),
            Ok(In::AccountUpdateMultiEnd) => {
                Decoder::<RemoteMarker<W>>::account_update_multi_end_msg(
                    &mut fields.into_iter(),
                    &mut local.0.wrapper,
                )
                .await
                .with_context(|| "account update multi end msg")
            }
            Ok(In::SecurityDefinitionOptionParameter) => {
                Decoder::<RemoteMarker<W>>::security_definition_option_parameter_msg(
                    &mut fields.into_iter(),
                    &mut local.0.wrapper,
                )
                .await
                .with_context(|| "security definition option parameter msg")
            }
            Ok(In::SecurityDefinitionOptionParameterEnd) => {
                Decoder::<RemoteMarker<W>>::security_definition_option_parameter_end_msg(
                    &mut fields.into_iter(),
                    &mut local.0.wrapper,
                )
                .await
                .with_context(|| "security definition option parameter end msg")
            }
            Ok(In::SoftDollarTiers) => Decoder::<RemoteMarker<W>>::soft_dollar_tiers_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "soft dollar tiers msg"),
            Ok(In::FamilyCodes) => Decoder::<RemoteMarker<W>>::family_codes_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "family codes msg"),
            Ok(In::SymbolSamples) => Decoder::<RemoteMarker<W>>::symbol_samples_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "symbol samples msg"),
            Ok(In::MktDepthExchanges) => Decoder::<RemoteMarker<W>>::mkt_depth_exchanges_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "mkt depth exchanges msg"),
            Ok(In::TickReqParams) => Decoder::<RemoteMarker<W>>::tick_req_params_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "tick req params msg"),
            Ok(In::SmartComponents) => Decoder::<RemoteMarker<W>>::smart_components_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "smart components msg"),
            Ok(In::NewsArticle) => Decoder::<RemoteMarker<W>>::news_article_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "news article msg"),
            Ok(In::TickNews) => Decoder::<RemoteMarker<W>>::tick_news_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "tick news msg"),
            Ok(In::NewsProviders) => Decoder::<RemoteMarker<W>>::news_providers_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "news providers msg"),
            Ok(In::HistoricalNews) => Decoder::<RemoteMarker<W>>::historical_news_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "historical news msg"),
            Ok(In::HistoricalNewsEnd) => Decoder::<RemoteMarker<W>>::historical_news_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "historical news end msg"),
            Ok(In::HeadTimestamp) => Decoder::<RemoteMarker<W>>::head_timestamp_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "head timestamp msg"),
            Ok(In::HistogramData) => Decoder::<RemoteMarker<W>>::histogram_data_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "histogram data msg"),
            Ok(In::HistoricalDataUpdate) => Decoder::<RemoteMarker<W>>::historical_data_update_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "historical data update msg"),
            Ok(In::RerouteMktDataReq) => Decoder::<RemoteMarker<W>>::reroute_mkt_data_req_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "reroute mkt data req msg"),
            Ok(In::RerouteMktDepthReq) => Decoder::<RemoteMarker<W>>::reroute_mkt_depth_req_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "reroute mkt depth req msg"),
            Ok(In::MarketRule) => Decoder::<RemoteMarker<W>>::market_rule_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "market rule msg"),
            Ok(In::Pnl) => {
                Decoder::<RemoteMarker<W>>::pnl_msg(&mut fields.into_iter(), &mut local.0.wrapper)
                    .await
                    .with_context(|| "pnl msg")
            }
            Ok(In::PnlSingle) => Decoder::<RemoteMarker<W>>::pnl_single_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "pnl single msg"),
            Ok(In::HistoricalTicks) => Decoder::<RemoteMarker<W>>::historical_ticks_midpoint_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "historical ticks msg"),
            Ok(In::HistoricalTicksBidAsk) => {
                Decoder::<RemoteMarker<W>>::historical_ticks_bid_ask_msg(
                    &mut fields.into_iter(),
                    &mut local.0.wrapper,
                )
                .await
                .with_context(|| "historical ticks bid ask msg")
            }
            Ok(In::HistoricalTicksLast) => Decoder::<RemoteMarker<W>>::historical_ticks_last_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "historical ticks last msg"),
            Ok(In::TickByTick) => Decoder::<RemoteMarker<W>>::tick_by_tick_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "tick by tick msg"),
            Ok(In::OrderBound) => Decoder::<RemoteMarker<W>>::order_bound_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "order bound msg"),
            Ok(In::CompletedOrder) => Decoder::<RemoteMarker<W>>::completed_order_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "completed order msg"),
            Ok(In::CompletedOrdersEnd) => Decoder::<RemoteMarker<W>>::completed_orders_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "completed orders end msg"),
            Ok(In::ReplaceFaEnd) => Decoder::<RemoteMarker<W>>::replace_fa_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "replace fa end msg"),
            Ok(In::WshMetaData) => Decoder::<RemoteMarker<W>>::wsh_meta_data_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "wsh meta data msg"),
            Ok(In::WshEventData) => Decoder::<RemoteMarker<W>>::wsh_event_data_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "wsh event data msg"),
            Ok(In::HistoricalSchedule) => Decoder::<RemoteMarker<W>>::historical_schedule_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "historical schedule msg"),
            Ok(In::UserInfo) => Decoder::<RemoteMarker<W>>::user_info_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "user info msg"),
            Err(e) => Err(e.into()),
        },
    };
    match status {
        Ok(()) => (),
        Err(e) => {
            println!("\x1B[31m{e}");
            println!("{}\x1B[0m", e.root_cause());
        }
    }
}

#[inline]
#[allow(clippy::too_many_lines)]
async fn decode_msg_local<'c, W>(
    fields: Vec<String>,
    local: &mut Decoder<LocalMarker<'c, W>>,
    tx: &mut mpsc::Sender<ToClient>,
    rx: &mut mpsc::Receiver<ToWrapper>,
) where
    W: Local<'c>,
{
    let status = match fields.first() {
        None => Err(anyhow::Error::msg("Empty fields received from reader")),
        Some(s) => match s.parse() {
            Ok(In::TickPrice) => Decoder::<LocalMarker<'c, W>>::tick_price_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "tick price msg"),
            Ok(In::TickSize) => Decoder::<LocalMarker<'c, W>>::tick_size_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "tick size msg"),
            Ok(In::OrderStatus) => Decoder::<LocalMarker<'c, W>>::order_status_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "order status msg"),
            Ok(In::ErrMsg) => Decoder::<LocalMarker<'c, W>>::err_msg_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "err msg msg"),
            Ok(In::OpenOrder) => Decoder::<LocalMarker<'c, W>>::open_order_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "open order msg"),
            Ok(In::AcctValue) => Decoder::<LocalMarker<'c, W>>::acct_value_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "acct value msg"),
            Ok(In::PortfolioValue) => Decoder::<LocalMarker<'c, W>>::portfolio_value_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "portfolio value msg"),
            Ok(In::AcctUpdateTime) => Decoder::<LocalMarker<'c, W>>::acct_update_time_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "acct update time msg"),
            Ok(In::NextValidId) => Decoder::<LocalMarker<'c, W>>::next_valid_id_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
                tx,
                rx,
            )
            .await
            .with_context(|| "next valid id msg"),
            Ok(In::ContractData) => Decoder::<LocalMarker<'c, W>>::contract_data_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
                tx,
                rx,
            )
            .await
            .with_context(|| "contract data msg"),
            Ok(In::ExecutionData) => Decoder::<LocalMarker<'c, W>>::execution_data_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "execution data msg"),
            Ok(In::MarketDepth) => Decoder::<LocalMarker<'c, W>>::market_depth_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "market depth msg"),
            Ok(In::MarketDepthL2) => Decoder::<LocalMarker<'c, W>>::market_depth_l2_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "market depth l2 msg"),
            Ok(In::NewsBulletins) => Decoder::<LocalMarker<'c, W>>::news_bulletins_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "news bulletins msg"),
            Ok(In::ManagedAccts) => Decoder::<LocalMarker<'c, W>>::managed_accts_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
                tx,
                rx,
            )
            .await
            .with_context(|| "managed accounts msg"),
            Ok(In::ReceiveFa) => Decoder::<LocalMarker<'c, W>>::receive_fa_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "receive fa msg"),
            Ok(In::HistoricalData) => Decoder::<LocalMarker<'c, W>>::historical_data_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "historical data msg"),
            Ok(In::BondContractData) => Decoder::<LocalMarker<'c, W>>::bond_contract_data_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "bond contract data msg"),
            Ok(In::ScannerParameters) => Decoder::<LocalMarker<'c, W>>::scanner_parameters_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "scanner parameters msg"),
            Ok(In::ScannerData) => Decoder::<LocalMarker<'c, W>>::scanner_data_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "scanner data msg"),
            Ok(In::TickOptionComputation) => {
                Decoder::<LocalMarker<'c, W>>::tick_option_computation_msg(
                    &mut fields.into_iter(),
                    &mut local.0.wrapper,
                )
                .await
                .with_context(|| "tick option computation msg")
            }
            Ok(In::TickGeneric) => Decoder::<LocalMarker<'c, W>>::tick_generic_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "tick generic msg"),
            Ok(In::TickString) => Decoder::<LocalMarker<'c, W>>::tick_string_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "tick string msg"),
            Ok(In::TickEfp) => Decoder::<LocalMarker<'c, W>>::tick_efp_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "tick efp msg"),
            Ok(In::CurrentTime) => Decoder::<LocalMarker<'c, W>>::current_time_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "current time msg"),
            Ok(In::RealTimeBars) => Decoder::<LocalMarker<'c, W>>::real_time_bars_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "real time bars msg"),
            Ok(In::FundamentalData) => Decoder::<LocalMarker<'c, W>>::fundamental_data_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "fundamental data msg"),
            Ok(In::ContractDataEnd) => Decoder::<LocalMarker<'c, W>>::contract_data_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "contract data end msg"),
            Ok(In::OpenOrderEnd) => Decoder::<LocalMarker<'c, W>>::open_order_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "open order end msg"),
            Ok(In::AcctDownloadEnd) => Decoder::<LocalMarker<'c, W>>::acct_download_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "acct download end msg"),
            Ok(In::ExecutionDataEnd) => Decoder::<LocalMarker<'c, W>>::execution_data_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "execution data end msg"),
            Ok(In::DeltaNeutralValidation) => {
                Decoder::<LocalMarker<'c, W>>::delta_neutral_validation_msg(
                    &mut fields.into_iter(),
                    &mut local.0.wrapper,
                )
                .await
                .with_context(|| "delta neutral validation msg")
            }
            Ok(In::TickSnapshotEnd) => Decoder::<LocalMarker<'c, W>>::tick_snapshot_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "tick snapshot end msg"),
            Ok(In::MarketDataType) => Decoder::<LocalMarker<'c, W>>::market_data_type_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "market data type msg"),
            Ok(In::CommissionReport) => Decoder::<LocalMarker<'c, W>>::commission_report_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "commission report msg"),
            Ok(In::PositionData) => Decoder::<LocalMarker<'c, W>>::position_data_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "position data msg"),
            Ok(In::PositionEnd) => Decoder::<LocalMarker<'c, W>>::position_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "position end msg"),
            Ok(In::AccountSummary) => Decoder::<LocalMarker<'c, W>>::account_summary_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "account summary msg"),
            Ok(In::AccountSummaryEnd) => Decoder::<LocalMarker<'c, W>>::account_summary_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "account summary end msg"),
            Ok(In::VerifyMessageApi) => Decoder::<LocalMarker<'c, W>>::verify_message_api_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "verify message api msg"),
            Ok(In::VerifyCompleted) => Decoder::<LocalMarker<'c, W>>::verify_completed_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "verify completed msg"),
            Ok(In::DisplayGroupList) => Decoder::<LocalMarker<'c, W>>::display_group_list_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "display group list msg"),
            Ok(In::DisplayGroupUpdated) => {
                Decoder::<LocalMarker<'c, W>>::display_group_updated_msg(
                    &mut fields.into_iter(),
                    &mut local.0.wrapper,
                )
                .await
                .with_context(|| "display group updated msg")
            }
            Ok(In::VerifyAndAuthMessageApi) => {
                Decoder::<LocalMarker<'c, W>>::verify_and_auth_message_api_msg(
                    &mut fields.into_iter(),
                    &mut local.0.wrapper,
                )
                .await
                .with_context(|| "verify and auth message api msg")
            }
            Ok(In::VerifyAndAuthCompleted) => {
                Decoder::<LocalMarker<'c, W>>::verify_and_auth_completed_msg(
                    &mut fields.into_iter(),
                    &mut local.0.wrapper,
                )
                .await
                .with_context(|| "verify and auth completed msg")
            }
            Ok(In::PositionMulti) => Decoder::<LocalMarker<'c, W>>::position_multi_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "position multi msg"),
            Ok(In::PositionMultiEnd) => Decoder::<LocalMarker<'c, W>>::position_multi_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "position multi end msg"),
            Ok(In::AccountUpdateMulti) => Decoder::<LocalMarker<'c, W>>::account_update_multi_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "account update multi msg"),
            Ok(In::AccountUpdateMultiEnd) => {
                Decoder::<LocalMarker<'c, W>>::account_update_multi_end_msg(
                    &mut fields.into_iter(),
                    &mut local.0.wrapper,
                )
                .await
                .with_context(|| "account update multi end msg")
            }
            Ok(In::SecurityDefinitionOptionParameter) => {
                Decoder::<LocalMarker<'c, W>>::security_definition_option_parameter_msg(
                    &mut fields.into_iter(),
                    &mut local.0.wrapper,
                )
                .await
                .with_context(|| "security definition option parameter msg")
            }
            Ok(In::SecurityDefinitionOptionParameterEnd) => {
                Decoder::<LocalMarker<'c, W>>::security_definition_option_parameter_end_msg(
                    &mut fields.into_iter(),
                    &mut local.0.wrapper,
                )
                .await
                .with_context(|| "security definition option parameter end msg")
            }
            Ok(In::SoftDollarTiers) => Decoder::<LocalMarker<'c, W>>::soft_dollar_tiers_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "soft dollar tiers msg"),
            Ok(In::FamilyCodes) => Decoder::<LocalMarker<'c, W>>::family_codes_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "family codes msg"),
            Ok(In::SymbolSamples) => Decoder::<LocalMarker<'c, W>>::symbol_samples_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "symbol samples msg"),
            Ok(In::MktDepthExchanges) => Decoder::<LocalMarker<'c, W>>::mkt_depth_exchanges_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "mkt depth exchanges msg"),
            Ok(In::TickReqParams) => Decoder::<LocalMarker<'c, W>>::tick_req_params_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "tick req params msg"),
            Ok(In::SmartComponents) => Decoder::<LocalMarker<'c, W>>::smart_components_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "smart components msg"),
            Ok(In::NewsArticle) => Decoder::<LocalMarker<'c, W>>::news_article_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "news article msg"),
            Ok(In::TickNews) => Decoder::<LocalMarker<'c, W>>::tick_news_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "tick news msg"),
            Ok(In::NewsProviders) => Decoder::<LocalMarker<'c, W>>::news_providers_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "news providers msg"),
            Ok(In::HistoricalNews) => Decoder::<LocalMarker<'c, W>>::historical_news_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "historical news msg"),
            Ok(In::HistoricalNewsEnd) => Decoder::<LocalMarker<'c, W>>::historical_news_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "historical news end msg"),
            Ok(In::HeadTimestamp) => Decoder::<LocalMarker<'c, W>>::head_timestamp_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "head timestamp msg"),
            Ok(In::HistogramData) => Decoder::<LocalMarker<'c, W>>::histogram_data_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "histogram data msg"),
            Ok(In::HistoricalDataUpdate) => {
                Decoder::<LocalMarker<'c, W>>::historical_data_update_msg(
                    &mut fields.into_iter(),
                    &mut local.0.wrapper,
                )
                .await
                .with_context(|| "historical data update msg")
            }
            Ok(In::RerouteMktDataReq) => Decoder::<LocalMarker<'c, W>>::reroute_mkt_data_req_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "reroute mkt data req msg"),
            Ok(In::RerouteMktDepthReq) => Decoder::<LocalMarker<'c, W>>::reroute_mkt_depth_req_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "reroute mkt depth req msg"),
            Ok(In::MarketRule) => Decoder::<LocalMarker<'c, W>>::market_rule_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "market rule msg"),
            Ok(In::Pnl) => Decoder::<LocalMarker<'c, W>>::pnl_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "pnl msg"),
            Ok(In::PnlSingle) => Decoder::<LocalMarker<'c, W>>::pnl_single_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "pnl single msg"),
            Ok(In::HistoricalTicks) => {
                Decoder::<LocalMarker<'c, W>>::historical_ticks_midpoint_msg(
                    &mut fields.into_iter(),
                    &mut local.0.wrapper,
                )
                .await
                .with_context(|| "historical ticks msg")
            }
            Ok(In::HistoricalTicksBidAsk) => {
                Decoder::<LocalMarker<'c, W>>::historical_ticks_bid_ask_msg(
                    &mut fields.into_iter(),
                    &mut local.0.wrapper,
                )
                .await
                .with_context(|| "historical ticks bid ask msg")
            }
            Ok(In::HistoricalTicksLast) => {
                Decoder::<LocalMarker<'c, W>>::historical_ticks_last_msg(
                    &mut fields.into_iter(),
                    &mut local.0.wrapper,
                )
                .await
                .with_context(|| "historical ticks last msg")
            }
            Ok(In::TickByTick) => Decoder::<LocalMarker<'c, W>>::tick_by_tick_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "tick by tick msg"),
            Ok(In::OrderBound) => Decoder::<LocalMarker<'c, W>>::order_bound_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "order bound msg"),
            Ok(In::CompletedOrder) => Decoder::<LocalMarker<'c, W>>::completed_order_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "completed order msg"),
            Ok(In::CompletedOrdersEnd) => Decoder::<LocalMarker<'c, W>>::completed_orders_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "completed orders end msg"),
            Ok(In::ReplaceFaEnd) => Decoder::<LocalMarker<'c, W>>::replace_fa_end_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "replace fa end msg"),
            Ok(In::WshMetaData) => Decoder::<LocalMarker<'c, W>>::wsh_meta_data_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "wsh meta data msg"),
            Ok(In::WshEventData) => Decoder::<LocalMarker<'c, W>>::wsh_event_data_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "wsh event data msg"),
            Ok(In::HistoricalSchedule) => Decoder::<LocalMarker<'c, W>>::historical_schedule_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "historical schedule msg"),
            Ok(In::UserInfo) => Decoder::<LocalMarker<'c, W>>::user_info_msg(
                &mut fields.into_iter(),
                &mut local.0.wrapper,
            )
            .await
            .with_context(|| "user info msg"),
            Err(e) => Err(e.into()),
        },
    };
    match status {
        Ok(()) => (),
        Err(e) => {
            println!("\x1B[31m{e}");
            println!("{}\x1B[0m", e.root_cause());
        }
    }
}

pub(crate) mod indicators {
    use super::Reader;
    use crate::message::{ToClient, ToWrapper};
    use std::collections::HashSet;
    use tokio::{net::tcp::OwnedReadHalf, sync::mpsc, task::JoinHandle};

    pub trait Status {}

    pub struct Inactive {
        pub(crate) reader: OwnedReadHalf,
        pub(crate) client_tx: mpsc::Sender<ToWrapper>,
        pub(crate) client_rx: mpsc::Receiver<ToClient>,
        pub(crate) wrapper_tx: mpsc::Sender<ToClient>,
        pub(crate) wrapper_rx: mpsc::Receiver<ToWrapper>,
    }

    impl Status for Inactive {}

    #[derive(Debug)]
    pub struct Active {
        pub(crate) r_thread: JoinHandle<Reader>,
        pub(crate) disconnect: tokio_util::sync::CancellationToken,
        pub(crate) tx: mpsc::Sender<ToWrapper>,
        pub(crate) rx: mpsc::Receiver<ToClient>,
        pub(crate) managed_accounts: HashSet<String>,
        pub(crate) order_id: core::ops::RangeFrom<i64>,
        pub(crate) req_id: core::ops::RangeFrom<i64>,
    }

    impl Status for Active {}
}

// =============================
// === Client Implementation ===
// =============================

#[derive(Debug)]
/// The principal client that handles all outgoing messages to the IBKR trading systems. It also
/// manages messages that are received from the "reader thread". Before any useful functionality is
/// available, an inactive client (which is created from [`Builder::connect`]) must call
/// [`Client::local`] or [`Client::remote`]. This method will return an active client that can make useful queries.
///
/// In general, [`Client`] has two types of methods: "req" methods and "get" methods.
///
/// "Req" methods require an active connection to the IBKR trading systems, and each method
/// corresponds to a single outgoing message. Note that all "req" methods are async and
/// therefore must be awaited before any useful message is sent.
///
/// "Get" methods can be called regardless of whether the client is active or inactive. These
/// methods return useful attributes of the client or other locally managed data.
pub struct Client<C: indicators::Status> {
    mode: Option<Mode>,
    host: Option<Host>,
    port: u16,
    address: std::net::Ipv4Addr,
    client_id: i64,
    server_version: u32,
    conn_time: chrono::NaiveDateTime,
    writer: Writer,
    status: C,
}

impl<S: indicators::Status> Client<S> {
    // ====================================================
    // === Methods That Return Attributes of the Client ===
    // ====================================================

    #[inline]
    /// Return the client's mode, if it was created with [`Builder::from_config_file`].
    ///
    /// # Returns
    /// The client's [`Mode`], if it exists; otherwise, [`None`].
    pub const fn get_mode(&self) -> Option<Mode> {
        self.mode
    }

    #[inline]
    /// Return the client's host, if it was created with [`Builder::from_config_file`].
    ///
    /// # Returns
    /// The client's [`Host`], if it exists; otherwise, [`None`].
    pub const fn get_host(&self) -> Option<Host> {
        self.host
    }

    #[inline]
    /// Return the client's port
    pub const fn get_port(&self) -> u16 {
        self.port
    }

    #[inline]
    /// Return the client's address
    pub const fn get_address(&self) -> std::net::Ipv4Addr {
        self.address
    }

    #[inline]
    /// Return the client's ID, which is used by the IBKR trading systems to distinguish it from
    /// other connections.
    pub const fn get_client_id(&self) -> i64 {
        self.client_id
    }

    #[inline]
    /// Return the time at which the client successfully connected.
    pub const fn get_conn_time(&self) -> chrono::NaiveDateTime {
        self.conn_time
    }

    #[inline]
    /// Return the version of the IBKR server with which the client is communicating.
    pub const fn get_server_version(&self) -> u32 {
        self.server_version
    }
}

#[inline]
fn spawn_reader_thread(
    rdr: OwnedReadHalf,
) -> (
    CancellationToken,
    Arc<SegQueue<Vec<String>>>,
    JoinHandle<Reader>,
) {
    let disconnect = CancellationToken::new();
    let queue = Arc::new(SegQueue::new());

    let r_queue = Arc::clone(&queue);
    let r_disconnect = disconnect.clone();
    let r_thread = tokio::spawn(async move {
        let reader = Reader::new(rdr, r_queue, r_disconnect);
        reader.run().await
    });
    (disconnect, queue, r_thread)
}

impl Client<indicators::Inactive> {
    // ==========================================
    // === Methods That Initiate the API Loop ===
    // ==========================================

    async fn start_api(&mut self) -> Result<(), anyhow::Error> {
        const VERSION: u8 = 2;

        self.writer
            .add_body((Out::StartApi, VERSION, self.client_id, None::<()>))?;
        self.writer.send().await?;
        Ok(())
    }

    #[allow(clippy::unwrap_used, clippy::missing_panics_doc)]
    fn into_active(self) -> IntoActive {
        let (disconnect, queue, r_thread) = spawn_reader_thread(self.status.reader);

        let (mut managed_accounts, mut valid_id) = (None, None);
        while managed_accounts.is_none() || valid_id.is_none() {
            if let Some(fields) = queue.pop() {
                match fields.first().and_then(|t| t.parse().ok()) {
                    Some(In::ManagedAccts) => {
                        managed_accounts = Some(
                            fields
                                .into_iter()
                                .skip(2)
                                .filter(|v| v.as_str() != "")
                                .collect::<std::collections::HashSet<String>>(),
                        );
                    }
                    Some(In::NextValidId) => {
                        valid_id = decode::nth(&mut fields.into_iter(), 2)
                            .with_context(|| "Expected ID, found none")
                            .ok()
                            .and_then(|t| {
                                t.parse::<i64>()
                                    .with_context(|| "Invalid value for ID")
                                    .ok()
                            });
                    }
                    Some(_) => queue.push(fields),
                    None => (),
                }
            }
        }
        let (managed_accounts, valid_id) = (managed_accounts.unwrap(), valid_id.unwrap()..);

        let client = Client {
            mode: self.mode,
            host: self.host,
            port: self.port,
            address: self.address,
            client_id: self.client_id,
            server_version: self.server_version,
            conn_time: self.conn_time,
            writer: self.writer,
            status: indicators::Active {
                r_thread,
                disconnect,
                tx: self.status.client_tx,
                rx: self.status.client_rx,
                managed_accounts,
                order_id: valid_id,
                req_id: 0_i64..,
            },
        };
        (
            client,
            self.status.wrapper_tx,
            self.status.wrapper_rx,
            queue,
        )
    }

    /// Initiates the main message loop and spawns all helper threads to manage the application.
    ///
    /// # Returns
    /// A [`Builder`] that can be used to reconnect to the IBKR TWS API.
    ///
    /// # Errors
    /// Any error that occurs in the [`Client<Active>::disconnect`] process
    pub async fn local<I: for<'c> Initializer<'c>>(
        self,
        init: I,
    ) -> Result<Builder, std::io::Error> {
        let (mut client, mut tx, mut rx, queue) = self.into_active();

        let temp = CancellationToken::new();
        let temp_2 = temp.clone();
        let con_fut = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = temp.cancelled() => { break (queue, tx, rx); },
                    () = async {
                        let _ = if let Some(fields) = queue.pop() {
                            match fields.first().and_then(|t| t.parse().ok()) {
                                Some(In::ContractData) => decode::decode_contract_no_wrapper(&mut fields.into_iter(), &mut tx, &mut rx).await.with_context(|| "contract data msg"),
                                Some(_) => { queue.push(fields); Ok(()) },
                                None => Ok(()),
                            }
                        } else { Ok(()) };
                    } => ()
                }
            }
        });

        let break_loop = CancellationToken::new();
        let mut decoder = Decoder(LocalMarker {
            wrapper: Initializer::build(init, &mut client, break_loop.clone()).await,
            _init_marker: &std::marker::PhantomData,
        });
        temp_2.cancel();
        let (queue, mut tx, mut rx) = con_fut.await?;

        loop {
            tokio::select! {
                () = break_loop.cancelled() => {
                    println!("Client loop: disconnecting");
                    break
                },
                () = async {
                    if let Some(fields) = queue.pop() {
                        decode_msg_local(fields, &mut decoder, &mut tx, &mut rx).await;
                    }
                } => (),
            }
        }
        drop(decoder);
        client.disconnect().await
    }

    /// Initiates the main message loop and spawns all helper threads to manage the application.
    ///
    /// # Returns
    /// An active [`Client`] that can be used to make API requests.
    pub fn remote<W: Remote + Send + 'static>(self, wrapper: W) -> Client<indicators::Active> {
        let (client, mut tx, mut rx, queue) = self.into_active();
        let c_loop_disconnect = client.status.disconnect.clone();
        let mut decoder = Decoder(RemoteMarker { wrapper });

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = c_loop_disconnect.cancelled() => {println!("Client loop: disconnecting"); break},
                    () = async {
                            if let Some(fields) = queue.pop() {
                                decode_msg_remote(fields, &mut decoder, &mut tx, &mut rx).await;
                            }
                    } => (),
                }
            }
        });

        client
    }
}

type ReqResult = Result<(), std::io::Error>;
type IdResult = Result<i64, std::io::Error>;

impl Client<indicators::Active> {
    // ====================================================
    // === Methods That Return Attributes of the Client ===
    // ====================================================

    // Don't worry about the allow: This function will NEVER panic
    #[inline]
    #[allow(clippy::missing_panics_doc, clippy::unwrap_used)]
    /// Get the next valid *order* ID, as determined by the client's internal counter
    ///
    /// # Returns
    /// The next valid order ID
    fn get_next_order_id(&mut self) -> i64 {
        self.status.order_id.next().unwrap()
    }

    // Don't worry about the allow: This function will NEVER panic
    #[inline]
    #[allow(clippy::missing_panics_doc, clippy::unwrap_used)]
    /// Get the next valid *request* ID, as determined by the client's internal counter
    ///
    /// # Returns
    /// The next valid request ID
    fn get_next_req_id(&mut self) -> i64 {
        self.status.req_id.next().unwrap()
    }

    #[inline]
    #[must_use]
    /// Get the set of accounts managed by the client
    ///
    /// # Returns
    /// A reference to the set of the client's managed accounts
    pub const fn get_managed_accounts(&self) -> &std::collections::HashSet<String> {
        &self.status.managed_accounts
    }

    // ===================================
    // === Methods That Make API Calls ===
    // ===================================

    // === General Functions ===

    /// Request the current time from the server.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn req_current_time(&mut self) -> ReqResult {
        const VERSION: u8 = 1;

        self.writer.add_body((Out::ReqCurrentTime, VERSION))?;
        self.writer.send().await
    }

    /// Requests the accounts to which the logged user has access to.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn req_managed_accounts(&mut self) -> ReqResult {
        const VERSION: u8 = 1;

        self.writer.add_body((Out::ReqManagedAccts, VERSION))?;
        self.writer.send().await
    }

    /// Creates a subscription to the TWS through which account and portfolio information is
    /// delivered. This information is the exact same as the one displayed within the TWS' Account
    /// Window.
    ///
    /// # Arguments
    /// * `account_number` - The account number for which to subscribe to account data (optional for
    /// single account structures)
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message. Additionally, returns an
    /// error if a provided `account_number` is not in the client's managed accounts.
    pub async fn req_account_updates(&mut self, account_number: Option<String>) -> ReqResult {
        const VERSION: u8 = 2;
        if let Some(acct_num) = &account_number {
            check_valid_account(self, acct_num)?;
        }

        self.writer
            .add_body((Out::ReqAcctData, VERSION, 1, account_number))?;
        self.writer.send().await
    }

    /// Cancels an existing subscription to receive account updates.
    ///
    /// # Arguments
    /// * `account_number` - The account number for which to subscribe to account data (optional for
    /// single account structures)
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message. Additionally, returns an
    /// error if a provided `account_number` is not in the client's managed accounts.
    pub async fn cancel_account_updates(&mut self, account_number: Option<String>) -> ReqResult {
        const VERSION: u8 = 2;
        if let Some(acct_num) = &account_number {
            check_valid_account(self, acct_num)?;
        }

        self.writer
            .add_body((Out::ReqAcctData, VERSION, 0, account_number))?;
        self.writer.send().await
    }

    /// Subscribes to position updates for all accessible accounts. All positions sent initially,
    /// and then only updates as positions change.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn req_positions(&mut self) -> ReqResult {
        const VERSION: u8 = 1;

        self.writer.add_body((Out::ReqPositions, VERSION))?;
        self.writer.send().await
    }

    /// Cancels a previous position subscription request made with [`Client::req_positions`].
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn cancel_positions(&mut self) -> ReqResult {
        const VERSION: u8 = 1;

        self.writer.add_body((Out::CancelPositions, VERSION))?;
        self.writer.send().await
    }

    /// Creates subscription for real time daily P&L and unrealized P&L updates.
    ///
    /// # Arguments
    /// * `account_number` - The account number with which to create the subscription.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message. Additionally, returns an
    /// error if a provided `account_number` is not in the client's managed accounts.
    ///
    /// # Returns
    /// Returns the unique ID associated with the request.
    pub async fn req_pnl(&mut self, account_number: String) -> IdResult {
        let req_id = self.get_next_req_id();
        check_valid_account(self, &account_number)?;

        self.writer
            .add_body((Out::ReqPnl, req_id, account_number, None::<()>))?;
        self.writer.send().await?;
        Ok(req_id)
    }

    /// Cancel subscription for real-time updates created by [`Client::req_pnl`]
    ///
    /// # Arguments
    /// * `req_id` - The ID of the [`Client::req_pnl`] subscription to cancel.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn cancel_pnl(&mut self, req_id: i64) -> ReqResult {
        self.writer.add_body((Out::CancelPnl, req_id))?;
        self.writer.send().await
    }

    /// Creates subscription for real time daily P&L and unrealized P&L updates, but only for a
    /// specific position.
    ///
    /// # Arguments
    /// * `account_number` - The account number with which to create the subscription.
    /// * `contract_id` - The contract ID to create a subscription to changes for a specific
    /// security
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message. Additionally, returns an
    /// error if a provided `account_number` is not in the client's managed accounts.
    ///
    /// # Returns
    /// Returns the unique ID associated with the request.
    pub async fn req_single_position_pnl(
        &mut self,
        account_number: String,
        contract_id: ContractId,
    ) -> IdResult {
        let req_id = self.get_next_req_id();
        check_valid_account(self, &account_number)?;

        self.writer.add_body((
            Out::ReqPnlSingle,
            req_id,
            account_number,
            None::<()>,
            contract_id,
        ))?;
        self.writer.send().await?;
        Ok(req_id)
    }

    /// Cancel subscription for real-time updates created by [`Client::req_single_position_pnl`]
    ///
    /// # Arguments
    /// * `req_id` - The ID of the [`Client::req_pnl`] subscription to cancel.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn cancel_pnl_single(&mut self, req_id: i64) -> ReqResult {
        self.writer.add_body((Out::CancelPnl, req_id))?;
        self.writer.send().await
    }

    /// Request completed orders.
    ///
    /// # Arguments
    /// * `api_only` - When true, only orders placed from the API returned.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn req_completed_orders(&mut self, api_only: bool) -> ReqResult {
        self.writer.add_body((Out::ReqCompletedOrders, api_only))?;
        self.writer.send().await
    }

    /// Request summary information about a specific account, creating a subscription to the same
    /// information as is shown in the TWS Account Summary tab.
    ///
    /// # Arguments
    /// * `tags` - The list of data tags to include in the subscription.
    ///
    /// # Returns
    /// Returns the unique ID associated with the request.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn req_account_summary(&mut self, tags: &Vec<Tag>) -> IdResult {
        const VERSION: u8 = 1;
        let req_id = self.get_next_req_id();

        self.writer
            .add_body((Out::ReqAccountSummary, VERSION, req_id, "All", tags))?;
        self.writer.send().await?;
        Ok(req_id)
    }

    /// Cancel an existing account summary subscription created by [`Client::req_account_summary`].
    ///
    /// # Arguments
    /// * `req_id` - The ID of the subscription to cancel.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn cancel_account_summary(&mut self, req_id: i64) -> ReqResult {
        const VERSION: u8 = 1;

        self.writer
            .add_body((Out::CancelAccountSummary, VERSION, req_id))?;
        self.writer.send().await
    }

    /// Request user info details for the user associated with the calling client.
    ///
    /// # Returns
    /// Returns the unique ID associated with the request.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn req_user_info(&mut self) -> IdResult {
        let req_id = self.get_next_req_id();

        self.writer.add_body((Out::ReqUserInfo, req_id))?;
        self.writer.send().await?;
        Ok(req_id)
    }

    // === Historical Market Data ===

    /// Request historical bar data for a given security. See [`historical_bar`] for
    /// types and traits that are used in this function.
    ///
    /// # Arguments
    /// * `security` - The security for which to request data.
    /// * `end_date_time` - The last datetime for which data will be returned.
    /// * `duration` - The duration for which historical data be returned (ie. the difference
    /// between the first bar's datetime and the last bar's datetime).
    /// * `bar_size` - The size of each individual bar.
    /// * `data` - The type of data that to return (price, volume, volatility, etc.).
    /// * `regular_trading_hours_only` - When [`true`], only return bars from regular trading hours.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    ///
    /// # Returns
    /// Returns the unique ID associated with the request.
    pub async fn req_historical_bar<S, D>(
        &mut self,
        security: &S,
        end_date_time: historical_bar::EndDateTime,
        duration: historical_bar::Duration,
        bar_size: historical_bar::Size,
        data: D,
        regular_trading_hours_only: bool,
    ) -> IdResult
    where
        S: Security,
        D: historical_bar::data_types::DataType<S>,
    {
        let id = self.get_next_req_id();

        self.writer.add_body((
            Out::ReqHistoricalData,
            id,
            security,
            false,
            end_date_time,
            bar_size,
            duration,
            regular_trading_hours_only,
            data,
            1,
            false,
            None::<()>,
        ))?;
        self.writer.send().await?;
        Ok(id)
    }

    /// Request historical bar data that remains updated for a given security.
    /// See [`historical_bar`] for types and traits that are used in this function.
    ///
    /// # Arguments
    /// * `security` - The security for which to request data.
    /// * `duration` - The duration for which historical data be returned (ie. the difference
    /// between the first bar's datetime and the last bar's datetime).
    /// * `bar_size` - The size of each individual bar.
    /// * `data` - The type of data that to return (price, volume, volatility, etc.).
    /// * `regular_trading_hours_only` - When [`true`], only return bars from regular trading hours.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    ///
    /// # Returns
    /// Returns the unique ID associated with the request.
    pub async fn req_updating_historical_bar<S, D>(
        &mut self,
        security: &S,
        duration: updating_historical_bar::Duration,
        bar_size: updating_historical_bar::Size,
        data: D,
        regular_trading_hours_only: bool,
    ) -> IdResult
    where
        S: Security,
        D: updating_historical_bar::data_types::DataType<S>,
    {
        let id = self.get_next_req_id();

        self.writer.add_body((
            Out::ReqHistoricalData,
            id,
            security,
            false,
            None::<()>,
            bar_size,
            duration,
            regular_trading_hours_only,
            data,
            1,
            true,
            None::<()>,
        ))?;
        self.writer.send().await?;
        Ok(id)
    }

    /// Cancel an existing [`historical_bar`] data request.
    ///
    /// # Arguments
    /// * `req_id` - The ID of the [`historical_bar`] request to cancel.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn cancel_updating_historical_bar(&mut self, req_id: i64) -> ReqResult {
        const VERSION: u8 = 1;

        self.writer
            .add_body((Out::CancelHistoricalData, VERSION, req_id))?;
        self.writer.send().await
    }

    /// Request the earliest available data point for a given security and data type.
    ///
    /// # Arguments
    /// `security` - The security for which to make the request.
    /// `data` - The data for which to make the request.
    /// * `regular_trading_hours_only` - When [`true`], only return ticks from regular trading
    /// hours.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    ///
    /// # Returns
    /// Returns the unique ID associated with the request.
    pub async fn req_head_timestamp<S, D>(
        &mut self,
        security: &S,
        data: D,
        regular_trading_hours_only: bool,
    ) -> IdResult
    where
        S: Security,
        D: historical_ticks::data_types::DataType<S>,
    {
        let id = self.get_next_req_id();

        self.writer.add_body((
            Out::ReqHeadTimestamp,
            id,
            security,
            None::<()>,
            regular_trading_hours_only,
            data,
            1,
        ))?;
        self.writer.send().await?;
        Ok(id)
    }

    /// Cancel an existing [`Client::req_head_timestamp`] data request.
    ///
    /// # Arguments
    /// * `req_id` - The ID of the [`Client::req_head_timestamp`] request to cancel.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn cancel_head_timestamp(&mut self, req_id: i64) -> ReqResult {
        self.writer.add_body((Out::CancelHeadTimestamp, req_id))?;
        self.writer.send().await
    }

    /// Request a histogram of historical data.
    ///
    /// # Arguments
    /// * `security` - The security for which to request histogram data.
    /// * `regular_trading_hours_only` - When [`true`], only return ticks from regular trading hours.
    /// * `duration` - The duration of data to return.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    ///
    /// # Returns
    /// Returns the unique ID associated with the request.
    pub async fn req_histogram_data<S>(
        &mut self,
        security: &S,
        regular_trading_hours_only: bool,
        duration: histogram::Duration,
    ) -> IdResult
    where
        S: Security,
    {
        let id = self.get_next_req_id();

        self.writer.add_body((
            Out::ReqHistogramData,
            id,
            security,
            None::<()>,
            regular_trading_hours_only,
            duration,
        ))?;
        self.writer.send().await?;
        Ok(id)
    }

    /// Cancel an existing [`histogram`] data request.
    ///
    /// # Arguments
    /// * `req_id` - The ID of the [`histogram`] data request to cancel.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn cancel_histogram_data(&mut self, req_id: i64) -> ReqResult {
        self.writer.add_body((Out::CancelHistogramData, req_id))?;
        self.writer.send().await
    }

    /// Request historical ticks for a given security. See [`historical_ticks`] for
    /// types and traits that are used in this function.
    ///
    /// # Arguments
    /// * `security` - The security for which to request data.
    /// * `timestamp` - The first/last datetime for which data will be returned.
    /// * `number_of_ticks` - The number of ticks to return.
    /// * `data` - The type of data to return (Trades, `BidAsk`, etc.).
    /// * `regular_trading_hours_only` - When [`true`], only return ticks from regular trading hours.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    ///
    /// # Returns
    /// Returns the unique ID associated with the request.
    pub async fn req_historical_ticks<S, D>(
        &mut self,
        security: &S,
        timestamp: historical_ticks::TimeStamp,
        number_of_ticks: historical_ticks::NumberOfTicks,
        data: D,
        regular_trading_hours_only: bool,
    ) -> IdResult
    where
        S: Security,
        D: historical_ticks::data_types::DataType<S>,
    {
        let id = self.get_next_req_id();

        self.writer.add_body((
            Out::ReqHistoricalTicks,
            id,
            security,
            None::<()>,
            timestamp,
            number_of_ticks,
            data,
            regular_trading_hours_only,
            None::<()>,
            None::<()>,
        ))?;
        self.writer.send().await?;
        Ok(id)
    }

    // === Live Market Data ===

    /// Request live data for a given security.
    ///
    /// # Arguments
    /// * `security` - The security for which to request data.
    /// * `data` - The type of data to return (`RealTimeVolume`, `MarkPrice`, etc.).
    /// * `refresh_type` - How often to refresh the data (a one-time snapshot or a continuous
    /// streaming connection)
    /// * `use_regulatory_snapshot` - When set to [`true`], return a NBBO snapshot even if no
    /// appropriate subscription exists for streaming data. Note that doing so will cost 1 cent per
    /// snapshot.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    ///
    /// # Returns
    /// Returns the unique ID associated with the request.
    pub async fn req_market_data<S, D>(
        &mut self,
        security: &S,
        additional_data: Vec<D>,
        refresh_type: live_data::RefreshType,
        use_regulatory_snapshot: bool,
    ) -> IdResult
    where
        S: Security,
        D: live_data::data_types::DataType<S>,
    {
        const VERSION: u8 = 11;
        let id = self.get_next_req_id();

        self.writer.add_body((
            Out::ReqMktData,
            VERSION,
            id,
            security,
            false,
            additional_data,
            refresh_type,
            use_regulatory_snapshot,
            None::<()>,
        ))?;
        self.writer.send().await?;
        Ok(id)
    }

    /// Cancel an open streaming data connection with a given `req_id`.
    ///
    /// # Arguments
    /// * `req_id` - The ID associated with the market data request to cancel.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn cancel_market_data(&mut self, req_id: i64) -> ReqResult {
        const VERSION: u8 = 2;

        self.writer
            .add_body((Out::CancelMktData, VERSION, req_id))?;
        self.writer.send().await
    }

    /// Set the market data variant for all succeeding `Client::req_market_data` requests.
    ///
    /// # Arguments
    /// * `variant` - The variant to set.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn req_market_data_type(&mut self, variant: live_data::Class) -> ReqResult {
        const VERSION: u8 = 1;

        self.writer
            .add_body((Out::ReqMarketDataType, VERSION, variant))?;
        self.writer.send().await
    }

    /// Request real-time, 5 second bars for a given security.
    ///
    /// # Arguments
    /// * `security` - The security for which to request the bars.
    /// * `data` - The type of data to return (trades, bid, ask, midpoint).
    /// * `regular_trading_hours_only` -  When [`true`], only return ticks from regular trading
    /// hours.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    ///
    /// # Returns
    /// Returns the unique ID associated with the request.
    pub async fn req_real_time_bars<S, D>(
        &mut self,
        security: &S,
        data: D,
        regular_trading_hours_only: bool,
    ) -> IdResult
    where
        S: Security,
        D: live_bar::data_types::DataType<S>,
    {
        const VERSION: u8 = 3;
        let id = self.get_next_req_id();

        self.writer.add_body((
            Out::ReqRealTimeBars,
            VERSION,
            id,
            security,
            5_u32,
            data,
            regular_trading_hours_only,
            None::<()>,
        ))?;
        self.writer.send().await?;
        Ok(id)
    }

    /// Cancel an existing real-time bars subscription.
    ///
    /// # Arguments
    /// `req_id` - The ID associated with the bar subscription to cancel.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn cancel_real_time_bars(&mut self, req_id: i64) -> ReqResult {
        const VERSION: u8 = 1;

        self.writer
            .add_body((Out::CancelRealTimeBars, VERSION, req_id))?;
        self.writer.send().await
    }

    // === Live Tick-by-Tick Data ===

    /// Request live tick-by-tick data for a given security.
    ///
    /// # Arguments
    /// * `security` - The security for which to request data.
    /// * `tick_data` - The type of data to return.
    /// * `number_of_historical_ticks` - The number of historical ticks to return before the live
    /// data.
    /// * `ignore_size` - Ignore the size parameter in the returned ticks when set to [`true`].
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    ///
    /// # Returns
    /// Returns the unique ID associated with the request.
    pub async fn req_tick_by_tick_data<S, D>(
        &mut self,
        security: &S,
        tick_data: D,
        number_of_historical_ticks: live_ticks::NumberOfTicks,
        ignore_size: bool,
    ) -> IdResult
    where
        S: Security,
        D: live_ticks::data_types::DataType<S>,
    {
        let id = self.get_next_req_id();

        self.writer.add_body((
            Out::ReqTickByTickData,
            id,
            security,
            tick_data,
            number_of_historical_ticks,
            ignore_size,
        ))?;
        self.writer.send().await?;
        Ok(id)
    }

    /// Cancel an existing tick-by-tick data subscription.
    ///
    /// # Arguments
    /// * `req_id` - The request ID of the subscription to cancel.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn cancel_tick_by_tick_data(&mut self, req_id: i64) -> ReqResult {
        self.writer.add_body((Out::CancelTickByTickData, req_id))?;
        self.writer.send().await
    }

    // === Market Depth ===

    /// Request market depth data for a given security.
    ///
    /// # Arguments
    /// * `security` - The security for which to return the market depth data.
    /// * `number_of_rows` - The maximum number of rows in the returned limit order book.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    ///
    /// # Returns
    /// Returns the unique ID associated with the request.
    pub async fn req_market_depth<S>(&mut self, security: &S, number_of_rows: u32) -> IdResult
    where
        S: Security,
    {
        const VERSION: u8 = 5;
        let id = self.get_next_req_id();

        self.writer.add_body((
            Out::ReqMktDepth,
            VERSION,
            id,
            security,
            number_of_rows,
            true,
            None::<()>,
        ))?;
        self.writer.send().await?;
        Ok(id)
    }

    /// Request exchanges available for market depth.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn req_market_depth_exchanges(&mut self) -> ReqResult {
        self.writer.add_body(Out::ReqMktDepthExchanges)?;
        self.writer.send().await
    }

    /// Cancel a market depth subscription for a given `req_id`.
    ///
    /// # Arguments
    /// * `req_id` - The request ID for which to cancel a market depth subscription.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn cancel_market_depth(&mut self, req_id: i64) -> ReqResult {
        const VERSION: u8 = 1;

        self.writer
            .add_body((Out::CancelMktDepth, VERSION, req_id))?;
        self.writer.send().await
    }

    /// Request exchanges comprising the aggregate SMART exchange
    ///
    /// # Arguments
    /// * `exchange_id` - The identifier containing information about the component exchanges, which
    /// is attained from an initial market data callback.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    ///
    /// # Returns
    /// Returns the unique ID associated with the request.
    pub async fn req_smart_components(&mut self, exchange_id: ExchangeId) -> IdResult {
        let id = self.get_next_req_id();

        self.writer
            .add_body((Out::ReqSmartComponents, id, exchange_id))?;
        self.writer.send().await?;
        Ok(id)
    }

    // === Orders and order management ===

    /// Place an order.
    ///
    /// # Arguments
    /// * `security` - The security on which to place the order.
    /// * `order` - The order to execute.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    ///
    /// # Returns
    /// Returns the unique ID associated with the request.
    pub async fn req_place_order<S, E>(&mut self, order: &Order<S, E>) -> IdResult
    where
        S: Security,
        E: Executable<S>,
    {
        let id = self.get_next_order_id();

        self.writer.add_body((
            Out::PlaceOrder,
            id,
            order.get_security(),
            None::<()>,
            None::<()>,
            order,
        ))?;
        self.writer.send().await?;
        Ok(id)
    }

    /// Modify an order.
    ///
    /// # Arguments
    /// * `security` - The security on which the original order was placed.
    /// * `order` - The original order.
    /// * `id` - The original order's ID.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    ///
    /// # Returns
    /// Returns the unique ID associated with the request.
    pub async fn req_modify_order<S, E>(&mut self, order: &Order<S, E>, id: i64) -> IdResult
    where
        S: Security,
        E: Executable<S>,
    {
        self.writer.add_body((
            Out::PlaceOrder,
            id,
            order.get_security(),
            None::<()>,
            None::<()>,
            order,
        ))?;
        self.writer.send().await?;
        Ok(id)
    }

    /// Cancel an order.
    ///
    /// # Arguments
    /// * `id` - The ID of the order to cancel.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn cancel_order(&mut self, id: i64) -> ReqResult {
        const VERSION: u8 = 1;

        self.writer
            .add_body((Out::CancelOrder, VERSION, id, None::<()>))?;
        self.writer.send().await
    }

    /// Cancel all currently open orders, including those placed in TWS.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn cancel_all_orders(&mut self) -> ReqResult {
        const VERSION: u8 = 1;

        self.writer.add_body((Out::ReqGlobalCancel, VERSION))?;
        self.writer.send().await
    }

    /// Request all the open orders placed from all API clients and from TWS.
    ///
    /// Note that this will request all of the orders associated with a given IBKR account and
    /// therefore will contain orders placed by another [`Client`].
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn req_all_open_orders(&mut self) -> ReqResult {
        const VERSION: u8 = 1;

        self.writer.add_body((Out::ReqAllOpenOrders, VERSION))?;
        self.writer.send().await
    }

    /// Request that all newly created TWS orders will be implicitly associated with the calling
    /// client. Therefore, the API will receive updates about TWS orders.
    ///
    /// Note! This can only be called from a client with ID 0.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message. Also returns an error if
    /// the calling client does not have ID 0.
    pub async fn req_auto_open_orders(&mut self) -> ReqResult {
        const VERSION: u8 = 1;

        self.writer
            .add_body((Out::ReqAutoOpenOrders, VERSION, true))?;
        self.writer.send().await
    }

    /// Request the open orders that were placed from the calling client.
    ///
    /// A Note that a client with an ID of 0 will also receive updates about orders placed with TWS.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn req_open_orders(&mut self) -> ReqResult {
        const VERSION: u8 = 1;

        self.writer.add_body((Out::ReqOpenOrders, VERSION))?;
        self.writer.send().await
    }

    // === Executions ===

    /// Request execution all execution reports that fit the criteria specified in the `filter`.
    ///
    /// In order to view executions beyond the past 24 hours, open the Trade Log in TWS and, while
    /// the Trade Log is displayed, request the executions again from the API.
    ///
    /// # Arguments
    /// `filter` - The conditions with which to determine whether an execution will be returned.
    ///
    /// # Errors
    /// Returns any error encountered while writing the outgoing message.
    pub async fn req_executions(&mut self, filter: Filter) -> IdResult {
        const VERSION: u8 = 3;
        let req_id = self.get_next_req_id();

        self.writer
            .add_body((Out::ReqExecutions, VERSION, req_id, filter))?;
        self.writer.send().await?;
        Ok(req_id)
    }

    // === Contract Creation ===

    #[inline]
    pub(crate) async fn send_contract_query(
        &mut self,
        contract_id: ContractId,
    ) -> anyhow::Result<()> {
        const VERSION: u8 = 8;
        let req_id = self.get_next_req_id();
        self.status
            .tx
            .send(ToWrapper::ContractQuery((contract_id, req_id)))
            .await?;

        self.writer.add_body((
            Out::ReqContractData,
            VERSION,
            req_id,
            contract_id,
            [None::<()>; 15],
        ))?;
        self.writer.send().await?;
        Ok(())
    }

    #[inline]
    pub(crate) async fn recv_contract_query(
        &mut self,
    ) -> anyhow::Result<crate::contract::Contract> {
        match self
            .status
            .rx
            .recv()
            .await
            .ok_or_else(|| anyhow::Error::msg("Failed to receive contract object"))?
        {
            ToClient::NewContract(c) => Ok(c),
        }
    }

    // === Disconnect ==

    #[inline]
    /// Terminate the connection with the IBKR trading systems and return a [`Builder`] that can
    /// be used to reconnect if necessary.
    ///
    /// # Errors
    /// Returns any error encountered while flushing and shutting down the outgoing buffer.
    ///
    /// # Returns
    /// Returns a [`Builder`] with the same port and address as the existing client.
    pub async fn disconnect(mut self) -> Result<Builder, std::io::Error> {
        self.writer.flush().await?;
        self.writer.shutdown().await?;
        self.status.disconnect.cancel();
        self.status.r_thread.await?;
        Ok(Builder(Inner::Manual {
            port: self.port,
            address: self.address,
        }))
    }
}

#[inline]
fn check_valid_account(
    client: &Client<indicators::Active>,
    account_number: &str,
) -> Result<(), std::io::Error> {
    if client.status.managed_accounts.contains(account_number) {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Invalid account number provided to req_account_updates",
        ))
    }
}
