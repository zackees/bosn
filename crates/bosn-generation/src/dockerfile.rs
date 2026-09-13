//! Pure Dockerfile context and external-image selection.  Input is an already
//! observed tree; this module never opens a host path.
use crate::{ContextEntry, ExternalImageIdentity};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DockerfileError {
    MissingDockerfile(String),
    InvalidUtf8(String),
    Parse(String),
    MissingSource(String),
    UnpinnedRemote(String),
    ResourceLimit(String),
}
impl std::fmt::Display for DockerfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingDockerfile(x) => write!(f, "Dockerfile {x:?} does not exist"),
            Self::InvalidUtf8(x) => write!(f, "Dockerfile {x:?} is not UTF-8"),
            Self::Parse(x) => f.write_str(x),
            Self::MissingSource(x) => write!(f, "Docker build source {x:?} does not exist"),
            Self::UnpinnedRemote(x) => f.write_str(x),
            Self::ResourceLimit(x) => {
                write!(f, "Docker context matcher resource limit exceeded: {x}")
            }
        }
    }
}
impl std::error::Error for DockerfileError {}

#[derive(Clone, Debug)]
struct Rule {
    pattern: String,
    negated: bool,
}
type CopyFlags = std::collections::BTreeMap<String, Option<String>>;
pub fn select_context(
    entries: &[ContextEntry],
    dockerfile: &str,
) -> Result<Vec<ContextEntry>, DockerfileError> {
    let df = file(entries, dockerfile)
        .ok_or_else(|| DockerfileError::MissingDockerfile(dockerfile.into()))?;
    let text =
        std::str::from_utf8(df).map_err(|_| DockerfileError::InvalidUtf8(dockerfile.into()))?;
    let specific = format!("{dockerfile}.dockerignore");
    let ignore = if let Some(b) = file(entries, &specific) {
        Some((specific.as_str(), b))
    } else {
        file(entries, ".dockerignore").map(|b| (".dockerignore", b))
    };
    let rules = match ignore.as_ref() {
        Some((_, b)) => std::str::from_utf8(b)
            .map(ignore_rules)
            .map_err(|_| DockerfileError::InvalidUtf8(".dockerignore".into()))?,
        None => Vec::new(),
    };
    let (sources, all) = local_sources(text)?;
    let mut selected = Vec::new();
    for e in entries {
        let p = path(e);
        let protected = p == dockerfile || ignore.as_ref().is_some_and(|(x, _)| p == *x);
        if protected || ((all || any_source_matches(&sources, p)?) && !ignored(&rules, p)?) {
            selected.push(e.clone());
        }
    }
    for source in sources {
        if !entries
            .iter()
            .map(|e| source_matches(&source, path(e)))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .any(|x| x)
        {
            return Err(DockerfileError::MissingSource(source));
        }
    }
    selected.sort_by(|a, b| path(a).cmp(path(b)));
    Ok(selected)
}
pub fn external_images(text: &str) -> Result<Vec<ExternalImageIdentity>, DockerfileError> {
    let mut args = std::collections::BTreeMap::new();
    let mut aliases = std::collections::BTreeSet::new();
    let mut staged = false;
    let mut out = Vec::new();
    for line in logical_lines(text) {
        let (op, body) = split_op(&line);
        if op == "ARG" && !staged {
            let (name, value) = body.split_once('=').unwrap_or((body, ""));
            if valid_arg(name) && body.contains('=') {
                args.insert(name.into(), expand(value, &args, true)?);
            }
            continue;
        }
        if op == "FROM" {
            let mut words = words(body)?;
            let mut platform = None;
            while words.first().is_some_and(|x| x.starts_with("--")) {
                let f = words.remove(0);
                if let Some(v) = f.strip_prefix("--platform=") {
                    platform = Some(expand(v, &args, true)?);
                }
            }
            if words.is_empty() {
                return Err(DockerfileError::Parse(
                    "FROM needs an image reference".into(),
                ));
            }
            let image = expand(&words[0], &args, true)?;
            if !stage(&image, &aliases) && !image.eq_ignore_ascii_case("scratch") {
                out.push(ExternalImageIdentity {
                    reference: image.clone(),
                    platform,
                    identity: None,
                });
            }
            if words.len() == 3 && words[1].eq_ignore_ascii_case("as") {
                aliases.insert(words[2].to_ascii_lowercase());
            } else if words.len() > 1 {
                return Err(DockerfileError::Parse("cannot parse FROM".into()));
            }
            staged = true;
            continue;
        }
        if op == "COPY" || op == "ADD" {
            let (_, flags) = copy(body, &op)?;
            if let Some(Some(from)) = flags.get("from") {
                let image = expand(from, &args, false)?;
                if !stage(&image, &aliases) && !image.eq_ignore_ascii_case("scratch") {
                    out.push(ExternalImageIdentity {
                        reference: image,
                        platform: None,
                        identity: None,
                    });
                }
            }
            continue;
        }
        if op == "RUN" {
            for mount in mounts(body)? {
                if let Some(from) = mount.get("from") {
                    let image = expand(from, &args, true)?;
                    if !stage(&image, &aliases) && !image.eq_ignore_ascii_case("scratch") {
                        out.push(ExternalImageIdentity {
                            reference: image,
                            platform: None,
                            identity: None,
                        });
                    }
                }
            }
        }
    }
    Ok(out)
}
fn local_sources(text: &str) -> Result<(Vec<String>, bool), DockerfileError> {
    let mut out = Vec::new();
    let mut all = false;
    for line in logical_lines(text) {
        let (op, body) = split_op(&line);
        if op == "COPY" || op == "ADD" {
            let (tokens, flags) = copy(body, &op)?;
            if flags.contains_key("from") {
                if flags["from"].as_deref().is_none_or(str::is_empty) {
                    return Err(DockerfileError::Parse(format!(
                        "{op} --from needs an image or stage"
                    )));
                }
                continue;
            }
            for s in &tokens[..tokens.len() - 1] {
                if s.starts_with("<<") {
                    continue;
                }
                if s.contains('$') {
                    all = true;
                    continue;
                }
                if op == "ADD" && remote(s) {
                    if git(s) && !full_commit(s) {
                        return Err(DockerfileError::UnpinnedRemote(format!(
                            "Git ADD source {s:?} needs a full commit reference"
                        )));
                    }
                    if !git(s)
                        && flags
                            .get("checksum")
                            .and_then(|x| x.as_deref())
                            .is_none_or(str::is_empty)
                    {
                        return Err(DockerfileError::UnpinnedRemote(format!(
                            "remote ADD source {s:?} needs --checksum"
                        )));
                    }
                    continue;
                }
                out.push(s.clone())
            }
        } else if op == "RUN" {
            for m in mounts(body)? {
                if m.get("type").is_some_and(|x| x != "bind") || m.contains_key("from") {
                    continue;
                }
                let s = m
                    .get("source")
                    .or(m.get("src"))
                    .cloned()
                    .unwrap_or_else(|| ".".into());
                if s.contains('$') {
                    all = true
                } else {
                    out.push(s)
                }
            }
        }
    }
    Ok((out, all))
}
fn copy(body: &str, op: &str) -> Result<(Vec<String>, CopyFlags), DockerfileError> {
    let mut b = body.trim();
    let mut flags = std::collections::BTreeMap::new();
    while b.starts_with("--") {
        let end = b
            .find(char::is_whitespace)
            .ok_or_else(|| DockerfileError::Parse(format!("{op} has no source or destination")))?;
        let f = &b[2..end];
        let (k, v) = f
            .split_once('=')
            .map_or((f, None), |(a, b)| (a, Some(b.into())));
        flags.insert(k.to_ascii_lowercase(), v);
        b = b[end..].trim();
    }
    let tokens = if b.starts_with('[') {
        json_strings(b)?
    } else {
        words(b)?
    };
    if tokens.len() < 2 {
        return Err(DockerfileError::Parse(format!(
            "{op} needs a source and destination"
        )));
    }
    Ok((tokens, flags))
}
fn mounts(body: &str) -> Result<Vec<std::collections::BTreeMap<String, String>>, DockerfileError> {
    let mut out = Vec::new();
    for w in words(body)? {
        if let Some(v) = w.strip_prefix("--mount=") {
            let mut m = std::collections::BTreeMap::new();
            for x in v.split(',') {
                let (k, v) = x.split_once('=').unwrap_or((x, "true"));
                m.insert(k.to_ascii_lowercase(), v.into());
            }
            out.push(m)
        } else if !w.starts_with("--") {
            break;
        }
    }
    Ok(out)
}
fn ignore_rules(b: &str) -> Vec<Rule> {
    b.lines()
        .filter_map(|raw| {
            let mut x = raw.trim();
            if x.is_empty() || x.starts_with('#') || x == "." {
                return None;
            }
            let n = x.starts_with('!');
            if n {
                x = &x[1..]
            }
            let x = x.replace('\\', "/").trim_matches('/').to_owned();
            (!x.is_empty()).then_some(Rule {
                pattern: x,
                negated: n,
            })
        })
        .collect()
}
fn ignored(rules: &[Rule], p: &str) -> Result<bool, DockerfileError> {
    let mut yes = false;
    for r in rules {
        if glob(&r.pattern, p)?
            || p.split('/')
                .scan(String::new(), |a, x| {
                    if !a.is_empty() {
                        a.push('/')
                    }
                    a.push_str(x);
                    Some(a.clone())
                })
                .take(p.matches('/').count())
                .map(|x| glob(&r.pattern, &x))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .any(|x| x)
        {
            yes = !r.negated
        }
    }
    Ok(yes)
}
fn any_source_matches(sources: &[String], p: &str) -> Result<bool, DockerfileError> {
    for s in sources {
        if source_matches(s, p)? {
            return Ok(true);
        }
    }
    Ok(false)
}
fn source_matches(source: &str, p: &str) -> Result<bool, DockerfileError> {
    let normalized = source.replace('\\', "/");
    let mut parts = Vec::new();
    for x in normalized.trim_start_matches('/').split('/') {
        match x {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            x => parts.push(x),
        }
    }
    let s = parts.join("/");
    if s.is_empty() || s == "." {
        return Ok(true);
    }
    if s.contains(['*', '?', '[']) {
        let direct = glob(&s, p)?;
        let prefix = p
            .split('/')
            .scan(String::new(), |a, x| {
                if !a.is_empty() {
                    a.push('/')
                }
                a.push_str(x);
                Some(a.clone())
            })
            .map(|x| glob(&s, &x))
            .collect::<Result<Vec<_>, _>>()
            .map(|x| x.into_iter().any(|x| x))?;
        Ok(direct || prefix)
    } else {
        Ok(p == s || p.starts_with(&(s + "/")))
    }
}
fn glob(p: &str, s: &str) -> Result<bool, DockerfileError> {
    if p.len() > 4_096 || s.len() > 4_000_000 {
        return Err(DockerfileError::ResourceLimit(
            "pattern or path input".into(),
        ));
    }
    let tokens = glob_tokens(p);
    let class_members = tokens
        .iter()
        .try_fold(0_usize, |total, token| match token {
            Glob::Class { set, .. } => total.checked_add(set.len()),
            _ => Some(total),
        })
        .ok_or_else(|| DockerfileError::ResourceLimit("class members".into()))?;
    if tokens
        .len()
        .checked_add(class_members)
        .is_none_or(|n| n > 1_000_000)
    {
        return Err(DockerfileError::ResourceLimit(
            "pattern or path input".into(),
        ));
    }
    let text: Vec<char> = s.chars().collect();
    // This is an explicit resource bound, not a timeout: no recursive calls
    // or unbounded backtracking are possible for hostile manifest input.
    let states = (tokens.len() + 1).checked_mul(text.len() + 1);
    let class_work = class_members.checked_mul(text.len() + 1);
    if states
        .zip(class_work)
        .and_then(|(states, classes)| states.checked_add(classes))
        .is_none_or(|n| n > 1_000_000)
    {
        return Err(DockerfileError::ResourceLimit("DP states".into()));
    }
    let n = text.len();
    let mut dp = vec![false; (tokens.len() + 1) * (n + 1)];
    let at = |i: usize, j: usize| i * (n + 1) + j;
    dp[at(tokens.len(), n)] = true;
    for i in (0..tokens.len()).rev() {
        let mut slash_suffix = vec![false; n + 1];
        for j in (0..n).rev() {
            slash_suffix[j] = slash_suffix[j + 1] || (text[j] == '/' && dp[at(i + 1, j + 1)]);
        }
        for j in (0..=n).rev() {
            let value = match &tokens[i] {
                Glob::Star => dp[at(i + 1, j)] || (j < n && text[j] != '/' && dp[at(i, j + 1)]),
                Glob::DoubleStar => dp[at(i + 1, j)] || (j < n && dp[at(i, j + 1)]),
                Glob::DoubleStarSlash => dp[at(i + 1, j)] || slash_suffix[j],
                Glob::One => j < n && text[j] != '/' && dp[at(i + 1, j + 1)],
                Glob::Class { set, negated } => {
                    j < n
                        && text[j] != '/'
                        && (class_hit(set, text[j]) != *negated)
                        && dp[at(i + 1, j + 1)]
                }
                Glob::Literal(c) => j < n && *c == text[j] && dp[at(i + 1, j + 1)],
            };
            dp[at(i, j)] = value;
        }
    }
    Ok(dp[0])
}
#[derive(Clone)]
enum Glob {
    Star,
    DoubleStar,
    DoubleStarSlash,
    One,
    Class {
        set: Vec<(char, Option<char>)>,
        negated: bool,
    },
    Literal(char),
}
fn glob_tokens(pattern: &str) -> Vec<Glob> {
    let c: Vec<char> = pattern.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '*' if i + 1 < c.len() && c[i + 1] == '*' => {
                i += 2;
                if i < c.len() && c[i] == '/' {
                    out.push(Glob::DoubleStarSlash);
                    i += 1
                } else {
                    out.push(Glob::DoubleStar)
                }
            }
            '*' => {
                out.push(Glob::Star);
                i += 1
            }
            '?' => {
                out.push(Glob::One);
                i += 1
            }
            '[' => {
                if let Some(end) = c[i + 1..].iter().position(|x| *x == ']') {
                    let mut x = &c[i + 1..i + 1 + end];
                    let neg = x.first() == Some(&'!');
                    if neg {
                        x = &x[1..]
                    }
                    let mut set = Vec::new();
                    let mut k = 0;
                    while k < x.len() {
                        if k + 2 < x.len() && x[k + 1] == '-' {
                            set.push((x[k], Some(x[k + 2])));
                            k += 3
                        } else {
                            set.push((x[k], None));
                            k += 1
                        }
                    }
                    out.push(Glob::Class { set, negated: neg });
                    i += end + 2
                } else {
                    out.push(Glob::Literal('['));
                    i += 1
                }
            }
            x => {
                out.push(Glob::Literal(x));
                i += 1
            }
        }
    }
    out
}
fn class_hit(set: &[(char, Option<char>)], c: char) -> bool {
    set.iter()
        .any(|(a, b)| b.is_some_and(|z| *a <= c && c <= z) || b.is_none() && *a == c)
}
fn logical_lines(text: &str) -> Vec<String> {
    let mut esc = "\\";
    for x in text.lines() {
        let t = x.trim();
        if let Some(v) = t.strip_prefix("# escape=") {
            if v == "\\" || v == "`" {
                esc = v
            }
            break;
        }
        if !t.is_empty() && !t.starts_with('#') {
            break;
        }
    }
    let mut out = Vec::new();
    let mut pending = String::new();
    let mut heredoc: Vec<(String, bool)> = Vec::new();
    for raw in text.lines() {
        if let Some((d, tabs)) = heredoc.first() {
            if (if *tabs {
                raw.trim_start_matches('\t')
            } else {
                raw
            }) == d
            {
                heredoc.remove(0);
            }
            continue;
        }
        let t = raw.trim();
        if t.is_empty() || pending.is_empty() && t.starts_with('#') {
            continue;
        }
        pending.push_str(t);
        if pending.ends_with(esc) {
            pending.truncate(pending.len() - esc.len());
            pending.push(' ')
        } else {
            heredoc.extend(delims(&pending));
            out.push(std::mem::take(&mut pending));
        }
    }
    if !pending.is_empty() {
        out.push(pending)
    }
    out
}
fn delims(s: &str) -> Vec<(String, bool)> {
    let mut r = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    let mut q = 0;
    while i + 2 < b.len() {
        if q != 0 {
            if b[i] == b'\\' && q == b'"' {
                i += 2;
                continue;
            }
            if b[i] == q {
                q = 0
            }
            i += 1;
            continue;
        }
        if b[i] == b'\'' || b[i] == b'"' {
            q = b[i];
            i += 1;
            continue;
        }
        if b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == b'<' && b[i + 1] == b'<' && (i == 0 || b[i - 1] != b'<') {
            let mut j = i + 2;
            let tabs = j < b.len() && b[j] == b'-';
            if tabs {
                j += 1
            }
            if j < b.len() && b[j] == b'\\' {
                j += 1;
            }
            let quote = if j < b.len() && (b[j] == b'\'' || b[j] == b'"') {
                let z = b[j];
                j += 1;
                Some(z)
            } else {
                None
            };
            let start = j;
            while j < b.len() && matches!(b[j],b'A'..=b'Z'|b'a'..=b'z'|b'0'..=b'9'|b'_'|b'.'|b'-') {
                j += 1
            }
            if j > start && quote.is_none_or(|x| j < b.len() && b[j] == x) {
                r.push((s[start..j].into(), tabs));
                if quote.is_some() {
                    j += 1
                }
                i = j;
                continue;
            }
        }
        i += 1
    }
    r
}
fn split_op(s: &str) -> (String, &str) {
    let x = s.find(char::is_whitespace).unwrap_or(s.len());
    (s[..x].to_ascii_uppercase(), s[x..].trim())
}
fn words(s: &str) -> Result<Vec<String>, DockerfileError> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut q = None;
    let mut slash = false;
    for c in s.chars() {
        if slash {
            cur.push(c);
            slash = false
        } else if c == '\\' {
            slash = true
        } else if q == Some(c) {
            q = None
        } else if q.is_none() && (c == '\'' || c == '"') {
            q = Some(c)
        } else if q.is_none() && c.is_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur))
            }
        } else {
            cur.push(c)
        }
    }
    if q.is_some() {
        return Err(DockerfileError::Parse("unterminated quote".into()));
    }
    if !cur.is_empty() {
        out.push(cur)
    }
    Ok(out)
}
fn json_strings(s: &str) -> Result<Vec<String>, DockerfileError> {
    serde_json::from_str(s).map_err(|_| DockerfileError::Parse("invalid JSON COPY/ADD".into()))
}
fn expand(
    v: &str,
    args: &std::collections::BTreeMap<String, String>,
    auto: bool,
) -> Result<String, DockerfileError> {
    let mut o = String::new();
    let mut rest = v;
    while let Some(i) = rest.find('$') {
        o += &rest[..i];
        rest = &rest[i + 1..];
        let (name, n) = if rest.starts_with('{') {
            let j = rest
                .find('}')
                .ok_or_else(|| DockerfileError::Parse("unterminated build argument".into()))?;
            (&rest[1..j], j + 1)
        } else {
            let n = rest
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(rest.len());
            (&rest[..n], n)
        };
        if auto
            && matches!(
                name,
                "BUILDPLATFORM"
                    | "BUILDOS"
                    | "BUILDARCH"
                    | "BUILDVARIANT"
                    | "TARGETPLATFORM"
                    | "TARGETOS"
                    | "TARGETARCH"
                    | "TARGETVARIANT"
            )
        {
            o.push('$');
            o += name
        } else {
            o += args.get(name).ok_or_else(|| {
                DockerfileError::Parse(format!(
                    "image reference {v:?} uses unresolved build argument {name:?}"
                ))
            })?
        }
        rest = &rest[n..]
    }
    o += rest;
    Ok(o)
}
fn path(e: &ContextEntry) -> &str {
    match e {
        ContextEntry::File { path, .. }
        | ContextEntry::Directory { path }
        | ContextEntry::Symlink { path, .. } => path,
    }
}
fn file<'a>(e: &'a [ContextEntry], p: &str) -> Option<&'a [u8]> {
    e.iter().find_map(|x| match x {
        ContextEntry::File { path, bytes } if path == p => Some(bytes.as_slice()),
        _ => None,
    })
}
fn remote(s: &str) -> bool {
    let x = s.to_ascii_lowercase();
    x.starts_with("git://")
        || x.starts_with("ssh://")
        || x.starts_with("git@")
        || x.starts_with("http://")
        || x.starts_with("https://")
}
fn git(s: &str) -> bool {
    let x = s.to_ascii_lowercase();
    (x.starts_with("git://") || x.starts_with("ssh://") || x.starts_with("git@"))
        || x.split(['#', '?'])
            .next()
            .is_some_and(|x| x.ends_with(".git"))
}
fn full_commit(s: &str) -> bool {
    let x = s
        .rsplit('#')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    (x.len() == 40 || x.len() == 64) && x.bytes().all(|b| b.is_ascii_hexdigit())
}
fn valid_arg(x: &str) -> bool {
    let mut c = x.chars();
    c.next()
        .is_some_and(|x| x == '_' || x.is_ascii_alphabetic())
        && c.all(|x| x == '_' || x.is_ascii_alphanumeric())
}
fn stage(x: &str, a: &std::collections::BTreeSet<String>) -> bool {
    x.parse::<usize>().is_ok() || a.contains(&x.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn file(path: &str, text: &str) -> ContextEntry {
        ContextEntry::File {
            path: path.into(),
            bytes: text.into(),
        }
    }
    #[test]
    fn external_args_stages_copy_and_mount_are_ordered() {
        let refs=external_images("ARG BASE=alpine:3.20\nFROM --platform=linux/amd64 ${BASE} AS build\nFROM scratch\nCOPY --from=build /local /local\nCOPY --from=busybox:1.36 /bin/busybox /busybox\nRUN --mount=target=/tool,from=debian:bookworm cat /tool\n").unwrap();
        assert_eq!(
            refs,
            vec![
                ExternalImageIdentity {
                    reference: "alpine:3.20".into(),
                    platform: Some("linux/amd64".into()),
                    identity: None
                },
                ExternalImageIdentity {
                    reference: "busybox:1.36".into(),
                    platform: None,
                    identity: None
                },
                ExternalImageIdentity {
                    reference: "debian:bookworm".into(),
                    platform: None,
                    identity: None
                }
            ]
        );
    }
    #[test]
    fn selected_copy_obeys_ignore_negation_and_protects_inputs() {
        let entries = vec![
            file("Dockerfile", "FROM x\nCOPY . /x\n"),
            file(".dockerignore", "*.md\n!keep.md\n"),
            file("gone.md", "a"),
            file("keep.md", "b"),
            file("sub/gone.md", "c"),
            ContextEntry::Directory {
                path: "empty".into(),
            },
        ];
        assert_eq!(
            select_context(&entries, "Dockerfile")
                .unwrap()
                .iter()
                .map(path)
                .collect::<Vec<_>>(),
            vec![
                ".dockerignore",
                "Dockerfile",
                "empty",
                "keep.md",
                "sub/gone.md"
            ]
        );
    }
    #[test]
    fn specific_ignore_wins_and_missing_source_is_error() {
        let entries = vec![
            file("Dockerfile", "FROM x\nCOPY hit /x\n"),
            file(".dockerignore", "hit\n"),
            file("Dockerfile.dockerignore", "!hit\n"),
            file("hit", "x"),
        ];
        assert!(
            select_context(&entries, "Dockerfile")
                .unwrap()
                .iter()
                .any(|x| path(x) == "hit")
        );
        assert!(matches!(
            select_context(&[file("Dockerfile", "COPY no /x")], "Dockerfile"),
            Err(DockerfileError::MissingSource(_))
        ));
    }
    #[test]
    fn heredoc_and_pinned_remote_rules_are_conservative() {
        assert!(
            select_context(
                &[file("Dockerfile", "RUN <<'EOF'\nCOPY missing /x\nEOF\n")],
                "Dockerfile"
            )
            .is_ok()
        );
        assert!(matches!(
            select_context(&[file("Dockerfile", "ADD https://x/y /x")], "Dockerfile"),
            Err(DockerfileError::UnpinnedRemote(_))
        ));
        assert!(
            select_context(
                &[file("Dockerfile", "ADD --checksum=sha256:a https://x/y /x")],
                "Dockerfile"
            )
            .is_ok()
        );
    }
    #[test]
    fn escaped_heredoc_delimiter_hides_its_body_but_not_later_copy() {
        let entries = vec![
            file(
                "Dockerfile",
                "RUN <<\\EOF\nCOPY missing /x\nEOF\nCOPY real /x\n",
            ),
            file("real", "x"),
        ];
        assert_eq!(
            select_context(&entries, "Dockerfile")
                .unwrap()
                .iter()
                .map(path)
                .collect::<Vec<_>>(),
            vec!["Dockerfile", "real"]
        );
    }
    #[test]
    fn escaped_quote_and_less_than_do_not_start_a_heredoc() {
        assert!(delims("RUN echo \"\\\" <<EOF\"").is_empty());
        assert!(delims("RUN echo \\<<EOF").is_empty());
    }
    #[test]
    fn source_paths_normalize_and_glob_directories_include_descendants() {
        assert!(source_matches("..", "anything").unwrap());
        assert!(source_matches("./foo/../bar", "bar/item").unwrap());
        assert!(source_matches("src*", "src-one/nested/file").unwrap());
        assert!(glob("**/a", "a").unwrap());
        assert!(glob("file[0-9].txt", "file7.txt").unwrap());
        assert!(glob("file[!0-9].txt", "filex.txt").unwrap());
        assert!(glob("?.txt", "é.txt").unwrap());
        let hostile = format!("{}b", "*a".repeat(256));
        assert!(!glob(&hostile, &"a".repeat(256)).unwrap());
        assert!(matches!(
            glob(&"*".repeat(3_000), &"x".repeat(1_000)),
            Err(DockerfileError::ResourceLimit(_))
        ));
        assert!(matches!(
            glob(&"*".repeat(4_097), "x"),
            Err(DockerfileError::ResourceLimit(_))
        ));
        // Each DP state scans every class member: state-count alone is not a
        // work bound for class-heavy patterns.
        let class = format!("[{}]", "a".repeat(4_000));
        assert!(matches!(
            glob(&class, &"z".repeat(300)),
            Err(DockerfileError::ResourceLimit(_))
        ));
    }
    #[test]
    fn json_copy_supports_commas_escapes_and_rejects_malformed_json() {
        let entries = vec![
            file("Dockerfile", "COPY [\"a,b\", \"\\u00e9\", \"/x\"]"),
            file("a,b", "x"),
            file("é", "x"),
        ];
        assert_eq!(select_context(&entries, "Dockerfile").unwrap().len(), 3);
        assert!(matches!(
            select_context(
                &[file("Dockerfile", "COPY [\"unterminated\", /x]")],
                "Dockerfile"
            ),
            Err(DockerfileError::Parse(_))
        ));
    }
    #[test]
    fn matcher_limit_is_not_silently_an_ignore_or_missing_source() {
        let pattern = "*".repeat(3_000);
        let path = "x".repeat(1_000);
        let ignored_entries = vec![
            file("Dockerfile", "COPY . /x"),
            file(".dockerignore", &format!("!{pattern}")),
            file(&path, "x"),
        ];
        assert!(matches!(
            select_context(&ignored_entries, "Dockerfile"),
            Err(DockerfileError::ResourceLimit(_))
        ));
        let source_entries = vec![
            file("Dockerfile", &format!("COPY {pattern} /x")),
            file(&path, "x"),
        ];
        assert!(matches!(
            select_context(&source_entries, "Dockerfile"),
            Err(DockerfileError::ResourceLimit(_))
        ));
    }
}
