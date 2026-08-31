//! The one D49 source-closure encoder used to generate `RulesetId.digest`.
//!
//! This crate intentionally lives only on the ruleset build-dependency paths.
//! It derives the normal first-party path-dependency closure directly from the
//! workspace manifests, then hashes normalized production Rust sources from
//! its members. Build scripts must never invoke Cargo: the outer Cargo process
//! owns the package-cache lock while they run.

#![forbid(unsafe_code)]

use std::collections::{BTreeSet, VecDeque};
use std::env;
use std::error::Error;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use proc_macro2::{Delimiter, Group, TokenStream, TokenTree};
use quote::ToTokens;
use toml::Value;

const DOMAIN: &[u8] = b"orrery-ruleset-digest-v1\0";

/// A result produced by the source-closure encoder.
pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// A manifest-derived D49 source closure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestClosure {
    manifest_path: PathBuf,
    packages: Vec<ContributingCrate>,
    rerun_inputs: BTreeSet<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContributingCrate {
    name: String,
    logical_path: String,
    manifest_path: PathBuf,
    sources: Vec<SourceUnit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceUnit {
    logical_path: String,
    disk_path: PathBuf,
}

/// One canonical source input and the hash of the token stream it contributes.
///
/// This is diagnostic data rather than another identity format: callers can
/// compare a checkout with a fresh clone without exposing a checkout-local
/// disk path to the digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestInput {
    /// Stable, workspace-relative identity of the Rust source file.
    pub logical_path: String,
    /// BLAKE3 of the canonical token stream encoded for this source file.
    pub canonical_tokens_hash: [u8; 32],
}

impl DigestClosure {
    /// Resolve the normal first-party path-dependency closure rooted at
    /// `manifest` without invoking Cargo.
    pub fn from_manifest(manifest: &Path) -> Result<Self> {
        let manifest_path = manifest.canonicalize()?;
        let workspace_manifest = workspace_manifest_for(&manifest_path)?;
        let workspace_root = workspace_manifest
            .parent()
            .ok_or("workspace manifest has no parent")?;
        let workspace_members = workspace_members(&workspace_manifest)?;
        let packages = contributing_crates(
            &manifest_path,
            &workspace_manifest,
            &workspace_members,
            workspace_root,
        )?;
        let mut rerun_inputs = BTreeSet::new();
        rerun_inputs.insert(workspace_manifest);
        for package in &packages {
            rerun_inputs.insert(package.manifest_path.clone());
            rerun_inputs.extend(
                package
                    .sources
                    .iter()
                    .map(|source| source.disk_path.clone()),
            );
        }
        Ok(Self {
            manifest_path,
            packages,
            rerun_inputs,
        })
    }

    /// Check that the encoded sources and Cargo rerun inputs still exactly
    /// match a fresh manifest-derived closure.
    ///
    /// This is intentionally independent from digest calculation. A future
    /// accidental omission cannot yield a plausible old digest: the build
    /// script stops before generating its Rust constant.
    pub fn verify_manifest_closure(&self) -> Result<()> {
        let fresh = Self::from_manifest(&self.manifest_path)?;
        if self.packages != fresh.packages || self.rerun_inputs != fresh.rerun_inputs {
            return Err("manifest closure and encoded/rerun input sets differ".into());
        }
        Ok(())
    }

    /// Every file Cargo must watch for this computed build identity.
    pub fn rerun_inputs(&self) -> impl Iterator<Item = &Path> {
        self.rerun_inputs.iter().map(PathBuf::as_path)
    }

    /// Compute blake3 over the versioned, length-prefixed closure encoding.
    pub fn digest(&self) -> Result<[u8; 32]> {
        let mut hasher = blake3::Hasher::new();
        write_record(&mut hasher, b"domain", DOMAIN);
        for package in &self.packages {
            write_record(&mut hasher, b"crate", package.name.as_bytes());
            write_record(&mut hasher, b"crate-path", package.logical_path.as_bytes());
            for source in &package.sources {
                write_record(&mut hasher, b"path", source.logical_path.as_bytes());
                let text = fs::read_to_string(&source.disk_path)?;
                write_record(&mut hasher, b"tokens", canonical_tokens(&text)?.as_bytes());
            }
        }
        Ok(*hasher.finalize().as_bytes())
    }

    /// Return the exact source identities and canonical token hashes used by
    /// [`Self::digest`], in digest order.
    pub fn diagnostic_inputs(&self) -> Result<Vec<DigestInput>> {
        let mut inputs = Vec::new();
        for package in &self.packages {
            for source in &package.sources {
                let text = fs::read_to_string(&source.disk_path)?;
                inputs.push(DigestInput {
                    logical_path: source.logical_path.clone(),
                    canonical_tokens_hash: *blake3::hash(canonical_tokens(&text)?.as_bytes())
                        .as_bytes(),
                });
            }
        }
        Ok(inputs)
    }
}

/// Generate the Rust constant and Cargo rerun directives for the package
/// whose build script calls this function.
///
/// The helper is shared so every build-script user has the same manifest
/// validation, source encoder, and fail-closed output path.
pub fn generate_build_output() -> Result<()> {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("missing CARGO_MANIFEST_DIR")?);
    let closure = DigestClosure::from_manifest(&manifest_dir.join("Cargo.toml"))?;
    closure.verify_manifest_closure()?;
    for input in closure.rerun_inputs() {
        println!("cargo:rerun-if-changed={}", input.display());
    }
    println!("cargo:rerun-if-env-changed=ORRERY_RULESET_DIGEST_DIAGNOSTICS");
    if env::var_os("ORRERY_RULESET_DIGEST_DIAGNOSTICS").is_some() {
        for input in closure.diagnostic_inputs()? {
            println!(
                "cargo:warning=orrery-ruleset-digest input {} {}",
                input.logical_path,
                lower_hex(&input.canonical_tokens_hash),
            );
        }
    }

    let digest = closure.digest()?;
    let output_dir = PathBuf::from(env::var_os("OUT_DIR").ok_or("missing OUT_DIR")?);
    fs::write(
        output_dir.join("ruleset_digest.rs"),
        format!("pub const RULESET_DIGEST: [u8; 32] = {digest:?};\n"),
    )?;
    Ok(())
}

fn workspace_manifest_for(manifest: &Path) -> Result<PathBuf> {
    let mut directory = manifest.parent();
    while let Some(candidate_directory) = directory {
        let candidate = candidate_directory.join("Cargo.toml");
        if candidate.is_file() {
            let document = read_manifest(&candidate)?;
            if document.get("workspace").is_some() {
                return Ok(candidate);
            }
        }
        directory = candidate_directory.parent();
    }
    Err(format!("no workspace manifest found above {}", manifest.display()).into())
}

fn workspace_members(workspace_manifest: &Path) -> Result<BTreeSet<PathBuf>> {
    let document = read_manifest(workspace_manifest)?;
    let members = document
        .get("workspace")
        .and_then(Value::as_table)
        .and_then(|workspace| workspace.get("members"))
        .and_then(Value::as_array)
        .ok_or("workspace manifest has no explicit members array")?;
    let workspace_root = workspace_manifest
        .parent()
        .ok_or("workspace manifest has no parent")?;
    members
        .iter()
        .map(|member| {
            let member = member.as_str().ok_or("workspace member is not a string")?;
            if member.contains(['*', '?', '[', ']']) {
                return Err(format!("workspace member {member:?} uses an unsupported glob").into());
            }
            Ok(workspace_root
                .join(member)
                .join("Cargo.toml")
                .canonicalize()?)
        })
        .collect()
}

fn contributing_crates(
    manifest: &Path,
    workspace_manifest: &Path,
    workspace_members: &BTreeSet<PathBuf>,
    workspace_root: &Path,
) -> Result<Vec<ContributingCrate>> {
    let mut pending = VecDeque::from([manifest.to_path_buf()]);
    let mut selected = BTreeSet::new();
    while let Some(manifest_path) = pending.pop_front() {
        if !workspace_members.contains(&manifest_path) {
            return Err(format!(
                "first-party path dependency {} is not a workspace member",
                manifest_path.display()
            )
            .into());
        }
        if !selected.insert(manifest_path.clone()) {
            continue;
        }
        pending.extend(path_dependencies(&manifest_path, workspace_manifest)?);
    }

    let mut closure = selected
        .iter()
        .map(|manifest_path| package_inputs(manifest_path, workspace_root))
        .collect::<Result<Vec<_>>>()?;
    closure.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.logical_path.cmp(&right.logical_path))
    });
    Ok(closure)
}

fn path_dependencies(manifest: &Path, workspace_manifest: &Path) -> Result<Vec<PathBuf>> {
    let document = read_manifest(manifest)?;
    let mut dependency_tables = Vec::new();
    if let Some(dependencies) = document.get("dependencies").and_then(Value::as_table) {
        dependency_tables.push(dependencies);
    }
    if let Some(targets) = document.get("target").and_then(Value::as_table) {
        for target in targets.values() {
            if let Some(dependencies) = target.get("dependencies").and_then(Value::as_table) {
                dependency_tables.push(dependencies);
            }
        }
    }

    let manifest_directory = manifest.parent().ok_or("package manifest has no parent")?;
    let mut paths = Vec::new();
    for dependencies in dependency_tables {
        for (name, dependency) in dependencies {
            let Some(dependency) = dependency.as_table() else {
                continue;
            };
            let direct_path = dependency
                .get("path")
                .map(|path| path.as_str().ok_or("dependency path is not a string"))
                .transpose()?
                .map(PathBuf::from);
            let path = if direct_path.is_some() {
                direct_path
            } else if dependency
                .get("workspace")
                .and_then(Value::as_bool)
                .is_some_and(|workspace| workspace)
            {
                workspace_dependency_path(workspace_manifest, name)?
            } else {
                None
            };
            if let Some(path) = path {
                let base = if dependency.get("path").is_some() {
                    manifest_directory
                } else {
                    workspace_manifest
                        .parent()
                        .ok_or("workspace manifest has no parent")?
                };
                paths.push(base.join(path).join("Cargo.toml").canonicalize()?);
            }
        }
    }
    Ok(paths)
}

fn workspace_dependency_path(workspace_manifest: &Path, name: &str) -> Result<Option<PathBuf>> {
    let document = read_manifest(workspace_manifest)?;
    let dependency = document
        .get("workspace")
        .and_then(Value::as_table)
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(Value::as_table)
        .and_then(|dependencies| dependencies.get(name));
    let Some(dependency) = dependency else {
        return Err(format!("workspace dependency {name:?} is not declared").into());
    };
    let Some(dependency) = dependency.as_table() else {
        return Ok(None);
    };
    Ok(dependency
        .get("path")
        .map(|path| {
            path.as_str()
                .ok_or("workspace dependency path is not a string")
        })
        .transpose()
        .map(|path| path.map(PathBuf::from))?)
}

fn package_inputs(manifest_path: &Path, workspace_root: &Path) -> Result<ContributingCrate> {
    let document = read_manifest(manifest_path)?;
    let name = document
        .get("package")
        .and_then(Value::as_table)
        .and_then(|package| package.get("name"))
        .and_then(Value::as_str)
        .ok_or("package manifest has no package.name")?
        .to_owned();
    let source_root = manifest_path
        .parent()
        .ok_or("package manifest has no parent")?
        .join("src");
    let source_paths = tracked_rust_sources(&source_root, workspace_root)?;
    let mut sources = source_paths
        .into_iter()
        .map(|disk_path| {
            let logical_path = workspace_relative_path(&disk_path, workspace_root)?;
            Ok(SourceUnit {
                logical_path,
                disk_path,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    sources.sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
    Ok(ContributingCrate {
        name,
        logical_path: workspace_relative_path(manifest_path, workspace_root)?,
        manifest_path: manifest_path.to_path_buf(),
        sources,
    })
}

/// Return a platform-neutral, workspace-relative source identity.
///
/// `PathBuf` is intentionally confined to filesystem access. The encoded
/// identity uses slash-separated UTF-8 components, so neither a checkout path
/// nor Windows' native separator can enter the digest.
fn workspace_relative_path(path: &Path, workspace_root: &Path) -> Result<String> {
    let relative = path.strip_prefix(workspace_root)?;
    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(component) => {
                let component = component.to_str().ok_or("source path is not valid UTF-8")?;
                if component.contains(['/', '\\']) {
                    return Err("source path component contains a separator".into());
                }
                components.push(component);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                return Err("source path is not a normal workspace-relative path".into());
            }
        }
    }
    if components.is_empty() {
        return Err("source path is the workspace root".into());
    }
    Ok(components.join("/"))
}

fn read_manifest(path: &Path) -> Result<Value> {
    Ok(toml::from_str(&fs::read_to_string(path)?)?)
}

/// Enumerate only Rust sources recorded in the repository index.
///
/// Walking `src/` is deliberately not sufficient: an editor file or generated
/// build artefact in a dirty worktree would then become a build identity input.
/// The repository's tracked set is the declared source set D49 hashes.
fn tracked_rust_sources(source_root: &Path, workspace_root: &Path) -> Result<Vec<PathBuf>> {
    let repository_root = Command::new("git")
        .args(["-C"])
        .arg(workspace_root)
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if !repository_root.status.success() {
        return Err("could not resolve the repository root for ruleset sources".into());
    }
    let repository_root = PathBuf::from(String::from_utf8(repository_root.stdout)?.trim());
    if repository_root.canonicalize()? != workspace_root.canonicalize()? {
        return Err(
            "the Cargo workspace root must be the repository root for ruleset sources".into(),
        );
    }

    let source_root_identity = workspace_relative_path(source_root, workspace_root)?;
    let output = Command::new("git")
        .args(["-C"])
        .arg(workspace_root)
        .args(["ls-files", "-z", "--"])
        .arg(&source_root_identity)
        .output()?;
    if !output.status.success() {
        return Err("could not enumerate tracked ruleset sources".into());
    }

    let mut sources = Vec::new();
    for identity in output.stdout.split(|byte| *byte == 0) {
        if identity.is_empty() {
            continue;
        }
        let identity = std::str::from_utf8(identity)?;
        let path = workspace_root.join(identity);
        if path.extension().is_some_and(|extension| extension == "rs") {
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(format!(
                    "tracked ruleset source {} is not a regular file",
                    path.display()
                )
                .into());
            }
            sources.push(path);
        }
    }
    sources.sort();
    Ok(sources)
}

fn write_record(hasher: &mut blake3::Hasher, tag: &[u8], bytes: &[u8]) {
    hasher.update(&(tag.len() as u64).to_le_bytes());
    hasher.update(tag);
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn lower_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn canonical_tokens(source: &str) -> Result<String> {
    let mut file = syn::parse_file(source)?;
    strip_test_items(&mut file.items);
    Ok(strip_doc_attributes(file.into_token_stream()).to_string())
}

fn strip_test_items(items: &mut Vec<syn::Item>) {
    items.retain(|item| !item_is_cfg_test(item));
    for item in items {
        if let syn::Item::Mod(module) = item {
            if let Some((_, nested)) = &mut module.content {
                strip_test_items(nested);
            }
        }
    }
}

fn item_is_cfg_test(item: &syn::Item) -> bool {
    let attributes = match item {
        syn::Item::Const(item) => &item.attrs,
        syn::Item::Enum(item) => &item.attrs,
        syn::Item::ExternCrate(item) => &item.attrs,
        syn::Item::Fn(item) => &item.attrs,
        syn::Item::ForeignMod(item) => &item.attrs,
        syn::Item::Impl(item) => &item.attrs,
        syn::Item::Macro(item) => &item.attrs,
        syn::Item::Mod(item) => &item.attrs,
        syn::Item::Static(item) => &item.attrs,
        syn::Item::Struct(item) => &item.attrs,
        syn::Item::Trait(item) => &item.attrs,
        syn::Item::TraitAlias(item) => &item.attrs,
        syn::Item::Type(item) => &item.attrs,
        syn::Item::Union(item) => &item.attrs,
        syn::Item::Use(item) => &item.attrs,
        syn::Item::Verbatim(_) => return false,
        _ => return false,
    };
    attributes.iter().any(is_cfg_test)
}

fn is_cfg_test(attribute: &syn::Attribute) -> bool {
    attribute.path().is_ident("cfg")
        && attribute
            .parse_args::<syn::Path>()
            .is_ok_and(|path| path.is_ident("test"))
}

fn strip_doc_attributes(tokens: TokenStream) -> TokenStream {
    let mut output = TokenStream::new();
    let mut input = tokens.into_iter().peekable();
    while let Some(token) = input.next() {
        if let TokenTree::Punct(punctuation) = &token {
            if punctuation.as_char() == '#' {
                if let Some(TokenTree::Group(group)) = input.peek() {
                    if group.delimiter() == Delimiter::Bracket && is_doc_attribute(group) {
                        input.next();
                        continue;
                    }
                }
            }
        }
        output.extend([match token {
            TokenTree::Group(group) => TokenTree::Group(Group::new(
                group.delimiter(),
                strip_doc_attributes(group.stream()),
            )),
            other => other,
        }]);
    }
    output
}

fn is_doc_attribute(group: &Group) -> bool {
    let mut tokens = group.stream().into_iter();
    matches!(
        (tokens.next(), tokens.next()),
        (Some(TokenTree::Ident(name)), Some(TokenTree::Punct(equals)))
            if name == "doc" && equals.as_char() == '='
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use tempfile::TempDir;

    use super::DigestClosure;

    fn fixture() -> TempDir {
        let directory = tempfile::tempdir().expect("temporary workspace");
        write(
            directory.path().join("Cargo.toml"),
            "[workspace]\nresolver = \"3\"\nmembers = [\"games\", \"core\"]\n",
        );
        write(
            directory.path().join("games/Cargo.toml"),
            "[package]\nname = \"orrery_games\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\norrery_core = { path = \"../core\" }\n",
        );
        write(
            directory.path().join("core/Cargo.toml"),
            "[package]\nname = \"orrery_core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(
            directory.path().join("games/src/lib.rs"),
            "pub const RULE: u32 = 7;\n#[cfg(test)] mod only_tests { const TEST_ONLY: u32 = 3; }\n",
        );
        write(
            directory.path().join("core/src/lib.rs"),
            "pub const CORE: u32 = 1;\n",
        );
        write(
            directory.path().join("games/tests/rules.rs"),
            "#[test] fn integration() {}\n",
        );
        git(directory.path(), &["init", "-q"]);
        git(directory.path(), &["add", "--all"]);
        git(
            directory.path(),
            &[
                "-c",
                "user.email=ruleset-digest@example.invalid",
                "-c",
                "user.name=Ruleset Digest Test",
                "commit",
                "-qm",
                "fixture",
            ],
        );
        directory
    }

    fn digest(directory: &TempDir) -> [u8; 32] {
        digest_at(directory.path())
    }

    fn digest_at(directory: &Path) -> [u8; 32] {
        let closure = DigestClosure::from_manifest(&directory.join("games/Cargo.toml"))
            .expect("derive manifest closure");
        closure
            .verify_manifest_closure()
            .expect("manifest closure check");
        closure.digest().expect("hash closure")
    }

    fn write(path: PathBuf, contents: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(path, contents).expect("write fixture source");
    }

    fn git(directory: &Path, arguments: &[&str]) {
        let status = Command::new("git")
            .current_dir(directory)
            .args(arguments)
            .status()
            .expect("run git for fixture");
        assert!(status.success(), "git {arguments:?} failed");
    }

    #[test]
    fn manifest_closure_and_rerun_inputs_are_complete() {
        let directory = fixture();
        let closure = DigestClosure::from_manifest(&directory.path().join("games/Cargo.toml"))
            .expect("derive manifest closure");
        closure
            .verify_manifest_closure()
            .expect("manifest closure check");
        let inputs = closure
            .rerun_inputs()
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        assert!(inputs
            .iter()
            .any(|input| input.ends_with("games/src/lib.rs")));
        assert!(inputs
            .iter()
            .any(|input| input.ends_with("core/src/lib.rs")));
        assert!(inputs
            .iter()
            .any(|input| input == &directory.path().join("Cargo.toml")));
        assert!(!inputs
            .iter()
            .any(|input| input.ends_with("games/tests/rules.rs")));
    }

    #[test]
    fn manifest_check_rejects_a_stale_rerun_set() {
        let directory = fixture();
        let mut closure = DigestClosure::from_manifest(&directory.path().join("games/Cargo.toml"))
            .expect("derive manifest closure");
        let omitted = closure
            .rerun_inputs
            .iter()
            .find(|input| input.ends_with("games/src/lib.rs"))
            .cloned()
            .expect("game source is an explicit rerun input");
        closure.rerun_inputs.remove(&omitted);
        assert!(closure.verify_manifest_closure().is_err());
    }

    #[test]
    fn manifest_derivation_rejects_a_path_dependency_outside_the_workspace() {
        let directory = fixture();
        write(
            directory.path().join("other/Cargo.toml"),
            "[package]\nname = \"other\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(
            directory.path().join("other/src/lib.rs"),
            "pub const OTHER: u32 = 1;\n",
        );
        write(
            directory.path().join("games/Cargo.toml"),
            "[package]\nname = \"orrery_games\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\norrery_core = { path = \"../core\" }\nother = { path = \"../other\" }\n",
        );
        assert!(DigestClosure::from_manifest(&directory.path().join("games/Cargo.toml")).is_err());
    }

    #[test]
    fn manifest_derivation_follows_workspace_path_dependencies() {
        let directory = fixture();
        write(
            directory.path().join("Cargo.toml"),
            "[workspace]\nresolver = \"3\"\nmembers = [\"games\", \"core\"]\n\n[workspace.dependencies]\nlocal_core = { path = \"core\" }\n",
        );
        write(
            directory.path().join("games/Cargo.toml"),
            "[package]\nname = \"orrery_games\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nlocal_core = { workspace = true }\n",
        );
        let closure = DigestClosure::from_manifest(&directory.path().join("games/Cargo.toml"))
            .expect("derive workspace path dependency");
        assert!(closure
            .rerun_inputs()
            .any(|input| input.ends_with("core/src/lib.rs")));
    }

    #[test]
    fn production_rule_source_changes_digest() {
        let directory = fixture();
        let baseline = digest(&directory);
        write(
            directory.path().join("games/src/lib.rs"),
            "pub const RULE: u32 = 8;\n#[cfg(test)] mod only_tests { const TEST_ONLY: u32 = 3; }\n",
        );
        assert_ne!(digest(&directory), baseline);
    }

    #[test]
    fn ordinary_comment_does_not_change_digest() {
        let directory = fixture();
        let baseline = digest(&directory);
        write(
            directory.path().join("games/src/lib.rs"),
            "// presentation only\npub const RULE: u32 = 7;\n#[cfg(test)] mod only_tests { const TEST_ONLY: u32 = 3; }\n",
        );
        assert_eq!(digest(&directory), baseline);
    }

    #[test]
    fn integration_test_does_not_change_digest() {
        let directory = fixture();
        let baseline = digest(&directory);
        write(
            directory.path().join("games/tests/rules.rs"),
            "#[test] fn integration() { assert!(true); }\n",
        );
        assert_eq!(digest(&directory), baseline);
    }

    #[test]
    fn fresh_clone_and_dirty_worktree_have_the_same_digest() {
        let dirty = fixture();
        let clone_parent = tempfile::tempdir().expect("clone parent");
        let clean = clone_parent.path().join("clean");
        let source = dirty.path().to_str().expect("fixture path is UTF-8");
        let destination = clean.to_str().expect("clone path is UTF-8");
        git(clone_parent.path(), &["clone", "-q", source, destination]);
        let fresh_clone_digest = digest_at(&clean);

        let untracked_source = dirty.path().join("games/src/local_untracked.rs");
        let build_artefact = dirty.path().join("games/src/.digest-build/generated.rs");
        write(untracked_source, "pub const LOCAL_ONLY: u32 = 8;\n");
        write(build_artefact, "pub const GENERATED: u32 = 9;\n");

        let closure = DigestClosure::from_manifest(&dirty.path().join("games/Cargo.toml"))
            .expect("derive dirty manifest closure");
        let encoded_paths = closure
            .diagnostic_inputs()
            .expect("list dirty closure inputs")
            .into_iter()
            .map(|input| input.logical_path)
            .collect::<Vec<_>>();
        assert!(
            !encoded_paths
                .iter()
                .any(|path| path.ends_with("local_untracked.rs")),
            "the untracked source is under games/src and must not enter the closure"
        );
        assert!(
            !encoded_paths
                .iter()
                .any(|path| path.ends_with(".digest-build/generated.rs")),
            "the build artefact is under games/src and must not enter the closure"
        );
        assert_eq!(
            digest(&dirty),
            fresh_clone_digest,
            "a fresh clone and a dirty worktree must encode the same tracked source closure"
        );
    }

    #[test]
    fn cfg_test_module_does_not_change_digest() {
        let directory = fixture();
        let baseline = digest(&directory);
        write(
            directory.path().join("games/src/lib.rs"),
            "pub const RULE: u32 = 7;\n#[cfg(test)] mod only_tests { const TEST_ONLY: u32 = 99; }\n",
        );
        assert_eq!(digest(&directory), baseline);
    }

    #[test]
    fn crlf_checkout_of_an_identical_source_tree_has_the_same_digest() {
        let lf = fixture();
        let crlf = fixture();
        for source in ["games/src/lib.rs", "core/src/lib.rs"] {
            let path = crlf.path().join(source);
            let contents = fs::read_to_string(&path).expect("read LF fixture source");
            assert!(!contents.contains("\r\n"), "fixture starts as LF");
            fs::write(&path, contents.replace('\n', "\r\n")).expect("rewrite source as CRLF");
        }

        assert_eq!(
            digest(&lf),
            digest(&crlf),
            "the source trees differ only in checkout line endings"
        );
    }

    #[test]
    fn encoded_source_paths_are_workspace_relative_and_slash_separated() {
        let directory = fixture();
        let closure = DigestClosure::from_manifest(&directory.path().join("games/Cargo.toml"))
            .expect("derive manifest closure");
        let paths = closure
            .packages
            .iter()
            .flat_map(|package| package.sources.iter())
            .map(|source| source.logical_path.as_str())
            .collect::<Vec<_>>();

        assert_eq!(paths, ["core/src/lib.rs", "games/src/lib.rs"]);
        assert!(paths.iter().all(|path| !path.contains('\\')));
    }
}
