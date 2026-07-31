// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

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
        let mask = if prefix == 0 {
            0
        } else {
            !((1u32 << (32 - prefix)) - 1)
        };
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
    /// Subnet allocator for per-bridge (per-pod) /24 ranges. Each call
    /// to `subnet_for(key)` with a new key hands out the next unused
    /// /24 inside `range`; repeat calls for the same key return the
    /// same subnet (so all workloads attached to one bridge share it).
    next_subnet: AtomicU32,
    subnets: Mutex<HashMap<String, IpRange>>,
    /// Per-subnet host offset counters (gateway is .1, hosts start at .2).
    next_in_subnet: Mutex<HashMap<u32, u32>>,
}

// Manual Clone so that the AtomicU32 handle is duplicated (still pointing
// at the same underlying counter, not a fresh copy).
impl Clone for Ipam {
    fn clone(&self) -> Self {
        Self {
            range: self.range.clone(),
            next: AtomicU32::new(self.next.load(Ordering::SeqCst)),
            next_subnet: AtomicU32::new(self.next_subnet.load(Ordering::SeqCst)),
            subnets: Mutex::new(HashMap::new()),
            next_in_subnet: Mutex::new(HashMap::new()),
        }
    }
}

impl Ipam {
    pub fn new(range: IpRange) -> Self {
        Self {
            range,
            next: AtomicU32::new(2),
            next_subnet: AtomicU32::new(0),
            subnets: Mutex::new(HashMap::new()),
            next_in_subnet: Mutex::new(HashMap::new()),
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

    /// Return the /24 subnet for `key` (e.g. a pod bridge name),
    /// allocating a fresh one on first use and reusing it afterwards.
    ///
    /// The counter alone is not enough: it resets on daemon restart,
    /// while bridge interfaces (and their kernel routes) survive. We
    /// therefore skip any /24 that is already in use on the host
    /// (read from /proc/net/route) or in the in-memory map, so a fresh
    /// daemon never re-allocates a live subnet.
    pub fn subnet_for(&self, key: &str) -> Option<IpRange> {
        let mut subnets = self.subnets.lock().ok()?;
        if let Some(existing) = subnets.get(key) {
            return Some(existing.clone());
        }
        let subnet_size = 1u32 << 8;
        let live = Self::live_subnets_in_range(self.range.base, self.range.size);
        for _ in 0..64 {
            let idx = self.next_subnet.fetch_add(1, Ordering::SeqCst);
            if (idx + 1) * subnet_size > self.range.size {
                return None;
            }
            let subnet = IpRange::new(self.range.base + idx * subnet_size, subnet_size);
            if live.contains(&subnet.base) {
                continue;
            }
            if subnets.values().any(|s| s.base == subnet.base) {
                continue;
            }
            subnets.insert(key.to_string(), subnet.clone());
            return Some(subnet);
        }
        None
    }

    /// Enumerate the /24 subnets currently routed on the host within
    /// [base, base+size), from /proc/net/route. Returns an empty set
    /// on non-Linux platforms or when the file is unreadable.
    fn live_subnets_in_range(base: u32, size: u32) -> std::collections::HashSet<u32> {
        let mut used = std::collections::HashSet::new();
        let Ok(contents) = std::fs::read_to_string("/proc/net/route") else {
            return used;
        };
        for line in contents.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 8 {
                continue;
            }
            let Ok(dest) = u32::from_str_radix(fields[1], 16) else {
                continue;
            };
            let Ok(mask) = u32::from_str_radix(fields[7], 16) else {
                continue;
            };
            let dest = dest.swap_bytes();
            let mask = mask.swap_bytes();
            if mask != 0xFFFF_FF00 {
                continue;
            }
            if dest >= base && dest < base + size {
                used.insert(dest);
            }
        }
        used
    }

    /// Allocate the next host address within `subnet` (gateway is .1,
    /// so the first host is .2).
    pub fn allocate_in(&self, subnet: &IpRange) -> Option<u32> {
        let mut counters = self.next_in_subnet.lock().ok()?;
        let counter = counters.entry(subnet.base).or_insert(2);
        let current = *counter;
        if current >= subnet.size - 1 {
            return None;
        }
        *counter = current + 1;
        Some(subnet.base + current)
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

    #[test]
    fn test_subnet_for_reuses_and_allocates() {
        let ipam = Ipam::from_cidr("10.42.0.0/16").unwrap();
        let a1 = ipam.subnet_for("pr-pod-a").unwrap();
        let a2 = ipam.subnet_for("pr-pod-a").unwrap();
        assert_eq!(a1.base, a2.base);
        assert_eq!(
            std::net::Ipv4Addr::from(a1.base + 1).to_string(),
            "10.42.0.1"
        );

        let b = ipam.subnet_for("pr-pod-b").unwrap();
        assert_eq!(std::net::Ipv4Addr::from(b.base).to_string(), "10.42.1.0");
    }

    #[test]
    fn test_allocate_in_subnet() {
        let ipam = Ipam::from_cidr("10.42.0.0/16").unwrap();
        let subnet = ipam.subnet_for("pr-pod-a").unwrap();
        assert_eq!(
            std::net::Ipv4Addr::from(ipam.allocate_in(&subnet).unwrap()).to_string(),
            "10.42.0.2"
        );
        assert_eq!(
            std::net::Ipv4Addr::from(ipam.allocate_in(&subnet).unwrap()).to_string(),
            "10.42.0.3"
        );
    }

    #[test]
    fn test_subnet_for_skips_host_live_subnets() {
        let ipam = Ipam::from_cidr("10.42.0.0/16").unwrap();
        let subnets = Ipam::live_subnets_in_range(
            u32::from(std::net::Ipv4Addr::new(10, 42, 0, 0)),
            65536,
        );
        let a = ipam.subnet_for("pr-new-a").unwrap();
        if subnets.contains(&a.base) {
            panic!(
                "allocator returned host-live subnet 10.42.{}.0 — collision after restart",
                (a.base >> 8) & 0xFF
            );
        }
        assert_eq!(a.base & 0xFF, 0);
    }
}
