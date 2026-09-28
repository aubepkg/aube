use crate::StoredFile;
use aube_util::collections::FxMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::hash_map;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

/// File metadata shared by fetch, materialization and peer-context placements.
/// Cloning shares the map; modifying a clone keeps the other views unchanged.
#[derive(Debug, Clone, Default)]
pub struct PackageIndex(Arc<FxMap<String, StoredFile>>);

impl Deref for PackageIndex {
    type Target = FxMap<String, StoredFile>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for PackageIndex {
    fn deref_mut(&mut self) -> &mut Self::Target {
        Arc::make_mut(&mut self.0)
    }
}

impl FromIterator<(String, StoredFile)> for PackageIndex {
    fn from_iter<T: IntoIterator<Item = (String, StoredFile)>>(iter: T) -> Self {
        Self(Arc::new(iter.into_iter().collect()))
    }
}

impl IntoIterator for PackageIndex {
    type Item = (String, StoredFile);
    type IntoIter = hash_map::IntoIter<String, StoredFile>;

    fn into_iter(self) -> Self::IntoIter {
        Arc::unwrap_or_clone(self.0).into_iter()
    }
}

impl<'a> IntoIterator for &'a PackageIndex {
    type Item = (&'a String, &'a StoredFile);
    type IntoIter = hash_map::Iter<'a, String, StoredFile>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl Serialize for PackageIndex {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.as_ref().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PackageIndex {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        FxMap::deserialize(deserializer).map(|files| Self(Arc::new(files)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> PackageIndex {
        [(
            "index.js".into(),
            StoredFile {
                hex_hash: "abc".into(),
                store_path: "/store/abc".into(),
                executable: false,
                size: Some(3),
            },
        )]
        .into_iter()
        .collect()
    }

    #[test]
    fn clones_share_files_until_mutated() {
        let original = index();
        let mut changed = original.clone();
        assert!(std::ptr::eq(&original["index.js"], &changed["index.js"]));
        changed.get_mut("index.js").unwrap().executable = true;
        changed.remove("index.js");
        assert!(changed.is_empty());
        assert!(!original["index.js"].executable);
    }

    #[test]
    fn retains_plain_map_cache_format() {
        let original = index();
        let json = serde_json::to_value(&original).unwrap();
        assert_eq!(json, serde_json::to_value(&*original).unwrap());
        let decoded: PackageIndex = serde_json::from_value(json).unwrap();
        assert_eq!(
            decoded["index.js"].store_path,
            original["index.js"].store_path
        );
        assert_eq!(decoded["index.js"].size, Some(3));
    }

    #[test]
    fn consuming_shared_index_preserves_other_view() {
        let original = index();
        let entries: Vec<_> = original.clone().into_iter().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "index.js");
        assert_eq!(original.len(), 1);
        assert_eq!(original.into_iter().count(), 1);
    }
}
