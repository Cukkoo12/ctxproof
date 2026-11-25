use std::collections::BTreeSet;
use std::path::Path;

use crate::symbols::RepoIndex;

/// Discovered runnable project metadata used to validate commands mentioned in docs.
#[derive(Debug, Default, Clone)]
pub struct ProjectManifests {
    pub cargo_packages: BTreeSet<String>,
    pub cargo_binaries: BTreeSet<String>,
    pub cargo_aliases: BTreeSet<String>,
    pub cargo_features: BTreeSet<String>,
    pub has_cargo_workspace: bool,
    pub npm_scripts: BTreeSet<String>,
    pub has_package_json: bool,
    pub detected_js_managers: BTreeSet<String>,
    pub has_pyproject: bool,
    pub python_tools: BTreeSet<String>,
    pub make_targets: BTreeSet<String>,
    pub just_recipes: BTreeSet<String>,
}

impl ProjectManifests {
    /// Collects real packages, scripts, features, aliases, and targets across the repo.
    #[must_use]
    pub fn discover(index: &RepoIndex) -> Self {
        let mut out = Self::default();
        for rel in index.files.keys() {
            let abs = index.root.join(rel);
            let file_name = abs.file_name().and_then(|n| n.to_str()).unwrap_or("");
            match file_name {
                "Cargo.toml" => out.ingest_cargo_toml(&abs),
                "config.toml" if rel == ".cargo/config.toml" => out.ingest_cargo_config(&abs),
                "package.json" => out.ingest_package_json(&abs),
                "pyproject.toml" => out.ingest_pyproject(&abs),
                "Makefile" | "makefile" => out.ingest_makefile(&abs),
                "justfile" | "Justfile" => out.ingest_justfile(&abs),
                "package-lock.json" => {
                    out.detected_js_managers.insert("npm".into());
                }
                "pnpm-lock.yaml" => {
                    out.detected_js_managers.insert("pnpm".into());
                }
                "yarn.lock" => {
                    out.detected_js_managers.insert("yarn".into());
                }
                "bun.lock" | "bun.lockb" => {
                    out.detected_js_managers.insert("bun".into());
                }
                _ => {}
            }
        }
        out
    }

    fn ingest_cargo_toml(&mut self, path: &Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(doc) = toml::from_str::<toml::Value>(&text) else {
            return;
        };
        if doc.get("workspace").is_some() {
            self.has_cargo_workspace = true;
        }
        if let Some(pkg) = doc.get("package") {
            self.has_cargo_workspace = true;
            if let Some(name) = pkg.get("name").and_then(|v| v.as_str()) {
                self.cargo_packages.insert(name.to_owned());
            }
        }
        if let Some(bins) = doc.get("bin").and_then(|v| v.as_array()) {
            for bin in bins {
                if let Some(name) = bin.get("name").and_then(|v| v.as_str()) {
                    self.cargo_binaries.insert(name.to_owned());
                }
            }
        }
        if let Some(features) = doc.get("features").and_then(|v| v.as_table()) {
            for feat in features.keys() {
                self.cargo_features.insert(feat.clone());
            }
        }
    }

    fn ingest_cargo_config(&mut self, path: &Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(doc) = toml::from_str::<toml::Value>(&text) else {
            return;
        };
        if let Some(alias) = doc.get("alias").and_then(|v| v.as_table()) {
            for name in alias.keys() {
                self.cargo_aliases.insert(name.clone());
            }
        }
    }

    fn ingest_package_json(&mut self, path: &Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) else {
            return;
        };
        self.has_package_json = true;
        if let Some(scripts) = val.get("scripts").and_then(|v| v.as_object()) {
            for key in scripts.keys() {
                self.npm_scripts.insert(key.clone());
            }
        }
        if let Some(pm) = val.get("packageManager").and_then(|v| v.as_str())
            && let Some(name) = pm.split('@').next()
            && matches!(name, "npm" | "pnpm" | "yarn" | "bun")
        {
            self.detected_js_managers.insert(name.to_owned());
        }
    }

    fn ingest_pyproject(&mut self, path: &Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(doc) = toml::from_str::<toml::Value>(&text) else {
            return;
        };
        self.has_pyproject = true;
        if let Some(tool) = doc.get("tool").and_then(|v| v.as_table()) {
            for name in tool.keys() {
                self.python_tools.insert(name.clone());
            }
        }
        if let Some(scripts) = doc
            .get("project")
            .and_then(|p| p.get("scripts"))
            .and_then(|s| s.as_table())
        {
            for name in scripts.keys() {
                self.python_tools.insert(name.clone());
            }
        }
    }

    fn ingest_makefile(&mut self, path: &Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        for line in text.lines() {
            if line.starts_with(['\t', ' ', '#']) {
                continue;
            }
            if let Some((target, _)) = line.split_once(':') {
                let t = target.trim();
                if !t.is_empty() && !t.starts_with('.') && !t.contains('=') {
                    for part in t.split_whitespace() {
                        self.make_targets.insert(part.to_owned());
                    }
                }
            }
        }
    }

    fn ingest_justfile(&mut self, path: &Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        for line in text.lines() {
            let trimmed = line.trim_start();
            if line.starts_with([' ', '\t', '#']) || trimmed.is_empty() {
                continue;
            }
            if let Some((head, _)) = trimmed.split_once(':') {
                let recipe = head
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_start_matches('@');
                if !recipe.is_empty()
                    && recipe
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                {
                    self.just_recipes.insert(recipe.to_owned());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_cargo_and_package_manifests() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n[features]\ndiagnostics = []\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join(".cargo")).unwrap();
        std::fs::write(
            dir.path().join(".cargo/config.toml"),
            "[alias]\nxtask = \"run --package xtask --\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            "{\"scripts\": {\"test\": \"vitest\", \"lint\": \"eslint .\"}}",
        )
        .unwrap();
        std::fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();

        let index = RepoIndex::build(dir.path()).unwrap();
        let manifests = ProjectManifests::discover(&index);
        assert!(manifests.cargo_packages.contains("demo"));
        assert!(manifests.cargo_features.contains("diagnostics"));
        assert!(manifests.cargo_aliases.contains("xtask"));
        assert!(manifests.npm_scripts.contains("test"));
        assert!(manifests.detected_js_managers.contains("pnpm"));
    }
}
