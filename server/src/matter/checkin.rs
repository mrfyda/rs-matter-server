//! Receiving ICD check-ins.
//!
//! An intermittently connected device sleeps with its radio off. It cannot be
//! reached on demand, so instead it *checks in*: every idle period it wakes,
//! sends one small message to each registered client, and stays awake briefly
//! in case that client wants something. `register_icd` is how this controller
//! becomes such a client; this is what happens when the device follows through.
//!
//! **A check-in says nothing about who sent it — except by decrypting.** It is
//! sent sessionlessly, and rs-matter's public API does not expose the peer
//! behind an accepted exchange. What it does expose is the codec: the message
//! is encrypted with the symmetric key this controller generated for that one
//! device at registration, and authenticated with a MIC. So the key that reads
//! a check-in *is* the sender's identity, and trying each registered key in
//! turn identifies the device — more strongly than a claimed node id in a
//! header would, because a forged check-in fails the MIC.
//!
//! The counter it carries must advance, or the message is a replay of one
//! already seen.

use std::sync::Arc;
use std::time::SystemTime;

use rs_matter::crypto::{CanonAeadKeyRef, Crypto};
use rs_matter::error::Error;
use rs_matter::sc::checkin::CheckIn;
use rs_matter::sc::AsyncScHandler;
use rs_matter::transport::exchange::Exchange;

use crate::api::ServerContext;

/// Reads incoming check-ins and records which node was awake, and when.
pub struct CheckInReceiver<C> {
    context: Arc<ServerContext>,
    crypto: C,
}

impl<C> CheckInReceiver<C> {
    pub fn new(context: Arc<ServerContext>, crypto: C) -> Self {
        Self { context, crypto }
    }
}

impl<C: Crypto> AsyncScHandler for CheckInReceiver<C> {
    async fn check_in(&self, mut exchange: Exchange<'_>) -> Result<(), Error> {
        // The payload is decrypted in place, and each candidate key needs an
        // untouched copy of it.
        exchange.recv_fetch().await?;
        let payload = exchange.rx()?.payload().to_vec();

        let Some((node_id, counter)) = self.identify(&payload) else {
            log::debug!("A check-in arrived that no registered key could read");
            return Ok(());
        };

        log::info!("Node {} checked in (counter {})", node_id, counter);
        if let Err(error) = self.context.config.note_icd_counter(node_id, counter) {
            log::warn!("Could not persist node {}'s check-in counter: {}", node_id, error);
        }
        self.context.note_check_in(node_id, SystemTime::now());

        // A check-in is unreliable and expects no answer: the device is
        // listening for what the client does next, not for an acknowledgement.
        Ok(())
    }
}

impl<C: Crypto> CheckInReceiver<C> {
    /// Which registered device sent this, by finding the key that reads it.
    ///
    /// `None` if no key does, or if the counter does not advance on the last
    /// one accepted from that device.
    fn identify(&self, payload: &[u8]) -> Option<(u64, u32)> {
        for (node_id, key, last_counter) in self.context.config.icd_registrations() {
            let Ok(key) = CanonAeadKeyRef::try_new(&key) else {
                log::warn!("Node {}'s stored check-in key is the wrong length", node_id);
                continue;
            };

            let mut buffer = payload.to_vec();
            let Ok(parsed) = CheckIn::new(key).parse(&self.crypto, &mut buffer) else {
                continue;
            };

            // The key authenticated the message, so this is the sender.
            if last_counter.is_some_and(|last| parsed.counter <= last) {
                log::warn!(
                    "Node {} replayed check-in counter {}; ignoring it",
                    node_id,
                    parsed.counter
                );
                return None;
            }
            return Some((node_id, parsed.counter));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tests_support::test_context;
    use rand_core::OsRng;
    use rs_matter::crypto::default_crypto;
    use rs_matter::dm::devices::test::DAC_PRIVKEY;

    const KEY_LEN: usize = 16;

    fn receiver(context: Arc<ServerContext>) -> CheckInReceiver<impl Crypto> {
        CheckInReceiver::new(context, default_crypto(OsRng, DAC_PRIVKEY))
    }

    /// A check-in as the device would send it: encrypted with the key that
    /// device was registered with.
    fn check_in(key: &[u8], counter: u32) -> Vec<u8> {
        let crypto = default_crypto(OsRng, DAC_PRIVKEY);
        let key = CanonAeadKeyRef::try_new(key).unwrap();
        let mut payload = vec![0u8; 64];
        let written = CheckIn::new(key)
            .generate(&crypto, counter, &[], &mut payload)
            .unwrap()
            .len();
        payload.truncate(written);
        payload
    }

    #[test]
    fn the_key_that_reads_a_check_in_names_the_device_that_sent_it() {
        let context = Arc::new(test_context());
        let theirs = [0xAAu8; KEY_LEN];
        let ours = [0xBBu8; KEY_LEN];
        context.config.set_icd_registration(1, &theirs).unwrap();
        context.config.set_icd_registration(2, &ours).unwrap();

        let receiver = receiver(context);
        assert_eq!(receiver.identify(&check_in(&ours, 5)), Some((2, 5)));
        assert_eq!(receiver.identify(&check_in(&theirs, 9)), Some((1, 9)));
    }

    #[test]
    fn a_check_in_no_registered_key_reads_is_ignored() {
        let context = Arc::new(test_context());
        context.config.set_icd_registration(1, &[0xAAu8; KEY_LEN]).unwrap();

        let receiver = receiver(context);
        // Encrypted with a key this controller never issued: a forgery, or a
        // device registered with somebody else.
        assert_eq!(receiver.identify(&check_in(&[0xCCu8; KEY_LEN], 1)), None);
        // Not a check-in at all.
        assert_eq!(receiver.identify(&[0u8; 40]), None);
        assert_eq!(receiver.identify(&[]), None);
    }

    #[test]
    fn a_replayed_counter_is_refused() {
        let context = Arc::new(test_context());
        let key = [0xAAu8; KEY_LEN];
        context.config.set_icd_registration(1, &key).unwrap();

        let receiver = receiver(context.clone());
        assert_eq!(receiver.identify(&check_in(&key, 4)), Some((1, 4)));
        context.config.note_icd_counter(1, 4).unwrap();

        // The same message again, and an older one.
        assert_eq!(receiver.identify(&check_in(&key, 4)), None);
        assert_eq!(receiver.identify(&check_in(&key, 3)), None);
        // The next one is accepted, and need not be consecutive: a device that
        // checked in while this server was down has moved its counter on.
        assert_eq!(receiver.identify(&check_in(&key, 12)), Some((1, 12)));
    }

    #[test]
    fn unregistering_forgets_the_key() {
        let context = Arc::new(test_context());
        let key = [0xAAu8; KEY_LEN];
        context.config.set_icd_registration(1, &key).unwrap();
        context.config.remove_icd_registration(1).unwrap();

        assert_eq!(receiver(context).identify(&check_in(&key, 1)), None);
    }

    #[test]
    fn re_registering_replaces_the_key() {
        let context = Arc::new(test_context());
        let first = [0xAAu8; KEY_LEN];
        let second = [0xBBu8; KEY_LEN];
        context.config.set_icd_registration(1, &first).unwrap();
        context.config.set_icd_registration(1, &second).unwrap();

        let receiver = receiver(context);
        assert_eq!(receiver.identify(&check_in(&second, 1)), Some((1, 1)));
        assert_eq!(receiver.identify(&check_in(&first, 1)), None);
    }
}
