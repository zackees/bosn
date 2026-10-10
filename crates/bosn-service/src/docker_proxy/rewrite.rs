//! Request rewrites of the Docker accounting proxy (#358); see the parent
//! module for what is rewritten and why.

use std::{collections::BTreeMap, io};

use serde_json::{Map, Value};

use super::ProxySettings;

/// Strip Docker's optional `/v1.NN` prefix and the query string.
pub(super) fn api_path(target: &str) -> &str {
    let path = target.split('?').next().unwrap_or(target);
    match path.strip_prefix("/v") {
        Some(rest) => match rest.find('/') {
            Some(slash)
                if rest[..slash]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b == b'.') =>
            {
                &rest[slash..]
            }
            _ => path,
        },
        None => path,
    }
}

/// What the proxy does with a request that is not one of the three creates.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Scope {
    /// Forwarded byte for byte.
    Forward,
    /// The run's label is added to the request's `filters`, so a listing or a
    /// prune sees only the run's own objects (#560).
    Label,
    /// A build-cache prune. BuildKit takes no label filter and its cache is
    /// shared with the host, so the prune is narrowed to a record id that never
    /// exists: it succeeds and removes nothing.
    BuildCache,
}

/// Classify a request. Image and network listings stay whole: images carry no
/// run label, and act resolves networks by listing them.
pub(super) fn scope(method: &str, target: &str) -> Scope {
    let path = api_path(target);
    match (method, path) {
        ("GET", "/containers/json" | "/volumes")
        | ("POST", "/containers/prune" | "/volumes/prune" | "/networks/prune" | "/images/prune") => {
            Scope::Label
        }
        ("POST", "/build/prune") => Scope::BuildCache,
        _ => Scope::Forward,
    }
}

/// Split a target into its path and decoded `key=value` query pairs.
pub(super) fn query_pairs(target: &str) -> (&str, Vec<(String, String)>) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let pairs = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(k), decode(v))
        })
        .collect();
    (path, pairs)
}

fn join_query(path: &str, pairs: &[(String, String)]) -> String {
    if pairs.is_empty() {
        return path.to_owned();
    }
    let query: Vec<String> = pairs
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                crate::docker_api::encode(k),
                crate::docker_api::encode(v)
            )
        })
        .collect();
    format!("{path}?{}", query.join("&"))
}

pub(super) fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = |b: u8| (b as char).to_digit(16);
                match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    (Some(high), Some(low)) => {
                        out.push((high * 16 + low) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `?name=x` becomes `?name=x-<suffix>`; a create without a name is kept.
pub(super) fn suffix_name(target: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        return target.to_owned();
    }
    let (path, mut pairs) = query_pairs(target);
    let mut changed = false;
    for (key, value) in &mut pairs {
        if key == "name" && !value.is_empty() && !value.ends_with(&format!("-{suffix}")) {
            value.push('-');
            value.push_str(suffix);
            changed = true;
        }
    }
    if changed {
        join_query(path, &pairs)
    } else {
        target.to_owned()
    }
}

/// The `filters` query parameter of a target, decoded, and its position.
fn filters_of(pairs: &[(String, String)]) -> io::Result<(Option<usize>, Map<String, Value>)> {
    let position = pairs.iter().position(|(k, _)| k == "filters");
    let filters = match position {
        Some(i) if !pairs[i].1.trim().is_empty() => serde_json::from_str(&pairs[i].1)
            .map_err(|_| io::Error::other("malformed request filters"))?,
        _ => Map::new(),
    };
    Ok((position, filters))
}

fn with_filters(
    path: &str,
    mut pairs: Vec<(String, String)>,
    position: Option<usize>,
    filters: Map<String, Value>,
) -> String {
    let encoded = Value::Object(filters).to_string();
    match position {
        Some(i) => pairs[i].1 = encoded,
        None => pairs.push(("filters".into(), encoded)),
    }
    join_query(path, &pairs)
}

/// Narrow a build-cache prune to a record id that never exists. BuildKit
/// matches `id` as a regular expression, and `^$` matches no record.
pub(super) fn scope_build_prune(target: &str) -> io::Result<String> {
    let (path, pairs) = query_pairs(target);
    let (position, mut filters) = filters_of(&pairs)?;
    filters.insert("id".into(), Value::Array(vec![Value::String("^$".into())]));
    Ok(with_filters(path, pairs, position, filters))
}

/// Add `label=com.zackees.bosn.run=<run>` to a listing's or prune's filters.
pub(super) fn scope_listing(target: &str, run: &str) -> io::Result<String> {
    let (path, pairs) = query_pairs(target);
    let label = Value::String(format!("{}={run}", crate::docker_api::LABEL_RUN));
    let (position, mut filters) = filters_of(&pairs)?;
    match filters.get_mut("label") {
        // Docker accepts a list, or the legacy {"k=v": true} map.
        Some(Value::Array(labels)) => labels.push(label),
        Some(Value::Object(labels)) => {
            labels.insert(label.as_str().unwrap().to_owned(), Value::Bool(true));
        }
        _ => {
            filters.insert("label".into(), Value::Array(vec![label]));
        }
    }
    Ok(with_filters(path, pairs, position, filters))
}

/// Which create call, if any, a request target names. Docker accepts an
/// optional `/v1.NN` version prefix and a query string.
pub(super) fn create_kind(method: &str, target: &str) -> Option<&'static str> {
    if method != "POST" {
        return None;
    }
    match api_path(target) {
        "/containers/create" => Some("container"),
        "/networks/create" => Some("network"),
        "/volumes/create" => Some("volume"),
        _ => None,
    }
}

/// Rewrite one create body; see the module documentation.
pub fn rewrite_create(kind: &str, body: &[u8], settings: &ProxySettings) -> io::Result<Vec<u8>> {
    let mut value: Value = if body.iter().all(u8::is_ascii_whitespace) {
        Value::Object(Map::new())
    } else {
        serde_json::from_slice(body).map_err(|e| io::Error::other(format!("body: {e}")))?
    };
    let object = value
        .as_object_mut()
        .ok_or_else(|| io::Error::other("body is not a JSON object"))?;
    add_labels(object, &settings.labels)?;
    match kind {
        "container" => {
            rewrite_container(object, settings)?;
            pin_cgroup_parent(object, settings);
        }
        "volume" => {
            if let Some(name) = object.get("Name").and_then(Value::as_str) {
                let mapped = settings.volumes.map_volume(name)?;
                if mapped != name {
                    note(settings, format!("[bosn] cache volume {name} -> {mapped}"));
                    object.insert("Name".into(), Value::String(mapped));
                }
            }
        }
        _ => {}
    }
    serde_json::to_vec(&value).map_err(io::Error::other)
}

/// Force the run's cgroup parent on every container, whatever the caller
/// (act's `--container-options`, a workflow's `container.options`, a service
/// container) asked for, and record it in a label.
fn pin_cgroup_parent(object: &mut Map<String, Value>, settings: &ProxySettings) {
    let Some(parent) = &settings.cgroup_parent else {
        return;
    };
    if let Some(Value::Object(labels)) = object.get_mut("Labels") {
        labels.insert(LABEL_CGROUP_PARENT.into(), Value::String(parent.clone()));
    }
    if let Some(Value::Object(host)) = object.get_mut("HostConfig") {
        let asked = host.insert("CgroupParent".into(), Value::String(parent.clone()));
        if let Some(Value::String(asked)) = asked
            && !asked.is_empty()
            && asked != *parent
        {
            note(
                settings,
                format!("[bosn] cgroup parent {asked} replaced by the run's {parent}"),
            );
        }
    }
}

/// Records the cgroup parent the proxy pinned on a container.
pub const LABEL_CGROUP_PARENT: &str = "com.zackees.bosn.cgroup-parent";

fn note(settings: &ProxySettings, line: String) {
    if let Some(notes) = &settings.notes {
        notes(line);
    }
}

fn add_labels(
    object: &mut Map<String, Value>,
    labels: &BTreeMap<String, String>,
) -> io::Result<()> {
    let entry = object
        .entry("Labels")
        .or_insert_with(|| Value::Object(Map::new()));
    if entry.is_null() {
        *entry = Value::Object(Map::new());
    }
    let existing = entry
        .as_object_mut()
        .ok_or_else(|| io::Error::other("Labels is not an object"))?;
    for (key, value) in labels {
        existing.insert(key.clone(), Value::String(value.clone()));
    }
    Ok(())
}

#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
fn rewrite_container(object: &mut Map<String, Value>, settings: &ProxySettings) -> io::Result<()> {
    let host = object
        .entry("HostConfig")
        .or_insert_with(|| Value::Object(Map::new()));
    if host.is_null() {
        *host = Value::Object(Map::new());
    }
    let host = host
        .as_object_mut()
        .ok_or_else(|| io::Error::other("HostConfig is not an object"))?;
    let int =
        |host: &Map<String, Value>, key: &str| host.get(key).and_then(Value::as_i64).unwrap_or(0);
    // CPU: cap at the slot's quota. Docker refuses NanoCpus together with an
    // explicit CFS period/quota, so a caller that chose those keeps them.
    if settings.nano_cpus > 0 && int(host, "CpuQuota") == 0 && int(host, "CpuPeriod") == 0 {
        let current = int(host, "NanoCpus");
        if current == 0 || current > settings.nano_cpus {
            host.insert("NanoCpus".into(), Value::from(settings.nano_cpus));
        }
    }
    if let Some(memory) = settings.memory.and_then(|m| i64::try_from(m).ok()) {
        let current = int(host, "Memory");
        if current == 0 || current > memory {
            host.insert("Memory".into(), Value::from(memory));
        }
        let effective = int(host, "Memory");
        let swap = int(host, "MemorySwap");
        if swap > 0 && swap < effective {
            host.insert("MemorySwap".into(), Value::from(effective));
        }
    }
    // Named volumes in Binds ("name:/target[:opts]") and Mounts.
    let mut targets = Vec::new();
    if let Some(binds) = host.get_mut("Binds").and_then(Value::as_array_mut) {
        for bind in binds.iter_mut() {
            let Some(text) = bind.as_str() else { continue };
            let mut parts = text.splitn(3, ':');
            let source = parts.next().unwrap_or_default().to_owned();
            let target = parts.next().unwrap_or_default().to_owned();
            let rest = parts.next().map(str::to_owned);
            targets.push(target.clone());
            if is_volume_name(&source) {
                let mapped = settings.volumes.map_volume(&source)?;
                if mapped != source {
                    note(
                        settings,
                        format!("[bosn] cache volume {source} -> {mapped} at {target}"),
                    );
                    *bind = Value::String(match rest {
                        Some(rest) => format!("{mapped}:{target}:{rest}"),
                        None => format!("{mapped}:{target}"),
                    });
                }
            }
        }
    }
    if let Some(mounts) = host.get_mut("Mounts").and_then(Value::as_array_mut) {
        for mount in mounts.iter_mut() {
            let Some(mount) = mount.as_object_mut() else {
                continue;
            };
            if let Some(target) = mount.get("Target").and_then(Value::as_str) {
                targets.push(target.to_owned());
            }
            if mount.get("Type").and_then(Value::as_str) != Some("volume") {
                continue;
            }
            let Some(source) = mount
                .get("Source")
                .and_then(Value::as_str)
                .map(str::to_owned)
            else {
                continue;
            };
            if source.is_empty() {
                continue;
            }
            let mapped = settings.volumes.map_volume(&source)?;
            if mapped != source {
                let target = mount.get("Target").and_then(Value::as_str).unwrap_or("");
                note(
                    settings,
                    format!("[bosn] cache volume {source} -> {mapped} at {target}"),
                );
                mount.insert("Source".into(), Value::String(mapped));
            }
        }
    }
    if let Some(volumes) = object.get("Volumes").and_then(Value::as_object) {
        targets.extend(volumes.keys().cloned());
    }
    let injected = settings.volumes.injected_mounts()?;
    if !injected.is_empty() {
        let host = object
            .get_mut("HostConfig")
            .and_then(Value::as_object_mut)
            .expect("HostConfig was normalized above");
        let mounts = host
            .entry("Mounts")
            .or_insert_with(|| Value::Array(Vec::new()));
        if mounts.is_null() {
            *mounts = Value::Array(Vec::new());
        }
        let mounts = mounts
            .as_array_mut()
            .ok_or_else(|| io::Error::other("Mounts is not an array"))?;
        for (target, volume) in injected {
            if targets
                .iter()
                .any(|t| t.trim_end_matches('/') == target.trim_end_matches('/'))
            {
                continue;
            }
            note(
                settings,
                format!("[bosn] cache volume {volume} mounted at {target}"),
            );
            mounts.push(serde_json::json!({"Type": "volume", "Source": volume, "Target": target}));
        }
    }
    Ok(())
}

/// A bind source without a slash is a named volume (Docker's rule).
fn is_volume_name(source: &str) -> bool {
    !source.is_empty() && !source.contains('/') && !source.contains('\\')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_proxy::{Activity, NoVolumes, VolumePolicy};
    use std::{path::PathBuf, sync::Arc};

    struct Renames(BTreeMap<&'static str, &'static str>, Vec<(String, String)>);
    impl VolumePolicy for Renames {
        fn map_volume(&self, name: &str) -> io::Result<String> {
            Ok(self.0.get(name).map_or(name, |v| v).to_string())
        }
        fn injected_mounts(&self) -> io::Result<Vec<(String, String)>> {
            Ok(self.1.clone())
        }
    }

    fn settings(volumes: Arc<dyn VolumePolicy>) -> ProxySettings {
        ProxySettings {
            upstream: PathBuf::from("/nonexistent"),
            run: "r-1".into(),
            name_suffix: "b1".into(),
            labels: BTreeMap::from([("com.zackees.bosn.run".into(), "r-1".into())]),
            nano_cpus: 4_000_000_000,
            memory: Some(1 << 30),
            cgroup_parent: None,
            volumes,
            activity: Arc::new(Activity::new()),
            notes: None,
        }
    }

    fn rewrite(kind: &str, body: Value, settings: &ProxySettings) -> Value {
        serde_json::from_slice(
            &rewrite_create(kind, body.to_string().as_bytes(), settings).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn container_creates_gain_labels_limits_and_cache_mounts() {
        let policy = Arc::new(Renames(
            BTreeMap::from([("act-toolcache", "bosn-cache-x-toolcache-0")]),
            vec![
                ("/cache".into(), "bosn-cache-x-shared".into()),
                (
                    "/opt/hostedtoolcache".into(),
                    "ignored-already-mounted".into(),
                ),
            ],
        ));
        let s = settings(policy);
        let out = rewrite(
            "container",
            serde_json::json!({
                "Image": "ubuntu",
                "Labels": {"keep": "me", "com.zackees.bosn.run": "forged"},
                "HostConfig": {
                    "NanoCpus": 16_000_000_000_i64,
                    "Memory": 0,
                    "Binds": ["/host/path:/w", "act-env:/var/run/act"],
                    "Mounts": [{"Type": "volume", "Source": "act-toolcache", "Target": "/opt/hostedtoolcache"}]
                }
            }),
            &s,
        );
        assert_eq!(out["Labels"]["keep"], "me");
        assert_eq!(
            out["Labels"]["com.zackees.bosn.run"], "r-1",
            "never forgeable"
        );
        assert_eq!(out["HostConfig"]["NanoCpus"], 4_000_000_000_i64);
        assert_eq!(out["HostConfig"]["Memory"], 1_i64 << 30);
        assert_eq!(out["HostConfig"]["Binds"][0], "/host/path:/w");
        assert_eq!(out["HostConfig"]["Binds"][1], "act-env:/var/run/act");
        assert_eq!(
            out["HostConfig"]["Mounts"][0]["Source"],
            "bosn-cache-x-toolcache-0"
        );
        let mounts = out["HostConfig"]["Mounts"].as_array().unwrap();
        assert_eq!(mounts.len(), 2, "an occupied target is not injected twice");
        assert_eq!(mounts[1]["Target"], "/cache");
    }

    #[test]
    fn a_smaller_or_explicit_cpu_request_is_kept() {
        let s = settings(Arc::new(NoVolumes));
        let small = rewrite(
            "container",
            serde_json::json!({"HostConfig": {"NanoCpus": 1_000_000_000_i64}}),
            &s,
        );
        assert_eq!(small["HostConfig"]["NanoCpus"], 1_000_000_000_i64);
        let quota = rewrite(
            "container",
            serde_json::json!({"HostConfig": {"CpuQuota": 50_000, "CpuPeriod": 100_000}}),
            &s,
        );
        assert!(quota["HostConfig"].get("NanoCpus").is_none());
        let bare = rewrite(
            "container",
            serde_json::json!({"Image": "x", "HostConfig": null}),
            &s,
        );
        assert_eq!(bare["HostConfig"]["NanoCpus"], 4_000_000_000_i64);
        let swap = rewrite(
            "container",
            serde_json::json!({"HostConfig": {"MemorySwap": 1024}}),
            &s,
        );
        assert_eq!(swap["HostConfig"]["MemorySwap"], 1_i64 << 30);
    }

    #[test]
    fn networks_and_volumes_are_labelled_and_volumes_mapped() {
        let s = settings(Arc::new(Renames(
            BTreeMap::from([("cache", "bosn-cache-cache-0")]),
            vec![],
        )));
        let network = rewrite("network", serde_json::json!({"Name": "act-net"}), &s);
        assert_eq!(network["Labels"]["com.zackees.bosn.run"], "r-1");
        let volume = rewrite(
            "volume",
            serde_json::json!({"Name": "cache", "Labels": null}),
            &s,
        );
        assert_eq!(volume["Name"], "bosn-cache-cache-0");
        assert_eq!(volume["Labels"]["com.zackees.bosn.run"], "r-1");
        // An empty body (docker volume create with defaults) still works.
        let out = rewrite_create("volume", b"", &s).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("r-1"));
        assert!(rewrite_create("container", b"[1]", &s).is_err());
        assert!(rewrite_create("container", b"{\"Labels\": 3}", &s).is_err());
    }

    #[test]
    fn create_targets_are_recognized_with_or_without_a_version_prefix() {
        assert_eq!(
            create_kind("POST", "/v1.47/containers/create?name=a"),
            Some("container")
        );
        assert_eq!(create_kind("POST", "/containers/create"), Some("container"));
        assert_eq!(
            create_kind("POST", "/v1.41/networks/create"),
            Some("network")
        );
        assert_eq!(create_kind("POST", "/volumes/create"), Some("volume"));
        assert_eq!(create_kind("GET", "/containers/create"), None);
        assert_eq!(create_kind("POST", "/containers/abc/start"), None);
        assert_eq!(create_kind("POST", "/vx/containers/create"), None);
    }

    #[test]
    fn concurrent_runs_cannot_see_or_reuse_each_others_container_names() {
        assert_eq!(
            suffix_name("/v1.47/containers/create?name=act-ci-build-1a2b", "b9"),
            "/v1.47/containers/create?name=act-ci-build-1a2b-b9"
        );
        assert_eq!(
            suffix_name("/containers/create?name=a-b9", "b9"),
            "/containers/create?name=a-b9",
            "idempotent"
        );
        assert_eq!(
            suffix_name("/containers/create", "b9"),
            "/containers/create"
        );
        assert_eq!(
            suffix_name("/containers/create?platform=linux%2Famd64&name=x", "b9"),
            "/containers/create?platform=linux%2Famd64&name=x-b9"
        );
        let scoped = scope_listing("/v1.47/containers/json?all=1", "r-1").unwrap();
        let (_, pairs) = query_pairs(&scoped);
        let filters: Value =
            serde_json::from_str(&pairs.iter().find(|(k, _)| k == "filters").unwrap().1).unwrap();
        assert_eq!(filters["label"][0], "com.zackees.bosn.run=r-1");
        let merged = scope_listing(
            &format!(
                "/containers/json?filters={}",
                crate::docker_api::encode(r#"{"name":["act-"],"label":["a=b"]}"#)
            ),
            "r-1",
        )
        .unwrap();
        let (_, pairs) = query_pairs(&merged);
        let filters: Value = serde_json::from_str(&pairs[0].1).unwrap();
        assert_eq!(filters["name"][0], "act-");
        assert_eq!(
            filters["label"],
            serde_json::json!(["a=b", "com.zackees.bosn.run=r-1"])
        );
        let legacy = scope_listing(
            &format!(
                "/containers/json?filters={}",
                crate::docker_api::encode(r#"{"label":{"a=b":true}}"#)
            ),
            "r-1",
        )
        .unwrap();
        let (_, pairs) = query_pairs(&legacy);
        let filters: Value = serde_json::from_str(&pairs[0].1).unwrap();
        assert_eq!(filters["label"]["com.zackees.bosn.run=r-1"], true);
        assert!(scope_listing("/containers/json?filters=%7Bbad", "r").is_err());
        assert_eq!(scope("GET", "/v1.41/containers/json?all=1"), Scope::Label);
        assert_eq!(scope("GET", "/containers/abc/json"), Scope::Forward);
        assert_eq!(decode("a%2Fb+c%"), "a/b c%");
    }
}
