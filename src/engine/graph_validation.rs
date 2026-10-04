//! Reuse successful profile checks for an unchanged chunk-table generation.

use super::*;
use std::sync::OnceLock;

const MAX_VALIDATED_GENERATIONS: usize = 128;

#[derive(Default)]
struct ProfileCache(VecDeque<(u64, usize, String)>);

impl ProfileCache {
    fn contains(&mut self, storage_id: u64, rows: usize, profile: &str) -> bool {
        let Some(position) = self
            .0
            .iter()
            .position(|entry| entry.0 == storage_id && entry.1 == rows && entry.2 == profile)
        else {
            return false;
        };
        let entry = self.0.remove(position).expect("existing cache entry");
        self.0.push_back(entry);
        true
    }

    fn insert(&mut self, storage_id: u64, rows: usize, profile: &str) {
        if self.contains(storage_id, rows, profile) {
            return;
        }
        while self.0.len() >= MAX_VALIDATED_GENERATIONS {
            self.0.pop_front();
        }
        self.0.push_back((storage_id, rows, profile.into()));
    }
}

static PROFILES: OnceLock<Mutex<ProfileCache>> = OnceLock::new();

pub(super) fn validate_profiles(chunks: &Table, profile: &str) -> Result<()> {
    // Dense storage IDs are globally unique content generations, not addresses.
    // Append and every UPDATE/DELETE (including profile/text-only edits) change
    // them; restore/recreate allocates fresh IDs. Catalog clones preserve IDs
    // only while the chunk contents are unchanged. This is also the lexical
    // cache's invalidation contract; selective rebuilds must preserve it.
    let generation = chunks
        .vector_columns
        .get(&8)
        .filter(|storage| storage.row_count == chunks.rows.len())
        .map(|storage| storage.storage_id);
    let cache = PROFILES.get_or_init(|| Mutex::new(ProfileCache::default()));
    if let Some(id) = generation {
        if cache
            .lock()
            .is_ok_and(|mut cache| cache.contains(id, chunks.rows.len(), profile))
        {
            return Ok(());
        }
    }
    // Do not retain failed checks or hold the cache mutex while scanning.
    for row in &chunks.rows {
        if text_at(row, 7)? != profile {
            return Err(invalid(
                "stored chunks do not match the collection embedding profile",
            ));
        }
    }
    remember_valid_profiles(chunks, profile);
    Ok(())
}

/// Record a proven-valid generation. In addition to the full check above, a
/// graph append can call this while holding the write lock: the old generation
/// was checked by collection(), and every appended profile was constructed
/// from that same validated profile. Never call for arbitrary SQL mutations.
pub(super) fn remember_valid_profiles(chunks: &Table, profile: &str) {
    if let Some(storage) = chunks
        .vector_columns
        .get(&8)
        .filter(|storage| storage.row_count == chunks.rows.len())
    {
        if let Ok(mut cache) = PROFILES
            .get_or_init(|| Mutex::new(ProfileCache::default()))
            .lock()
        {
            cache.insert(storage.storage_id, chunks.rows.len(), profile);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_generations_are_bounded_and_profile_specific() {
        let mut cache = ProfileCache::default();
        cache.insert(1, 2, "profile-a");
        assert!(cache.contains(1, 2, "profile-a"));
        assert!(!cache.contains(2, 2, "profile-a"));
        assert!(!cache.contains(1, 3, "profile-a"));
        assert!(!cache.contains(1, 2, "profile-b"));
        for generation in 2..=MAX_VALIDATED_GENERATIONS as u64 + 1 {
            cache.insert(generation, 2, "profile-a");
        }
        assert_eq!(cache.0.len(), MAX_VALIDATED_GENERATIONS);
        assert!(!cache.contains(1, 2, "profile-a"));
        assert!(cache.contains(2, 2, "profile-a"));
        cache.insert(1000, 2, "profile-a");
        assert!(!cache.contains(3, 2, "profile-a"));
        assert!(cache.contains(2, 2, "profile-a"));
    }
}
