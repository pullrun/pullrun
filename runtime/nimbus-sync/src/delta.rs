use crate::bloom::BloomFilter;

/// Compute which digests from `local_blobs` are NOT present in the
/// peer's bloom filter. These are the blobs we need to send to the peer.
pub fn compute_delta(local_blobs: &[String], peer_filter: &BloomFilter) -> Vec<String> {
    local_blobs
        .iter()
        .filter(|d| !peer_filter.contains(d))
        .cloned()
        .collect()
}

/// Given two bloom filters, compute an approximate symmetric delta.
/// Returns the list of digests from `local_blobs` that the peer
/// likely doesn't have. This is an approximation — bloom filter
/// false positives mean we might skip sending a blob the peer
/// actually needs, but the peer can request it explicitly via GetBlobs.
#[allow(unused_variables)]
pub fn approximate_missing(local_blobs: &[String], local_filter: &BloomFilter, peer_filter: &BloomFilter) -> Vec<String> {
    local_blobs
        .iter()
        .filter(|d| !peer_filter.contains(d))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_delta() {
        let mut pf = BloomFilter::optimal(10);
        pf.insert("a");
        pf.insert("b");

        let blobs = vec!["a".into(), "b".into(), "c".into()];
        let delta = compute_delta(&blobs, &pf);
        assert_eq!(delta, vec!["c".to_string()]);
    }
}
