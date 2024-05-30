// Copyright 2018-2024 the Deno authors. MIT license.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::panic::AssertUnwindSafe;

use deno_lockfile::Lockfile;
use deno_lockfile::PackagesContent;
use deno_lockfile::SetWorkspaceConfigOptions;
use deno_lockfile::WorkspaceConfig;
use deno_lockfile::WorkspaceMemberConfig;
use file_test_runner::collect_and_run_tests;
use file_test_runner::collection::strategies::TestPerFileCollectionStrategy;
use file_test_runner::collection::CollectOptions;
use file_test_runner::collection::CollectedTest;
use file_test_runner::RunOptions;
use file_test_runner::TestResult;
use helpers::ConfigChangeSpec;
use serde::Deserialize;
use serde::Serialize;

mod helpers;

fn main() {
  collect_and_run_tests(
    CollectOptions {
      base: "tests/specs".into(),
      strategy: Box::new(TestPerFileCollectionStrategy::default()),
      filter_override: None,
    },
    RunOptions { parallel: true },
    |test| {
      TestResult::from_maybe_panic(AssertUnwindSafe(|| {
        run_test(test);
      }))
    },
  )
}

fn run_test(test: &CollectedTest) {
  if test.name.starts_with("specs::config_changes::") {
    config_changes_test(test);
  } else {
    panic!("Unknown test: {}", test.name);
  }
}

fn config_changes_test(test: &CollectedTest) {
  #[derive(Debug, Default, Clone, Serialize, Deserialize, Hash)]
  #[serde(rename_all = "camelCase")]
  struct LockfilePackageJsonContent {
    #[serde(default)]
    dependencies: BTreeSet<String>,
  }

  #[derive(Debug, Default, Clone, Deserialize, Hash)]
  #[serde(rename_all = "camelCase")]
  struct WorkspaceMemberConfigContent {
    #[serde(default)]
    dependencies: BTreeSet<String>,
    #[serde(default)]
    package_json: LockfilePackageJsonContent,
  }

  #[derive(Debug, Default, Clone, Deserialize, Hash)]
  #[serde(rename_all = "camelCase")]
  struct WorkspaceConfigContent {
    #[serde(default, flatten)]
    root: WorkspaceMemberConfigContent,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    #[serde(default)]
    members: BTreeMap<String, WorkspaceMemberConfigContent>,
  }
  impl WorkspaceConfigContent {
    fn into_workspace_config(self) -> WorkspaceConfig {
      WorkspaceConfig {
        root: WorkspaceMemberConfig {
          dependencies: self.root.dependencies,
          package_json_deps: self.root.package_json.dependencies,
        },
        members: self
          .members
          .into_iter()
          .map(|(k, v)| {
            (
              k,
              WorkspaceMemberConfig {
                dependencies: v.dependencies,
                package_json_deps: v.package_json.dependencies,
              },
            )
          })
          .collect(),
      }
    }
  }

  let is_update = std::env::var("UPDATE") == Ok("1".to_string());
  let mut spec =
    ConfigChangeSpec::parse(test.path.clone(), &test.read_to_string().unwrap());
  let mut config_file = Lockfile::with_lockfile_content(
    spec.path.with_extension("lock"),
    &spec.original_text.text,
    false,
  )
  .unwrap();
  for change_and_output in &mut spec.change_and_outputs {
    // setting the new workspace config should change the has_content_changed flag
    config_file.has_content_changed = false;
    let config = serde_json::from_str::<WorkspaceConfigContent>(
      &change_and_output.change.text,
    )
    .unwrap()
    .into_workspace_config();
    let no_npm = change_and_output.change.title.contains("--no-npm");
    let no_config = change_and_output.change.title.contains("--no-config");
    config_file.set_workspace_config(SetWorkspaceConfigOptions {
      no_config,
      no_npm,
      config: config.clone(),
    });
    assert_eq!(
      config_file.has_content_changed,
      !change_and_output.change.title.contains("no change"),
      "Failed for {}",
      change_and_output.change.title,
    );

    // now try resetting it and the flag should remain the same
    config_file.has_content_changed = false;
    config_file.set_workspace_config(SetWorkspaceConfigOptions {
      no_config,
      no_npm,
      config: config.clone(),
    });
    assert!(!config_file.has_content_changed);

    let expected_text = change_and_output.output.text.clone();
    let actual_text = config_file.as_json_string();
    if is_update {
      change_and_output.output.text = actual_text;
    } else {
      assert_eq!(
        actual_text.trim(),
        expected_text.trim(),
        "Failed for: {} - {}",
        spec.path.display(),
        change_and_output.change.title,
      );
    }
    verify_packages_content(&config_file.content.packages);
  }
  if is_update {
    std::fs::write(&spec.path, spec.emit()).unwrap();
  }
}

fn verify_packages_content(packages: &PackagesContent) {
  // verify the specifiers
  for id in packages.specifiers.values() {
    if let Some(npm_id) = id.strip_prefix("npm:") {
      assert!(packages.npm.contains_key(npm_id), "Missing: {}", id);
    } else if let Some(jsr_id) = id.strip_prefix("jsr:") {
      assert!(packages.jsr.contains_key(jsr_id), "Missing: {}", id);
    } else {
      panic!("Invalid package id: {}", id);
    }
  }
  for (pkg_id, package) in &packages.npm {
    for dep_id in package.dependencies.values() {
      assert!(
        packages.npm.contains_key(dep_id),
        "Missing '{}' dep in '{}'",
        pkg_id,
        dep_id,
      );
    }
  }
  for (pkg_id, package) in &packages.jsr {
    for req in &package.dependencies {
      let dep_id = match packages.specifiers.get(req) {
        Some(dep_id) => dep_id,
        None => panic!("Missing specifier for '{}' in '{}'", req, pkg_id),
      };
      if let Some(npm_id) = dep_id.strip_prefix("npm:") {
        assert!(
          packages.npm.contains_key(npm_id),
          "Missing: '{}' dep in '{}'",
          dep_id,
          pkg_id,
        );
      } else if let Some(jsr_id) = dep_id.strip_prefix("jsr:") {
        assert!(
          packages.jsr.contains_key(jsr_id),
          "Missing: '{}' dep in '{}'",
          dep_id,
          pkg_id,
        );
      } else {
        panic!("Invalid package id: {}", dep_id);
      }
    }
  }
}