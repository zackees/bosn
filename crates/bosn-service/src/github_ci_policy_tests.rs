use super::*;

fn policy(writes: GithubWrites) -> CiPolicy {
    CiPolicy {
        fake: FakeToken::for_test(7),
        repository: "obs-rust/obs-studio".into(),
        writes,
    }
}

#[test]
fn the_fake_token_has_192_bits_and_a_prefix_no_real_token_uses() {
    let fake = FakeToken::for_test(1);
    let value = fake.secret_value();
    assert!(value.starts_with(FAKE_TOKEN_PREFIX));
    assert_eq!(value.len(), FAKE_TOKEN_PREFIX.len() + 48);
    assert!(FakeToken::looks_fake(value));
    for real in [
        "ghp_0123456789abcdef0123456789abcdef0123",
        "gho_0123456789abcdef0123456789abcdef0123",
        "github_pat_11AAAAAAA0123456789",
        "0123456789abcdef0123456789abcdef01234567",
        "bosn-proxy-short",
        "bosn-proxy-ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ",
    ] {
        assert!(!FakeToken::looks_fake(real), "{real}");
    }
    assert_eq!(format!("{fake:?}"), "FakeToken(***)");
}

#[test]
fn only_this_runs_fake_token_is_authorized() {
    let p = policy(GithubWrites::Off);
    let fake = p.fake.secret_value().to_owned();
    assert_eq!(p.authorize(Some(&format!("Bearer {fake}"))), Ok(()));
    assert_eq!(p.authorize(Some(&format!("token {fake}"))), Ok(()));
    // A real token is refused: the container must never hold one.
    let foreign = p.authorize(Some("Bearer ghp_0123456789abcdef0123456789abcdef0123"));
    assert!(foreign.unwrap_err().contains("foreign token"));
    // Another run's fake token is refused too.
    let other = FakeToken::for_test(8);
    let other = p.authorize(Some(&format!("Bearer {}", other.secret_value())));
    assert!(other.unwrap_err().contains("another bosn run"));
    assert!(p.authorize(None).is_err());
    assert!(
        p.authorize(Some(&fake)).is_err(),
        "a bare token has no scheme"
    );
    assert!(p.authorize(Some(&format!("Bearer {fake}x"))).is_err());
}

#[test]
fn janitor_reads_are_forwarded_for_the_runs_own_repository_only() {
    let p = policy(GithubWrites::Off);
    for path in [
        "/repos/obs-rust/obs-studio/actions/caches",
        "/repos/OBS-Rust/OBS-Studio/actions/caches",
        "/repos/obs-rust/obs-studio/actions/cache/usage",
        "/repos/obs-rust/obs-studio/pulls/123",
        "/repos/obs-rust/obs-studio/pulls",
        // The read-only allowlist still applies, to any repository.
        "/repos/zackees/soldr/releases/latest",
        "/rate_limit",
    ] {
        assert_eq!(
            p.decide("GET", path, None, &[]),
            Decision::Forward,
            "{path}"
        );
    }
    for path in [
        "/repos/someone/else/actions/caches",
        "/repos/someone/else/pulls/1",
        "/repos/obs-rust/obs-studio/actions/secrets",
        "/repos/obs-rust/obs-studio/pulls/1/files/x",
        "/repos/obs-rust/obs-studio/actions/caches/../../hooks",
        "/user",
    ] {
        assert!(
            matches!(p.decide("GET", path, None, &[]), Decision::Refuse(403, _)),
            "{path}"
        );
    }
}

#[test]
fn graphql_queries_pass_and_mutations_do_not() {
    let p = policy(GithubWrites::On);
    let query = br#"{"query":"query { repository(owner:\"o\", name:\"r\") { pr1: pullRequest(number: 1) { state } } }"}"#;
    assert_eq!(p.decide("POST", "/graphql", None, query), Decision::Forward);
    let shorthand = br#"{"query":"{ viewer { login } }"}"#;
    assert_eq!(
        p.decide("POST", "/graphql", None, shorthand),
        Decision::Forward
    );
    for bad in [
        &br#"{"query":"mutation { deleteRef(input:{refId:\"x\"}) { clientMutationId } }"}"#[..],
        br#"{"query":"MUTATION{x}"}"#,
        br#"{"query":"query a { x } mutation b { y }"}"#,
        br#"{"query":"subscription { x }"}"#,
        br#"not json"#,
        br#"{"variables":{}}"#,
    ] {
        assert!(
            matches!(
                p.decide("POST", "/graphql", None, bad),
                Decision::Refuse(403, _)
            ),
            "{}",
            String::from_utf8_lossy(bad)
        );
    }
    let huge = format!(r#"{{"query":"{}"}}"#, "x".repeat(MAX_GRAPHQL_BODY));
    assert!(matches!(
        p.decide("POST", "/graphql", None, huge.as_bytes()),
        Decision::Refuse(403, _)
    ));
}

#[test]
fn cache_deletes_follow_the_writes_mode() {
    let by_id = "/repos/obs-rust/obs-studio/actions/caches/42";
    let by_key = "/repos/obs-rust/obs-studio/actions/caches";
    let off = policy(GithubWrites::Off);
    assert!(
        matches!(off.decide("DELETE", by_id, None, &[]), Decision::Refuse(403, m) if m.contains("writes are off"))
    );
    let dry = policy(GithubWrites::DryRun);
    assert_eq!(dry.decide("DELETE", by_id, None, &[]), Decision::DryRun);
    assert_eq!(
        dry.decide("DELETE", by_key, Some("key=abc&ref=refs/pull/1/merge"), &[]),
        Decision::DryRun
    );
    let on = policy(GithubWrites::On);
    assert_eq!(on.decide("DELETE", by_id, None, &[]), Decision::Forward);
}

#[test]
fn writes_outside_the_allowlist_are_refused_even_when_writes_are_on() {
    let on = policy(GithubWrites::On);
    for (method, path, query) in [
        ("DELETE", "/repos/someone/else/actions/caches/42", None),
        (
            "DELETE",
            "/repos/obs-rust/obs-studio/actions/caches/abc",
            None,
        ),
        ("DELETE", "/repos/obs-rust/obs-studio/actions/caches", None),
        (
            "DELETE",
            "/repos/obs-rust/obs-studio/actions/caches",
            Some("ref=x"),
        ),
        ("DELETE", "/repos/obs-rust/obs-studio", None),
        ("POST", "/repos/obs-rust/obs-studio/issues", None),
        ("PATCH", "/repos/obs-rust/obs-studio/pulls/1", None),
        ("PUT", "/repos/obs-rust/obs-studio/actions/caches/42", None),
        ("POST", "/repos/obs-rust/obs-studio/actions/caches", None),
    ] {
        assert!(
            matches!(
                on.decide(method, path, query, &[]),
                Decision::Refuse(403, _)
            ),
            "{method} {path}"
        );
    }
}

#[test]
fn writes_mode_parses_the_flag_values() {
    assert_eq!(GithubWrites::parse("off"), Ok(GithubWrites::Off));
    assert_eq!(GithubWrites::parse("dry-run"), Ok(GithubWrites::DryRun));
    assert_eq!(GithubWrites::parse("on"), Ok(GithubWrites::On));
    assert!(GithubWrites::parse("yes").is_err());
    assert_eq!(GithubWrites::default(), GithubWrites::Off);
    assert_eq!(
        serde_json::to_string(&GithubWrites::DryRun).unwrap(),
        "\"dry-run\""
    );
}
