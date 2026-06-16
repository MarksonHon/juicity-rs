use x509_parser::certificate::X509Certificate;
use x509_parser::extensions::ParsedExtension;
use x509_parser::prelude::*;

/// Domain information extracted from a TLS certificate.
#[derive(Debug, Clone)]
pub struct CertDomains {
    /// Common Name (CN) extracted from the Subject field.
    pub cn: Option<String>,
    /// Subject Alternative Names (SANs) — DNSName entries only.
    pub sans: Vec<String>,
    /// Whether any of the domains is a wildcard (starts with `*.`).
    pub is_wildcard: bool,
}

/// OID for Common Name: 2.5.4.3
const OID_CN: &[u8] = &[85, 4, 3]; // 2.5.4.3 in dotted form

/// Parse a PEM certificate file and extract domain information (CN + SANs).
///
/// # Arguments
/// - `cert_path`: Path to the PEM-encoded certificate file.
///
/// # Returns
/// - `Ok(CertDomains)` containing the parsed CN and SANs.
/// - `Err(...)` if the file cannot be read or the certificate cannot be parsed.
pub fn parse_cert_domains(cert_path: &str) -> anyhow::Result<CertDomains> {
    let data = std::fs::read(cert_path)?;

    // Same approach as server/src/server.rs:load_certs
    let mut reader = std::io::BufReader::new(&data[..]);
    let der_bytes: Vec<u8> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No certificate found in PEM file: {}", cert_path))?
        .to_vec();

    // Parse the DER certificate with x509-parser
    let (_, cert) = X509Certificate::from_der(&der_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to parse X.509 certificate: {}", e))?;

    // Extract Common Name (CN) from the Subject
    let cn = extract_cn(&cert);

    // Extract Subject Alternative Names (SANs)
    let sans = extract_sans(&cert);

    let is_wildcard = cn.as_deref().map_or(false, |name| is_wildcard_name(name))
        || sans.iter().any(|name| is_wildcard_name(name));

    Ok(CertDomains { cn, sans, is_wildcard })
}

/// Pick the preferred domain for use as SNI in a share link.
///
/// # Logic
/// - If `user_domain` is provided, use it directly.
/// - If the certificate contains a single non-wildcard domain, return it.
/// - If the certificate is a wildcard-only cert, return `None` (caller should prompt).
/// - If the certificate has multiple SANs, return the first non-wildcard one (fallback).
///
/// # Returns
/// - `Some(domain)` — a domain that can be used as SNI.
/// - `None` — no suitable domain could be auto-selected.
pub fn pick_preferred_domain(domains: &CertDomains, user_domain: Option<&str>) -> Option<String> {
    if let Some(d) = user_domain {
        return Some(d.to_string());
    }

    // Collect all non-wildcard domains, preferring CN first
    if let Some(cn) = &domains.cn {
        if !is_wildcard_name(cn) {
            return Some(cn.clone());
        }
    }

    // Fall back to first non-wildcard SAN
    domains
        .sans
        .iter()
        .find(|s| !is_wildcard_name(s))
        .cloned()
}

// ── Helpers ──

/// Extract Common Name (CN) from the certificate Subject.
fn extract_cn(cert: &X509Certificate) -> Option<String> {
    for rdn in cert.subject().iter_rdn() {
        for attr in rdn.iter() {
            // Check if the attribute type OID matches CN (2.5.4.3)
            if attr.attr_type().as_bytes() == OID_CN {
                // Try to get the value as a string
                if let Ok(s) = attr.attr_value().as_str() {
                    return Some(s.to_string());
                }
                // Fallback: try to get value as a string via TryFrom<&AttributeTypeAndValue>
                if let Ok(s) = <&str>::try_from(attr) {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

/// Extract DNSName entries from the Subject Alternative Names extension (OID 2.5.29.17).
fn extract_sans(cert: &X509Certificate) -> Vec<String> {
    let mut sans = Vec::new();

    for ext in cert.extensions() {
        // The parsed_extension() method returns &ParsedExtension directly
        if let ParsedExtension::SubjectAlternativeName(san) = ext.parsed_extension() {
            for general_name in &san.general_names {
                if let GeneralName::DNSName(name) = general_name {
                    sans.push(name.to_string());
                }
            }
        }
    }

    sans
}

/// Check if a domain name is a wildcard (starts with `*.`).
pub fn is_wildcard_name(name: &str) -> bool {
    name.starts_with("*.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pick_preferred_single_domain() {
        let domains = CertDomains {
            cn: Some("example.com".to_string()),
            sans: vec!["example.com".to_string()],
            is_wildcard: false,
        };
        assert_eq!(
            pick_preferred_domain(&domains, None),
            Some("example.com".to_string())
        );
    }

    #[test]
    fn test_pick_preferred_wildcard_only() {
        let domains = CertDomains {
            cn: Some("*.example.com".to_string()),
            sans: vec!["*.example.com".to_string()],
            is_wildcard: true,
        };
        // Wildcard-only → None (caller should prompt)
        assert_eq!(pick_preferred_domain(&domains, None), None);

        // With user override → return the override
        assert_eq!(
            pick_preferred_domain(&domains, Some("myapp.example.com")),
            Some("myapp.example.com".to_string())
        );
    }

    #[test]
    fn test_pick_preferred_multi_domain() {
        let domains = CertDomains {
            cn: Some("example.com".to_string()),
            sans: vec!["example.com".to_string(), "api.example.com".to_string()],
            is_wildcard: false,
        };
        // Prefer CN
        assert_eq!(
            pick_preferred_domain(&domains, None),
            Some("example.com".to_string())
        );
    }

    #[test]
    fn test_pick_preferred_mixed_wildcard() {
        let domains = CertDomains {
            cn: Some("example.com".to_string()),
            sans: vec!["*.example.com".to_string(), "example.com".to_string()],
            is_wildcard: true,
        };
        // CN is non-wildcard, so use that
        assert_eq!(
            pick_preferred_domain(&domains, None),
            Some("example.com".to_string())
        );
    }

    #[test]
    fn test_pick_preferred_no_cn_only_san() {
        let domains = CertDomains {
            cn: None,
            sans: vec!["api.example.com".to_string()],
            is_wildcard: false,
        };
        assert_eq!(
            pick_preferred_domain(&domains, None),
            Some("api.example.com".to_string())
        );
    }

    #[test]
    fn test_pick_preferred_no_domains() {
        let domains = CertDomains {
            cn: None,
            sans: vec![],
            is_wildcard: false,
        };
        assert_eq!(pick_preferred_domain(&domains, None), None);
    }

    #[test]
    fn test_is_wildcard_name() {
        assert!(is_wildcard_name("*.example.com"));
        assert!(!is_wildcard_name("example.com"));
        assert!(!is_wildcard_name("sub.example.com"));
        assert!(is_wildcard_name("*."));
        assert!(!is_wildcard_name("*"));
    }
}
