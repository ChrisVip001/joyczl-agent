//! Settings responsibilities of the app-server.

use crate::{EventSink, Server};
use anyhow::Result;
use joyczl_config::Settings;
use joyczl_protocol::{SettingsPatch, SettingsView};
use joyczl_provider::Resolved;
use joyczl_state::{Chat, Episodes, Facts};
use sqlx::sqlite::SqlitePool;
use std::sync::Arc;

impl Server {
    /// The home this server was booted with (connectors.json, outbox — same file the
    /// ops side reads; one machine, one truth).
    pub(crate) fn home(&self) -> std::path::PathBuf {
        self.settings
            .read()
            .expect("the settings lock should not be poisoned")
            .home
            .clone()
    }

    /// A snapshot of the current settings. The copy you get can be used for any length of time —
    /// concurrent writes only affect the next turn.
    /// The state-db connection pool (joy schedule's scheduling state goes into this same db — one dataset, no separate store). Clone is cheap.
    pub fn state_pool(&self) -> SqlitePool {
        self.pool.clone()
    }

    /// The notifier (M3a). Used by joy schedule for job-completion delivery; None when unconfigured.
    pub fn notifier(&self) -> Option<Arc<dyn joyczl_tools::notify::Notifier>> {
        self.notifier.clone()
    }

    pub fn settings(&self) -> Settings {
        self.settings
            .read()
            .expect("the settings lock should not be poisoned")
            .clone()
    }

    // ---- Read surface for in-process clients (terminal REPL) ------------------------------
    // The fields themselves are pub(crate): writes may only go through protocol methods; these
    // read-only handles serve local commands like /memory and /sessions.

    pub fn facts(&self) -> &Facts {
        &self.facts
    }

    pub fn episodes(&self) -> &Episodes {
        &self.episodes
    }

    pub fn chat(&self) -> &Chat {
        &self.chat
    }

    /// This session's deferred-tool activation set, minted on first use.
    ///
    /// Shared with the turn's tool view (`session_view_with`) so `find_tools` in one turn and
    /// `schemas()` in the next read the same cell.
    pub(crate) fn activation_set(&self, session_id: &str) -> joyczl_tools::ActivationSet {
        let mut sets = match self.activations.lock() {
            Ok(sets) => sets,
            Err(poisoned) => poisoned.into_inner(),
        };
        sets.entry(session_id.to_string())
            .or_insert_with(|| Arc::new(std::sync::Mutex::new(Default::default())))
            .clone()
    }

    /// A snapshot of the current provider resolution (None when no key is configured).
    pub fn resolved(&self) -> Option<Resolved> {
        self.resolved
            .read()
            .expect("the resolved lock should not be poisoned")
            .clone()
    }

    pub(crate) fn set_settings(&self, settings: Settings) {
        *self
            .settings
            .write()
            .expect("the settings lock should not be poisoned") = settings;
    }

    pub(crate) fn set_resolved(&self, resolved: Option<Resolved>) {
        *self
            .resolved
            .write()
            .expect("the resolved lock should not be poisoned") = resolved;
        if tokio::runtime::Handle::try_current().is_ok() {
            let (sink, receiver) = EventSink::channel();
            drop(receiver);
            crate::task::dispatch::kick(self.clone(), sink);
        }
    }

    /// Inject a provider resolution. The injection point for eval and tests: the normal path goes
    /// through `joyczl_provider::resolve` (env vars supply the key); eval needs a scripted model to
    /// pin harness behavior offline and deterministically — that is when one is plugged in from outside.
    pub fn install_provider(&self, resolved: Resolved) {
        self.set_resolved(Some(resolved));
    }

    /// Restrict an embedded host's tool surface. This can only remove capabilities.
    pub fn restrict_tools(&mut self, allowed: &[&str]) {
        let names: Vec<String> = self.tools.names().into_iter().map(str::to_string).collect();
        let mut tools = (*self.tools).clone();
        for name in names {
            if !allowed.contains(&name.as_str()) {
                tools = tools.without(&name);
            }
        }
        self.tools = Arc::new(tools);
    }

    /// The body of `config/write`: validate patch → accumulate and persist → apply → re-resolve provider.
    ///
    /// A validation failure returns an error with the reason and changes nothing — a patch is all-or-nothing;
    /// half a patch is worse than none.
    pub(crate) fn apply_config_patch(&self, patch: &SettingsPatch) -> Result<SettingsView, String> {
        validate_patch(patch)?;

        // Accumulate: saved patches are the foundation of new ones. If persisting fails, memory is left
        // untouched — "changed but not remembered" lies harder than "not changed".
        let home = self.settings().home.clone();
        let mut saved = joyczl_config::load_patch(&home);
        saved.merge_newer(patch);
        joyczl_config::save_patch(&home, &saved)
            .map_err(|e| format!("could not write settings.json: {e}"))?;

        let mut settings = self.settings();
        joyczl_config::apply_patch(&saved, &mut settings);

        // Changed provider / model means re-resolve. A resolution failure is not fatal — record the reason;
        // `turn/*` will spell out "missing key"; the old client has been replaced and must not be used again.
        let resolved = match joyczl_provider::resolve(&settings) {
            Ok(resolved) => Some(resolved),
            Err(reason) => {
                eprintln!("(joy) after config/write no model is configured; turn/* unavailable — {reason}");
                None
            }
        };
        self.set_settings(settings.clone());
        self.set_resolved(resolved);
        Ok(settings.view())
    }
}

/// This used to also check "the provider must exist" — now provider and models belong to
/// `<home>/models.json` (change them via `model/*`, validated in models.rs), so the patch
/// only carries runtime parameters and one bounds table is enough.
pub(crate) fn validate_patch(patch: &SettingsPatch) -> Result<(), String> {
    joyczl_config::validate_patch_values(patch)
}
