/// Representation of a vulnerable signed driver provider used by KDU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KduProvider {
    /// Provider ID for the KDU command line (e.g. 11 for MSI).
    pub id: u32,
    /// Human-readable name of the provider.
    pub name: &'static str,
    /// Vulnerable driver file name.
    pub driver_name: &'static str,
    /// Whether this provider is known to be in the Microsoft Vulnerable Driver Blocklist.
    pub is_blocklisted: bool,
    /// Reliability score from 1 to 10 (10 being most reliable).
    pub reliability: u8,
}

pub const PROVIDERS: &[KduProvider] = &[
    KduProvider {
        id: 11,
        name: "MSI EneTechIo64",
        driver_name: "RTCore64.sys",
        is_blocklisted: false,
        reliability: 10,
    },
    KduProvider {
        id: 21,
        name: "ASUSTeK GPUTweak",
        driver_name: "AsIO3.sys",
        is_blocklisted: false,
        reliability: 8,
    },
    KduProvider {
        id: 1,
        name: "ASUSTeK ASUSIO",
        driver_name: "ASUSIO64.sys",
        is_blocklisted: true,
        reliability: 9,
    },
    KduProvider {
        id: 14,
        name: "GIGABYTE GIO",
        driver_name: "gdrv.sys",
        is_blocklisted: true,
        reliability: 9,
    },
];

impl KduProvider {
    /// Returns the default provider (MSI EneTechIo64, ID 11).
    pub fn default_provider() -> Self {
        PROVIDERS[0]
    }

    /// Selects the best provider based on blocklist status and reliability.
    ///
    /// Prefers non-blocklisted, highly reliable providers.
    pub fn select_best() -> Self {
        let mut best: Option<KduProvider> = None;

        for provider in PROVIDERS {
            if !provider.is_blocklisted {
                if best.is_none() || provider.reliability > best.unwrap().reliability {
                    best = Some(*provider);
                }
            }
        }

        best.unwrap_or_else(Self::default_provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_selection() {
        let default_prov = KduProvider::default_provider();
        assert_eq!(default_prov.id, 11);

        let best_prov = KduProvider::select_best();
        assert!(!best_prov.is_blocklisted);
        assert_eq!(best_prov.id, 11); // MSI should be chosen
    }
}
