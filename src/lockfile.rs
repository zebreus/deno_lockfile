// Copyright 2018-2024 the Deno authors. MIT license.

mod workspace_config;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::printer::print_v4_content;
use crate::transforms::transform1_to_2;
use crate::transforms::transform2_to_3;
use crate::transforms::transform3_to_4;
use crate::DeserializationError;
use crate::LockfileError;
use crate::LockfileErrorReason;
pub use workspace_config::*;

use crate::graphs::LockfilePackageGraph;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NpmPackageLockfileInfo {
  pub serialized_id: String,
  pub integrity: String,
  pub dependencies: Vec<NpmPackageDependencyLockfileInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NpmPackageDependencyLockfileInfo {
  pub name: String,
  pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct NpmPackageInfo {
  pub integrity: String,
  #[serde(default)]
  pub dependencies: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash)]
pub struct JsrPackageInfo {
  pub integrity: String,
  /// List of package requirements found in the dependency.
  ///
  /// This is used to tell when a package can be removed from the lockfile.
  #[serde(skip_serializing_if = "BTreeSet::is_empty")]
  #[serde(default)]
  pub dependencies: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Hash)]
#[serde(rename_all = "camelCase")]
pub struct LockfileContent {
  /// The lockfile version
  pub(crate) version: String,
  /// Mapping between requests for deno specifiers and resolved packages, eg.
  /// {
  ///   "jsr:@foo/bar@^2.1": "jsr:@foo/bar@2.1.3",
  ///   "npm:@ts-morph/common@^11": "npm:@ts-morph/common@11.0.0",
  /// }
  #[serde(skip_serializing_if = "BTreeMap::is_empty")]
  #[serde(default)]
  pub specifiers: BTreeMap<String, String>,

  /// Mapping between resolved jsr specifiers and their associated info, eg.
  /// {
  ///   "@oak/oak@12.6.3": {
  ///     "dependencies": [
  ///       "jsr:@std/bytes@0.210",
  ///       // ...etc...
  ///       "npm:path-to-regexpr@^6.2"
  ///     ]
  ///   }
  /// }
  #[serde(skip_serializing_if = "BTreeMap::is_empty")]
  #[serde(default)]
  pub jsr: BTreeMap<String, JsrPackageInfo>,

  /// Mapping between resolved npm specifiers and their associated info, eg.
  /// {
  ///   "chalk@5.0.0": {
  ///     "integrity": "sha512-...",
  ///     "dependencies": {
  ///       "ansi-styles": "ansi-styles@4.1.0",
  ///     }
  ///   }
  /// }
  #[serde(skip_serializing_if = "BTreeMap::is_empty")]
  #[serde(default)]
  pub npm: BTreeMap<String, NpmPackageInfo>,
  #[serde(skip_serializing_if = "BTreeMap::is_empty")]
  #[serde(default)]
  pub redirects: BTreeMap<String, String>,
  // todo(dsherret): in the next lockfile version we should skip
  // serializing this when it's empty
  /// Mapping between URLs and their checksums for "http:" and "https:" deps
  #[serde(default)]
  pub(crate) remote: BTreeMap<String, String>,
  #[serde(skip_serializing_if = "WorkspaceConfigContent::is_empty")]
  #[serde(default)]
  pub(crate) workspace: WorkspaceConfigContent,
}

impl LockfileContent {
  /// Parse the content of a JSON string representing a lockfile in the latest version
  pub fn from_json(
    json: serde_json::Value,
  ) -> Result<Self, DeserializationError> {
    fn extract_nv_from_id(value: &str) -> Option<(&str, &str)> {
      if value.is_empty() {
        return None;
      }
      let at_index = value[1..].find('@').map(|i| i + 1)?;
      let name = &value[..at_index];
      let version = &value[at_index + 1..];
      Some((name, version))
    }

    fn split_pkg_req(value: &str) -> Option<(&str, Option<&str>)> {
      if value.len() < 5 {
        return None;
      }
      // 5 is length of `jsr:@`/`npm:@`
      let Some(at_index) = value[5..].find('@').map(|i| i + 5) else {
        // no version requirement
        // ex. `npm:jsonc-parser` or `jsr:@pkg/scope`
        return Some((value, None));
      };
      let name = &value[..at_index];
      let version = &value[at_index + 1..];
      Some((name, Some(version)))
    }

    #[derive(Debug, Deserialize)]
    struct RawNpmPackageInfo {
      pub integrity: String,
      #[serde(default)]
      pub dependencies: Vec<String>,
    }

    #[derive(Debug, Deserialize)]
    struct RawJsrPackageInfo {
      pub integrity: String,
      #[serde(default)]
      pub dependencies: Vec<String>,
    }

    fn deserialize_section<T: DeserializeOwned + Default>(
      json: &mut serde_json::Map<String, serde_json::Value>,
      key: &'static str,
    ) -> Result<T, DeserializationError> {
      match json.remove(key) {
        Some(value) => serde_json::from_value(value)
          .map_err(|err| DeserializationError::FailedDeserializing(key, err)),
        None => Ok(Default::default()),
      }
    }

    use serde_json::Value;

    let Value::Object(mut json) = json else {
      return Ok(Self::empty());
    };

    // TODO: This code is just copied from the previous implementation, that allowed parsing old lockfiles. It can probably be significantly simplified.
    let (jsr, specifiers, npm) = {
      let specifiers: BTreeMap<String, String> =
        deserialize_section(&mut json, "specifiers")?;
      let mut npm: BTreeMap<String, NpmPackageInfo> = Default::default();
      let raw_npm: BTreeMap<String, RawNpmPackageInfo> =
        deserialize_section(&mut json, "npm")?;
      if !raw_npm.is_empty() {
        // collect the versions
        let mut version_by_dep_name: HashMap<String, String> =
          HashMap::with_capacity(raw_npm.len());
        for id in raw_npm.keys() {
          let Some((name, version)) = extract_nv_from_id(id) else {
            return Err(DeserializationError::InvalidNpmPackageId(
              id.to_string(),
            ));
          };
          version_by_dep_name.insert(name.to_string(), version.to_string());
        }

        // now go through and create the resolved npm package information
        for (key, value) in raw_npm {
          let mut dependencies = BTreeMap::new();
          for dep in value.dependencies {
            let (left, right) = match extract_nv_from_id(&dep) {
              Some((name, version)) => (name, version),
              None => match version_by_dep_name.get(&dep) {
                Some(version) => (dep.as_str(), version.as_str()),
                None => return Err(DeserializationError::MissingPackage(dep)),
              },
            };
            let (key, package_name, version) = match right.strip_prefix("npm:")
            {
              Some(right) => {
                // ex. key@npm:package-a@version
                match extract_nv_from_id(right) {
                  Some((package_name, version)) => {
                    (left, package_name, version)
                  }
                  None => {
                    return Err(
                      DeserializationError::InvalidNpmPackageDependency(
                        dep.to_string(),
                      ),
                    );
                  }
                }
              }
              None => (left, left, right),
            };
            dependencies
              .insert(key.to_string(), format!("{}@{}", package_name, version));
          }
          npm.insert(
            key,
            NpmPackageInfo {
              integrity: value.integrity,
              dependencies,
            },
          );
        }
      }
      let mut jsr: BTreeMap<String, JsrPackageInfo> = Default::default();
      {
        let raw_jsr: BTreeMap<String, RawJsrPackageInfo> =
          deserialize_section(&mut json, "jsr")?;
        if !raw_jsr.is_empty() {
          // collect the specifier information
          let mut to_resolved_specifiers: HashMap<&str, Option<&str>> =
            HashMap::with_capacity(specifiers.len() * 2);
          // first insert the specifiers that should be left alone
          for specifier in specifiers.keys() {
            to_resolved_specifiers.insert(specifier, None);
          }
          // then insert the mapping specifiers
          for specifier in specifiers.keys() {
            let Some((name, req)) = split_pkg_req(specifier) else {
              return Err(DeserializationError::InvalidPackageSpecifier(
                specifier.to_string(),
              ));
            };
            if req.is_some() {
              let entry = to_resolved_specifiers.entry(name);
              // if an entry is occupied that means there's multiple specifiers
              // for the same name, such as one without a req, so ignore inserting
              // here
              if let std::collections::hash_map::Entry::Vacant(entry) = entry {
                entry.insert(Some(specifier));
              }
            }
          }

          // now go through the dependencies mapping to the new ones
          for (key, value) in raw_jsr {
            let mut dependencies = BTreeSet::new();
            for dep in value.dependencies {
              let Some(maybe_specifier) =
                to_resolved_specifiers.get(dep.as_str())
              else {
                todo!();
              };
              dependencies
                .insert(maybe_specifier.map(|s| s.to_string()).unwrap_or(dep));
            }
            jsr.insert(
              key,
              JsrPackageInfo {
                integrity: value.integrity,
                dependencies,
              },
            );
          }
        }
      }

      (jsr, specifiers, npm)
    };

    Ok(LockfileContent {
      version: json
        .remove("version")
        .and_then(|v| match v {
          Value::String(v) => Some(v),
          _ => None,
        })
        .unwrap_or_else(|| "3".to_string()),
      jsr,
      specifiers,
      npm,
      redirects: deserialize_section(&mut json, "redirects")?,
      remote: deserialize_section(&mut json, "remote")?,
      workspace: deserialize_section(&mut json, "workspace")?,
    })
  }

  /// Convert the lockfile content to a v4 lockfile
  ///
  /// You should probably use [Lockfile::]
  pub fn to_json(&self) -> String {
    // TODO: Think about adding back support for older lockfile versions
    let mut text = String::new();
    print_v4_content(&self, &mut text);
    return text;
  }

  fn empty() -> Self {
    Self {
      version: "4".to_string(),
      redirects: Default::default(),
      remote: BTreeMap::new(),
      workspace: Default::default(),
      jsr: Default::default(),
      specifiers: Default::default(),
      npm: Default::default(),
    }
  }

  pub fn is_empty(&self) -> bool {
    self.jsr.is_empty()
      && self.npm.is_empty()
      && self.specifiers.is_empty()
      && self.redirects.is_empty()
      && self.remote.is_empty()
      && self.workspace.is_empty()
  }
}

#[derive(Debug, Clone, Hash)]
pub struct Lockfile {
  /// If this flag is set, the current content of the lockfile is ignored and a new lockfile is generated.
  ///
  /// If it is unset, the lockfile will only be changed, if the content changed.
  overwrite: bool,
  /// Automatically set to true, if the content of the lockfile has changed.
  ///
  /// Once this flag is set to true, it will never be reset to false, except through [Lockfile::resolve_write_bytes]
  has_content_changed: bool,
  /// Current content of the lockfile
  content: LockfileContent,
  /// Path of the lockfile
  filename: PathBuf,
  /// Original content of the lockfile
  ///
  /// We need to store this, so that [Lockfile::to_json] can return the exact original content, if there were no changes
  original_content: Option<String>,
}

impl Lockfile {
  pub fn new_empty(filename: PathBuf, overwrite: bool) -> Lockfile {
    Lockfile {
      overwrite,
      has_content_changed: false,
      content: LockfileContent::empty(),
      filename,
      original_content: Option::Some(String::new()),
    }
  }

  pub fn has_content_changed(&self) -> bool {
    self.has_content_changed
  }

  /// Create a new [`Lockfile`] instance from given filename and its content.
  ///
  /// TODO: Is this function our main way
  pub fn with_lockfile_content(
    filename: PathBuf,
    file_content: &str,
    overwrite: bool,
  ) -> Result<Lockfile, LockfileError> {
    fn load_content(
      content: &str,
    ) -> Result<LockfileContent, LockfileErrorReason> {
      let value: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(content)
          .map_err(LockfileErrorReason::ParseError)?;
      let version = value.get("version").and_then(|v| v.as_str());
      let value = match version {
        Some("4") => value,
        Some("3") => transform3_to_4(value)?,
        Some("2") => transform3_to_4(transform2_to_3(value))?,
        None => transform3_to_4(transform2_to_3(transform1_to_2(value)))?,
        Some(version) => {
          return Err(LockfileErrorReason::UnsupportedVersion {
            version: version.to_string(),
          });
        }
      };
      let content = LockfileContent::from_json(value.into())
        .map_err(LockfileErrorReason::DeserializationError)?;

      Ok(content)
    }

    // Writing a lock file always uses the new format.
    if overwrite {
      return Ok(Lockfile::new_empty(filename, overwrite));
    }

    if file_content.trim().is_empty() {
      return Err(LockfileError {
        filename: filename.display().to_string(),
        reason: LockfileErrorReason::Empty,
      });
    }

    let content =
      load_content(file_content).map_err(|reason| LockfileError {
        filename: filename.display().to_string(),
        reason,
      })?;
    Ok(Lockfile {
      overwrite,
      has_content_changed: false,
      content,
      filename,
      original_content: Some(file_content.into()),
    })
  }

  /// Get the lockfile contents as a formatted JSON string
  ///
  /// If no changes were done, this will return the exact lockfile content that was used to create this lockfile.
  ///
  /// If the lockfile was changed, it will be returned as an upgraded v4 lockfile
  pub fn to_json(&self) -> String {
    if let Some(original_content) = &self.original_content {
      if !self.has_content_changed && !self.overwrite {
        return original_content.clone();
      }
    }

    if self.content.version != "4" {
      panic!("Should never happen; for now only v4 lockfiles can be printed")
    }
    self.content.to_json()
  }

  pub fn set_workspace_config(&mut self, options: SetWorkspaceConfigOptions) {
    let was_empty_before = self.content.is_empty();
    let old_workspace_config = self.content.workspace.clone();

    // Update the workspace
    let config = WorkspaceConfig::new(options, &self.content.workspace);
    self.content.workspace.update(config);

    // We dont need to do the rest, if we changed nothing
    if old_workspace_config == self.content.workspace {
      return;
    }

    // If the lockfile is empty, it's most likely not created yet and so
    // we don't want workspace configuration being added to the lockfile to cause
    // a lockfile to be created.
    // So we only set has_content_changed if it wasnt empty before
    if !was_empty_before {
      // revert it back so this change doesn't by itself cause
      // a lockfile to be created.
      self.has_content_changed = true;
    }

    let old_deps: BTreeSet<&String> =
      old_workspace_config.get_all_dep_reqs().collect();
    let new_deps: BTreeSet<&String> =
      self.content.workspace.get_all_dep_reqs().collect();
    let removed_deps: BTreeSet<&String> =
      old_deps.difference(&new_deps).copied().collect();

    if removed_deps.is_empty() {
      return;
    }

    // Remove removed dependencies from packages and remote
    let npm = std::mem::take(&mut self.content.npm);
    let jsr = std::mem::take(&mut self.content.jsr);
    let specifiers = std::mem::take(&mut self.content.specifiers);
    let mut graph = LockfilePackageGraph::from_lockfile(
      npm,
      jsr,
      specifiers,
      old_deps.iter().map(|dep| dep.as_str()),
    );
    graph.remove_root_packages(removed_deps.into_iter());
    graph.populate_packages(
      &mut self.content.npm,
      &mut self.content.jsr,
      &mut self.content.specifiers,
    );
  }

  /// Gets the bytes that should be written to the disk.
  ///
  /// Ideally when the caller should use an "atomic write"
  /// when writing this—write to a temporary file beside the
  /// lockfile, then rename to overwrite. This will make the
  /// lockfile more resilient when multiple processes are
  /// writing to it.
  ///
  /// If you dont write the bytes received by this function to the lockfile, it will result in undefined behaviour
  // TODO: Resetting `has_content_change` probably has some funny side effects; investigate
  pub fn resolve_write_bytes(&mut self) -> Option<Vec<u8>> {
    if !self.has_content_changed && !self.overwrite {
      return None;
    }

    // This weird order is neccessary, because to_json will return the original_content, if there
    let json_string = self.to_json();
    self.has_content_changed = false;
    self.original_content = Some(json_string.clone());
    Some(json_string.into_bytes())
  }

  pub fn remote(&self) -> &BTreeMap<String, String> {
    &self.content.remote
  }

  pub fn content(&self) -> &LockfileContent {
    &self.content
  }

  /// Inserts a remote specifier into the lockfile replacing the existing package if it exists.
  ///
  /// WARNING: It is up to the caller to ensure checksums of remote modules are
  /// valid before it is inserted here.
  pub fn insert_remote(&mut self, specifier: String, hash: String) {
    let entry = self.content.remote.entry(specifier);
    match entry {
      Entry::Vacant(entry) => {
        entry.insert(hash);
        self.has_content_changed = true;
      }
      Entry::Occupied(mut entry) => {
        if entry.get() != &hash {
          entry.insert(hash);
          self.has_content_changed = true;
        }
      }
    }
  }

  /// Inserts an npm package into the lockfile replacing the existing package if it exists.
  ///
  /// WARNING: It is up to the caller to ensure checksums of packages are
  /// valid before it is inserted here.
  pub fn insert_npm_package(&mut self, package_info: NpmPackageLockfileInfo) {
    let dependencies = package_info
      .dependencies
      .into_iter()
      .map(|dep| (dep.name, dep.id))
      .collect::<BTreeMap<String, String>>();

    let entry = self.content.npm.entry(package_info.serialized_id);
    let package_info = NpmPackageInfo {
      integrity: package_info.integrity,
      dependencies,
    };
    match entry {
      Entry::Vacant(entry) => {
        entry.insert(package_info);
        self.has_content_changed = true;
      }
      Entry::Occupied(mut entry) => {
        if *entry.get() != package_info {
          entry.insert(package_info);
          self.has_content_changed = true;
        }
      }
    }
  }

  /// Inserts a package specifier into the lockfile.
  pub fn insert_package_specifier(
    &mut self,
    serialized_package_req: String,
    serialized_package_id: String,
  ) {
    let entry = self.content.specifiers.entry(serialized_package_req);
    match entry {
      Entry::Vacant(entry) => {
        entry.insert(serialized_package_id);
        self.has_content_changed = true;
      }
      Entry::Occupied(mut entry) => {
        if *entry.get() != serialized_package_id {
          entry.insert(serialized_package_id);
          self.has_content_changed = true;
        }
      }
    }
  }

  /// Inserts a JSR package into the lockfile replacing the existing package's integrity
  /// if they differ.
  ///
  /// WARNING: It is up to the caller to ensure checksums of packages are
  /// valid before it is inserted here.
  pub fn insert_package(&mut self, name: String, integrity: String) {
    let entry = self.content.jsr.entry(name);
    match entry {
      Entry::Vacant(entry) => {
        entry.insert(JsrPackageInfo {
          integrity,
          dependencies: Default::default(),
        });
        self.has_content_changed = true;
      }
      Entry::Occupied(mut entry) => {
        if *entry.get().integrity != integrity {
          entry.get_mut().integrity = integrity;
          self.has_content_changed = true;
        }
      }
    }
  }

  /// Adds package dependencies of a JSR package. This is only used to track
  /// when packages can be removed from the lockfile.
  pub fn add_package_deps(
    &mut self,
    name: &str,
    deps: impl Iterator<Item = String>,
  ) {
    if let Some(pkg) = self.content.jsr.get_mut(name) {
      let start_count = pkg.dependencies.len();
      pkg.dependencies.extend(deps);
      let end_count = pkg.dependencies.len();
      if start_count != end_count {
        self.has_content_changed = true;
      }
    }
  }

  /// Adds a redirect to the lockfile
  pub fn insert_redirect(&mut self, from: String, to: String) {
    if from.starts_with("jsr:") {
      return;
    }

    let entry = self.content.redirects.entry(from);
    match entry {
      Entry::Vacant(entry) => {
        entry.insert(to);
        self.has_content_changed = true;
      }
      Entry::Occupied(mut entry) => {
        if *entry.get() != to {
          entry.insert(to);
          self.has_content_changed = true;
        }
      }
    }
  }

  /// Removes a redirect from the lockfile
  ///
  /// Returns the target of the removed redirect.
  pub fn remove_redirect(&mut self, from: &str) -> Option<String> {
    let removed_value = self.content.redirects.remove(from);
    if removed_value.is_some() {
      self.has_content_changed = true;
    }
    removed_value
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use pretty_assertions::assert_eq;

  const LOCKFILE_JSON: &str = r#"
{
  "version": "3",
  "packages": {
    "specifiers": {},
    "npm": {
      "nanoid@3.3.4": {
        "integrity": "sha512-MqBkQh/OHTS2egovRtLk45wEyNXwF+cokD+1YPf9u5VfJiRdAiRwB2froX5Co9Rh20xs4siNPm8naNotSD6RBw==",
        "dependencies": {}
      },
      "picocolors@1.0.0": {
        "integrity": "sha512-foobar",
        "dependencies": {}
      }
    }
  },
  "remote": {
    "https://deno.land/std@0.71.0/textproto/mod.ts": "3118d7a42c03c242c5a49c2ad91c8396110e14acca1324e7aaefd31a999b71a4",
    "https://deno.land/std@0.71.0/async/delay.ts": "35957d585a6e3dd87706858fb1d6b551cb278271b03f52c5a2cb70e65e00c26a"
  }
}"#;

  fn setup(overwrite: bool) -> Result<Lockfile, LockfileError> {
    let file_path =
      std::env::current_dir().unwrap().join("valid_lockfile.json");
    Lockfile::with_lockfile_content(file_path, LOCKFILE_JSON, overwrite)
  }

  #[test]
  fn future_version_unsupported() {
    let file_path = PathBuf::from("lockfile.json");
    assert_eq!(
      Lockfile::with_lockfile_content(
        file_path,
        "{ \"version\": \"2000\" }",
        false
      )
      .err()
      .unwrap().to_string(),
      "Unsupported lockfile version '2000'. Try upgrading Deno or recreating the lockfile at 'lockfile.json'.".to_string()
    );
  }

  #[test]
  fn new_valid_lockfile() {
    let lockfile = setup(false).unwrap();

    let remote = lockfile.content.remote;
    let keys: Vec<String> = remote.keys().cloned().collect();
    let expected_keys = vec![
      String::from("https://deno.land/std@0.71.0/async/delay.ts"),
      String::from("https://deno.land/std@0.71.0/textproto/mod.ts"),
    ];

    assert_eq!(keys.len(), 2);
    assert_eq!(keys, expected_keys);
  }

  #[test]
  fn with_lockfile_content_for_valid_lockfile() {
    let file_path = PathBuf::from("/foo");
    let result =
      Lockfile::with_lockfile_content(file_path, LOCKFILE_JSON, false).unwrap();

    let remote = result.content.remote;
    let keys: Vec<String> = remote.keys().cloned().collect();
    let expected_keys = vec![
      String::from("https://deno.land/std@0.71.0/async/delay.ts"),
      String::from("https://deno.land/std@0.71.0/textproto/mod.ts"),
    ];

    assert_eq!(keys.len(), 2);
    assert_eq!(keys, expected_keys);
  }

  #[test]
  fn new_lockfile_from_file_and_insert() {
    let mut lockfile = setup(false).unwrap();

    lockfile.insert_remote(
      "https://deno.land/std@0.71.0/io/util.ts".to_string(),
      "checksum-1".to_string(),
    );

    let remote = lockfile.content.remote;
    let keys: Vec<String> = remote.keys().cloned().collect();
    let expected_keys = vec![
      String::from("https://deno.land/std@0.71.0/async/delay.ts"),
      String::from("https://deno.land/std@0.71.0/io/util.ts"),
      String::from("https://deno.land/std@0.71.0/textproto/mod.ts"),
    ];
    assert_eq!(keys.len(), 3);
    assert_eq!(keys, expected_keys);
  }

  #[test]
  fn new_lockfile_and_write() {
    let mut lockfile = setup(true).unwrap();

    // true since overwrite was true
    // assert!(lockfile.resolve_write_bytes().is_some());

    lockfile.insert_remote(
      "https://deno.land/std@0.71.0/textproto/mod.ts".to_string(),
      "checksum-1".to_string(),
    );
    lockfile.insert_remote(
      "https://deno.land/std@0.71.0/io/util.ts".to_string(),
      "checksum-2".to_string(),
    );
    lockfile.insert_remote(
      "https://deno.land/std@0.71.0/async/delay.ts".to_string(),
      "checksum-3".to_string(),
    );

    let bytes = lockfile.resolve_write_bytes().unwrap();
    let contents_json =
      serde_json::from_slice::<serde_json::Value>(&bytes).unwrap();
    let object = contents_json["remote"].as_object().unwrap();

    assert_eq!(
      object
        .get("https://deno.land/std@0.71.0/textproto/mod.ts")
        .and_then(|v| v.as_str()),
      Some("checksum-1")
    );

    // confirm that keys are sorted alphabetically
    let mut keys = object.keys().map(|k| k.as_str());
    assert_eq!(
      keys.next(),
      Some("https://deno.land/std@0.71.0/async/delay.ts")
    );
    assert_eq!(keys.next(), Some("https://deno.land/std@0.71.0/io/util.ts"));
    assert_eq!(
      keys.next(),
      Some("https://deno.land/std@0.71.0/textproto/mod.ts")
    );
    assert!(keys.next().is_none());
  }

  #[test]
  fn check_or_insert_lockfile() {
    let mut lockfile = setup(false).unwrap();
    // Setup lockfile
    lockfile.insert_remote(
      "https://deno.land/std@0.71.0/textproto/mod.ts".to_string(),
      "checksum-1".to_string(),
    );
    // By reading the bytes we reset the changed state
    assert!(lockfile.resolve_write_bytes().is_some());
    // Verify that the lockfile has no unwritten changes
    assert!(lockfile.resolve_write_bytes().is_none());

    // Not a change, should not cause changes
    lockfile.insert_remote(
      "https://deno.land/std@0.71.0/textproto/mod.ts".to_string(),
      "checksum-1".to_string(),
    );
    assert!(lockfile.resolve_write_bytes().is_none());

    // This is a change, it should cause a write
    lockfile.insert_remote(
      "https://deno.land/std@0.71.0/textproto/mod.ts".to_string(),
      "checksum-new".to_string(),
    );
    assert!(lockfile.resolve_write_bytes().is_some());

    // Not present in lockfile yet, should be inserted and check passed.
    lockfile.insert_remote(
      "https://deno.land/std@0.71.0/http/file_server.ts".to_string(),
      "checksum-1".to_string(),
    );
    assert!(lockfile.resolve_write_bytes().is_some());
  }

  #[test]
  fn returns_the_correct_value_as_json_even_after_writing() {
    let file_path =
      std::env::current_dir().unwrap().join("valid_lockfile.json");
    let lockfile_json = r#"{
  "version": "3",
  "remote": {}
}
"#;
    let mut lockfile =
      Lockfile::with_lockfile_content(file_path, lockfile_json, false).unwrap();

    // Change lockfile
    lockfile.insert_remote(
      "https://deno.land/std@0.71.0/textproto/mod.ts".to_string(),
      "checksum-1".to_string(),
    );
    // Assert it changed
    assert_ne!(lockfile.to_json(), lockfile_json);
    // Assert that to_json returns the changed lockfile even after writing it
    lockfile.resolve_write_bytes();
    assert_ne!(lockfile.to_json(), lockfile_json);
  }

  #[test]
  fn does_always_write_bytes_if_overwrite_is_set() {
    let mut lockfile = setup(true).unwrap();
    assert!(lockfile.resolve_write_bytes().is_some());
  }

  #[test]
  fn does_not_write_bytes_if_overwrite_is_not_set_and_there_are_no_changes() {
    let mut lockfile = setup(false).unwrap();
    assert!(lockfile.resolve_write_bytes().is_none());
  }

  #[test]
  fn does_write_bytes_if_there_are_changes() {
    let mut lockfile = setup(false).unwrap();
    lockfile.insert_remote(
      "https://deno.land/std@0.71.0/http/file_server.ts".to_string(),
      "checksum-1".to_string(),
    );
    assert!(lockfile.resolve_write_bytes().is_some());
  }

  #[test]
  fn does_not_write_bytes_if_all_changes_were_already_written() {
    let mut lockfile = setup(false).unwrap();
    lockfile.insert_remote(
      "https://deno.land/std@0.71.0/http/file_server.ts".to_string(),
      "checksum-1".to_string(),
    );
    assert!(lockfile.resolve_write_bytes().is_some());
    assert!(lockfile.resolve_write_bytes().is_none());
  }

  // // TODO: Currently we always write, when overwrite is set, even if we already wrote the changes before. I think it would be more sane, if we only wrote, when there are unwritten changes. This would probably also mean, that we could just remove the overwrite flag and replace it by setting `has_content_changed` to true, when a lockfile is created with overwrite.
  // #[test]
  // fn does_not_write_bytes_if_overwrite_was_set_but_already_written() {
  //   let mut lockfile = setup(true).unwrap();
  //   assert!(lockfile.resolve_write_bytes().is_some());
  //   assert!(lockfile.resolve_write_bytes().is_none());
  // }

  #[test]
  fn check_or_insert_lockfile_npm() {
    let mut lockfile = setup(false).unwrap();

    // already in lockfile
    let npm_package = NpmPackageLockfileInfo {
      serialized_id: "nanoid@3.3.4".to_string(),
      integrity: "sha512-MqBkQh/OHTS2egovRtLk45wEyNXwF+cokD+1YPf9u5VfJiRdAiRwB2froX5Co9Rh20xs4siNPm8naNotSD6RBw==".to_string(),
      dependencies: vec![],
    };
    lockfile.insert_npm_package(npm_package);
    assert!(!lockfile.has_content_changed);

    // insert package that exists already, but has slightly different properties
    let npm_package = NpmPackageLockfileInfo {
      serialized_id: "picocolors@1.0.0".to_string(),
      integrity: "sha512-1fygroTLlHu66zi26VoTDv8yRgm0Fccecssto+MhsZ0D/DGW2sm8E8AjW7NU5VVTRt5GxbeZ5qBuJr+HyLYkjQ==".to_string(),
      dependencies: vec![],
    };
    lockfile.insert_npm_package(npm_package);
    assert!(lockfile.has_content_changed);

    lockfile.has_content_changed = false;
    let npm_package = NpmPackageLockfileInfo {
      serialized_id: "source-map-js@1.0.2".to_string(),
      integrity: "sha512-R0XvVJ9WusLiqTCEiGCmICCMplcCkIwwR11mOSD9CR5u+IXYdiseeEuXCVAjS54zqwkLcPNnmU4OeJ6tUrWhDw==".to_string(),
      dependencies: vec![],
    };
    // Not present in lockfile yet, should be inserted
    lockfile.insert_npm_package(npm_package.clone());
    assert!(lockfile.has_content_changed);
    lockfile.has_content_changed = false;

    // this one should not say the lockfile has changed because it's the same
    lockfile.insert_npm_package(npm_package);
    assert!(!lockfile.has_content_changed);

    let npm_package = NpmPackageLockfileInfo {
      serialized_id: "source-map-js@1.0.2".to_string(),
      integrity: "sha512-foobar".to_string(),
      dependencies: vec![],
    };
    // Now present in lockfile, should be changed due to different integrity
    lockfile.insert_npm_package(npm_package);
    assert!(lockfile.has_content_changed);
  }

  #[test]
  fn lockfile_with_redirects() {
    let mut lockfile = Lockfile::with_lockfile_content(
      PathBuf::from("/foo/deno.lock"),
      r#"{
  "version": "4",
  "redirects": {
    "https://deno.land/x/std/mod.ts": "https://deno.land/std@0.190.0/mod.ts"
  }
}"#,
      false,
    )
    .unwrap();
    lockfile.insert_redirect(
      "https://deno.land/x/other/mod.ts".to_string(),
      "https://deno.land/x/other@0.1.0/mod.ts".to_string(),
    );
    assert_eq!(
      lockfile.to_json(),
      r#"{
  "version": "4",
  "redirects": {
    "https://deno.land/x/other/mod.ts": "https://deno.land/x/other@0.1.0/mod.ts",
    "https://deno.land/x/std/mod.ts": "https://deno.land/std@0.190.0/mod.ts"
  }
}
"#,
    );
  }

  #[test]
  fn test_version_does_not_change_if_lockfile_did_not_change() {
    let original_content = r#"{
  "version": "3",
  "redirects": {
    "https://deno.land/x/std/mod.ts": "https://deno.land/std@0.190.0/mod.ts"
  },
  "remote": {}
}"#;
    let mut lockfile = Lockfile::with_lockfile_content(
      PathBuf::from("/foo/deno.lock"),
      original_content,
      false,
    )
    .unwrap();
    // Insert already existing redirect
    lockfile.insert_redirect(
      "https://deno.land/x/std/mod.ts".to_string(),
      "https://deno.land/std@0.190.0/mod.ts".to_string(),
    );
    assert!(!lockfile.has_content_changed());
    assert_eq!(lockfile.to_json(), original_content,);
  }

  #[test]
  fn test_insert_redirect() {
    let mut lockfile = Lockfile::with_lockfile_content(
      PathBuf::from("/foo/deno.lock"),
      r#"{
  "version": "3",
  "redirects": {
    "https://deno.land/x/std/mod.ts": "https://deno.land/std@0.190.0/mod.ts"
  },
  "remote": {}
}"#,
      false,
    )
    .unwrap();
    lockfile.insert_redirect(
      "https://deno.land/x/std/mod.ts".to_string(),
      "https://deno.land/std@0.190.0/mod.ts".to_string(),
    );
    assert!(!lockfile.has_content_changed);
    lockfile.insert_redirect(
      "https://deno.land/x/std/mod.ts".to_string(),
      "https://deno.land/std@0.190.1/mod.ts".to_string(),
    );
    assert!(lockfile.has_content_changed);
    lockfile.insert_redirect(
      "https://deno.land/x/std/other.ts".to_string(),
      "https://deno.land/std@0.190.1/other.ts".to_string(),
    );
    assert_eq!(
      lockfile.to_json(),
      r#"{
  "version": "4",
  "redirects": {
    "https://deno.land/x/std/mod.ts": "https://deno.land/std@0.190.1/mod.ts",
    "https://deno.land/x/std/other.ts": "https://deno.land/std@0.190.1/other.ts"
  }
}
"#,
    );
  }

  #[test]
  fn test_insert_jsr() {
    let mut lockfile = Lockfile::with_lockfile_content(
      PathBuf::from("/foo/deno.lock"),
      r#"{
  "version": "3",
  "packages": {
    "specifiers": {
      "jsr:path": "jsr:@std/path@0.75.0"
    }
  },
  "remote": {}
}"#,
      false,
    )
    .unwrap();
    lockfile.insert_package_specifier(
      "jsr:path".to_string(),
      "jsr:@std/path@0.75.0".to_string(),
    );
    assert!(!lockfile.has_content_changed);
    lockfile.insert_package_specifier(
      "jsr:path".to_string(),
      "jsr:@std/path@0.75.1".to_string(),
    );
    assert!(lockfile.has_content_changed);
    lockfile.insert_package_specifier(
      "jsr:@foo/bar@^2".to_string(),
      "jsr:@foo/bar@2.1.2".to_string(),
    );
    assert_eq!(
      lockfile.to_json(),
      r#"{
  "version": "4",
  "specifiers": {
    "jsr:@foo/bar@^2": "jsr:@foo/bar@2.1.2",
    "jsr:path": "jsr:@std/path@0.75.1"
  }
}
"#,
    );
  }

  #[test]
  fn read_version_1() {
    let content: &str = r#"{
      "https://deno.land/std@0.71.0/textproto/mod.ts": "3118d7a42c03c242c5a49c2ad91c8396110e14acca1324e7aaefd31a999b71a4",
      "https://deno.land/std@0.71.0/async/delay.ts": "35957d585a6e3dd87706858fb1d6b551cb278271b03f52c5a2cb70e65e00c26a"
    }"#;
    let file_path = PathBuf::from("lockfile.json");
    let lockfile =
      Lockfile::with_lockfile_content(file_path, content, false).unwrap();
    assert_eq!(lockfile.content.version, "4");
    assert_eq!(lockfile.content.remote.len(), 2);
  }

  #[test]
  fn read_version_2() {
    let content: &str = r#"{
      "version": "2",
      "remote": {
        "https://deno.land/std@0.71.0/textproto/mod.ts": "3118d7a42c03c242c5a49c2ad91c8396110e14acca1324e7aaefd31a999b71a4",
        "https://deno.land/std@0.71.0/async/delay.ts": "35957d585a6e3dd87706858fb1d6b551cb278271b03f52c5a2cb70e65e00c26a"
      },
      "npm": {
        "specifiers": {
          "nanoid": "nanoid@3.3.4"
        },
        "packages": {
          "nanoid@3.3.4": {
            "integrity": "sha512-MqBkQh/OHTS2egovRtLk45wEyNXwF+cokD+1YPf9u5VfJiRdAiRwB2froX5Co9Rh20xs4siNPm8naNotSD6RBw==",
            "dependencies": {}
          },
          "picocolors@1.0.0": {
            "integrity": "sha512-foobar",
            "dependencies": {}
          }
        }
      }
    }"#;
    let file_path = PathBuf::from("lockfile.json");
    let lockfile =
      Lockfile::with_lockfile_content(file_path, content, false).unwrap();
    assert_eq!(lockfile.content.version, "4");
    assert_eq!(lockfile.content.npm.len(), 2);
    assert_eq!(
      lockfile.content.specifiers,
      BTreeMap::from([(
        "npm:nanoid".to_string(),
        "npm:nanoid@3.3.4".to_string()
      )])
    );
    assert_eq!(lockfile.content.remote.len(), 2);
  }

  #[test]
  fn insert_package_deps_changes_empty_insert() {
    let content: &str = r#"{
      "version": "2",
      "remote": {}
    }"#;
    let file_path = PathBuf::from("lockfile.json");
    let mut lockfile =
      Lockfile::with_lockfile_content(file_path, content, false).unwrap();

    assert!(!lockfile.has_content_changed);
    lockfile.insert_package("dep".to_string(), "integrity".to_string());
    // has changed even though it was empty
    assert!(lockfile.has_content_changed);

    // now try inserting the same package
    lockfile.has_content_changed = false;
    lockfile.insert_package("dep".to_string(), "integrity".to_string());
    assert!(!lockfile.has_content_changed);

    // now with new deps
    lockfile.add_package_deps("dep", vec!["dep2".to_string()].into_iter());
    assert!(lockfile.has_content_changed);
  }

  #[test]
  fn empty_lockfile_nicer_error() {
    let content: &str = r#"  "#;
    let file_path = PathBuf::from("lockfile.json");
    let err = Lockfile::with_lockfile_content(file_path, content, false)
      .err()
      .unwrap();
    assert_eq!(
      err.to_string(),
      "Unable to read lockfile. Lockfile was empty at 'lockfile.json'."
    );
  }

  #[test]
  fn should_maintain_changed_false_flag_when_adding_a_workspace_to_an_empty_lockfile(
  ) {
    // should maintain the has_content_changed flag when lockfile empty
    let mut lockfile = Lockfile::new_empty(PathBuf::from("./deno.lock"), false);

    assert!(!lockfile.has_content_changed());
    lockfile.set_workspace_config(SetWorkspaceConfigOptions {
      no_config: false,
      no_npm: false,
      config: WorkspaceConfig {
        root: WorkspaceMemberConfig {
          dependencies: BTreeSet::from(["jsr:@scope/package".to_string()]),
          package_json_deps: Default::default(),
        },
        members: BTreeMap::new(),
      },
    });
    assert!(!lockfile.has_content_changed()); // should not have changed
  }

  #[test]
  fn should_maintain_changed_true_flag_when_adding_a_workspace_to_an_empty_lockfile(
  ) {
    // should maintain has_content_changed flag when true and lockfile is empty
    let mut lockfile = Lockfile::new_empty(PathBuf::from("./deno.lock"), false);
    lockfile.insert_redirect("a".to_string(), "b".to_string());
    lockfile.remove_redirect("a");

    lockfile.set_workspace_config(SetWorkspaceConfigOptions {
      no_config: false,
      no_npm: false,
      config: WorkspaceConfig {
        root: WorkspaceMemberConfig {
          dependencies: BTreeSet::from(["jsr:@scope/package2".to_string()]),
          package_json_deps: Default::default(),
        },
        members: BTreeMap::new(),
      },
    });
    assert!(lockfile.has_content_changed());
  }

  #[test]
  fn should_be_changed_if_a_workspace_is_added_and_the_lockfile_is_not_emtpy() {
    // should not maintain the has_content_changed flag when lockfile is not empty
    let mut lockfile = Lockfile::new_empty(PathBuf::from("./deno.lock"), true);
    lockfile.insert_redirect("a".to_string(), "b".to_string());
    // Reset has_content_changed flag by writing
    lockfile.resolve_write_bytes();
    assert!(!lockfile.has_content_changed());

    lockfile.set_workspace_config(SetWorkspaceConfigOptions {
      no_config: false,
      no_npm: false,
      config: WorkspaceConfig {
        root: WorkspaceMemberConfig {
          dependencies: BTreeSet::from(["jsr:@scope/package".to_string()]),
          package_json_deps: Default::default(),
        },
        members: BTreeMap::new(),
      },
    });

    assert!(lockfile.has_content_changed()); // should have changed since lockfile was not empty
  }

  #[test]
  fn should_be_changed_if_a_dep_is_removed_from_the_workspace() {
    // Setup
    let mut lockfile = Lockfile::new_empty(PathBuf::from("./deno.lock"), true);
    lockfile.insert_package("beta".to_string(), "checksum".to_string());
    lockfile.set_workspace_config(SetWorkspaceConfigOptions {
      no_config: false,
      no_npm: false,
      config: WorkspaceConfig {
        root: Default::default(),
        members: BTreeMap::from([(
          "thing".into(),
          WorkspaceMemberConfig {
            dependencies: BTreeSet::from(["beta".into()]),
            package_json_deps: BTreeSet::new(),
          },
        )]),
      },
    });
    lockfile.resolve_write_bytes();
    assert!(!lockfile.has_content_changed());

    lockfile.set_workspace_config(SetWorkspaceConfigOptions {
      no_config: false,
      no_npm: false,
      config: WorkspaceConfig {
        root: Default::default(),
        members: BTreeMap::new(),
      },
    });
    assert!(lockfile.has_content_changed());
  }

  #[test]
  fn should_be_changed_if_a_dep_is_moved_workspace_root_to_a_member_a() {
    // Setup
    let mut lockfile = Lockfile::new_empty(PathBuf::from("./deno.lock"), true);
    lockfile.insert_package("beta".to_string(), "checksum".to_string());
    lockfile.set_workspace_config(SetWorkspaceConfigOptions {
      no_config: false,
      no_npm: false,
      config: WorkspaceConfig {
        root: WorkspaceMemberConfig {
          dependencies: BTreeSet::from(["beta".into()]),
          package_json_deps: BTreeSet::new(),
        },
        members: BTreeMap::from([("thing".into(), Default::default())]),
      },
    });
    lockfile.resolve_write_bytes();
    assert!(!lockfile.has_content_changed());

    lockfile.set_workspace_config(SetWorkspaceConfigOptions {
      no_config: false,
      no_npm: false,
      config: WorkspaceConfig {
        root: Default::default(),
        members: BTreeMap::from([(
          "thing".into(),
          WorkspaceMemberConfig {
            dependencies: BTreeSet::from(["beta".into()]),
            package_json_deps: BTreeSet::new(),
          },
        )]),
      },
    });
    assert!(lockfile.has_content_changed());
  }

  #[test]
  fn should_be_changed_if_a_dep_is_moved_workspace_root_to_a_member_b() {
    // Setup
    let mut lockfile = Lockfile::new_empty(PathBuf::from("./deno.lock"), true);
    lockfile.insert_package("beta".to_string(), "checksum".to_string());
    lockfile.set_workspace_config(SetWorkspaceConfigOptions {
      no_config: false,
      no_npm: false,
      config: WorkspaceConfig {
        root: WorkspaceMemberConfig {
          dependencies: BTreeSet::from(["beta".into()]),
          package_json_deps: BTreeSet::new(),
        },
        members: Default::default(),
      },
    });
    lockfile.resolve_write_bytes();
    assert!(!lockfile.has_content_changed());

    lockfile.set_workspace_config(SetWorkspaceConfigOptions {
      no_config: false,
      no_npm: false,
      config: WorkspaceConfig {
        root: Default::default(),
        members: BTreeMap::from([(
          "thing".into(),
          WorkspaceMemberConfig {
            dependencies: BTreeSet::from(["beta".into()]),
            package_json_deps: BTreeSet::new(),
          },
        )]),
      },
    });
    assert!(lockfile.has_content_changed());
  }

  #[test]
  fn should_preserve_workspace_on_no_npm() {
    // Setup
    let mut lockfile = Lockfile::new_empty(PathBuf::from("./deno.lock"), true);
    lockfile.insert_package("alpha".to_string(), "checksum".to_string());
    lockfile.insert_package("beta".to_string(), "checksum".to_string());
    lockfile.insert_package("gamma".to_string(), "checksum".to_string());
    lockfile.set_workspace_config(SetWorkspaceConfigOptions {
      no_config: false,
      no_npm: false,
      config: WorkspaceConfig {
        root: WorkspaceMemberConfig {
          dependencies: BTreeSet::from(["alpha".into()]),
          package_json_deps: BTreeSet::new(),
        },
        members: BTreeMap::from([(
          "thing".into(),
          WorkspaceMemberConfig {
            dependencies: BTreeSet::from(["beta".into()]),
            package_json_deps: BTreeSet::from(["gamma".into()]),
          },
        )]),
      },
    });
    lockfile.resolve_write_bytes();
    assert!(!lockfile.has_content_changed());

    lockfile.set_workspace_config(SetWorkspaceConfigOptions {
      no_config: true,
      no_npm: false,
      config: WorkspaceConfig {
        root: Default::default(),
        members: Default::default(),
      },
    });
    assert!(!lockfile.has_content_changed());
  }
}