use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Debug, Clone)]
pub struct IpRange {
    pub base: u32,
    pub size: u32,
}

impl IpRange {
    pub fn new(base: u32, size: u32) -> Self {
        Self { base, size }
    }

    pub fn from_cidr(cidr: &str) -> Result<Self, String> {
        let parts: Vec<&str> = cidr.split('/').collect();
        if parts.len() != 2 {
            return Err(format!("invalid CIDR: {cidr}"));
        }

        let ip: std::net::Ipv4Addr = parts[0]
            .parse()
            .map_err(|_| format!("invalid IP: {}", parts[0]))?;
        let prefix: u32 = parts[1]
            .parse()
            .map_err(|_| format!("invalid prefix: {}", parts[1]))?;

        if prefix > 32 {
            return Err(format!("prefix > 32: {prefix}"));
        }

        let ip_int = u32::from(ip);
        let mask = if prefix == 0 { 0 } else { !((1u32 << (32 - prefix)) - 1) };
        let base = ip_int & mask;
        let size = 1u32 << (32 - prefix);

        Ok(Self { base, size })
    }

    pub fn to_ip(&self, offset: u32) -> std::net::Ipv4Addr {
        std::net::Ipv4Addr::from(self.base + offset)
    }
}

pub struct Ipam {
    range: IpRange,
    next: AtomicU32,
}

// Manual Clone so that the AtomicU32 handle is duplicated (still pointing
// at the same underlying counter, not a fresh copy).
impl Clone for Ipam {
    fn clone(&self) -> Self {
        Self {
            range: self.range.clone(),
            next: AtomicU32::new(self.next.load(Ordering::SeqCst)),
        }
    }
}

impl Ipam {
    pub fn new(range: IpRange) -> Self {
        Self {
            range,
            next: AtomicU32::new(2),
        }
    }

    pub fn from_cidr(cidr: &str) -> Result<Self, String> {
        Ok(Self::new(IpRange::from_cidr(cidr)?))
    }

    pub fn allocate(&self) -> Option<u32> {
        loop {
            let current = self.next.load(Ordering::SeqCst);
            if current >= self.range.size - 1 {
                return None;
            }
            if self
                .next
                .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Some(self.range.base + current);
            }
        }
    }

    pub fn release(&self, _ip: u32) {
        // For now, no-op. Could implement a free list for reuse.
    }

    pub fn gateway(&self) -> u32 {
        self.range.base + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cidr_parsing() {
        let range = IpRange::from_cidr("10.42.0.0/16").unwrap();
        assert_eq!(range.base, u32::from(std::net::Ipv4Addr::new(10, 42, 0, 0)));
        assert_eq!(range.size, 65536);
    }

    #[test]
    fn test_cidr_small() {
        let range = IpRange::from_cidr("10.42.0.0/24").unwrap();
        assert_eq!(range.base, u32::from(std::net::Ipv4Addr::new(10, 42, 0, 0)));
        assert_eq!(range.size, 256);
        assert_eq!(range.to_ip(1).to_string(), "10.42.0.1");
        assert_eq!(range.to_ip(10).to_string(), "10.42.0.10");
    }

    #[test]
    fn test_ipam_allocate() {
        let ipam = Ipam::from_cidr("10.42.0.0/24").unwrap();
        let ip1 = ipam.allocate().unwrap();
        let ip2 = ipam.allocate().unwrap();
        assert_eq!(std::net::Ipv4Addr::from(ip1).to_string(), "10.42.0.2");
        assert_eq!(std::net::Ipv4Addr::from(ip2).to_string(), "10.42.0.3");
    }
}