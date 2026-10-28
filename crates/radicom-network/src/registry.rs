use crate::{NetworkError, Result};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};

static REGISTERED: OnceLock<Mutex<HashSet<SocketAddr>>> = OnceLock::new();

fn registered() -> &'static Mutex<HashSet<SocketAddr>> {
    REGISTERED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Process-local registry of DICOM server addresses.
///
/// The operating system remains the authority for socket binding. This
/// registry gives callers a cheap availability check and keeps addresses
/// reserved for the lifetime of the returned registration guard.
pub struct DicomServerRegistry;

impl DicomServerRegistry {
    /// Returns `true` when no server has registered `address` in this process.
    pub fn is_available(address: SocketAddr) -> bool {
        !registered().lock().expect("server registry lock poisoned").contains(&address)
    }

    /// Reserves `address` until the returned guard is dropped.
    pub fn register(address: SocketAddr) -> Result<DicomServerRegistration> {
        let mut addresses = registered().lock().expect("server registry lock poisoned");
        if !addresses.insert(address) {
            return Err(NetworkError::protocol("server address is already registered"));
        }
        Ok(DicomServerRegistration { address })
    }
}

/// A registration held by a bound server.
pub struct DicomServerRegistration {
    address: SocketAddr,
}

impl Drop for DicomServerRegistration {
    fn drop(&mut self) {
        if let Ok(mut addresses) = registered().lock() {
            addresses.remove(&self.address);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DicomServerRegistry;
    use std::net::SocketAddr;

    #[test]
    fn registered_address_is_reported_unavailable_until_released() {
        let address: SocketAddr = "127.0.0.1:42424".parse().unwrap();
        let registration = DicomServerRegistry::register(address).unwrap();
        assert!(!DicomServerRegistry::is_available(address));
        drop(registration);
        assert!(DicomServerRegistry::is_available(address));
    }

    #[test]
    fn duplicate_registration_is_rejected() {
        let address: SocketAddr = "127.0.0.1:42425".parse().unwrap();
        let _registration = DicomServerRegistry::register(address).unwrap();
        assert!(DicomServerRegistry::register(address).is_err());
    }
}
