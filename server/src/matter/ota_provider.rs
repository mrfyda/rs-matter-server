//! Serving firmware images to devices.
//!
//! `check_node_update` finds updates and `POST /ota-upload/<id>` stores them,
//! but a device does not accept an image; it *fetches* one. The Matter flow is
//! that the controller tells the device where a provider is
//! (`AnnounceOTAProvider`), the device asks that provider what it has
//! (`QueryImage` on the OTA Software Update Provider cluster), downloads it
//! over BDX, and applies it on its own schedule. Every step after the first is
//! the device talking to a node it expects to be there.
//!
//! So this server becomes that node. rs-matter supplies both halves —
//! [`OtaProviderHandler`] answers the cluster commands and [`OtaBdxHandler`]
//! streams the bytes — over two traits this module implements against the
//! images `api::ota` already holds:
//!
//! * [`OtaImagesRegistry`] decides what to offer a device that asks.
//! * [`OtaImages`] hands over the bytes when it downloads.
//!
//! **The file designator is the lookup key.** A `QueryImage` answer carries a
//! `bdx://` URI naming a file, and the device asks for that name back when it
//! downloads. Rather than keep per-transfer state, the name *is* the query:
//! `<vendor>-<product>-<version>.ota` says exactly which stored image it means,
//! so a download that arrives after a restart either finds the image or does
//! not, instead of finding a dangling handle.

use std::num::NonZeroU8;
use std::sync::Arc;

use rs_matter::acl::{AclEntry, AuthMode, Target};
use rs_matter::dm::Privilege;
use rs_matter::dm::clusters::ota_prov::{
    OtaImageMeta, OtaImages, OtaImagesRegistry, OtaQueryOutcome,
};
use rs_matter::error::{Error, ErrorCode};
use rs_matter::Matter;

use crate::api::ServerContext;

/// The endpoint this server hosts the OTA Provider cluster on.
///
/// Zero, as the reference OTA provider application does: a controller has no
/// other endpoints to distinguish it from, and it is the endpoint announced to
/// the device.
pub const OTA_PROVIDER_ENDPOINT: u16 = 0;

/// The OTA Software Update Provider cluster id.
pub const OTA_PROVIDER_CLUSTER: u32 = 0x0029;

/// The update token handed to the device and echoed back when it applies.
/// Eight bytes is the Matter minimum, and a version is exactly that.
const UPDATE_TOKEN_LEN: usize = 8;

/// Name the image a device would download for `(vendor, product, version)`.
fn designator(vendor_id: u16, product_id: u16, version: u64) -> String {
    format!("{}-{}-{}.ota", vendor_id, product_id, version)
}

/// Read a designator back into the image it names.
fn parse_designator(designator: &[u8]) -> Option<(u16, u16, u64)> {
    let text = std::str::from_utf8(designator).ok()?.strip_suffix(".ota")?;
    let mut parts = text.split('-');
    let vendor_id = parts.next()?.parse().ok()?;
    let product_id = parts.next()?.parse().ok()?;
    let version = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((vendor_id, product_id, version))
}

/// Let one node invoke on this server's OTA Provider cluster.
///
/// A device downloads an update by talking *to* the controller, and an
/// incoming invoke is access-controlled like any other: with an empty access
/// list, a node this server commissioned itself is still a stranger to it.
/// This grants the narrowest thing that works — one node, `Operate`, on the
/// provider cluster alone — rather than opening the node up.
///
/// Adding the same grant twice is not an error; it is checked for first, since
/// the list is small and finite.
pub fn grant_ota_access(
    matter: &Matter<'_>,
    fabric_index: NonZeroU8,
    node_id: u64,
) -> Result<bool, Error> {
    matter.with_state(|state| {
        let Some(fabric) = state.fabrics.get_mut(fabric_index) else {
            return Err(ErrorCode::NotFound.into());
        };

        if fabric.acl_iter().any(|entry| grants_ota_access(entry, node_id)) {
            return Ok(false);
        }

        let mut entry = AclEntry::new(Some(fabric_index), Privilege::OPERATE, AuthMode::Case);
        entry.add_subject(node_id)?;
        entry.add_target(Target::new(
            Some(OTA_PROVIDER_ENDPOINT),
            Some(OTA_PROVIDER_CLUSTER),
            None,
        ))?;
        fabric.acl_add(entry)?;
        Ok(true)
    })
}

/// Whether an entry is one of the grants above, for this node.
///
/// Matched on subject and target alone: rs-matter exposes no getter for an
/// entry's privilege, and the only writer of this list is the function above,
/// which always writes `Operate`.
fn grants_ota_access(entry: &AclEntry, node_id: u64) -> bool {
    let names_node = entry
        .subjects()
        .as_opt_ref()
        .is_some_and(|subjects| subjects.contains(&node_id));
    let names_cluster = entry.targets().as_opt_ref().is_some_and(|targets| {
        targets.iter().any(|target| {
            target.cluster == Some(OTA_PROVIDER_CLUSTER)
                && matches!(target.endpoint, None | Some(OTA_PROVIDER_ENDPOINT))
        })
    });
    names_node && names_cluster && entry.auth_mode() == AuthMode::Case
}

/// The images this server holds, as the OTA Provider cluster sees them.
pub struct ImageStore {
    context: Arc<ServerContext>,
}

impl ImageStore {
    pub fn new(context: Arc<ServerContext>) -> Self {
        Self { context }
    }
}

impl OtaImagesRegistry for ImageStore {
    async fn query<'b>(
        &self,
        vendor_id: u16,
        product_id: u16,
        current_version: u32,
        _requestor_can_consent: bool,
        designator_buf: &'b mut [u8],
    ) -> OtaQueryOutcome<'b> {
        let Some(image) =
            self.context
                .ota
                .best_match(vendor_id, product_id, current_version as u64)
        else {
            return OtaQueryOutcome::NotAvailable;
        };
        let size = self
            .context
            .ota
            .image_size(vendor_id, product_id, image.software_version);

        let name = designator(vendor_id, product_id, image.software_version);
        let name = name.as_bytes();
        if designator_buf.len() < name.len() + UPDATE_TOKEN_LEN {
            log::warn!("No room to name the image offered to {}/{}", vendor_id, product_id);
            return OtaQueryOutcome::NotAvailable;
        }

        // The designator and the token share the caller's buffer because both
        // borrow from it for as long as the answer lives.
        designator_buf[..name.len()].copy_from_slice(name);
        designator_buf[name.len()..name.len() + UPDATE_TOKEN_LEN]
            .copy_from_slice(&image.software_version.to_be_bytes());
        let written: &'b [u8] = &designator_buf[..name.len() + UPDATE_TOKEN_LEN];
        let (name, update_token) = written.split_at(name.len());

        OtaQueryOutcome::Available(OtaImageMeta {
            version: image.software_version as u32,
            // Written from a `String` above, so it is valid UTF-8.
            file_designator: std::str::from_utf8(name).unwrap_or_default(),
            update_token,
            size,
            // Consent is the client's business: a user asked for this update
            // through `update_node` before the device was ever told about it.
            user_consent_needed: false,
        })
    }
}

impl OtaImages for ImageStore {
    async fn size(&self, file_designator: &[u8]) -> Option<u64> {
        let (vendor_id, product_id, version) = parse_designator(file_designator)?;
        self.context.ota.image_size(vendor_id, product_id, version)
    }

    async fn read(
        &self,
        file_designator: &[u8],
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, Error> {
        let Some((vendor_id, product_id, version)) = parse_designator(file_designator) else {
            return Err(ErrorCode::NotFound.into());
        };
        self.context
            .ota
            .read_image(vendor_id, product_id, version, offset, buf)
            .ok_or_else(|| ErrorCode::NotFound.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_designator_names_one_stored_image() {
        assert_eq!(designator(5264, 1, 16908353), "5264-1-16908353.ota");
        assert_eq!(
            parse_designator(b"5264-1-16908353.ota"),
            Some((5264, 1, 16908353))
        );
    }

    /// The designator arrives from the device, so it is untrusted input.
    #[test]
    fn a_designator_that_names_nothing_is_refused() {
        assert_eq!(parse_designator(b""), None);
        assert_eq!(parse_designator(b"5264-1-16908353"), None);
        assert_eq!(parse_designator(b"5264-1.ota"), None);
        assert_eq!(parse_designator(b"5264-1-2-3.ota"), None);
        assert_eq!(parse_designator(b"../../etc/passwd.ota"), None);
        assert_eq!(parse_designator(b"99999999-1-1.ota"), None);
        assert_eq!(parse_designator(&[0xFF, 0xFE]), None);
    }

    #[test]
    fn a_grant_is_added_once_and_recognised_afterwards() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let matter = crate::matter::controller::init_matter(
            tmp.path().to_str().unwrap(),
            &crate::matter::controller::FabricConfig::default(),
        )
        .expect("a fabric");
        let fabric_index = std::num::NonZeroU8::new(1).unwrap();

        // A fresh fabric already carries the controller's own admin entry, so
        // what matters is what this adds to it.
        let before = matter.with_state(|state| state.fabrics.get(fabric_index).unwrap().acl_iter().count());

        assert!(grant_ota_access(&matter, fabric_index, 42).unwrap());
        // Asking again changes nothing.
        assert!(!grant_ota_access(&matter, fabric_index, 42).unwrap());
        // A different node needs its own grant.
        assert!(grant_ota_access(&matter, fabric_index, 43).unwrap());

        matter.with_state(|state| {
            let fabric = state.fabrics.get(fabric_index).unwrap();
            assert_eq!(fabric.acl_iter().count(), before + 2);
            assert_eq!(
                fabric.acl_iter().filter(|e| grants_ota_access(e, 42)).count(),
                1
            );
            assert_eq!(
                fabric.acl_iter().filter(|e| grants_ota_access(e, 43)).count(),
                1
            );
        });
    }

    /// A grant for one node must not be read as a grant for another, and a
    /// grant for a different cluster is not one of ours.
    #[test]
    fn a_grant_is_recognised_by_node_and_cluster() {
        let mut entry = AclEntry::new(
            std::num::NonZeroU8::new(1),
            Privilege::OPERATE,
            AuthMode::Case,
        );
        entry.add_subject(42).unwrap();
        entry.add_target(Target::new(Some(0), Some(OTA_PROVIDER_CLUSTER), None))
            .unwrap();
        assert!(grants_ota_access(&entry, 42));
        assert!(!grants_ota_access(&entry, 43));

        let mut other_cluster = AclEntry::new(
            std::num::NonZeroU8::new(1),
            Privilege::OPERATE,
            AuthMode::Case,
        );
        other_cluster.add_subject(42).unwrap();
        other_cluster
            .add_target(Target::new(Some(0), Some(6), None))
            .unwrap();
        assert!(!grants_ota_access(&other_cluster, 42));
    }

    /// rs-matter caps a file designator at 128 bytes, and the token shares
    /// the same buffer. The longest name this produces has to fit in both.
    #[test]
    fn the_longest_designator_fits_the_buffer_it_shares() {
        let longest = designator(u16::MAX, u16::MAX, u64::MAX);
        assert!(
            longest.len() + UPDATE_TOKEN_LEN <= 128,
            "{} is too long to name",
            longest
        );
    }
}
