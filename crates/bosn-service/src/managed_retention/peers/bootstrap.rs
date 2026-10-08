//! A new registry may coexist with objects whose original ownership is intact.

use super::*;
use crate::PriorObjectKind;
use bosn_core::ResourceKind;

pub(super) fn proves_at(
    root: &Path,
    current: &Path,
    prior: &crate::PriorIdentity,
) -> Result<bool, String> {
    if !prior.unreadable.is_empty() {
        return Ok(false);
    }
    let entries = catalog_entries(root, current)?;
    for object in &prior.objects {
        let Some(entry) = entries
            .iter()
            .find(|entry| entry.registry_id == object.registry_id)
        else {
            return Ok(false);
        };
        // Never mint a replacement for the same path whose database was lost.
        // Only a separately proven registry can account for this object.
        let state = recovery::resolve(root, entry)?;
        let ownership = registered::RegisteredOwnership::load_local(&state)?;
        let kind = match object.kind {
            PriorObjectKind::Container => ResourceKind::Container,
            PriorObjectKind::Volume => ResourceKind::Volume,
            PriorObjectKind::Image => ResourceKind::Image,
        };
        let identity = if kind == ResourceKind::Image {
            let Some(id) = object.image_id.as_deref() else {
                return Ok(false);
            };
            if !id.starts_with("sha256:")
                || id.len() != 71
                || !id[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Ok(false);
            }
            id
        } else {
            &object.name
        };
        if !ownership.records_object(kind, identity) {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_probe_retains_digest_when_a_repository_tag_exists() {
        let id = format!("sha256:{}", "a".repeat(64));
        let mut prior = crate::PriorIdentity::default();
        let document = format!(
            r#"[{{"Id":"{id}","RepoTags":["mutable:latest"],"Config":{{"Labels":{{"{}":"owner"}}}}}}]"#,
            bosn_core::LABEL_REGISTRY
        );
        crate::service::registry_identity::parse_kind(
            &mut prior,
            PriorObjectKind::Image,
            &document,
            bosn_core::LABEL_REGISTRY,
        );
        assert_eq!(prior.objects[0].name, "mutable:latest");
        assert_eq!(prior.objects[0].image_id.as_deref(), Some(id.as_str()));
    }

    #[test]
    fn a_catalog_uuid_alone_does_not_prove_an_object() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = root.path().join("peer");
        let current = root.path().join("new");
        std::fs::create_dir(&state).unwrap();
        std::fs::create_dir(&current).unwrap();
        let owner = "11111111-2222-4333-8444-555555555555";
        let _writer = Registry::create_writer(state.join("registry.sqlite3"), owner).unwrap();
        let catalog = root.path().join("catalog");
        register_at(&catalog, &state, owner).unwrap();
        let prior = crate::PriorIdentity {
            objects: vec![crate::PriorObject {
                kind: PriorObjectKind::Volume,
                name: "unrecorded".into(),
                image_id: None,
                registry_id: owner.into(),
                bytes: None,
            }],
            unreadable: Vec::new(),
        };
        assert!(!proves_at(&catalog, &current, &prior).unwrap());
        assert!(!proves_at(&catalog, &state, &prior).unwrap());
    }

    #[test]
    fn recorded_peer_objects_allow_coexistence_but_never_same_path_rekeying() {
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = root.path().join("peer");
        let current = root.path().join("new");
        std::fs::create_dir(&state).unwrap();
        std::fs::create_dir(&current).unwrap();
        let owner = "11111111-2222-4333-8444-555555555555";
        let mut writer = Registry::create_writer(state.join("registry.sqlite3"), owner).unwrap();
        let digest = format!("sha256:{}", "b".repeat(64));
        let mut prior = crate::PriorIdentity::default();
        let mut transaction = writer.begin_immediate().unwrap();
        for (kind, observed_kind, name) in [
            (
                ResourceKind::Container,
                PriorObjectKind::Container,
                "container",
            ),
            (ResourceKind::Volume, PriorObjectKind::Volume, "volume"),
            (ResourceKind::Image, PriorObjectKind::Image, "image"),
        ] {
            transaction
                .put_resource(&bosn_registry::Resource {
                    id: format!("{}:{name}", kind.as_str()),
                    kind,
                    name: name.into(),
                    stack: "fixture".into(),
                    generation: digest.clone(),
                    scope: bosn_core::Scope::Stack,
                    workspace: state.to_string_lossy().into_owned(),
                    created_at: 1.0,
                    last_used: 1.0,
                    state: bosn_core::ResourceState::Active,
                    retention: bosn_core::Retention::Warm,
                })
                .unwrap();
            prior.objects.push(crate::PriorObject {
                kind: observed_kind,
                name: name.into(),
                image_id: (kind == ResourceKind::Image).then(|| digest.clone()),
                registry_id: owner.into(),
                bytes: None,
            });
        }
        transaction.commit().unwrap();
        let catalog = root.path().join("catalog");
        register_at(&catalog, &state, owner).unwrap();
        assert!(proves_at(&catalog, &current, &prior).unwrap());
        assert!(!proves_at(&catalog, &state, &prior).unwrap());
        prior.unreadable.push("incomplete image census".into());
        assert!(!proves_at(&catalog, &current, &prior).unwrap());
        prior.unreadable.clear();
        prior.objects[2].image_id = Some(format!("sha256:{}", "c".repeat(64)));
        assert!(!proves_at(&catalog, &current, &prior).unwrap());
    }
}
