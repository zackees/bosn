//! Unit tests for Act artifact acquisition.

use super::*;
use serde_json::json;
use std::{
    os::unix::fs::{PermissionsExt, symlink},
    sync::{Arc, Mutex},
};

#[derive(Clone)]
struct Reply {
    status: u16,
    bytes: Vec<u8>,
    location: Option<Vec<u8>>,
    length: Option<Vec<u8>>,
    encoding: Option<Vec<u8>>,
    chunk: usize,
}
impl Reply {
    fn bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            bytes: bytes.into(),
            location: None,
            length: None,
            encoding: None,
            chunk: 3,
        }
    }
    fn redirect(url: &str) -> Self {
        let mut value = Self::bytes(Vec::new());
        value.status = 302;
        value.location = Some(url.as_bytes().to_vec());
        value
    }
}
struct FakeBody {
    reply: Reply,
    offset: usize,
    cancel: Option<Arc<async_engine::CancellationSource>>,
}
impl Body for FakeBody {
    fn read<'a>(&'a mut self, buffer: &'a mut [u8]) -> Pending<'a, usize> {
        Box::pin(async move {
            let count = (self.reply.bytes.len() - self.offset)
                .min(self.reply.chunk)
                .min(buffer.len());
            buffer[..count].copy_from_slice(&self.reply.bytes[self.offset..self.offset + count]);
            self.offset += count;
            if let Some(cancel) = self.cancel.take() {
                cancel.cancel();
            }
            Ok(count)
        })
    }
}
#[derive(Default)]
struct FakeTransport {
    replies: Mutex<BTreeMap<String, Reply>>,
    calls: Mutex<Vec<(String, Option<String>)>>,
    cancel: Option<Arc<async_engine::CancellationSource>>,
}
impl Transport for FakeTransport {
    fn request<'a>(
        &'a self,
        url: &'a str,
        auth: Option<&'a str>,
        _ceiling: u64,
        _remaining: Duration,
    ) -> Pending<'a, Response> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push((url.into(), auth.map(str::to_owned)));
            let reply = self
                .replies
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .ok_or_else(|| refused("unexpected fixture network request"))?;
            Ok(Response {
                status: reply.status,
                location: reply.location.clone(),
                length: reply.length.clone(),
                encoding: reply.encoding.clone(),
                body: Box::new(FakeBody {
                    reply,
                    offset: 0,
                    cancel: self.cancel.clone(),
                }),
            })
        })
    }
}
fn runtime() -> async_engine::Runtime {
    async_engine::RuntimeBuilder::multi_thread()
        .enable_all()
        .build()
        .unwrap()
}
fn budget() -> Budget {
    Budget {
        deadline: Instant::now() + Duration::from_secs(10),
        cancellation: async_engine::CancellationSource::new().token(),
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    cache: PathBuf,
    manifest: String,
    config: String,
    archive: String,
    binary: String,
    binary_size: u64,
    layer: String,
    transport: FakeTransport,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let source = dir.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let binary = b"pinned synthetic Act binary bytes";
        std::fs::write(source.join("act"), binary).unwrap();
        std::fs::set_permissions(source.join("act"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let archive_file = dir.path().join("act.tar.gz");
        assert!(
            std::process::Command::new("tar")
                .args([
                    "--format=ustar",
                    "--mtime=@0",
                    "--owner=0",
                    "--group=0",
                    "-czf"
                ])
                .arg(&archive_file)
                .arg("-C")
                .arg(&source)
                .arg("act")
                .status()
                .unwrap()
                .success()
        );
        let archive = std::fs::read(archive_file).unwrap();
        let layer = b"verified base layer bytes";
        let layer_pin = digest(layer);
        let config=serde_json::to_vec(&json!({"architecture":"amd64","os":"linux","config":{"Volumes":{}},"rootfs":{"type":"layers","diff_ids":[layer_pin]}})).unwrap();
        let config_pin = digest(&config);
        let manifest=serde_json::to_vec(&json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":config_pin,"size":config.len()},"layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":layer_pin,"size":layer.len()}]})).unwrap();
        let manifest_pin = digest(&manifest);
        let transport = FakeTransport::default();
        transport.replies.lock().unwrap().extend([
            (
                TOKEN_URL.into(),
                Reply::bytes(br#"{"token":"anonymous-fixture-pull"}"#.to_vec()),
            ),
            (
                format!("{MANIFEST_URL}{manifest_pin}"),
                Reply::bytes(manifest),
            ),
            (format!("{BLOB_URL}{config_pin}"), Reply::bytes(config)),
            (
                format!("{BLOB_URL}{layer_pin}"),
                Reply::bytes(layer.to_vec()),
            ),
            (
                ACT_URL.into(),
                Reply::redirect("https://release-assets.githubusercontent.com/fixture"),
            ),
            (
                "https://release-assets.githubusercontent.com/fixture".into(),
                Reply::bytes(archive.clone()),
            ),
        ]);
        Self {
            dir,
            cache,
            manifest: manifest_pin,
            config: config_pin,
            archive: digest(&archive),
            binary: digest(binary),
            binary_size: binary.len() as u64,
            layer: layer_pin,
            transport,
        }
    }
    fn pins(&self) -> Pins<'_> {
        Pins {
            manifest: &self.manifest,
            config: &self.config,
            archive: &self.archive,
            binary: &self.binary,
            binary_size: self.binary_size,
        }
    }
}
#[test]
fn full_acquisition_streams_verified_graph_and_rehashes_offline_cache() {
    let fixture = Fixture::new();
    let cancellation = async_engine::CancellationSource::new();
    runtime().run(async {
        let artifacts = acquire(
            &fixture.transport,
            &fixture.cache,
            Default::default(),
            &cancellation.token(),
            &fixture.pins(),
        )
        .await
        .unwrap();
        assert_eq!(artifacts.package().runner_manifest_digest, fixture.manifest);
        assert_eq!(artifacts.package().runner_config_digest, fixture.config);
        assert_eq!(artifacts.package().binary_digest, fixture.binary);
        assert_eq!(artifacts.archive_blobs()[0].digest, fixture.layer);
        let mut archive = Vec::new();
        crate::act_archive::write_act_oci_archive(
            artifacts.package(),
            &artifacts.archive_blobs(),
            "bosn-act",
            2 << 30,
            &mut archive,
        )
        .unwrap();
        assert!(archive.windows(10).any(|v| v == b"oci-layout"));
        let calls = fixture.transport.calls.lock().unwrap().clone();
        assert!(
            calls
                .iter()
                .filter(|(url, _)| !url.starts_with("https://ghcr.io/"))
                .all(|(_, auth)| auth.is_none())
        );
        assert!(
            calls
                .iter()
                .filter(|(url, _)| url.starts_with(BLOB_URL))
                .all(|(_, auth)| auth.as_deref() == Some("Bearer anonymous-fixture-pull"))
        );
        fixture.transport.replies.lock().unwrap().clear();
        let count = calls.len();
        let cached = acquire(
            &fixture.transport,
            &fixture.cache,
            Default::default(),
            &cancellation.token(),
            &fixture.pins(),
        )
        .await
        .unwrap();
        assert_eq!(cached.package(), artifacts.package());
        assert_eq!(fixture.transport.calls.lock().unwrap().len(), count);
        let layer = fixture.cache.join(digest_name(&fixture.layer).unwrap());
        let corrupt = vec![b'!'; std::fs::metadata(&layer).unwrap().len() as usize];
        std::fs::write(&layer, &corrupt).unwrap();
        assert!(
            acquire(
                &fixture.transport,
                &fixture.cache,
                Default::default(),
                &cancellation.token(),
                &fixture.pins()
            )
            .await
            .is_err()
        );
        assert_eq!(
            fixture.transport.calls.lock().unwrap().len(),
            count,
            "corrupt cache cannot fall back to network"
        );
        assert_eq!(std::fs::read(layer).unwrap(), corrupt);
    });
}
#[test]
fn redirects_are_bounded_and_never_restore_registry_authorization() {
    let transport = FakeTransport::default();
    transport.replies.lock().unwrap().extend([
        (
            "https://ghcr.io/first".into(),
            Reply::redirect("https://pkg-containers.githubusercontent.com/second"),
        ),
        (
            "https://pkg-containers.githubusercontent.com/second".into(),
            Reply::redirect("https://ghcr.io/final"),
        ),
        ("https://ghcr.io/final".into(), Reply::bytes(b"ok".to_vec())),
    ]);
    runtime().run(async {
        let response = open_response(
            &transport,
            "https://ghcr.io/first",
            Some("Bearer owned-public-token"),
            2,
            &budget(),
        )
        .await
        .unwrap();
        let mut bytes = Vec::new();
        copy_body(response, &mut bytes, Some(2), 2, None, &budget())
            .await
            .unwrap();
        assert_eq!(bytes, b"ok");
    });
    let calls = transport.calls.lock().unwrap();
    assert!(calls[0].1.is_some());
    assert!(calls[1].1.is_none());
    assert!(calls[2].1.is_none());
    drop(calls);
    for bad in [
        "http://ghcr.io/insecure",
        "https://evil.invalid/blob",
        "https://ghcr.io@evil.invalid/blob",
        "https://ghcr.io:443/blob",
        "https://github.com/blob#fragment",
        "https://github.com/\\evil",
    ] {
        assert!(publisher_host(bad).is_err(), "{bad}");
    }
    transport.replies.lock().unwrap().insert(
        "https://ghcr.io/loop".into(),
        Reply::redirect("https://ghcr.io/loop"),
    );
    runtime().run(async {
        assert!(
            open_response(&transport, "https://ghcr.io/loop", None, 1, &budget())
                .await
                .is_err()
        );
    });
    assert_eq!(
        transport
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(url, _)| url.ends_with("/loop"))
            .count(),
        4
    );
}
#[test]
fn body_size_hash_length_and_cancellation_refuse_publication() {
    for mode in ["hash", "over", "short", "length", "cancel"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("cache");
        let owner = private_cache(&root).unwrap();
        let transport = FakeTransport::default();
        let mut reply = Reply::bytes(b"abc".to_vec());
        let source = Arc::new(async_engine::CancellationSource::new());
        let pin = if mode == "hash" {
            digest(b"xyz")
        } else {
            digest(b"abc")
        };
        let (size, ceiling) = match mode {
            "over" => (None, 2),
            "short" => (Some(4), 4),
            _ => (Some(3), 3),
        };
        if mode == "length" {
            reply.length = Some(b"2".to_vec());
        }
        transport
            .replies
            .lock()
            .unwrap()
            .insert("https://github.com/test".into(), reply);
        let transport = FakeTransport {
            cancel: if mode == "cancel" {
                Some(source.clone())
            } else {
                None
            },
            ..transport
        };
        let budget = Budget {
            deadline: Instant::now() + Duration::from_secs(10),
            cancellation: source.token(),
        };
        runtime().run(async {
            assert!(
                fetch_cached(
                    &transport,
                    &root,
                    owner,
                    "https://github.com/test",
                    None,
                    ArtifactSpec {
                        pin: &pin,
                        size,
                        ceiling
                    },
                    &budget
                )
                .await
                .is_err(),
                "{mode}"
            );
        });
        assert!(!root.join(digest_name(&pin).unwrap()).exists());
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            0,
            "staging cleanup after {mode}"
        );
    }
}
#[test]
fn cache_ownership_and_exclusive_lock_refuse_foreign_paths() {
    let fixture = Fixture::new();
    let owner = private_cache(&fixture.cache).unwrap();
    let file = fixture.dir.path().join("foreign");
    std::fs::write(&file, b"foreign bytes").unwrap();
    let link = fixture.cache.join(digest_name(&fixture.manifest).unwrap());
    symlink(&file, &link).unwrap();
    assert!(verified_file(&link, owner, &fixture.manifest, None, JSON).is_err());
    assert_eq!(std::fs::read(&file).unwrap(), b"foreign bytes");
    let alias = fixture.dir.path().join("alias");
    symlink(&fixture.cache, &alias).unwrap();
    assert!(private_cache(&alias).is_err());
    let parent_alias = fixture.dir.path().join("parent-alias");
    symlink(fixture.dir.path(), &parent_alias).unwrap();
    assert!(private_cache(&parent_alias.join("new-cache")).is_err());
    let lock = fs::try_lock_exclusive_owned(
        fs::open_lock_file(&fixture.cache.join(".acquire.lock")).unwrap(),
    )
    .unwrap();
    let cancellation = async_engine::CancellationSource::new();
    runtime().run(async {
        assert!(
            acquire(
                &fixture.transport,
                &fixture.cache,
                Default::default(),
                &cancellation.token(),
                &fixture.pins()
            )
            .await
            .is_err()
        );
    });
    assert!(fixture.transport.calls.lock().unwrap().is_empty());
    drop(lock);
}
#[test]
fn proxy_and_publisher_failures_do_not_expose_ambient_credentials() {
    for name in [
        "HTTP_PROXY",
        "http_proxy",
        "hTtPs_PrOxY",
        "ALL_PROXY",
        "all_proxy",
    ] {
        let failure =
            proxy_environment([(name.into(), "https://user:secret@proxy.invalid".into())])
                .unwrap_err();
        assert!(!failure.to_string().contains("secret"));
    }
    proxy_environment([
        ("NO_PROXY".into(), "localhost".into()),
        ("HTTP_PROXY".into(), "".into()),
        ("GITHUB_TOKEN".into(), "secret".into()),
    ])
    .unwrap();
    let transport = FakeTransport::default();
    let mut reply = Reply::bytes(Vec::new());
    reply.status = 401;
    transport
        .replies
        .lock()
        .unwrap()
        .insert(TOKEN_URL.into(), reply);
    runtime().run(async {
        assert!(
            open_response(&transport, TOKEN_URL, None, TOKEN, &budget())
                .await
                .is_err()
        );
    });
    assert!(transport.calls.lock().unwrap()[0].1.is_none());
}
#[test]
fn verified_archive_still_refuses_duplicate_link_traversal_and_bomb() {
    for mode in ["duplicate", "symlink", "traversal", "oversized"] {
        let mut fixture = Fixture::new();
        let source = fixture.dir.path().join("source");
        if mode == "symlink" {
            std::fs::rename(source.join("act"), source.join("original")).unwrap();
            symlink("../foreign", source.join("act")).unwrap();
        }
        if mode == "oversized" {
            std::fs::File::create(source.join("act"))
                .unwrap()
                .set_len(BINARY + 1)
                .unwrap();
        }
        let archive_file = fixture.dir.path().join("unsafe.tar.gz");
        let mut command = std::process::Command::new("tar");
        command
            .args([
                "--format=ustar",
                "--mtime=@0",
                "--owner=0",
                "--group=0",
                "-czf",
            ])
            .arg(&archive_file)
            .arg("-C")
            .arg(&source);
        if mode == "traversal" {
            command.arg("--transform=s,^act$,../escape,");
        }
        command.arg("act");
        if mode == "duplicate" {
            command.arg("act");
        }
        assert!(command.status().unwrap().success());
        let archive = std::fs::read(archive_file).unwrap();
        fixture.archive = digest(&archive);
        fixture.transport.replies.lock().unwrap().insert(
            "https://release-assets.githubusercontent.com/fixture".into(),
            Reply::bytes(archive),
        );
        let cancellation = async_engine::CancellationSource::new();
        runtime().run(async {
            assert!(
                acquire(
                    &fixture.transport,
                    &fixture.cache,
                    Default::default(),
                    &cancellation.token(),
                    &fixture.pins()
                )
                .await
                .is_err(),
                "{mode}"
            );
        });
        assert!(
            !fixture
                .cache
                .join(digest_name(&fixture.binary).unwrap())
                .exists(),
            "{mode}"
        );
        assert!(!fixture.dir.path().join("escape").exists());
        assert!(
            std::fs::read_dir(&fixture.cache).unwrap().all(|e| !e
                .unwrap()
                .file_type()
                .unwrap()
                .is_dir()),
            "extractor joined before staging cleanup"
        );
        assert!(
            !fixture
                .transport
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|(url, _)| url.ends_with(&fixture.layer)),
            "invalid archive cannot fetch runner layers"
        );
    }
}
struct BlockedBody;
impl Body for BlockedBody {
    fn read<'a>(&'a mut self, _buffer: &'a mut [u8]) -> Pending<'a, usize> {
        Box::pin(std::future::pending())
    }
}
struct BlockedTransport;
impl Transport for BlockedTransport {
    fn request<'a>(
        &'a self,
        _url: &'a str,
        _auth: Option<&'a str>,
        _ceiling: u64,
        _remaining: Duration,
    ) -> Pending<'a, Response> {
        Box::pin(std::future::pending())
    }
}
#[test]
fn cancellation_interrupts_blocked_request_and_body_and_deadline_refuses() {
    runtime().run(async {
        for phase in ["request", "body"] {
            let source = async_engine::CancellationSource::new();
            let budget = Budget {
                deadline: Instant::now() + Duration::from_secs(10),
                cancellation: source.token(),
            };
            let cancel = async_engine::launch(async move {
                async_engine::sleep(Duration::from_millis(20)).await;
                source.cancel();
            });
            let mut output = Vec::new();
            let operation = async {
                if phase == "request" {
                    open_response(&BlockedTransport, ACT_URL, None, 8, &budget)
                        .await
                        .map(|_| ())
                } else {
                    copy_body(
                        Response {
                            status: 200,
                            location: None,
                            length: None,
                            encoding: None,
                            body: Box::new(BlockedBody),
                        },
                        &mut output,
                        None,
                        8,
                        None,
                        &budget,
                    )
                    .await
                    .map(|_| ())
                }
            };
            let error = async_engine::timeout(Duration::from_secs(2), operation)
                .await
                .expect("token must interrupt blocked HTTP before overall deadline")
                .unwrap_err();
            assert!(error.to_string().contains("cancelled"));
            assert!(output.is_empty());
            cancel.await.unwrap();
        }
        let source = async_engine::CancellationSource::new();
        let budget = Budget {
            deadline: Instant::now() + Duration::from_millis(20),
            cancellation: source.token(),
        };
        let error = open_response(&BlockedTransport, ACT_URL, None, 8, &budget)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("deadline"));
    });
}
#[test]
fn aggregate_descriptor_ceiling_refuses_before_blob_download_or_allocation() {
    let mut fixture = Fixture::new();
    let mut manifest: Value = serde_json::from_slice(
        &fixture.transport.replies.lock().unwrap()[&format!("{MANIFEST_URL}{}", fixture.manifest)]
            .bytes,
    )
    .unwrap();
    manifest["layers"][0]["size"] = json!(AGGREGATE + 1);
    let bytes = serde_json::to_vec(&manifest).unwrap();
    fixture.manifest = digest(&bytes);
    fixture.transport.replies.lock().unwrap().insert(
        format!("{MANIFEST_URL}{}", fixture.manifest),
        Reply::bytes(bytes),
    );
    let cancellation = async_engine::CancellationSource::new();
    runtime().run(async {
        let result = acquire(
            &fixture.transport,
            &fixture.cache,
            Default::default(),
            &cancellation.token(),
            &fixture.pins(),
        )
        .await;
        let failure = result.err().expect("oversized descriptors must refuse");
        assert!(failure.to_string().contains("aggregate layer bytes"));
    });
    assert!(
        !fixture
            .transport
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(url, _)| url.ends_with(&fixture.layer))
    );
    assert!(
        !fixture
            .cache
            .join(digest_name(&fixture.layer).unwrap())
            .exists()
    );
}
/// Explicit network evidence only. The operator supplies a fresh owned
/// cache; downloaded bytes remain there and are never automatically removed.
#[test]
#[ignore = "requires explicit private BOSN_ACT_ARTIFACT_PROBE_CACHE and publisher network access"]
fn real_fixed_publisher_acquisition_and_cached_reread() {
    assert_eq!(
        std::env::consts::ARCH,
        "x86_64",
        "native Linux AMD64 acquisition only"
    );
    let root = PathBuf::from(
        std::env::var_os("BOSN_ACT_ARTIFACT_PROBE_CACHE")
            .expect("explicit private artifact cache path required"),
    );
    assert!(root.is_absolute(), "artifact probe cache must be absolute");
    match std::fs::symlink_metadata(&root) {
        Ok(metadata) => {
            assert!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "fresh real directory required"
            );
            assert_eq!(
                std::fs::read_dir(&root).unwrap().count(),
                0,
                "real acquisition requires a fresh empty cache"
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => panic!("artifact cache metadata unavailable: {error}"),
    }
    let cancellation = async_engine::CancellationSource::new();
    runtime().run(async {
        let first=acquire_pinned_act_artifacts(&root,Default::default(),&cancellation.token()).await.expect("fixed publisher acquisition refused");
        assert_eq!(first.package().runner_manifest_digest,RUNNER);
        assert_eq!(first.package().runner_config_digest,CONFIG);
        assert_eq!(first.package().binary_digest,ACT_BINARY);
        assert_eq!(first.receipt().act_archive_digest,ACT_ARCHIVE);
        assert_eq!(first.receipt().unique_layer_bytes,546285712);
        assert_eq!(first.receipt().unique_layer_count,6);
        let package=first.package().clone();
        let receipt=serde_json::to_value(first.receipt()).unwrap();
        let identities=first.archive_blobs().iter().map(|b| (b.digest.to_owned(),digest(b.bytes),b.bytes.len())).collect::<Vec<_>>();
        drop(first); // Do not hold two complete compressed graphs at once.
        let cached=acquire_pinned_act_artifacts(&root,Default::default(),&cancellation.token()).await.expect("verified cached reread refused");
        assert_eq!(cached.package(),&package);
        assert_eq!(serde_json::to_value(cached.receipt()).unwrap(),receipt);
        assert_eq!(cached.archive_blobs().iter().map(|b| (b.digest.to_owned(),digest(b.bytes),b.bytes.len())).collect::<Vec<_>>(),identities);
        let archive_bytes=crate::act_archive::write_act_oci_archive(cached.package(),&cached.archive_blobs(),"bosn-act",2<<30,&mut std::io::sink()).unwrap();
        println!("{}",serde_json::to_string(&json!({"scope":"fixed publisher acquisition and OCI graph validation only","receipt":receipt,"oci_archive_bytes":archive_bytes,"cache_retained":true})).unwrap());
    });
}
