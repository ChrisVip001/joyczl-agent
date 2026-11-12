//! The model catalog: the built-in table, extended at boot by `<home>/models-catalog.json`.
//!
//! Local-first means a user can add a provider (or retune a default) **without forking the
//! code**: the built-in `PROVIDERS` table stays the compile-time base, and one local JSON file
//! patches existing rows by id or appends new ones. No network, no new dependencies.
//!
//! Failure rules (all loud, none fatal): a missing file is the normal case; a broken file or
//! a broken row costs one stderr line and keeps the built-in table — a typo in a config file
//! must not take the assistant down, but it must not pass silently either.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{ProviderInfo, Wire, PROVIDERS};

/// Where the local override lives: `<home>/models-catalog.json`.
pub fn catalog_path(home: &Path) -> std::path::PathBuf {
    home.join("models-catalog.json")
}

/// One catalog row, **owned** — rows come from a user file and outlive any 'static table.
/// `key_env` empty = a keyless local endpoint (ollama-style).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub id: String,
    pub wire: Wire,
    #[serde(default)]
    pub key_env: String,
    #[serde(default)]
    pub base_url: Option<String>,
    pub model: String,
    #[serde(default)]
    pub small_model: Option<String>,
    pub context_window: u32,
    /// Whether the provider's models accept image input (see `ProviderInfo::vision`).
    /// Absent in the file = `false` — be conservative, override explicitly.
    #[serde(default)]
    pub vision: bool,
}

impl CatalogEntry {
    /// A keyless local endpoint (ollama-style): no Authorization header is sent.
    pub fn needs_key(&self) -> bool {
        !self.key_env.is_empty()
    }
}

impl From<&'static ProviderInfo> for CatalogEntry {
    fn from(info: &'static ProviderInfo) -> Self {
        Self {
            id: info.id.to_string(),
            wire: info.wire,
            key_env: info.key_env.to_string(),
            base_url: info.base_url.map(str::to_string),
            model: info.model.to_string(),
            small_model: Some(info.small_model.to_string()),
            context_window: info.context_window,
            vision: info.vision,
        }
    }
}

/// A user-file row: every field except `id` is optional, so an existing provider can be
/// patched with one line (`{ "id": "deepseek", "context_window": 999999 }`). Unknown fields
/// are rejected — a typo that silently does nothing is worse than an error.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogPatch {
    id: String,
    wire: Option<Wire>,
    #[serde(default)]
    key_env: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    model: Option<String>,
    #[serde(default)]
    small_model: Option<String>,
    context_window: Option<u32>,
    vision: Option<bool>,
}

impl CatalogPatch {
    /// Apply onto an existing row: present fields replace (`""` key_env = keyless, `""`
    /// base_url = clear), absent fields keep the old value.
    fn apply_to(self, entry: &mut CatalogEntry) {
        if let Some(wire) = self.wire {
            entry.wire = wire;
        }
        if let Some(key_env) = self.key_env {
            entry.key_env = key_env;
        }
        if let Some(base_url) = self.base_url {
            entry.base_url = (!base_url.is_empty()).then_some(base_url);
        }
        if let Some(model) = self.model {
            entry.model = model;
        }
        if let Some(small_model) = self.small_model {
            entry.small_model = (!small_model.is_empty()).then_some(small_model);
        }
        if let Some(context_window) = self.context_window {
            entry.context_window = context_window;
        }
        if let Some(vision) = self.vision {
            entry.vision = vision;
        }
    }

    /// Turn a patch for an **unknown** id into a full row — a new provider needs enough
    /// fields to actually make a request. `Ok(None)` = nothing usable to add.
    fn into_entry(self) -> Result<CatalogEntry, String> {
        let model = self.model.ok_or("a new provider needs model")?;
        let wire = self
            .wire
            .ok_or("a new provider needs wire (anthropic or openai)")?;
        // small_model defaults to the main model: one model serving both tiers is honest
        // (a bit pricier gate calls) and better than an invented name that resolves to nothing.
        let small_model = self
            .small_model
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| model.clone());
        Ok(CatalogEntry {
            id: self.id,
            wire,
            key_env: self.key_env.unwrap_or_default(),
            base_url: self.base_url.filter(|u| !u.is_empty()),
            model,
            small_model: Some(small_model),
            context_window: self.context_window.unwrap_or(32_768),
            vision: self.vision.unwrap_or(false),
        })
    }
}

/// The merged catalog. Immutable after boot — switching models re-resolves, it does not
/// re-read the catalog; `model/save` still validates against it.
#[derive(Debug, Clone)]
pub struct Catalog {
    entries: Vec<CatalogEntry>,
}

impl Catalog {
    /// The compile-time table, as entries. The base every load starts from.
    pub fn built_in() -> Self {
        Self {
            entries: PROVIDERS.iter().map(CatalogEntry::from).collect(),
        }
    }

    /// Built-in table merged with `<home>/models-catalog.json` (if present). Every problem
    /// is one stderr line plus a skip; the catalog itself never fails to build.
    pub fn load(home: &Path) -> Self {
        let mut catalog = Self::built_in();
        let path = catalog_path(home);
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return catalog; // no override file: the normal case
        };
        let patches: Vec<CatalogPatch> = match serde_json::from_str(&raw) {
            Ok(patches) => patches,
            Err(e) => {
                eprintln!(
                    "(joy) {} could not be parsed ({e}) — this start uses the built-in provider table only",
                    path.display()
                );
                return catalog;
            }
        };
        let mut merged = 0usize;
        for patch in patches {
            match patch.apply_or_add(&mut catalog) {
                Ok(()) => merged += 1,
                Err(why) => eprintln!("(joy) a catalog entry was skipped: {why}"),
            }
        }
        if merged > 0 {
            eprintln!(
                "(joy) model catalog: {} merged {} entries ({} built in, {} in total now)",
                path.display(),
                merged,
                PROVIDERS.len(),
                catalog.entries.len()
            );
        }
        catalog
    }

    pub fn lookup(&self, id: &str) -> Option<&CatalogEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn ids(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.id.as_str()).collect()
    }

    pub fn entries(&self) -> &[CatalogEntry] {
        &self.entries
    }
}

impl CatalogPatch {
    fn apply_or_add(self, catalog: &mut Catalog) -> Result<(), String> {
        if self.id.is_empty() {
            return Err("a catalog entry is missing its id".to_string());
        }
        match catalog.entries.iter_mut().find(|e| e.id == self.id) {
            Some(entry) => {
                self.apply_to(entry);
                Ok(())
            }
            None => {
                let id = self.id.clone();
                let entry = self.into_entry().map_err(|why| format!("{id}：{why}"))?;
                catalog.entries.push(entry);
                Ok(())
            }
        }
    }
}
