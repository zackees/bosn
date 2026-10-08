//! Read shared routing immediately before execution; malformed is never absent.
use super::{CONTROL_DEADLINE, DockerActBackend};
use crate::ci::{
    cache_cohort::{CacheRoute, Namespace},
    cache_policy::CachePolicy,
    cache_routing::RoutingRecord,
};

impl DockerActBackend {
    pub(super) async fn published_cache_route(
        &self,
        engine: &str,
        namespace: &Namespace,
        policy: CachePolicy,
    ) -> Result<CacheRoute, String> {
        let script = read_script(namespace);
        let output = self
            .checked(
                "shared cache routing",
                Self::exec(engine, &script),
                CONTROL_DEADLINE,
            )
            .await?;
        let route = decode(&output, namespace, policy)?;
        if matches!(route, CacheRoute::Cohort { .. }) {
            self.require_cache_policy(engine, policy).await?;
        }
        Ok(route)
    }
}

fn read_script(namespace: &Namespace) -> String {
    let record = namespace.routing_record_path();
    format!(
        r#"set -eu
record={record}
directory=${{record%/*}}
parent=${{directory%/*}}
[ ! -L "$parent" ] && [ ! -L "$directory" ] && [ ! -L "$record" ] || exit 78
if [ ! -e "$record" ]; then
    [ ! -e "$directory" ] || [ -d "$directory" ] || exit 78
    printf 'absent'
    exit 0
fi
[ -f "$record" ] || exit 78
printf 'present\n'
head -c 4097 "$record"
printf '\nend'
"#
    )
}

fn decode(output: &str, namespace: &Namespace, policy: CachePolicy) -> Result<CacheRoute, String> {
    if output == "absent" {
        return Ok(CacheRoute::Legacy(namespace.clone()));
    }
    let record = output
        .strip_prefix("present\n")
        .and_then(|body| body.strip_suffix("\nend"))
        .ok_or("shared cache route read is incomplete")?;
    RoutingRecord::parse(record.as_bytes(), namespace, policy)?;
    Ok(CacheRoute::Cohort {
        namespace: namespace.clone(),
        policy,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn published_routing_selects_cohort_and_invalid_publication_never_falls_back() {
        let directory = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let cache = directory.path().to_str().unwrap();
        let transport = "import subprocess,sys\nscript=sys.argv[-1].replace('/bosn/cache',sys.argv[1])\nsys.exit(subprocess.run(['sh','-ec',script]).returncode)\n";
        let backend = DockerActBackend::new(bosn_engine::DockerEngine::synthetic_for_test(
            "python3",
            ["-c", transport, cache],
        ));
        let namespace = Namespace::parse("0123456789abcdef").unwrap();
        let policy = CachePolicy::default();
        kernal_api::async_engine::RuntimeBuilder::multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                assert!(matches!(
                    backend
                        .published_cache_route("fixture", &namespace, policy)
                        .await
                        .unwrap(),
                    CacheRoute::Legacy(_)
                ));
                backend.agree_cache_policy("fixture", policy).await.unwrap();
                let routes = directory.path().join("actcache/.bosn-cohort-routes-v1");
                std::fs::create_dir(&routes).unwrap();
                let record = routes.join(namespace.as_str());
                let encoded = RoutingRecord::new(&namespace, policy, &"a".repeat(64))
                    .unwrap()
                    .encode()
                    .unwrap();
                std::fs::write(&record, &encoded).unwrap();
                assert!(matches!(
                    backend
                        .published_cache_route("fixture", &namespace, policy)
                        .await
                        .unwrap(),
                    CacheRoute::Cohort { .. }
                ));
                for invalid in [b"invalid".to_vec(), vec![b'x'; 4097]] {
                    std::fs::write(&record, invalid).unwrap();
                    assert!(
                        backend
                            .published_cache_route("fixture", &namespace, policy)
                            .await
                            .is_err()
                    );
                }
                std::fs::remove_file(&record).unwrap();
                std::os::unix::fs::symlink(routes.join("missing"), &record).unwrap();
                assert!(
                    backend
                        .published_cache_route("fixture", &namespace, policy)
                        .await
                        .is_err()
                );
            });
    }
}
