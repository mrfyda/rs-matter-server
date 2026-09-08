//! Finding a device over Bluetooth, and pumping BTP to it.
//!
//! rs-matter splits BLE into two halves and so does this module. `Btp` is the
//! transport-side state machine: it is chained into the Matter transport next
//! to UDP, so a Bluetooth peer is just another `Address::Btp` and the
//! commissioning flow above it does not change. `run_central` is the OS half —
//! it holds the GATT connection to one device and moves bytes between the
//! adapter and `Btp`, so it runs only while a commissioning is in flight.
//!
//! BlueZ is reached over the system D-Bus bus, which is why this is Linux
//! only. rs-matter has no CoreBluetooth backend, and a plain CLI binary on
//! macOS could not use one without an app bundle and entitlements.

// `btp::gatt` is a private module, but `btp` re-exports its public children,
// so the backend lives at `btp::bluez` rather than `btp::gatt::bluez`.
use rs_matter::transport::network::btp::bluez;
use rs_matter::transport::network::btp::Btp;
use rs_matter::transport::network::mdns::CommissionableFilter;
use rs_matter::transport::network::BtAddr;
use rs_matter::utils::zbus::fdo::ObjectManagerProxy;
use rs_matter::utils::zbus::Connection;

use crate::protocol::error::ApiError;

/// The BlueZ adapter that commissioning runs over.
pub struct BleAdapter {
    /// rs-matter re-exports zbus, so this is *its* `Connection` type. A second
    /// copy of the crate in the tree would produce a type the backend will not
    /// accept, which is why zbus is not a direct dependency here.
    connection: Connection,
    /// `None` lets BlueZ pick the first powered adapter, which is what a
    /// single-radio host wants; a name pins it when there is more than one.
    name: Option<String>,
}

impl BleAdapter {
    /// Connect to the system bus.
    ///
    /// Failing here means BlueZ is unreachable — no `bluetoothd`, or no access
    /// to the bus socket — which is a startup diagnosis rather than a
    /// per-command error.
    pub async fn open(name: Option<String>) -> Result<Self, ApiError> {
        let connection = Connection::system().await.map_err(|error| {
            ApiError::sdk(format!(
                "Cannot reach BlueZ on the system D-Bus bus: {error}. Bluetooth \
                 commissioning needs bluetoothd running and the bus socket readable."
            ))
        })?;

        Ok(Self { connection, name })
    }

    /// Whether BlueZ is actually offering an adapter.
    ///
    /// `server_info.bluetooth_enabled` is what a client reads to decide
    /// whether to offer Bluetooth commissioning at all, so answering yes on a
    /// host with no radio would be a lie that only surfaces later as a
    /// confusing commissioning failure. Reaching the bus is not enough —
    /// `bluetoothd` can be running with no adapter attached — so this asks it
    /// for one.
    pub async fn adapter_present(&self) -> bool {
        const BLUEZ: &str = "org.bluez";
        const ADAPTER: &str = "org.bluez.Adapter1";

        let Ok(manager) = ObjectManagerProxy::new(&self.connection, BLUEZ, "/").await else {
            return false;
        };
        let Ok(objects) = manager.get_managed_objects().await else {
            return false;
        };

        objects.values().any(|interfaces| {
            interfaces
                .keys()
                .any(|interface| interface.as_str() == ADAPTER)
        })
    }

    /// Scan for the first commissionable device matching `filter`.
    ///
    /// The filter is the one the pairing code already produced, so a QR code
    /// matches on the full discriminator and a manual code on its top 4 bits,
    /// exactly as mDNS discovery does.
    pub async fn scan(
        &self,
        filter: &CommissionableFilter,
        timeout_secs: u16,
    ) -> Result<BtAddr, ApiError> {
        // `on_found` returning `Some` stops the scan, so the first match wins.
        bluez::scan(
            &self.connection,
            self.name.as_deref(),
            filter,
            Some(timeout_secs),
            |addr, _adv| Some(addr),
        )
        .await
        .map_err(|error| {
            ApiError::commission_failed(format!(
                "No commissionable device was found over Bluetooth within {timeout_secs}s \
                 ({:?}). A factory-fresh device advertises for about 15 minutes after a \
                 reset.",
                error.code()
            ))
        })
    }

    /// Hold the GATT connection to `addr` and pump BTP over it.
    ///
    /// Runs until the peer disconnects, so the caller races it against the
    /// commissioning flow rather than awaiting it: the pump ending first is a
    /// failure, the flow ending first is success and drops the pump.
    pub async fn pump(&self, addr: BtAddr, btp: &Btp) -> Result<(), ApiError> {
        bluez::run_central(&self.connection, self.name.as_deref(), addr, btp)
            .await
            .map_err(|error| {
                ApiError::commission_failed(format!(
                    "The Bluetooth connection to {addr} failed: {:?}",
                    error.code()
                ))
            })
    }
}
