//! Check the immutable writer sources and keep PDF reading out of production exports.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

const SHARED_GIT: &str = "https://github.com/storytold/craft-libs";
const SHARED_REV: &str = "92d392cff132eaf53e13afff77f38b5c11b73aa0";
const REGISTRY: &str = "registry+https://github.com/rust-lang/crates.io-index";

/// Checks that the writer resolves once, to craft-libs at its pinned commit, with its expected
/// features on each target, and that no reader reaches the production exporter. A build of a
/// fork of craft-libs names the fork with `CRAFT_LIBS_GIT` and `CRAFT_LIBS_REV` (see `source`).
pub fn run(root: &Path) -> Result<(), String> {
    let (source, forked) = source_from_env(|name| std::env::var_os(name))?;
    let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .current_dir(root)
        .args(["metadata", "--locked", "--format-version", "1"])
        .output()
        .map_err(|e| format!("cargo metadata: {e}"))?;
    if !output.status.success() {
        return Err(format!("cargo metadata: {}", String::from_utf8_lossy(&output.stderr)));
    }
    let metadata: Value = serde_json::from_slice(&output.stdout).map_err(|e| format!("cargo metadata: {e}"))?;
    validate(&metadata, &source)?;
    // Metadata merges target features even with --filter-platform. Check Cargo's
    // selected feature graph separately so native compression never enables threads on WASM.
    let output =
        Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into())).arg("-vV").output().map_err(|e| format!("rustc host: {e}"))?;
    if !output.status.success() {
        return Err(format!("rustc host: {}", String::from_utf8_lossy(&output.stderr)));
    }
    let version = String::from_utf8(output.stdout).map_err(|e| format!("rustc host: {e}"))?;
    let host = version.lines().find_map(|line| line.strip_prefix("host: ")).ok_or("rustc did not report its host target")?;
    for (target, parallel) in [(host, true), ("wasm32-unknown-unknown", false)] {
        let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .current_dir(root)
            .args([
                "tree",
                "--locked",
                "--target",
                target,
                "--edges",
                "normal,build",
                "--package",
                "deckcraft-pdf",
                "--depth",
                "2",
                "--no-dedupe",
                "--prefix",
                "none",
                "--format",
                "{p}|{f}",
            ])
            .output()
            .map_err(|e| format!("cargo tree ({target}): {e}"))?;
        if !output.status.success() {
            return Err(format!("cargo tree ({target}): {}", String::from_utf8_lossy(&output.stderr)));
        }
        validate_target_features(&String::from_utf8_lossy(&output.stdout), parallel).map_err(|e| format!("{target}: {e}"))?;
    }
    if forked {
        println!("craft-libs: {source}, named by CRAFT_LIBS_GIT and CRAFT_LIBS_REV");
    }
    println!("PDF writer: pinned craft-pdf/Krilla; native-only parallel compression; parser assertions are development-only");
    Ok(())
}

/// The source `run` checks against, read from the build environment through `var`, and whether
/// `CRAFT_LIBS_GIT` and `CRAFT_LIBS_REV` named it. A variable that is set counts, even when it is
/// empty, so a mistake is reported rather than ignored.
fn source_from_env(var: impl Fn(&str) -> Option<OsString>) -> Result<(String, bool), String> {
    let text = |name: &str| var(name).map(|v| v.into_string().map_err(|_| format!("{name} is not valid Unicode"))).transpose();
    let (git, rev) = (text("CRAFT_LIBS_GIT")?, text("CRAFT_LIBS_REV")?);
    let forked = git.is_some() || rev.is_some();
    Ok((source(git, rev)?, forked))
}

/// The craft-libs source the writer and Krilla must resolve to, as Cargo records it: upstream's pin, or a
/// fork's. A fork is named by both `CRAFT_LIBS_GIT`, the URL as Cargo.lock records it, and
/// `CRAFT_LIBS_REV`, the full commit hash, so that a branch or tag can never take its place.
fn source(git: Option<String>, rev: Option<String>) -> Result<String, String> {
    let (git, rev) = match (git, rev) {
        (None, None) => (SHARED_GIT.to_owned(), SHARED_REV.to_owned()),
        (Some(git), Some(rev)) => (git, rev),
        _ => return Err("set both CRAFT_LIBS_GIT and CRAFT_LIBS_REV, or neither".into()),
    };
    check_url(&git)?;
    if rev.len() != 40 || !rev.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(format!("CRAFT_LIBS_REV must be a full 40-digit lowercase commit hash, as Cargo.lock records it, not {rev:?}"));
    }
    Ok(format!("git+{git}?rev={rev}#{rev}"))
}

/// Refuses a `CRAFT_LIBS_GIT` that isn't a `scheme://` URL, as Cargo needs, or that carries
/// credentials: only an `ssh://` URL may name a user, and never with a password, since an
/// `https://` user name is often a token. The error never repeats the URL.
fn check_url(git: &str) -> Result<(), String> {
    let Some((scheme, rest)) = git.split_once("://") else {
        return Err("CRAFT_LIBS_GIT must be a URL such as https://host/craft-libs or ssh://git@host/craft-libs".into());
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    match authority.rsplit_once('@') {
        None => Ok(()),
        Some((user, _)) if scheme.eq_ignore_ascii_case("ssh") && !user.contains(':') => Ok(()),
        Some(_) => Err(
            "CRAFT_LIBS_GIT must not carry credentials: use a credential helper or an SSH key (an ssh:// URL may name a user, without a password)"
                .into(),
        ),
    }
}

/// Checks the resolved graph: the writer and Krilla once each from `source`, the parser once from
/// crates.io, each with exactly its expected features, and no reader in the production
/// exporter's normal and build dependencies.
fn validate(metadata: &Value, source: &str) -> Result<(), String> {
    let packages = metadata.get("packages").and_then(Value::as_array).ok_or("cargo metadata has no packages")?;
    let nodes = metadata.get("resolve").and_then(|r| r.get("nodes")).and_then(Value::as_array).ok_or("cargo metadata has no resolved nodes")?;
    let one_package = |name: &str| -> Result<&Value, String> {
        let matches: Vec<_> = packages.iter().filter(|p| p.get("name").and_then(Value::as_str) == Some(name)).collect();
        if matches.len() != 1 {
            return Err(format!("{name}: expected one resolved package, found {}", matches.len()));
        }
        matches.first().copied().ok_or_else(|| format!("missing {name}"))
    };
    for (name, version, expected_source, expected_features) in [
        ("craft-pdf", "0.1.0", source, vec!["write", "write-simple-text"]),
        ("krilla", "0.8.2", source, vec!["raster-images", "rayon", "simple-text"]),
        ("hayro-syntax", "0.7.2", REGISTRY, vec!["std"]),
    ] {
        let package = one_package(name)?;
        if package.get("version").and_then(Value::as_str) != Some(version) || package.get("source").and_then(Value::as_str) != Some(expected_source) {
            return Err(format!("{name}: expected {version} from {expected_source}; got {package}"));
        }
        let id = package.get("id").and_then(Value::as_str).ok_or_else(|| format!("{name}: missing package id"))?;
        let node = nodes.iter().find(|n| n.get("id").and_then(Value::as_str) == Some(id)).ok_or_else(|| format!("{name}: no resolved node"))?;
        let features = node.get("features").and_then(Value::as_array).ok_or_else(|| format!("{name}: no resolved features"))?;
        let actual: BTreeSet<_> = features.iter().filter_map(Value::as_str).collect();
        let expected: BTreeSet<_> = expected_features.into_iter().collect();
        if actual != expected {
            return Err(format!("{name}: expected features {expected:?}; got {features:?}"));
        }
    }
    // Metadata includes development dependencies. Follow only normal/build edges from
    // the production exporter so a parser accidentally promoted into it cannot hide there.
    let by_id: HashMap<_, _> = packages.iter().filter_map(|p| p.get("id").and_then(Value::as_str).map(|id| (id, p))).collect();
    let root = one_package("deckcraft-pdf")?.get("id").and_then(Value::as_str).ok_or("deckcraft-pdf has no id")?;
    let mut pending = vec![root];
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        let package = by_id.get(id).ok_or_else(|| format!("unresolved package {id}"))?;
        let name = package.get("name").and_then(Value::as_str).ok_or("resolved package has no name")?;
        if name == "hayro" || name.starts_with("hayro-") {
            return Err(format!("unexpected production reader dependency {name}"));
        }
        let node = nodes.iter().find(|n| n.get("id").and_then(Value::as_str) == Some(id)).ok_or_else(|| format!("no resolved node for {id}"))?;
        for dep in node.get("deps").and_then(Value::as_array).ok_or_else(|| format!("no dependency edges for {id}"))? {
            let kinds = dep.get("dep_kinds").and_then(Value::as_array).ok_or("dependency has no kinds")?;
            if kinds.is_empty() {
                return Err("dependency has no kinds".into());
            }
            if kinds.iter().any(|k| k.get("kind").and_then(Value::as_str) != Some("dev")) {
                pending.push(dep.get("pkg").and_then(Value::as_str).ok_or("dependency has no package id")?);
            }
        }
    }
    Ok(())
}

fn validate_target_features(tree: &str, parallel: bool) -> Result<(), String> {
    let mut expected = BTreeSet::from(["raster-images", "simple-text"]);
    if parallel {
        expected.insert("rayon");
    }
    let mut found = false;
    for (package, features) in tree.lines().filter_map(|line| line.split_once('|')).filter(|(package, _)| package.starts_with("krilla v")) {
        found = true;
        let actual: BTreeSet<_> = features.split(',').collect();
        if actual != expected {
            return Err(format!("{package}: expected features {expected:?}; got {actual:?}"));
        }
    }
    if !found {
        return Err("missing Krilla in the exporter's target dependency graph".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FORK_GIT: &str = "https://example.org/craft-libs";
    const FORK_REV: &str = "ba45867c71639154c5f2b03ff609e34bd0da149e";

    /// The check as upstream runs it, with neither variable set.
    fn validate(metadata: &Value) -> Result<(), String> {
        super::validate(metadata, &source(None, None).unwrap())
    }

    fn fork(rev: &str) -> Result<String, String> {
        source(Some(FORK_GIT.into()), Some(rev.into()))
    }

    #[test]
    fn accepts_a_fork_of_craft_libs_only_where_it_is_named() {
        let fork = fork(FORK_REV).unwrap();
        let mut m = metadata();
        for writer in [1, 2] {
            m["packages"][writer]["source"] = json!(fork);
        }
        assert!(super::validate(&m, &fork).is_ok());
        assert!(validate(&m).is_err());
        assert!(super::validate(&metadata(), &fork).is_err());
        // The parser still comes from crates.io, and the writer still from git.
        let mut parser_from_fork = m.clone();
        parser_from_fork["packages"][3]["source"] = json!(fork);
        assert!(super::validate(&parser_from_fork, &fork).is_err());
        let mut writer_from_registry = m;
        writer_from_registry["packages"][1]["source"] = json!(REGISTRY);
        assert!(super::validate(&writer_from_registry, &fork).is_err());
    }

    #[test]
    fn naming_upstream_is_the_same_as_naming_nothing() {
        assert_eq!(source(None, None).unwrap(), format!("git+https://github.com/storytold/craft-libs?rev={SHARED_REV}#{SHARED_REV}"));
        assert_eq!(source(Some(SHARED_GIT.into()), Some(SHARED_REV.into())), source(None, None));
    }

    #[test]
    fn rejects_half_a_fork_moving_or_short_revisions_and_credentials() {
        assert!(source(Some(FORK_GIT.into()), None).is_err());
        assert!(source(None, Some(FORK_REV.into())).is_err());
        let (upper, long) = (FORK_REV.to_uppercase(), format!("{FORK_REV}0"));
        for rev in ["main", "v1.0.0", "ba45867", upper.as_str(), long.as_str()] {
            assert!(fork(rev).is_err(), "{rev}");
        }
        for git in [
            "https://name:secret@example.org/craft-libs",
            "https://secret@example.org/craft-libs",
            "ssh://git:secret@example.org/craft-libs",
            "secret@example.org:craft-libs",
            "craft-libs",
            "",
        ] {
            let error = source(Some(git.into()), Some(FORK_REV.into())).unwrap_err();
            assert!(!error.contains("secret") && !error.contains("example"), "{git}: {error}");
        }
    }

    #[test]
    fn accepts_a_user_name_only_in_an_ssh_url() {
        for git in ["ssh://git@example.org/craft-libs", "ssh://deploy@example.org/craft-libs", "https://example.org/a@b/craft-libs"] {
            assert_eq!(source(Some(git.into()), Some(FORK_REV.into())), Ok(format!("git+{git}?rev={FORK_REV}#{FORK_REV}")));
        }
    }

    fn env<'a>(vars: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| vars.iter().find(|(n, _)| *n == name).map(|(_, v)| OsString::from(v))
    }

    #[test]
    fn reads_the_fork_from_the_environment_as_run_does() {
        let both = [("CRAFT_LIBS_GIT", FORK_GIT), ("CRAFT_LIBS_REV", FORK_REV)];
        assert_eq!(source_from_env(env(&both)), Ok((fork(FORK_REV).unwrap(), true)));
        assert_eq!(source_from_env(env(&[])), Ok((source(None, None).unwrap(), false)));
        for set in [&[("CRAFT_LIBS_GIT", ""), ("CRAFT_LIBS_REV", "")][..], &[("CRAFT_LIBS_GIT", FORK_GIT)], &[("CRAFT_LIBS_REV", "")]] {
            assert!(source_from_env(env(set)).is_err(), "{set:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_variable_that_is_not_unicode() {
        use std::os::unix::ffi::OsStringExt;
        let var = |name: &str| (name == "CRAFT_LIBS_GIT").then(|| OsString::from_vec(vec![0xff]));
        assert_eq!(source_from_env(var), Err("CRAFT_LIBS_GIT is not valid Unicode".into()));
    }

    fn metadata() -> Value {
        let source = format!("git+https://github.com/storytold/craft-libs?rev={SHARED_REV}#{SHARED_REV}");
        json!({
            "packages": [
                {"id":"app", "name":"deckcraft-pdf"},
                {"id":"writer", "name":"craft-pdf", "version":"0.1.0", "source":source},
                {"id":"backend", "name":"krilla", "version":"0.8.2", "source":source},
                {"id":"parser", "name":"hayro-syntax", "version":"0.7.2", "source":REGISTRY}
            ],
            "resolve":{"nodes":[
                {"id":"app", "deps":[{"pkg":"writer", "dep_kinds":[{"kind":null}]}, {"pkg":"parser", "dep_kinds":[{"kind":"dev"}]}]},
                {"id":"writer", "features":["write","write-simple-text"], "deps":[{"pkg":"backend", "dep_kinds":[{"kind":null}]}]},
                {"id":"backend", "features":["raster-images","rayon","simple-text"], "deps":[]},
                {"id":"parser", "features":["std"], "deps":[]}
            ]}
        })
    }

    #[test]
    fn parallel_compression_is_required_only_on_native_targets() {
        let serial = "krilla v0.8.2 (pinned)|raster-images,simple-text\n";
        let parallel = "krilla v0.8.2 (pinned)|raster-images,rayon,simple-text\n";
        assert!(validate_target_features(serial, false).is_ok());
        assert!(validate_target_features(parallel, true).is_ok());
        assert!(validate_target_features(serial, true).is_err());
        assert!(validate_target_features(parallel, false).is_err());
        assert!(validate_target_features("", false).is_err());
    }

    #[test]
    fn rejects_unpinned_or_duplicate_writer_sources() {
        assert!(validate(&metadata()).is_ok());
        for source in [Value::Null, json!(REGISTRY), json!("git+https://github.com/storytold/craft-libs#other")] {
            let mut m = metadata();
            m["packages"][1]["source"] = source;
            assert!(validate(&m).is_err());
        }
        let mut m = metadata();
        let duplicate = m["packages"][2].clone();
        m["packages"].as_array_mut().unwrap().push(duplicate);
        assert!(validate(&m).is_err());
    }

    #[test]
    fn rejects_import_features_and_missing_text_shaping() {
        for (node, features) in [
            (1, json!([])),
            (1, json!(["write"])),
            (1, json!(["write", "write-simple-text", "write-pdf"])),
            (2, json!(["raster-images", "rayon", "simple-text", "pdf"])),
            (2, json!(["raster-images"])),
            (3, json!(["std", "images"])),
        ] {
            let mut m = metadata();
            m["resolve"]["nodes"][node]["features"] = features;
            assert!(validate(&m).is_err());
        }
    }

    #[test]
    fn rejects_dev_parser_promoted_to_production_directly_or_transitively() {
        for kind in [Value::Null, json!("build")] {
            let mut m = metadata();
            m["resolve"]["nodes"][0]["deps"][1]["dep_kinds"][0]["kind"] = kind;
            assert!(validate(&m).is_err());
        }
        let mut m = metadata();
        m["resolve"]["nodes"][2]["deps"] = json!([{"pkg":"parser", "dep_kinds":[{"kind":null}]}]);
        assert!(validate(&m).is_err());
    }

    #[test]
    fn rejects_incomplete_dependency_metadata() {
        let mut m = metadata();
        m["resolve"]["nodes"][2]["deps"] = json!([{"pkg":"missing", "dep_kinds":[{"kind":null}]}]);
        assert!(validate(&m).is_err());
        let mut m = metadata();
        m["resolve"] = Value::Null;
        assert!(validate(&m).is_err());
        let mut m = metadata();
        m["packages"].as_array_mut().unwrap().retain(|p| p["name"] != "hayro-syntax");
        assert!(validate(&m).is_err());
    }
}
