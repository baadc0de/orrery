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
use std::path::{Path, PathBuf};

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
    manifest_path: PathBuf,
    sources: Vec<SourceUnit>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SourceUnit {
    logical_path: PathBuf,
    disk_path: PathBuf,
}

impl DigestClosure {
    /// Resolve the normal first-party path-dependency closure rooted at
    /// `manifest` without invoking Cargo.
    pub fn from_manifest(manifest: &Path) -> Result<Self> {
        let manifest_path = manifest.canonicalize()?;
        let workspace_manifest = workspace_manifest_for(&manifest_path)?;
        let workspace_members = workspace_members(&workspace_manifest)?;
        let packages =
            contributing_crates(&manifest_path, &workspace_manifest, &workspace_members)?;
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
            for source in &package.sources {
                write_record(
                    &mut hasher,
                    b"path",
                    source.logical_path.to_string_lossy().as_bytes(),
                );
                let text = fs::read_to_string(&source.disk_path)?;
                write_record(&mut hasher, b"tokens", canonical_tokens(&text)?.as_bytes());
            }
        }
        Ok(*hasher.finalize().as_bytes())
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
        .map(|manifest_path| package_inputs(manifest_path))
        .collect::<Result<Vec<_>>>()?;
    closure.sort_by(|left, right| left.name.cmp(&right.name));
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

fn package_inputs(manifest_path: &Path) -> Result<ContributingCrate> {
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
    let mut source_paths = Vec::new();
    collect_rust_sources(&source_root, &mut source_paths)?;
    source_paths.sort();
    let sources = source_paths
        .into_iter()
        .map(|disk_path| {
            let logical_path = disk_path
                .strip_prefix(manifest_path.parent().expect("manifest parent checked"))?
                .to_path_buf();
            Ok(SourceUnit {
                logical_path,
                disk_path,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ContributingCrate {
        name,
        manifest_path: manifest_path.to_path_buf(),
        sources,
    })
}

fn read_manifest(path: &Path) -> Result<Value> {
    Ok(toml::from_str(&fs::read_to_string(path)?)?)
}

fn collect_rust_sources(directory: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_rust_sources(&path, output)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            output.push(path);
        }
    }
    Ok(())
}

fn write_record(hasher: &mut blake3::Hasher, tag: &[u8], bytes: &[u8]) {
    hasher.update(&(tag.len() as u64).to_le_bytes());
    hasher.update(tag);
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
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
        directory
    }

    fn digest(directory: &TempDir) -> [u8; 32] {
        let closure = DigestClosure::from_manifest(&directory.path().join("games/Cargo.toml"))
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
    fn cfg_test_module_does_not_change_digest() {
        let directory = fixture();
        let baseline = digest(&directory);
        write(
            directory.path().join("games/src/lib.rs"),
            "pub const RULE: u32 = 7;\n#[cfg(test)] mod only_tests { const TEST_ONLY: u32 = 99; }\n",
        );
        assert_eq!(digest(&directory), baseline);
    }
}
