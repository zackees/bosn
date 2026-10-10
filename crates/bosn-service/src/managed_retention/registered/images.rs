//! Prove a Bosn-built setup or manifest image by its registry record (#545).
//!
//! An inline-Dockerfile setup or manifest image is built with the content-addressed tag
//! `bosn-setup:<sha256>` and no ownership labels, and an existing image can never gain one. Its
//! proof is the record setup or manifest ensure wrote, `setup-image:<id>` or `manifest-image:<id>`,
//! whose generation is Docker's image id. Only an image whose every tag is a `bosn-setup:` tag
//! qualifies: a pulled, pinned image may be shared with the operator's own work, so it is never
//! proven this way and stays outside managed retention.

use std::collections::BTreeMap;

use bosn_core::{ResourceKind, ResourceLabels, Retention};

use super::{Normalized, RegisteredOwnership};

/// The tag repository every inline setup and manifest build uses.
pub(in crate::managed_retention) const BUILT_IMAGE_REPOSITORY: &str = "bosn-setup";

/// Whether `tags` is non-empty and every tag is a content-addressed `bosn-setup:<sha256>` tag.
pub(in crate::managed_retention) fn only_built_tags(tags: &[String]) -> bool {
    !tags.is_empty()
        && tags.iter().all(|tag| {
            tag.strip_prefix(BUILT_IMAGE_REPOSITORY)
                .and_then(|rest| rest.strip_prefix(':'))
                .is_some_and(|digest| {
                    digest.len() == 64
                        && digest
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
        })
}

impl RegisteredOwnership {
    /// Prove an unlabelled Bosn-built image by its id, or `None` when nothing proves it.
    ///
    /// Every record naming this image id contributes: the image is as recently used as its most
    /// recently used record, and protected if any of them is. Canonical labels come from that
    /// record. An image row's retention is bookkeeping (every ensure wrote `pinned`), so only an
    /// explicit pin label could pin an image, and a built image carries none.
    pub(crate) fn normalize_image(
        &self,
        id: &str,
        tags: &[String],
        labels: &BTreeMap<String, String>,
        now: f64,
    ) -> Option<Normalized> {
        if labels.contains_key(bosn_core::LABEL_REGISTRY)
            || labels.contains_key(bosn_core::LABEL_KIND)
            || !only_built_tags(tags)
        {
            return None;
        }
        let records: Vec<_> = self
            .resources
            .iter()
            .filter(|resource| {
                resource.kind == ResourceKind::Image
                    && resource.generation == id
                    && (resource.id == format!("setup-image:{id}")
                        || resource.id == format!("manifest-image:{id}"))
            })
            .collect();
        if records
            .iter()
            .any(|resource| !resource.last_used.is_finite())
        {
            return None;
        }
        let latest = records
            .iter()
            .copied()
            .max_by(|left, right| left.last_used.total_cmp(&right.last_used))?;
        let canonical = ResourceLabels::new(
            &self.owner,
            ResourceKind::Image,
            &latest.stack,
            &latest.generation,
            latest.scope,
            &latest.workspace,
            &latest.created_at.to_string(),
            Some(Retention::Warm),
        )
        .ok()?;
        let mut normalized = labels.clone();
        normalized.extend(
            canonical
                .to_map()
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value)),
        );
        Some(Normalized {
            labels: normalized,
            idle_seconds: (now - latest.last_used).max(0.0),
            protected: records.iter().any(|resource| {
                self.protected.contains(&resource.id) || self.protected.contains(&resource.name)
            }),
        })
    }
}
