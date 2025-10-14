//! Network interface: `OtNetif` - a `Netif` trait implementation for `openthread`
//! mDNS impl: `OtMdns` - an mDNS trait implementation for `openthread` using Thread SRP

use core::fmt::Write;
use core::future::poll_fn;

use embassy_futures::select::select;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::{Duration, Instant, Timer};

use openthread::{
    Channels, OpenThread, OtError, OtResources, OtSrpResources, OtUdpResources, RamSettings,
    RamSettingsChange, SettingsKey, SharedRamSettings, SrpConf, SrpService,
};
use rs_matter_stack::matter::transport::network::mdns::Service;
use rs_matter_stack::matter::Matter;
use rs_matter_stack::mdns::Mdns;
use rs_matter_stack::nal::noop::NoopNet;
use rs_matter_stack::nal::{NetStack, UdpBind};

use crate::fmt::Bytes;

use crate::matter::dm::clusters::gen_diag::{InterfaceTypeEnum, NetifDiag, NetifInfo};
use crate::matter::dm::clusters::net_comm::{
    NetCtl, NetCtlError, NetworkScanInfo, NetworkType, WirelessCreds,
};
use crate::matter::dm::clusters::thread_diag::{
    NeighborTable, NetworkFaultEnum, OperationalDatasetComponents, RouteTable, RoutingRoleEnum,
    SecurityPolicy, ThreadDiag,
};
use crate::matter::dm::clusters::wifi_diag::WirelessDiag;
use crate::matter::dm::networks::NetChangeNotif;
use crate::matter::error::Error;
use crate::matter::error::ErrorCode;
use crate::matter::transport::network::MAX_RX_PACKET_SIZE;
use crate::matter::utils::init::zeroed;
use crate::matter::utils::init::{init, Init};
use crate::matter::utils::storage::Vec;
use crate::stack::persist::{KvBlobStore, SharedKvBlobStore, VENDOR_KEYS_START};

/// Re-export the `openthread` crate
pub mod openthread {
    pub use openthread::*;
}

/// The maximum number of sockets that the Matter stack would use:
/// - One, for the UDP socket used by the Matter protocol
// TODO: Make it configurable with a feature
const OT_MAX_UDP_SOCKETS: usize = 1;

const OT_MAX_SRP_RECORDS: usize = 4;
const OT_SRP_BUF_SZ: usize = 512;

const OT_SETTINGS_BUF_SZ: usize = 1024;

/// A struct that holds all the resources required by the OpenThread stack,
/// as used by Matter.
pub struct OtMatterResources {
    /// The OpenThread main resources
    pub ot: OtResources,
    /// The OpenThread UDP resources
    pub udp: OtUdpResources<OT_MAX_UDP_SOCKETS, MAX_RX_PACKET_SIZE>,
    /// The OpenThread SRP resources
    pub srp: OtSrpResources<OT_MAX_SRP_RECORDS, OT_SRP_BUF_SZ>,
    /// The OpenThread `RamSettings` buffer
    pub settings_buf: [u8; OT_SETTINGS_BUF_SZ],
}

impl OtMatterResources {
    /// Create a new `OtMatterResources` instance
    pub const fn new() -> Self {
        Self {
            ot: OtResources::new(),
            udp: OtUdpResources::new(),
            srp: OtSrpResources::new(),
            settings_buf: [0; OT_SETTINGS_BUF_SZ],
        }
    }

    /// Return an in-place initializer for `OtMatterResources`
    pub fn init() -> impl Init<Self> {
        init!(Self {
            ot: OtResources::new(),
            udp: OtUdpResources::new(),
            srp: OtSrpResources::new(),
            settings_buf <- zeroed(),
        })
    }
}

impl Default for OtMatterResources {
    fn default() -> Self {
        Self::new()
    }
}

/// An implementation of `NetStack` for `openthread`
pub struct OtNetStack<'d>(OpenThread<'d>);

impl<'d> OtNetStack<'d> {
    /// Create a new `OtNetStack` instance
    pub const fn new(ot: OpenThread<'d>) -> Self {
        Self(ot)
    }
}

impl<'d> NetStack for OtNetStack<'d> {
    type UdpBind<'t>
        = &'t OpenThread<'d>
    where
        Self: 't;

    type UdpConnect<'t>
        = NoopNet
    where
        Self: 't;

    type TcpBind<'t>
        = NoopNet
    where
        Self: 't;

    type TcpConnect<'t>
        = NoopNet
    where
        Self: 't;

    type Dns<'t>
        = NoopNet
    where
        Self: 't;

    fn udp_bind(&self) -> Option<Self::UdpBind<'_>> {
        Some(&self.0)
    }

    fn udp_connect(&self) -> Option<Self::UdpConnect<'_>> {
        None
    }

    fn tcp_bind(&self) -> Option<Self::TcpBind<'_>> {
        None
    }

    fn tcp_connect(&self) -> Option<Self::TcpConnect<'_>> {
        None
    }

    fn dns(&self) -> Option<Self::Dns<'_>> {
        None
    }
}

/// A `NetifDiag` anf `NetChangeNotif` traits implementation for `openthread`
pub struct OtNetif<'d>(OpenThread<'d>);

impl<'d> OtNetif<'d> {
    /// Create a new `OtNetif` instance
    pub const fn new(ot: OpenThread<'d>) -> Self {
        Self(ot)
    }
}

impl NetifDiag for OtNetif<'_> {
    fn netifs(&self, f: &mut dyn FnMut(&NetifInfo) -> Result<(), Error>) -> Result<(), Error> {
        let status = self.0.net_status();

        let mut addrs = Vec::<_, 6>::new();

        let _ = self.0.ipv6_addrs(|addr| {
            if let Some((addr, _)) = addr {
                if addrs.len() < addrs.capacity() {
                    unwrap!(addrs.push(addr));
                }
            }

            Ok(())
        }); // TODO

        f(&NetifInfo {
            name: "ot",
            operational: status.ip6_enabled && status.role.is_connected(),
            offprem_svc_reachable_ipv4: None,
            offprem_svc_reachable_ipv6: None,
            hw_addr: &self.0.ieee_eui64(),
            ipv4_addrs: &[],
            ipv6_addrs: &addrs,
            netif_type: InterfaceTypeEnum::Thread,
            netif_index: 0, // Not used with OT
        })?;

        Ok(())
    }
}

impl NetChangeNotif for OtNetif<'_> {
    async fn wait_changed(&self) {
        self.0.wait_changed().await
    }
}

/// A `NetCtl`, `NetChangeNotif`, `WirelessDiag` and `ThreadDiag` traits implementation for `openthread`
pub struct OtNetCtl<'a>(OpenThread<'a>);

impl<'a> OtNetCtl<'a> {
    /// Create a new instance of the `OtNetCtl` type.
    pub fn new(ot: OpenThread<'a>) -> Self {
        Self(ot)
    }
}

impl NetCtl for OtNetCtl<'_> {
    fn net_type(&self) -> NetworkType {
        NetworkType::Thread
    }

    async fn scan<F>(&self, network: Option<&[u8]>, mut f: F) -> Result<(), NetCtlError>
    where
        F: FnMut(&NetworkScanInfo) -> Result<(), Error>,
    {
        const SCAN_DURATION_MILLIS: u16 = 500;

        info!("Thread scan request");

        //let _ = self.0.enable_thread(true);

        self.0
            .scan(Channels::all(), SCAN_DURATION_MILLIS, |scan_result| {
                let Some(scan_result) = scan_result else {
                    debug!("Thread scan reported as complete");
                    return;
                };

                if network
                    .map(|id| id == scan_result.extended_pan_id.to_be_bytes())
                    .unwrap_or(true)
                {
                    let info = NetworkScanInfo::Thread {
                        pan_id: scan_result.pan_id,
                        ext_pan_id: scan_result.extended_pan_id,
                        network_name: scan_result.network_name,
                        channel: scan_result.channel as _,
                        version: scan_result.version,
                        ext_addr: &scan_result.ext_address.to_be_bytes(),
                        rssi: scan_result.rssi,
                        lqi: scan_result.lqi,
                    };

                    info!(
                        "Found Thread network: {:?}",
                        Bytes(&scan_result.extended_pan_id.to_be_bytes())
                    );

                    let _ = f(&info);
                }
            })
            .await
            .map_err(to_net_matter_err)?;

        info!("Thread scan complete");

        Ok(())
    }

    async fn connect(&self, creds: &WirelessCreds<'_>) -> Result<(), NetCtlError> {
        let WirelessCreds::Thread { dataset_tlv } = creds else {
            return Err(NetCtlError::Other(ErrorCode::InvalidAction.into()));
        };

        const TIMEOUT_SECS: u64 = 20;

        let _ = self.0.enable_thread(false);

        // NOTE: Printing the dataset is a security issue, but we do it for now for debugging purposes
        // (i.e. for running some of the pseudo-eth examples the user needs the Thread network dataset)
        info!(
            "Connecting to Thread network, dataset: {:x}",
            Bytes(dataset_tlv)
        );

        self.0
            .set_active_dataset_tlv(dataset_tlv)
            .map_err(to_net_matter_err)?;

        self.0.enable_thread(true).map_err(to_net_matter_err)?;

        let now = Instant::now();

        while !self.0.net_status().role.is_connected() {
            if now.elapsed().as_secs() > TIMEOUT_SECS {
                let _ = self.0.enable_thread(false);

                return Err(NetCtlError::OtherConnectionFailure);
            }

            select(self.0.wait_changed(), Timer::after(Duration::from_secs(1))).await;
        }

        Ok(())
    }
}

impl NetChangeNotif for OtNetCtl<'_> {
    async fn wait_changed(&self) {
        self.0.wait_changed().await
    }
}

impl WirelessDiag for OtNetCtl<'_> {
    fn connected(&self) -> Result<bool, Error> {
        let status = self.0.net_status();

        Ok(status.role.is_connected())
    }
}

// TODO
impl ThreadDiag for OtNetCtl<'_> {
    fn channel(&self) -> Result<Option<u16>, Error> {
        Ok(None)
    }

    fn routing_role(&self) -> Result<Option<RoutingRoleEnum>, Error> {
        Ok(None)
    }

    fn network_name(
        &self,
        f: &mut dyn FnMut(Option<&str>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        f(None)
    }

    fn pan_id(&self) -> Result<Option<u16>, Error> {
        Ok(None)
    }

    fn extended_pan_id(&self) -> Result<Option<u64>, Error> {
        let status = self.0.net_status();

        Ok(status.ext_pan_id)
    }

    fn mesh_local_prefix(
        &self,
        f: &mut dyn FnMut(Option<&[u8]>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        f(None)
    }

    fn neightbor_table(
        &self,
        _f: &mut dyn FnMut(&NeighborTable) -> Result<(), Error>,
    ) -> Result<(), Error> {
        Ok(())
    }

    fn route_table(
        &self,
        _f: &mut dyn FnMut(&RouteTable) -> Result<(), Error>,
    ) -> Result<(), Error> {
        Ok(())
    }

    fn partition_id(&self) -> Result<Option<u32>, Error> {
        Ok(None)
    }

    fn weighting(&self) -> Result<Option<u16>, Error> {
        Ok(None)
    }

    fn data_version(&self) -> Result<Option<u16>, Error> {
        Ok(None)
    }

    fn stable_data_version(&self) -> Result<Option<u16>, Error> {
        Ok(None)
    }

    fn leader_router_id(&self) -> Result<Option<u8>, Error> {
        Ok(None)
    }

    fn security_policy(&self) -> Result<Option<SecurityPolicy>, Error> {
        Ok(None)
    }

    fn channel_page0_mask(
        &self,
        f: &mut dyn FnMut(Option<&[u8]>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        f(None)
    }

    fn operational_dataset_components(
        &self,
        f: &mut dyn FnMut(Option<&OperationalDatasetComponents>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        f(None)
    }

    fn active_network_faults_list(
        &self,
        _f: &mut dyn FnMut(NetworkFaultEnum) -> Result<(), Error>,
    ) -> Result<(), Error> {
        Ok(())
    }
}

/// An mDNS trait implementation for `openthread` using Thread SRP
pub struct OtMdns<'d> {
    ot: OpenThread<'d>,
}

impl<'d> OtMdns<'d> {
    /// Create a new `OtMdns` instance
    pub const fn new(ot: OpenThread<'d>) -> Self {
        Self { ot }
    }

    /// Run the `OtMdns` instance by listening to the mDNS services and registering them with the SRP server
    pub async fn run(&self, matter: &Matter<'_>) -> Result<(), OtError> {
        loop {
            // TODO: Not very efficient to remove and re-add everything

            let ieee_eui64 = self.ot.ieee_eui64();

            let mut hostname = heapless::String::<16>::new();
            unwrap!(
                write!(
                    hostname,
                    "{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
                    ieee_eui64[0],
                    ieee_eui64[1],
                    ieee_eui64[2],
                    ieee_eui64[3],
                    ieee_eui64[4],
                    ieee_eui64[5],
                    ieee_eui64[6],
                    ieee_eui64[7]
                ),
                "Unreachable"
            );

            // If the device was restarted, this call will make sure that
            // the SRP records are removed from the SRP server
            let _ = self.ot.srp_set_conf(&SrpConf {
                host_name: hostname.as_str(),
                ..Default::default()
            });

            let _ = self.ot.srp_remove_all(false);

            // TODO: Something is still not quite right with the SRP
            // We seem to get stuck here
            while !self.ot.srp_is_empty()? {
                debug!("Waiting for SRP records to be removed...");
                select(
                    Timer::after(Duration::from_secs(1)),
                    self.ot.srp_wait_changed(),
                )
                .await;
            }

            self.ot.srp_set_conf(&SrpConf {
                host_name: hostname.as_str(),
                ..Default::default()
            })?;

            info!("Registered SRP host {}", hostname);

            unwrap!(matter.mdns_services(|matter_service| {
                Service::call_with(
                    &matter_service,
                    matter.dev_det(),
                    matter.port(),
                    |service| {
                        unwrap!(self.ot.srp_add_service(&SrpService {
                            name: service.service_protocol,
                            instance_name: service.name,
                            port: service.port,
                            subtype_labels: service.service_subtypes.iter().cloned(),
                            txt_entries: service
                                .txt_kvs
                                .iter()
                                .cloned()
                                .filter(|(k, _)| !k.is_empty())
                                .map(|(k, v)| (k, v.as_bytes())),
                            priority: 0,
                            weight: 0,
                            lease_secs: 0,
                            key_lease_secs: 0,
                        })); // TODO

                        info!("Added service {:?}", matter_service);

                        Ok(())
                    },
                )
            }));

            matter.wait_mdns().await;
        }
    }
}

impl Mdns for OtMdns<'_> {
    async fn run<U>(
        &mut self,
        matter: &Matter<'_>,
        _udp: U,
        _mac: &[u8],
        _ipv4: core::net::Ipv4Addr,
        _ipv6: core::net::Ipv6Addr,
        _interface: u32,
    ) -> Result<(), Error>
    where
        U: UdpBind,
    {
        OtMdns::run(self, matter).await.map_err(to_matter_err)
    }
}

/// The `KvBlobStore` key used to persist OpenThread's SRP ECDSA key
///
/// While persisting other keys is optional, this one _must_ get persisted,
/// or else - upon device restart - it will fail to re-register its SRP services
/// in the SRP server.
const OT_SRP_ECDSA_KEY: u16 = VENDOR_KEYS_START;

/// A struct for implementing persistance of `openthread` settings - volatitle and
/// non-volatile (for selected keys)
pub struct OtPersist<'a, 'd, S> {
    settings: SharedRamSettings<'d, NoopRawMutex, fn(RamSettingsChange) -> bool>,
    store: &'a SharedKvBlobStore<'a, S>,
}

impl<'a, 'd, S> OtPersist<'a, 'd, S>
where
    S: KvBlobStore,
{
    /// Create a new `OtPersist` instance
    ///
    /// # Arguments
    /// - `settings_buf`: A mutable reference to a buffer for storing `openthread` settings before they are persisted
    /// - `store`: A reference to the `KvBlobStore` instance used for persisting a subset of the settings to non-volatile storage
    pub const fn new(settings_buf: &'d mut [u8], store: &'a SharedKvBlobStore<'a, S>) -> Self {
        Self {
            settings: SharedRamSettings::new(RamSettings::new_with_signal_change(
                settings_buf,
                |change| match change {
                    RamSettingsChange::Added { key, .. }
                    | RamSettingsChange::Removed { key, .. }
                        if key == SettingsKey::SrpEcdsaKey as u16 =>
                    {
                        true
                    }
                    RamSettingsChange::Clear => true,
                    _ => false,
                },
            )),
            store,
        }
    }

    /// Return a reference to the `SharedRamSettings` instance to be used with `openthread`
    pub const fn settings(
        &self,
    ) -> &SharedRamSettings<'d, NoopRawMutex, fn(RamSettingsChange) -> bool> {
        &self.settings
    }

    /// Load (a selected subset of) the settings from the `KvBlobStore` non-volatile storage
    pub async fn load(&self) -> Result<(), Error> {
        let (mut kv, mut buf) = self.store.get().await;

        kv.load(OT_SRP_ECDSA_KEY, &mut buf, |data| {
            if let Some(data) = data {
                self.settings.with(|settings| {
                    let mut offset = 0;

                    while offset < data.len() {
                        let key = u16::from_le_bytes([data[offset], data[offset + 1]]);
                        offset += 2;

                        let value = &data[offset..];

                        unwrap!(settings.add(key, value));

                        offset += value.len();
                    }
                })
            }

            Ok(())
        })
        .await
    }

    /// Store (a selected subset of) the settings to the `KvBlobStore` non-volatile storage
    pub async fn store(&self) -> Result<(), Error> {
        let (mut kv, mut buf) = self.store.get().await;

        kv.store(OT_SRP_ECDSA_KEY, &mut buf, |buf| {
            self.settings.with(|settings| {
                let mut offset = 0;

                for (key, value) in settings
                    .iter()
                    .filter(|(key, _)| *key == SettingsKey::SrpEcdsaKey as u16)
                {
                    assert!(value.len() + 2 <= buf.len() - offset);

                    buf[offset..offset + 2].copy_from_slice(&key.to_le_bytes());
                    offset += 2;

                    buf[offset..offset + value.len()].copy_from_slice(value);
                    offset += value.len();
                }

                Ok(offset)
            })
        })
        .await
    }

    /// Run the `OtPersist` instance by waiting for changes in the settings and persisting them
    /// to non-volatile storage
    pub async fn run(&self) -> Result<(), Error> {
        let wait_changed = || poll_fn(|cx| self.settings.poll_changed(cx));

        loop {
            wait_changed().await;

            self.store().await?;
        }
    }
}

pub(crate) fn to_net_matter_err(err: OtError) -> NetCtlError {
    NetCtlError::Other(to_matter_err(err))
}

pub(crate) fn to_matter_err(err: OtError) -> Error {
    error!("OpenThread error: {:?}", err);

    ErrorCode::NoNetworkInterface.into()
}
