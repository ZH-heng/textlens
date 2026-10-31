use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use parking_lot::RwLock;
use serde::Deserialize;
use thiserror::Error;
use uuid::Uuid;

#[path = "atomic_file.rs"]
pub(crate) mod atomic_file;

use crate::local_secrets::LocalEncryptedStore;
use crate::models::{
    ActionDefinition, ActionKind, AppSettings, CreateProviderInput, Locale, ProviderConfig,
    ProviderModel, PublicSettings, ResultDismissMode, SettingsUpdate, TranslationSettings,
    UpdateProviderInput, WindowSize, DEFAULT_ASK_PROMPT, DEFAULT_EXPLAIN_PROMPT,
    DEFAULT_PROVIDER_ID, DEFAULT_REFINE_PROMPT, DEFAULT_SUMMARY_PROMPT, DEFAULT_TRANSLATE_PROMPT,
    LEGACY_V11_CONCISE_EXPLAIN_PROMPT, LEGACY_V11_CONCISE_TRANSLATE_PROMPT,
    LEGACY_V11_EXPLAIN_PROMPT, LEGACY_V11_SUMMARY_PROMPT, LEGACY_V11_TIGHT_EXPLAIN_PROMPT,
    LEGACY_V11_TRANSLATE_PROMPT, LEGACY_V3_EXPLAIN_PROMPT, LEGACY_V3_REFINE_PROMPT,
    LEGACY_V3_SUMMARY_PROMPT, LEGACY_V3_TRANSLATE_PROMPT, LEGACY_V4_EXPLAIN_PROMPT,
    LEGACY_V4_REFINE_PROMPT, LEGACY_V4_SUMMARY_PROMPT, LEGACY_V4_TRANSLATE_PROMPT,
    LEGACY_V5_TRANSLATE_PROMPT, SETTINGS_VERSION,
};

const SECRET_ACCOUNT_PREFIX: &str = "provider-api-key:";
const MAX_API_KEY_LENGTH: usize = 16_384;
// Historical directory name retained only for one-time product rename migration.
const LEGACY_APP_CONFIG_DIRECTORY: &str = "com.local.selectionbar";

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("无法读取或保存设置：{0}")]
    Io(#[from] std::io::Error),
    #[error("设置文件格式无效：{0}")]
    Json(#[from] serde_json::Error),
    #[error("设置内容无效：{0}")]
    Validation(String),
    #[error("找不到服务商：{0}")]
    ProviderNotFound(String),
    #[error("无法访问本地加密密钥存储")]
    SecretStorage,
}

/// A deliberately small abstraction so repository tests can use an isolated
/// in-memory backend while production stores encrypted values outside settings.json.
pub trait SecretStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<String>, SettingsError>;
    fn set(&self, account: &str, secret: &str) -> Result<(), SettingsError>;
    fn delete(&self, account: &str) -> Result<(), SettingsError>;
}

pub struct SettingsRepository {
    path: PathBuf,
    state: RwLock<AppSettings>,
    secrets: Arc<dyn SecretStore>,
}

impl std::fmt::Debug for SettingsRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SettingsRepository")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl SettingsRepository {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, SettingsError> {
        let path = path.into();
        let secrets = Arc::new(LocalEncryptedStore::for_settings_file(&path)?);
        Self::with_secret_store(path, secrets)
    }

    pub fn new_with_legacy_migration(path: impl Into<PathBuf>) -> Result<Self, SettingsError> {
        let path = path.into();
        let repository = Self::new(&path)?;
        let Some(legacy_path) = legacy_settings_path(&path) else {
            return Ok(repository);
        };
        if !legacy_path.exists() {
            return Ok(repository);
        }

        let legacy = match Self::new(legacy_path) {
            Ok(legacy) => legacy,
            Err(_) => {
                eprintln!("[settings] legacy configuration could not be read; migration skipped");
                return Ok(repository);
            }
        };
        if repository.import_legacy_if_pristine(&legacy).is_err() {
            eprintln!("[settings] legacy configuration migration failed; current settings kept");
        }
        Ok(repository)
    }

    pub fn with_secret_store(
        path: impl Into<PathBuf>,
        secrets: Arc<dyn SecretStore>,
    ) -> Result<Self, SettingsError> {
        let path = path.into();
        let loaded = load_settings(&path)?;
        let repository = Self {
            path,
            state: RwLock::new(loaded.settings),
            secrets,
        };

        if let Some(key) = loaded
            .legacy_api_key
            .as_deref()
            .filter(|key| !key.trim().is_empty())
        {
            repository
                .secrets
                .set(&secret_account(DEFAULT_PROVIDER_ID), key.trim())?;
        }
        if loaded.must_persist {
            repository.persist(&repository.state.read())?;
        }
        Ok(repository)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get_settings(&self) -> AppSettings {
        self.state.read().clone()
    }

    pub fn get_public_settings(&self) -> Result<PublicSettings, SettingsError> {
        let settings = self.state.read().clone();
        let mut status = Vec::with_capacity(settings.providers.len());
        for provider in &settings.providers {
            status.push((
                provider.id.clone(),
                self.secrets
                    .get(&secret_account(&provider.id))?
                    .is_some_and(|value| !value.trim().is_empty()),
            ));
        }
        Ok(PublicSettings::from_settings(&settings, |id| {
            status
                .iter()
                .find(|(provider_id, _)| provider_id == id)
                .is_some_and(|(_, configured)| *configured)
        }))
    }

    pub fn update_result_last_size(
        &self,
        size: WindowSize,
    ) -> Result<PublicSettings, SettingsError> {
        self.transact(|settings| {
            if settings.result.remember_size {
                settings.result.last_size = Some(size);
            }
            Ok(())
        })?;
        self.get_public_settings()
    }

    pub fn clear_result_last_size(&self) -> Result<PublicSettings, SettingsError> {
        self.transact(|settings| {
            settings.result.last_size = None;
            Ok(())
        })?;
        self.get_public_settings()
    }

    pub fn update(&self, update: SettingsUpdate) -> Result<PublicSettings, SettingsError> {
        self.transact_with_provider_cleanup(|settings| {
            if let Some(value) = update.enabled {
                settings.enabled = value;
            }
            if let Some(value) = update.capture_shortcut {
                settings.capture_shortcut = value;
            }
            if let Some(value) = update.locale {
                settings.locale = value;
            }
            if let Some(value) = update.translate {
                settings.translate = value;
            }
            if let Some(value) = update.toolbar {
                settings.toolbar = value;
            }
            if let Some(value) = update.appearance {
                settings.appearance = value;
            }
            if let Some(mut value) = update.result {
                // Window dimensions are runtime-owned state. A settings page
                // can stay open with a stale public snapshot while a result
                // window is resized, so an ordinary form save must not write
                // that stale value back over the latest persisted size.
                value.last_size = settings.result.last_size;
                settings.result = value;
            }
            if let Some(value) = update.trigger {
                settings.trigger = value;
            }
            if let Some(value) = update.application {
                settings.application = value;
            }
            if let Some(value) = update.filter {
                settings.filter = value;
            }
            if let Some(value) = update.selection_capture {
                settings.selection_capture = value;
            }
            if let Some(value) = update.providers {
                settings.providers = value;
            }
            if let Some(value) = update.actions {
                settings.actions = value;
            }
            Ok(())
        })?;
        self.get_public_settings()
    }

    /// Replaces all non-secret settings with a renderer-provided public copy.
    /// `keyConfigured` values are intentionally ignored.
    pub fn replace_public(&self, public: PublicSettings) -> Result<PublicSettings, SettingsError> {
        self.transact_with_provider_cleanup(|settings| {
            *settings = AppSettings {
                version: SETTINGS_VERSION,
                enabled: public.enabled,
                capture_shortcut: public.capture_shortcut,
                locale: public.locale,
                translate: public.translate,
                toolbar: public.toolbar,
                appearance: public.appearance,
                result: public.result,
                trigger: public.trigger,
                application: public.application,
                filter: public.filter,
                selection_capture: public.selection_capture,
                providers: public
                    .providers
                    .into_iter()
                    .map(|provider| ProviderConfig {
                        id: provider.id,
                        name: provider.name,
                        enabled: provider.enabled,
                        protocol: provider.protocol,
                        base_url: provider.base_url,
                        models: provider.models,
                    })
                    .collect(),
                actions: public.actions,
            };
            Ok(())
        })?;
        self.get_public_settings()
    }

    pub fn create_provider(
        &self,
        input: CreateProviderInput,
    ) -> Result<PublicSettings, SettingsError> {
        let provider_id = format!("provider-{}", Uuid::new_v4().simple());
        self.transact(|settings| {
            settings.providers.push(ProviderConfig {
                id: provider_id,
                name: input.name,
                enabled: true,
                protocol: input.protocol,
                base_url: input.base_url,
                models: Vec::new(),
            });
            Ok(())
        })?;
        self.get_public_settings()
    }

    pub fn update_provider(
        &self,
        provider_id: &str,
        update: UpdateProviderInput,
    ) -> Result<PublicSettings, SettingsError> {
        let provider_id = provider_id.to_owned();
        self.transact(|settings| {
            let valid_models = {
                let provider = settings
                    .providers
                    .iter_mut()
                    .find(|provider| provider.id == provider_id)
                    .ok_or_else(|| SettingsError::ProviderNotFound(provider_id.clone()))?;
                if let Some(name) = update.name {
                    provider.name = name;
                }
                if let Some(base_url) = update.base_url {
                    provider.base_url = base_url;
                }
                if let Some(protocol) = update.protocol {
                    provider.protocol = protocol;
                }
                update.models.map(|models| {
                    provider.models = models;
                    provider
                        .models
                        .iter()
                        .map(|model| model.id.clone())
                        .collect::<std::collections::HashSet<_>>()
                })
            };
            if let Some(valid_models) = valid_models {
                for action in &mut settings.actions {
                    if action.provider_id.as_deref() == Some(provider_id.as_str())
                        && action
                            .model_id
                            .as_deref()
                            .is_some_and(|model| !model.is_empty() && !valid_models.contains(model))
                    {
                        action.model_id = Some(String::new());
                    }
                }
            }
            Ok(())
        })?;
        self.get_public_settings()
    }

    pub fn delete_provider(&self, provider_id: &str) -> Result<PublicSettings, SettingsError> {
        let provider_id = provider_id.to_owned();
        self.transact_with_provider_cleanup(|settings| {
            if !settings
                .providers
                .iter()
                .any(|provider| provider.id == provider_id)
            {
                return Err(SettingsError::ProviderNotFound(provider_id.clone()));
            }
            settings
                .providers
                .retain(|provider| provider.id != provider_id);
            for action in &mut settings.actions {
                if action.provider_id.as_deref() == Some(provider_id.as_str()) {
                    action.provider_id = Some(String::new());
                    action.model_id = Some(String::new());
                }
            }
            Ok(())
        })?;
        self.get_public_settings()
    }

    pub fn set_provider_api_key(
        &self,
        provider_id: &str,
        api_key: &str,
    ) -> Result<PublicSettings, SettingsError> {
        // Hold the writer lock across ensure + secret write so concurrent
        // delete_provider cannot remove the provider between check and set,
        // leaving an orphan secret entry.
        {
            let state = self.state.write();
            if !state
                .providers
                .iter()
                .any(|provider| provider.id == provider_id)
            {
                return Err(SettingsError::ProviderNotFound(provider_id.to_owned()));
            }
            let api_key = api_key.trim();
            if api_key.is_empty() || api_key.len() > MAX_API_KEY_LENGTH {
                return Err(SettingsError::Validation(
                    "API Key 应为 1–16384 个字符".to_owned(),
                ));
            }
            self.secrets.set(&secret_account(provider_id), api_key)?;
        }
        self.get_public_settings()
    }

    pub fn clear_provider_api_key(
        &self,
        provider_id: &str,
    ) -> Result<PublicSettings, SettingsError> {
        // Same write-lock scope as set_provider_api_key / delete_provider:
        // ensure and secret delete must not race with provider lifecycle.
        {
            let state = self.state.write();
            if !state
                .providers
                .iter()
                .any(|provider| provider.id == provider_id)
            {
                return Err(SettingsError::ProviderNotFound(provider_id.to_owned()));
            }
            self.secrets.delete(&secret_account(provider_id))?;
        }
        self.get_public_settings()
    }

    pub fn get_provider(&self, provider_id: &str) -> Result<ProviderConfig, SettingsError> {
        self.state
            .read()
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .cloned()
            .ok_or_else(|| SettingsError::ProviderNotFound(provider_id.to_owned()))
    }

    pub(crate) fn get_api_key(&self, provider_id: &str) -> Result<Option<String>, SettingsError> {
        self.ensure_provider(provider_id)?;
        self.secrets.get(&secret_account(provider_id))
    }

    fn import_legacy_if_pristine(
        &self,
        legacy: &SettingsRepository,
    ) -> Result<bool, SettingsError> {
        let current = self.get_settings();
        let defaults = AppSettings::default()
            .normalize_and_validate()
            .map_err(SettingsError::Validation)?;
        if current != defaults || self.has_configured_secret()? {
            return Ok(false);
        }

        let legacy_settings = legacy.get_settings();
        if !has_usable_ai_route(&legacy_settings) {
            return Ok(false);
        }

        let mut secret_snapshots = Vec::new();
        for provider in &legacy_settings.providers {
            let account = secret_account(&provider.id);
            let Some(secret) = legacy.secrets.get(&account)? else {
                continue;
            };
            let previous = self.secrets.get(&account)?;
            if previous.as_deref() == Some(secret.as_str()) {
                continue;
            }
            secret_snapshots.push((account.clone(), previous));
            if let Err(error) = self.secrets.set(&account, &secret) {
                return match self.restore_secret_snapshots(&secret_snapshots) {
                    Ok(()) => Err(error),
                    Err(rollback_error) => Err(rollback_error),
                };
            }
        }

        if let Err(error) = self.persist(&legacy_settings) {
            return match self.restore_secret_snapshots(&secret_snapshots) {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(rollback_error),
            };
        }
        *self.state.write() = legacy_settings;
        Ok(true)
    }

    fn has_configured_secret(&self) -> Result<bool, SettingsError> {
        for provider in &self.state.read().providers {
            if self
                .secrets
                .get(&secret_account(&provider.id))?
                .is_some_and(|secret| !secret.trim().is_empty())
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn ensure_provider(&self, provider_id: &str) -> Result<(), SettingsError> {
        if self
            .state
            .read()
            .providers
            .iter()
            .any(|provider| provider.id == provider_id)
        {
            Ok(())
        } else {
            Err(SettingsError::ProviderNotFound(provider_id.to_owned()))
        }
    }

    fn transact<F>(&self, operation: F) -> Result<(), SettingsError>
    where
        F: FnOnce(&mut AppSettings) -> Result<(), SettingsError>,
    {
        // Hold the writer guard through persistence so concurrent Tauri
        // commands cannot both derive from the same stale snapshot and lose
        // one another's changes.
        let mut state = self.state.write();
        let mut candidate = state.clone();
        operation(&mut candidate)?;
        candidate = candidate
            .normalize_and_validate()
            .map_err(SettingsError::Validation)?;
        self.persist(&candidate)?;
        *state = candidate;
        Ok(())
    }

    fn transact_with_provider_cleanup<F>(&self, operation: F) -> Result<(), SettingsError>
    where
        F: FnOnce(&mut AppSettings) -> Result<(), SettingsError>,
    {
        // Hold the settings writer lock across both stores. This prevents a
        // provider key from being recreated concurrently between validation,
        // encrypted-secret cleanup and the JSON commit.
        let mut state = self.state.write();
        let previous = state.clone();
        let mut candidate = previous.clone();
        operation(&mut candidate)?;
        candidate = candidate
            .normalize_and_validate()
            .map_err(SettingsError::Validation)?;

        let retained = candidate
            .providers
            .iter()
            .map(|provider| provider.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let removed_accounts = previous
            .providers
            .iter()
            .filter(|provider| !retained.contains(provider.id.as_str()))
            .map(|provider| secret_account(&provider.id))
            .collect::<Vec<_>>();
        let secret_snapshots = removed_accounts
            .iter()
            .map(|account| {
                self.secrets
                    .get(account)
                    .map(|secret| (account.clone(), secret))
            })
            .collect::<Result<Vec<_>, _>>()?;

        self.persist(&candidate)?;
        for account in &removed_accounts {
            if let Err(delete_error) = self.secrets.delete(account) {
                let restore_result = self.restore_secret_snapshots(&secret_snapshots);
                if let Err(rollback_error) = self.persist(&previous) {
                    // The candidate is still the durable JSON state. Keep the
                    // in-memory copy aligned with it even though cleanup failed.
                    *state = candidate;
                    return Err(rollback_error);
                }
                return match restore_result {
                    Ok(()) => Err(delete_error),
                    Err(restore_error) => Err(restore_error),
                };
            }
        }
        *state = candidate;
        Ok(())
    }

    fn restore_secret_snapshots(
        &self,
        snapshots: &[(String, Option<String>)],
    ) -> Result<(), SettingsError> {
        let mut first_error = None;
        for (account, secret) in snapshots {
            let result = match secret {
                Some(secret) => self.secrets.set(account, secret),
                None => self.secrets.delete(account),
            };
            if first_error.is_none() {
                first_error = result.err();
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn persist(&self, settings: &AppSettings) -> Result<(), SettingsError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(settings)?;
        let temporary = self
            .path
            .with_extension(format!("{}.tmp", Uuid::new_v4().simple()));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let write_result = (|| -> Result<(), std::io::Error> {
            let mut file = options.open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            // Windows refuses ReplaceFileW/MoveFileExW while this process
            // still owns an open handle to the replacement file.
            drop(file);
            atomic_file::replace_file(&temporary, &self.path)?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result.map_err(SettingsError::Io)
    }
}

fn secret_account(provider_id: &str) -> String {
    format!("{SECRET_ACCOUNT_PREFIX}{provider_id}")
}

fn legacy_settings_path(settings_path: &Path) -> Option<PathBuf> {
    let current_directory = settings_path.parent()?;
    if current_directory.file_name()?.to_string_lossy() == LEGACY_APP_CONFIG_DIRECTORY {
        return None;
    }
    Some(
        current_directory
            .parent()?
            .join(LEGACY_APP_CONFIG_DIRECTORY)
            .join(settings_path.file_name()?),
    )
}

fn has_usable_ai_route(settings: &AppSettings) -> bool {
    settings.actions.iter().any(|action| {
        if !action.kind.is_ai() {
            return false;
        }
        let (Some(provider_id), Some(model_id)) =
            (action.provider_id.as_deref(), action.model_id.as_deref())
        else {
            return false;
        };
        settings.providers.iter().any(|provider| {
            provider.id == provider_id && provider.models.iter().any(|model| model.id == model_id)
        })
    })
}

struct LoadedSettings {
    settings: AppSettings,
    legacy_api_key: Option<String>,
    must_persist: bool,
}

fn load_settings(path: &Path) -> Result<LoadedSettings, SettingsError> {
    if !path.exists() {
        return Ok(LoadedSettings {
            settings: AppSettings::default()
                .normalize_and_validate()
                .map_err(SettingsError::Validation)?,
            legacy_api_key: None,
            must_persist: true,
        });
    }
    let bytes = fs::read(path)?;
    if bytes.len() > 5 * 1024 * 1024 {
        return Err(SettingsError::Validation("设置文件体积异常".to_owned()));
    }
    let mut value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let version = value.get("version").and_then(serde_json::Value::as_u64);
    if version == Some(SETTINGS_VERSION as u64) {
        let missing_result_defaults = value.pointer("/result/dismissMode").is_none()
            || value.pointer("/result/fontSize").is_none();
        let missing_application_defaults = value.pointer("/application/closeBehavior").is_none();
        let had_legacy_search_fields = value.pointer("/searchEngines").is_some()
            || value.pointer("/activeSearchEngineId").is_some()
            || value.pointer("/searchEngine").is_some()
            || value.pointer("/searchTemplate").is_some();
        let had_quote_action = value_has_quote_action(&value);
        migrate_v10_search_actions_value(&mut value);
        migrate_quote_to_ask_value(&mut value);
        let mut settings: AppSettings = serde_json::from_value(value)?;
        let persisted_settings = settings.clone();
        migrate_default_action_prompts(&mut settings);
        migrate_search_actions(&mut settings);
        let normalized = settings
            .normalize_and_validate()
            .map_err(SettingsError::Validation)?;
        let must_persist = missing_result_defaults
            || missing_application_defaults
            || had_legacy_search_fields
            || had_quote_action
            || normalized != persisted_settings;
        return Ok(LoadedSettings {
            settings: normalized,
            legacy_api_key: None,
            must_persist,
        });
    }
    if matches!(version, Some(11 | 12)) {
        migrate_v10_search_actions_value(&mut value);
        migrate_quote_to_ask_value(&mut value);
        let mut settings: AppSettings = serde_json::from_value(value)?;
        migrate_default_action_prompts(&mut settings);
        migrate_search_actions(&mut settings);
        let settings = settings
            .normalize_and_validate()
            .map_err(SettingsError::Validation)?;
        return Ok(LoadedSettings {
            settings,
            legacy_api_key: None,
            must_persist: true,
        });
    }
    if version == Some(10) {
        migrate_v10_search_actions_value(&mut value);
        migrate_quote_to_ask_value(&mut value);
        let mut settings: AppSettings = serde_json::from_value(value)?;
        migrate_default_action_prompts(&mut settings);
        migrate_search_actions(&mut settings);
        let settings = settings
            .normalize_and_validate()
            .map_err(SettingsError::Validation)?;
        return Ok(LoadedSettings {
            settings,
            legacy_api_key: None,
            must_persist: true,
        });
    }
    if version == Some(9) {
        migrate_v10_search_actions_value(&mut value);
        migrate_quote_to_ask_value(&mut value);
        let mut settings: AppSettings = serde_json::from_value(value)?;
        migrate_default_action_prompts(&mut settings);
        migrate_search_actions(&mut settings);
        let settings = settings
            .normalize_and_validate()
            .map_err(SettingsError::Validation)?;
        return Ok(LoadedSettings {
            settings,
            legacy_api_key: None,
            must_persist: true,
        });
    }
    if version == Some(8) {
        migrate_v8_search_engines_value(&mut value);
        migrate_v10_search_actions_value(&mut value);
        migrate_quote_to_ask_value(&mut value);
        let mut settings: AppSettings = serde_json::from_value(value)?;
        migrate_default_action_prompts(&mut settings);
        migrate_search_actions(&mut settings);
        let settings = settings
            .normalize_and_validate()
            .map_err(SettingsError::Validation)?;
        return Ok(LoadedSettings {
            settings,
            legacy_api_key: None,
            must_persist: true,
        });
    }
    if matches!(version, Some(3 | 4 | 5 | 6 | 7)) {
        migrate_v8_search_engines_value(&mut value);
        migrate_v10_search_actions_value(&mut value);
        migrate_quote_to_ask_value(&mut value);
        let mut settings: AppSettings = serde_json::from_value(value)?;
        migrate_default_action_prompts(&mut settings);
        migrate_search_actions(&mut settings);
        let settings = settings
            .normalize_and_validate()
            .map_err(SettingsError::Validation)?;
        return Ok(LoadedSettings {
            settings,
            legacy_api_key: None,
            must_persist: true,
        });
    }
    if version == Some(2) {
        // v2 already has the complete provider/action model. Parsing it as v1
        // would silently discard per-action provider/model bindings. v3 was a
        // one-time behavior migration: old persisted `manual` becomes the new
        // default `blur`. Later versions also upgrade only untouched built-in
        // prompts.
        migrate_v8_search_engines_value(&mut value);
        migrate_v10_search_actions_value(&mut value);
        migrate_quote_to_ask_value(&mut value);
        let mut settings: AppSettings = serde_json::from_value(value)?;
        settings.result.dismiss_mode = ResultDismissMode::Blur;
        migrate_default_action_prompts(&mut settings);
        migrate_search_actions(&mut settings);
        let settings = settings
            .normalize_and_validate()
            .map_err(SettingsError::Validation)?;
        return Ok(LoadedSettings {
            settings,
            legacy_api_key: None,
            must_persist: true,
        });
    }
    if version.is_some_and(|version| version != 1) {
        return Err(SettingsError::Validation(
            "设置文件版本不受当前应用支持".to_owned(),
        ));
    }
    // Electron Store wrapped the v1 payload in a top-level `settings` field.
    // The old Electron ciphertext cannot and must not be treated as plaintext
    // here; the user can re-enter it into the local encrypted store.
    let legacy_value = value
        .get("settings")
        .filter(|candidate| candidate.is_object())
        .cloned()
        .unwrap_or(value);
    migrate_v1(legacy_value)
}

fn migrate_default_action_prompts(settings: &mut AppSettings) {
    for action in settings.actions.iter_mut() {
        let is_builtin = matches!(
            (action.kind, action.id.as_str()),
            (ActionKind::Translate, "translate")
                | (ActionKind::Summary, "summary")
                | (ActionKind::Explain, "explain")
                | (ActionKind::Refine, "refine")
        );
        if !is_builtin {
            continue;
        }
        let replacement = match action.kind {
            ActionKind::Translate
                if matches!(
                    action.prompt.as_deref(),
                    Some(
                        LEGACY_V3_TRANSLATE_PROMPT
                            | LEGACY_V4_TRANSLATE_PROMPT
                            | LEGACY_V5_TRANSLATE_PROMPT
                            | LEGACY_V11_TRANSLATE_PROMPT
                            | LEGACY_V11_CONCISE_TRANSLATE_PROMPT
                    )
                ) =>
            {
                Some(DEFAULT_TRANSLATE_PROMPT)
            }
            ActionKind::Summary
                if matches!(
                    action.prompt.as_deref(),
                    Some(
                        LEGACY_V3_SUMMARY_PROMPT
                            | LEGACY_V4_SUMMARY_PROMPT
                            | LEGACY_V11_SUMMARY_PROMPT
                    )
                ) =>
            {
                Some(DEFAULT_SUMMARY_PROMPT)
            }
            ActionKind::Explain
                if matches!(
                    action.prompt.as_deref(),
                    Some(
                        LEGACY_V3_EXPLAIN_PROMPT
                            | LEGACY_V4_EXPLAIN_PROMPT
                            | LEGACY_V11_EXPLAIN_PROMPT
                            | LEGACY_V11_CONCISE_EXPLAIN_PROMPT
                            | LEGACY_V11_TIGHT_EXPLAIN_PROMPT
                    )
                ) =>
            {
                Some(DEFAULT_EXPLAIN_PROMPT)
            }
            ActionKind::Refine
                if matches!(
                    action.prompt.as_deref(),
                    Some(LEGACY_V3_REFINE_PROMPT | LEGACY_V4_REFINE_PROMPT)
                ) =>
            {
                Some(DEFAULT_REFINE_PROMPT)
            }
            _ => None,
        };
        if let Some(prompt) = replacement {
            action.prompt = Some(prompt.to_owned());
        }
    }
}

fn migrate_search_actions(settings: &mut AppSettings) {
    if let Some(search) = settings
        .actions
        .iter_mut()
        .find(|action| action.id == "search" && action.kind == ActionKind::Search)
    {
        if search.name == "谷歌" {
            search.name = "搜索".to_owned();
        }
        if search.icon == "globe" {
            search.icon = "search".to_owned();
        }
    }
    settings.actions.retain(|action| {
        !matches!(
            (action.id.as_str(), action.kind),
            ("search-bing", ActionKind::Search) | ("search-baidu", ActionKind::Search)
        )
    });
    for (order, action) in settings.actions.iter_mut().enumerate() {
        action.order = order as u32;
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyAiSettings {
    #[serde(default = "default_base_url")]
    base_url: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    api_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyAction {
    id: String,
    name: String,
    icon: String,
    #[serde(rename = "type")]
    kind: ActionKind,
    enabled: bool,
    order: u32,
    #[serde(default)]
    prompt: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacySettings {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    capture_shortcut: String,
    #[serde(default)]
    locale: Locale,
    // Retained only so older Electron-era settings still deserialize.
    #[serde(default = "default_search_template")]
    #[allow(dead_code)]
    search_template: String,
    #[serde(default)]
    translate: TranslationSettings,
    #[serde(default)]
    ai: LegacyAiSettings,
    #[serde(default)]
    actions: Vec<LegacyAction>,
}

fn migrate_v1(value: serde_json::Value) -> Result<LoadedSettings, SettingsError> {
    let legacy: LegacySettings = serde_json::from_value(value)?;
    let model = legacy.ai.model.trim().to_owned();
    let provider = ProviderConfig {
        id: DEFAULT_PROVIDER_ID.to_owned(),
        name: "OpenAI Compatible".to_owned(),
        enabled: true,
        protocol: crate::models::ProviderProtocol::OpenAiChat,
        base_url: legacy.ai.base_url,
        models: if model.is_empty() {
            Vec::new()
        } else {
            vec![ProviderModel {
                id: model.clone(),
                name: model.clone(),
                thinking_levels: Vec::new(),
                thinking_capability: None,
                thinking_enabled: true,
                thinking_default: None,
            }]
        },
    };
    let defaults = AppSettings::default();
    let default_actions = defaults
        .actions
        .iter()
        .map(|action| (action.kind, action.clone()))
        .collect::<std::collections::HashMap<_, _>>();
    let mut actions = if legacy.actions.is_empty() {
        defaults.actions.clone()
    } else {
        legacy
            .actions
            .into_iter()
            .map(|action| {
                let default_prompt = default_actions
                    .get(&action.kind)
                    .and_then(|action| action.prompt.clone());
                ActionDefinition {
                    id: action.id,
                    name: action.name,
                    icon: action.icon,
                    kind: action.kind,
                    enabled: action.enabled,
                    order: action.order,
                    prompt: if action.kind.is_ai() {
                        action.prompt.or(default_prompt)
                    } else {
                        None
                    },
                    provider_id: action.kind.is_ai().then(|| DEFAULT_PROVIDER_ID.to_owned()),
                    model_id: action.kind.is_ai().then(|| model.clone()),
                    search_engine_id: (action.kind == ActionKind::Search)
                        .then(|| crate::models::DEFAULT_ACTIVE_SEARCH_ENGINE_ID.to_owned()),
                    thinking_mode: crate::models::ThinkingMode::Off,
                }
            })
            .collect()
    };
    for new_kind in [ActionKind::Refine, ActionKind::Ask] {
        if !actions.iter().any(|action| action.kind == new_kind) {
            if let Some(default) = default_actions.get(&new_kind) {
                let mut added = default.clone();
                added.enabled = actions.iter().filter(|action| action.enabled).count() < 8;
                added.order = actions.len() as u32;
                added.model_id = added.kind.is_ai().then(|| model.clone());
                actions.push(added);
            }
        }
    }
    let mut settings = AppSettings {
        version: SETTINGS_VERSION,
        enabled: legacy.enabled,
        capture_shortcut: legacy.capture_shortcut,
        locale: legacy.locale,
        translate: legacy.translate,
        toolbar: Default::default(),
        appearance: Default::default(),
        result: Default::default(),
        trigger: Default::default(),
        application: Default::default(),
        filter: Default::default(),
        selection_capture: Default::default(),
        providers: vec![provider],
        actions,
    };
    migrate_search_actions(&mut settings);
    let settings = settings
        .normalize_and_validate()
        .map_err(SettingsError::Validation)?;
    Ok(LoadedSettings {
        settings,
        legacy_api_key: (!legacy.ai.api_key.trim().is_empty()).then_some(legacy.ai.api_key),
        must_persist: true,
    })
}

fn default_true() -> bool {
    true
}

fn default_base_url() -> String {
    "https://api.openai.com/v1".to_owned()
}

fn migrate_v8_search_engines_value(value: &mut serde_json::Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if object.contains_key("searchEngines") && object.contains_key("activeSearchEngineId") {
        object.remove("searchEngine");
        object.remove("searchTemplate");
        return;
    }

    let mut engines = crate::models::default_search_engines();
    if let Some(template) = object
        .get("searchTemplate")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| crate::models::validate_search_template(value).is_ok())
    {
        if let Some(google) = engines.iter_mut().find(|engine| engine.id == "google") {
            google.template = template.to_owned();
        }
    }

    let active = object
        .get("searchEngine")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| engines.iter().any(|engine| engine.id == *value))
        .unwrap_or(crate::models::DEFAULT_ACTIVE_SEARCH_ENGINE_ID)
        .to_owned();

    object.insert(
        "searchEngines".to_owned(),
        serde_json::to_value(engines).unwrap_or_else(|_| serde_json::json!([])),
    );
    object.insert(
        "activeSearchEngineId".to_owned(),
        serde_json::Value::String(active),
    );
    object.remove("searchEngine");
    object.remove("searchTemplate");
    object.insert("version".to_owned(), serde_json::Value::Number(9.into()));
}

fn value_has_quote_action(value: &serde_json::Value) -> bool {
    value
        .get("actions")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|actions| {
            actions.iter().any(|action| {
                let kind = action
                    .get("kind")
                    .or_else(|| action.get("type"))
                    .and_then(serde_json::Value::as_str);
                let id = action.get("id").and_then(serde_json::Value::as_str);
                kind == Some("quote") || id == Some("quote")
            })
        })
}

/// Rewrite local quote clipboard actions into the ask-ai AI action (SETTINGS_VERSION 11).
fn migrate_quote_to_ask_value(value: &mut serde_json::Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let Some(actions) = object
        .get_mut("actions")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };

    let mut saw_ask = false;
    let mut rewritten = Vec::with_capacity(actions.len());
    for action in actions.drain(..) {
        let Some(action_object) = action.as_object() else {
            rewritten.push(action);
            continue;
        };
        let kind = action_object
            .get("kind")
            .or_else(|| action_object.get("type"))
            .and_then(serde_json::Value::as_str);
        let id = action_object.get("id").and_then(serde_json::Value::as_str);
        let is_quote = kind == Some("quote") || id == Some("quote");
        let is_ask = kind == Some("ask") || id == Some("ask-ai");
        if is_quote || is_ask {
            if saw_ask {
                continue;
            }
            saw_ask = true;
            let name = action_object
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty() && *value != "引用")
                .unwrap_or("问AI");
            let icon = action_object
                .get("icon")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty() && *value != "quote")
                .unwrap_or("message-circle-question");
            let enabled = action_object
                .get("enabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            let order = action_object
                .get("order")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(6);
            let prompt = action_object
                .get("prompt")
                .and_then(serde_json::Value::as_str)
                .filter(|value| value.contains("{{text}}"))
                .unwrap_or(DEFAULT_ASK_PROMPT);
            let provider_id = action_object
                .get("providerId")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(DEFAULT_PROVIDER_ID);
            let model_id = action_object
                .get("modelId")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let thinking_mode = action_object
                .get("thinkingMode")
                .cloned()
                .unwrap_or_else(|| serde_json::Value::String("off".to_owned()));
            rewritten.push(serde_json::json!({
                "id": "ask-ai",
                "name": name,
                "icon": icon,
                "kind": "ask",
                "enabled": enabled,
                "order": order,
                "prompt": prompt,
                "providerId": provider_id,
                "modelId": model_id,
                "thinkingMode": thinking_mode,
            }));
            continue;
        }
        rewritten.push(action);
    }
    for (order, action) in rewritten.iter_mut().enumerate() {
        if let Some(object) = action.as_object_mut() {
            object.insert(
                "order".to_owned(),
                serde_json::Value::Number((order as u64).into()),
            );
        }
    }
    *actions = rewritten;
    object.insert(
        "version".to_owned(),
        serde_json::Value::Number(SETTINGS_VERSION.into()),
    );
}

fn migrate_v10_search_actions_value(value: &mut serde_json::Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };

    let global_active = object
        .get("activeSearchEngineId")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| matches!(*value, "google" | "bing-china" | "baidu"))
        .map(str::to_owned);
    let legacy_engine = object
        .get("searchEngine")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| matches!(*value, "google" | "bing-china" | "baidu"))
        .map(str::to_owned);
    // v8/v9 stored a global preference; when present it is the migration source of truth.
    let preferred_global = global_active.or(legacy_engine);
    let fallback = preferred_global
        .clone()
        .unwrap_or_else(|| crate::models::DEFAULT_ACTIVE_SEARCH_ENGINE_ID.to_owned());

    if let Some(actions) = object
        .get_mut("actions")
        .and_then(serde_json::Value::as_array_mut)
    {
        for action in actions {
            let Some(action_object) = action.as_object_mut() else {
                continue;
            };
            let is_search = action_object
                .get("kind")
                .or_else(|| action_object.get("type"))
                .and_then(serde_json::Value::as_str)
                == Some("search");
            if !is_search {
                action_object.remove("searchEngineId");
                continue;
            }
            let existing = action_object
                .get("searchEngineId")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| matches!(*value, "google" | "bing-china" | "baidu"))
                .map(str::to_owned);
            let engine_id = preferred_global
                .clone()
                .or(existing)
                .unwrap_or_else(|| fallback.clone());
            action_object.insert(
                "searchEngineId".to_owned(),
                serde_json::Value::String(engine_id),
            );
        }
    }

    object.remove("searchEngines");
    object.remove("activeSearchEngineId");
    object.remove("searchEngine");
    object.remove("searchTemplate");
    object.insert(
        "version".to_owned(),
        serde_json::Value::Number(crate::models::SETTINGS_VERSION.into()),
    );
}

fn default_search_template() -> String {
    crate::models::DEFAULT_SEARCH_TEMPLATE.to_owned()
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Mutex};

    use tempfile::tempdir;

    use super::*;
    use crate::models::{
        ThinkingCapability, ThinkingCapabilitySource, ThinkingDialect, ThinkingLevel,
    };

    #[derive(Default)]
    struct MemorySecrets(Mutex<HashMap<String, String>>);

    impl SecretStore for MemorySecrets {
        fn get(&self, account: &str) -> Result<Option<String>, SettingsError> {
            Ok(self.0.lock().unwrap().get(account).cloned())
        }

        fn set(&self, account: &str, secret: &str) -> Result<(), SettingsError> {
            self.0
                .lock()
                .unwrap()
                .insert(account.to_owned(), secret.to_owned());
            Ok(())
        }

        fn delete(&self, account: &str) -> Result<(), SettingsError> {
            self.0.lock().unwrap().remove(account);
            Ok(())
        }
    }

    #[derive(Default)]
    struct DeleteFailingSecrets(Mutex<HashMap<String, String>>);

    impl SecretStore for DeleteFailingSecrets {
        fn get(&self, account: &str) -> Result<Option<String>, SettingsError> {
            Ok(self.0.lock().unwrap().get(account).cloned())
        }

        fn set(&self, account: &str, secret: &str) -> Result<(), SettingsError> {
            self.0
                .lock()
                .unwrap()
                .insert(account.to_owned(), secret.to_owned());
            Ok(())
        }

        fn delete(&self, _account: &str) -> Result<(), SettingsError> {
            Err(SettingsError::SecretStorage)
        }
    }

    fn configure_ai_repository(repository: &SettingsRepository, model_id: &str, api_key: &str) {
        let mut settings = repository.get_settings();
        settings.providers[0].name = "Configured Provider".to_owned();
        settings.providers[0].base_url = "https://example.com/v1".to_owned();
        settings.providers[0].models = vec![ProviderModel {
            id: model_id.to_owned(),
            name: model_id.to_owned(),
            thinking_levels: Vec::new(),
            thinking_capability: None,
            thinking_enabled: true,
            thinking_default: None,
        }];
        for action in settings
            .actions
            .iter_mut()
            .filter(|action| action.kind.is_ai())
        {
            action.provider_id = Some(DEFAULT_PROVIDER_ID.to_owned());
            action.model_id = Some(model_id.to_owned());
        }
        repository
            .update(SettingsUpdate {
                providers: Some(settings.providers),
                actions: Some(settings.actions),
                ..Default::default()
            })
            .unwrap();
        repository
            .set_provider_api_key(DEFAULT_PROVIDER_ID, api_key)
            .unwrap();
    }

    #[test]
    fn api_keys_are_never_written_to_json_or_public_settings() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let repository =
            SettingsRepository::with_secret_store(&path, Arc::new(MemorySecrets::default()))
                .unwrap();
        let public = repository
            .set_provider_api_key(DEFAULT_PROVIDER_ID, "sk-super-secret")
            .unwrap();
        assert!(public.providers[0].key_configured);
        let json = fs::read_to_string(path).unwrap();
        assert!(!json.contains("sk-super-secret"));
        assert!(!json.contains("apiKey"));
    }

    #[test]
    fn fresh_repository_has_only_the_default_openai_address_and_no_api_key() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let repository = SettingsRepository::new(&path).unwrap();

        let settings = repository.get_settings();
        assert_eq!(settings.providers.len(), 1);
        assert_eq!(settings.providers[0].id, DEFAULT_PROVIDER_ID);
        assert_eq!(settings.providers[0].base_url, "https://api.openai.com/v1");
        assert!(settings.providers[0].models.is_empty());
        assert_eq!(repository.get_api_key(DEFAULT_PROVIDER_ID).unwrap(), None);
        assert!(!repository.get_public_settings().unwrap().providers[0].key_configured);

        let persisted = fs::read_to_string(path).unwrap();
        assert!(!persisted.contains("apiKey"));
        assert!(!directory
            .path()
            .join(crate::local_secrets::ENCRYPTED_SECRETS_FILE_NAME)
            .exists());
    }

    #[test]
    fn default_repository_encrypts_api_keys_locally_and_reopens_them() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let repository = SettingsRepository::new(&path).unwrap();
        repository
            .set_provider_api_key(DEFAULT_PROVIDER_ID, "sk-encrypted-locally")
            .unwrap();
        drop(repository);

        let encrypted_path = directory
            .path()
            .join(crate::local_secrets::ENCRYPTED_SECRETS_FILE_NAME);
        let key_path = directory
            .path()
            .join(crate::local_secrets::LOCAL_SECRET_KEY_FILE_NAME);
        assert!(!fs::read_to_string(&path)
            .unwrap()
            .contains("sk-encrypted-locally"));
        assert!(!fs::read_to_string(&encrypted_path)
            .unwrap()
            .contains("sk-encrypted-locally"));
        assert_eq!(fs::read(&key_path).unwrap().len(), 32);

        let reopened = SettingsRepository::new(path).unwrap();
        assert_eq!(
            reopened
                .get_api_key(DEFAULT_PROVIDER_ID)
                .unwrap()
                .as_deref(),
            Some("sk-encrypted-locally")
        );
        assert!(reopened.get_public_settings().unwrap().providers[0].key_configured);
    }

    #[test]
    fn imports_pristine_textlens_settings_and_secrets_from_the_legacy_directory() {
        let root = tempdir().unwrap();
        let legacy_path = root
            .path()
            .join(LEGACY_APP_CONFIG_DIRECTORY)
            .join("settings.json");
        let current_path = root.path().join("com.local.textlens").join("settings.json");
        let legacy = SettingsRepository::new(&legacy_path).unwrap();
        configure_ai_repository(&legacy, "legacy-model", "legacy-secret");
        drop(legacy);

        let migrated = SettingsRepository::new_with_legacy_migration(&current_path).unwrap();
        let settings = migrated.get_settings();
        assert_eq!(settings.providers[0].name, "Configured Provider");
        assert_eq!(settings.providers[0].models[0].id, "legacy-model");
        assert!(settings
            .actions
            .iter()
            .filter(|action| action.kind.is_ai())
            .all(|action| action.model_id.as_deref() == Some("legacy-model")));
        assert_eq!(
            migrated
                .get_api_key(DEFAULT_PROVIDER_ID)
                .unwrap()
                .as_deref(),
            Some("legacy-secret")
        );
        let encrypted = fs::read_to_string(
            current_path.with_file_name(crate::local_secrets::ENCRYPTED_SECRETS_FILE_NAME),
        )
        .unwrap();
        assert!(!encrypted.contains("legacy-secret"));
    }

    #[test]
    fn legacy_import_never_overwrites_configured_textlens_settings() {
        let root = tempdir().unwrap();
        let legacy_path = root
            .path()
            .join(LEGACY_APP_CONFIG_DIRECTORY)
            .join("settings.json");
        let current_path = root.path().join("com.local.textlens").join("settings.json");
        let legacy = SettingsRepository::new(&legacy_path).unwrap();
        configure_ai_repository(&legacy, "legacy-model", "legacy-secret");
        let current = SettingsRepository::new(&current_path).unwrap();
        configure_ai_repository(&current, "current-model", "current-secret");
        drop((legacy, current));

        let reopened = SettingsRepository::new_with_legacy_migration(&current_path).unwrap();
        assert_eq!(
            reopened.get_settings().providers[0].models[0].id,
            "current-model"
        );
        assert_eq!(
            reopened
                .get_api_key(DEFAULT_PROVIDER_ID)
                .unwrap()
                .as_deref(),
            Some("current-secret")
        );
    }

    #[test]
    fn migrates_v7_by_removing_legacy_secondary_search_actions() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut settings = AppSettings::default();
        settings.version = 7;
        let search = settings
            .actions
            .iter()
            .find(|action| action.id == "search")
            .unwrap()
            .clone();
        settings.actions.push(ActionDefinition {
            id: "search-bing".to_owned(),
            name: "legacy-bing".to_owned(),
            ..search.clone()
        });
        settings.actions.push(ActionDefinition {
            id: "search-baidu".to_owned(),
            name: "legacy-baidu".to_owned(),
            ..search
        });
        for (order, action) in settings.actions.iter_mut().enumerate() {
            action.order = order as u32;
        }
        fs::write(&path, serde_json::to_vec_pretty(&settings).unwrap()).unwrap();

        let migrated = SettingsRepository::new(&path).unwrap().get_settings();
        assert_eq!(migrated.version, SETTINGS_VERSION);
        let search_actions = migrated
            .actions
            .iter()
            .filter(|action| action.kind == ActionKind::Search)
            .map(|action| action.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(search_actions, vec!["search"]);
    }

    #[test]
    fn migrates_v1_provider_model_actions_and_plain_key() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(
            &path,
            r#"{
              "version": 1,
              "enabled": true,
              "captureShortcut": "",
              "locale": "zh-CN",
              "searchEngines": [
                {
                  "id": "google",
                  "name": "Google",
                  "template": "https://www.google.com/search?q={{text}}",
                  "builtin": true
                },
                {
                  "id": "bing-china",
                  "name": "Bing",
                  "template": "https://cn.bing.com/search?q={{text}}",
                  "builtin": true
                },
                {
                  "id": "baidu",
                  "name": "百度",
                  "template": "https://www.baidu.com/s?wd={{text}}",
                  "builtin": true
                }
              ],
              "activeSearchEngineId": "google",
              "translate": {"primaryLanguage":"zh-CN","alternateLanguage":"en-US"},
              "ai": {"baseUrl":"https://example.com/v1","model":"gpt-test","apiKey":"legacy-secret"},
              "actions": [
                {"id":"copy","name":"复制","icon":"copy","type":"copy","enabled":true,"order":0},
                {"id":"translate","name":"翻译","icon":"languages","type":"translate","enabled":true,"order":1}
              ]
            }"#,
        )
        .unwrap();
        let secrets = Arc::new(MemorySecrets::default());
        let repository = SettingsRepository::with_secret_store(&path, secrets).unwrap();
        let settings = repository.get_settings();
        assert_eq!(settings.version, SETTINGS_VERSION);
        assert_eq!(
            settings.translate.default_language,
            crate::models::TranslationLanguage::EnUs
        );
        assert_eq!(
            settings.translate.target_language,
            crate::models::TranslationLanguage::ZhCn
        );
        assert_eq!(settings.providers[0].models[0].id, "gpt-test");
        assert_eq!(settings.actions[1].model_id.as_deref(), Some("gpt-test"));
        assert!(repository.get_public_settings().unwrap().providers[0].key_configured);
        assert!(!fs::read_to_string(path).unwrap().contains("legacy-secret"));
    }

    #[test]
    fn persists_new_result_defaults_when_loading_older_v2_settings() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut value = serde_json::to_value(AppSettings::default()).unwrap();
        let result = value
            .get_mut("result")
            .and_then(serde_json::Value::as_object_mut)
            .unwrap();
        result.remove("dismissMode");
        result.remove("fontSize");
        fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        let repository =
            SettingsRepository::with_secret_store(&path, Arc::new(MemorySecrets::default()))
                .unwrap();
        let settings = repository.get_settings();
        assert_eq!(
            settings.result.dismiss_mode,
            crate::models::ResultDismissMode::Blur
        );
        assert_eq!(
            settings.result.font_size,
            crate::models::DEFAULT_RESULT_FONT_SIZE
        );

        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            persisted
                .pointer("/result/dismissMode")
                .and_then(|value| value.as_str()),
            Some("blur")
        );
        assert_eq!(
            persisted
                .pointer("/result/fontSize")
                .and_then(|value| value.as_u64()),
            Some(14)
        );
    }

    #[test]
    fn set_api_key_fails_if_provider_missing() {
        let directory = tempdir().unwrap();
        let secrets = Arc::new(MemorySecrets::default());
        let repository = SettingsRepository::with_secret_store(
            directory.path().join("settings.json"),
            secrets.clone(),
        )
        .unwrap();

        let err = repository.set_provider_api_key("gone", "sk-x").unwrap_err();
        assert!(matches!(
            err,
            SettingsError::ProviderNotFound(ref id) if id == "gone"
        ));
        assert!(secrets.get(&secret_account("gone")).unwrap().is_none());
    }

    #[test]
    fn set_api_key_after_delete_fails_and_does_not_leave_orphan_secret() {
        let directory = tempdir().unwrap();
        let secrets = Arc::new(MemorySecrets::default());
        let repository = SettingsRepository::with_secret_store(
            directory.path().join("settings.json"),
            secrets.clone(),
        )
        .unwrap();
        repository
            .set_provider_api_key(DEFAULT_PROVIDER_ID, "sk-before-delete")
            .unwrap();
        repository.delete_provider(DEFAULT_PROVIDER_ID).unwrap();

        let err = repository
            .set_provider_api_key(DEFAULT_PROVIDER_ID, "sk-orphan")
            .unwrap_err();
        assert!(matches!(
            err,
            SettingsError::ProviderNotFound(ref id) if id == DEFAULT_PROVIDER_ID
        ));
        assert!(secrets
            .get(&secret_account(DEFAULT_PROVIDER_ID))
            .unwrap()
            .is_none());
    }

    #[test]
    fn clear_api_key_fails_if_provider_missing() {
        let directory = tempdir().unwrap();
        let repository = SettingsRepository::with_secret_store(
            directory.path().join("settings.json"),
            Arc::new(MemorySecrets::default()),
        )
        .unwrap();

        let err = repository.clear_provider_api_key("gone").unwrap_err();
        assert!(matches!(
            err,
            SettingsError::ProviderNotFound(ref id) if id == "gone"
        ));
    }

    #[test]
    fn provider_deletion_unbinds_actions() {
        let directory = tempdir().unwrap();
        let secrets = Arc::new(MemorySecrets::default());
        let repository = SettingsRepository::with_secret_store(
            directory.path().join("settings.json"),
            secrets.clone(),
        )
        .unwrap();
        repository
            .set_provider_api_key(DEFAULT_PROVIDER_ID, "sk-to-delete")
            .unwrap();
        let public = repository.delete_provider(DEFAULT_PROVIDER_ID).unwrap();
        assert!(public.providers.is_empty());
        assert!(public
            .actions
            .iter()
            .filter(|action| action.kind.is_ai())
            .all(|action| action.provider_id.as_deref() == Some("")));
        assert!(public
            .actions
            .iter()
            .filter(|action| action.kind.is_ai())
            .all(|action| action.model_id.as_deref() == Some("")));
        assert!(secrets
            .get(&secret_account(DEFAULT_PROVIDER_ID))
            .unwrap()
            .is_none());
    }

    #[test]
    fn provider_deletion_rolls_back_when_secret_cleanup_fails() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let repository =
            SettingsRepository::with_secret_store(&path, Arc::new(DeleteFailingSecrets::default()))
                .unwrap();
        repository
            .set_provider_api_key(DEFAULT_PROVIDER_ID, "sk-must-survive")
            .unwrap();
        let before = fs::read_to_string(&path).unwrap();

        assert!(matches!(
            repository.delete_provider(DEFAULT_PROVIDER_ID),
            Err(SettingsError::SecretStorage)
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        let public = repository.get_public_settings().unwrap();
        assert_eq!(public.providers.len(), 1);
        assert!(public.providers[0].key_configured);
        assert!(public
            .actions
            .iter()
            .filter(|action| action.kind.is_ai())
            .all(|action| action.provider_id.as_deref() == Some(DEFAULT_PROVIDER_ID)));
    }

    #[test]
    fn full_settings_update_rolls_back_provider_removal_when_secret_cleanup_fails() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let repository =
            SettingsRepository::with_secret_store(&path, Arc::new(DeleteFailingSecrets::default()))
                .unwrap();
        repository
            .set_provider_api_key(DEFAULT_PROVIDER_ID, "sk-must-survive")
            .unwrap();
        let before = fs::read_to_string(&path).unwrap();
        let actions = repository
            .get_settings()
            .actions
            .into_iter()
            .map(|mut action| {
                if action.kind.is_ai() {
                    action.provider_id = Some(String::new());
                    action.model_id = Some(String::new());
                }
                action
            })
            .collect();

        assert!(matches!(
            repository.update(SettingsUpdate {
                providers: Some(Vec::new()),
                actions: Some(actions),
                ..Default::default()
            }),
            Err(SettingsError::SecretStorage)
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        let public = repository.get_public_settings().unwrap();
        assert_eq!(public.providers.len(), 1);
        assert!(public.providers[0].key_configured);
    }

    #[test]
    fn replacing_provider_list_removes_orphaned_secret_entries() {
        let directory = tempdir().unwrap();
        let secrets = Arc::new(MemorySecrets::default());
        let repository = SettingsRepository::with_secret_store(
            directory.path().join("settings.json"),
            secrets.clone(),
        )
        .unwrap();
        repository
            .set_provider_api_key(DEFAULT_PROVIDER_ID, "sk-to-remove")
            .unwrap();

        repository
            .update(SettingsUpdate {
                providers: Some(Vec::new()),
                actions: Some(
                    repository
                        .get_settings()
                        .actions
                        .into_iter()
                        .map(|mut action| {
                            if action.kind.is_ai() {
                                action.provider_id = Some(String::new());
                                action.model_id = Some(String::new());
                            }
                            action
                        })
                        .collect(),
                ),
                ..Default::default()
            })
            .unwrap();

        assert!(secrets
            .get(&secret_account(DEFAULT_PROVIDER_ID))
            .unwrap()
            .is_none());
    }

    #[test]
    fn migrates_electron_store_wrapped_v1_settings() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(
            &path,
            r#"{
              "settings": {
                "version": 1,
                "enabled": false,
                "searchTemplate": "https://example.com/?q={{text}}",
                "ai": {"baseUrl":"https://gateway.example/v1","model":"model-a"}
              },
              "encryptedApiKey": "not-plaintext-and-not-migrated"
            }"#,
        )
        .unwrap();
        let repository =
            SettingsRepository::with_secret_store(&path, Arc::new(MemorySecrets::default()))
                .unwrap();
        let settings = repository.get_settings();
        assert!(!settings.enabled);
        assert_eq!(settings.providers[0].base_url, "https://gateway.example/v1");
        assert_eq!(settings.providers[0].models[0].id, "model-a");
        assert!(!fs::read_to_string(path)
            .unwrap()
            .contains("encryptedApiKey"));
    }

    #[test]
    fn migrates_v2_without_losing_provider_action_or_secret_binding() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut v2 = AppSettings::default();
        v2.version = 2;
        v2.result.dismiss_mode = ResultDismissMode::Manual;
        v2.providers[0].name = "Private Gateway".to_owned();
        v2.providers[0].base_url = "http://gateway.example/v1".to_owned();
        v2.providers[0].models = vec![ProviderModel {
            id: "model-selected".to_owned(),
            name: "Selected Model".to_owned(),
            thinking_levels: Vec::new(),
            thinking_capability: None,
            thinking_enabled: true,
            thinking_default: None,
        }];
        for action in v2.actions.iter_mut().filter(|action| action.kind.is_ai()) {
            action.model_id = Some("model-selected".to_owned());
        }
        v2.actions[0].prompt = Some("保留这个自定义提示词：{{text}}".to_owned());
        let mut value = serde_json::to_value(v2).unwrap();
        value
            .pointer_mut("/result")
            .and_then(serde_json::Value::as_object_mut)
            .unwrap()
            .remove("fontSize");
        fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        let secrets = Arc::new(MemorySecrets::default());
        secrets
            .set(
                &secret_account(DEFAULT_PROVIDER_ID),
                "provider-specific-key",
            )
            .unwrap();
        let repository = SettingsRepository::with_secret_store(&path, secrets).unwrap();
        let migrated = repository.get_settings();
        assert_eq!(migrated.version, SETTINGS_VERSION);
        assert_eq!(migrated.result.dismiss_mode, ResultDismissMode::Blur);
        assert_eq!(
            migrated.result.font_size,
            crate::models::DEFAULT_RESULT_FONT_SIZE
        );
        assert_eq!(migrated.providers[0].name, "Private Gateway");
        assert_eq!(migrated.providers[0].models[0].id, "model-selected");
        assert_eq!(
            migrated.actions[0].prompt.as_deref(),
            Some("保留这个自定义提示词：{{text}}")
        );
        assert_eq!(
            migrated.actions[0].model_id.as_deref(),
            Some("model-selected")
        );
        assert!(repository.get_public_settings().unwrap().providers[0].key_configured);

        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(persisted["version"], SETTINGS_VERSION);
        assert_eq!(persisted["result"]["dismissMode"], "blur");
    }

    #[test]
    fn migrates_only_canonical_untouched_v3_default_prompts() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut v3 = AppSettings::default();
        v3.version = 3;
        let translate = v3
            .actions
            .iter_mut()
            .find(|action| action.kind == ActionKind::Translate)
            .unwrap();
        translate.prompt = Some(LEGACY_V3_TRANSLATE_PROMPT.to_owned());
        let mut second_translate = translate.clone();
        second_translate.id = "translate-second".to_owned();
        second_translate.name = "第二个翻译".to_owned();
        second_translate.enabled = false;
        second_translate.order = v3.actions.len() as u32;
        v3.actions.push(second_translate);
        v3.actions
            .iter_mut()
            .find(|action| action.kind == ActionKind::Summary)
            .unwrap()
            .prompt = Some(LEGACY_V3_SUMMARY_PROMPT.to_owned());
        v3.actions
            .iter_mut()
            .find(|action| action.kind == ActionKind::Refine)
            .unwrap()
            .prompt = Some(LEGACY_V3_REFINE_PROMPT.to_owned());
        let explain = v3
            .actions
            .iter_mut()
            .find(|action| action.kind == ActionKind::Explain)
            .unwrap();
        explain.prompt = Some("保留用户自己的解释规则：{{text}}".to_owned());
        fs::write(&path, serde_json::to_vec_pretty(&v3).unwrap()).unwrap();

        let repository =
            SettingsRepository::with_secret_store(&path, Arc::new(MemorySecrets::default()))
                .unwrap();
        let migrated = repository.get_settings();
        assert_eq!(migrated.version, SETTINGS_VERSION);
        assert_eq!(
            migrated
                .actions
                .iter()
                .find(|action| action.kind == ActionKind::Translate)
                .and_then(|action| action.prompt.as_deref()),
            Some(DEFAULT_TRANSLATE_PROMPT)
        );
        assert_eq!(
            migrated
                .actions
                .iter()
                .find(|action| action.kind == ActionKind::Explain)
                .and_then(|action| action.prompt.as_deref()),
            Some("保留用户自己的解释规则：{{text}}")
        );
        assert_eq!(
            migrated
                .actions
                .iter()
                .find(|action| action.id == "translate-second")
                .and_then(|action| action.prompt.as_deref()),
            Some(LEGACY_V3_TRANSLATE_PROMPT)
        );
        assert_eq!(
            migrated
                .actions
                .iter()
                .find(|action| action.id == "summary")
                .and_then(|action| action.prompt.as_deref()),
            Some(DEFAULT_SUMMARY_PROMPT)
        );
        assert_eq!(
            migrated
                .actions
                .iter()
                .find(|action| action.id == "refine")
                .and_then(|action| action.prompt.as_deref()),
            Some(DEFAULT_REFINE_PROMPT)
        );
    }

    #[test]
    fn migrates_only_canonical_untouched_v4_default_prompts() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut v4 = AppSettings::default();
        v4.version = 4;
        for action in &mut v4.actions {
            action.prompt = match action.kind {
                ActionKind::Translate => Some(LEGACY_V4_TRANSLATE_PROMPT.to_owned()),
                ActionKind::Summary => Some(LEGACY_V4_SUMMARY_PROMPT.to_owned()),
                ActionKind::Explain => Some(LEGACY_V4_EXPLAIN_PROMPT.to_owned()),
                ActionKind::Refine => Some(LEGACY_V4_REFINE_PROMPT.to_owned()),
                _ => action.prompt.clone(),
            };
        }
        let summary = v4
            .actions
            .iter_mut()
            .find(|action| action.kind == ActionKind::Summary)
            .unwrap();
        summary.prompt = Some(LEGACY_V4_SUMMARY_PROMPT.to_owned());
        let mut second_summary = summary.clone();
        second_summary.id = "summary-second".to_owned();
        second_summary.name = "第二个总结".to_owned();
        second_summary.enabled = false;
        second_summary.order = v4.actions.len() as u32;
        second_summary.prompt = Some("保留新增动作自己的总结规则：{{text}}".to_owned());
        v4.actions.push(second_summary);
        fs::write(&path, serde_json::to_vec_pretty(&v4).unwrap()).unwrap();

        let repository =
            SettingsRepository::with_secret_store(&path, Arc::new(MemorySecrets::default()))
                .unwrap();
        let migrated = repository.get_settings();
        assert_eq!(migrated.version, SETTINGS_VERSION);
        assert_eq!(
            migrated
                .actions
                .iter()
                .find(|action| action.id == "summary")
                .and_then(|action| action.prompt.as_deref()),
            Some(DEFAULT_SUMMARY_PROMPT)
        );
        assert_eq!(
            migrated
                .actions
                .iter()
                .find(|action| action.id == "summary-second")
                .and_then(|action| action.prompt.as_deref()),
            Some("保留新增动作自己的总结规则：{{text}}")
        );
        for (id, expected) in [
            ("translate", DEFAULT_TRANSLATE_PROMPT),
            ("summary", DEFAULT_SUMMARY_PROMPT),
            ("explain", DEFAULT_EXPLAIN_PROMPT),
            ("refine", DEFAULT_REFINE_PROMPT),
        ] {
            assert_eq!(
                migrated
                    .actions
                    .iter()
                    .find(|action| action.id == id)
                    .and_then(|action| action.prompt.as_deref()),
                Some(expected)
            );
        }
    }

    #[test]
    fn migrates_v10_quote_action_to_ask_ai() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut current = AppSettings::default();
        // Force a v10-shaped file with the legacy quote clipboard action.
        current.version = 10;
        if let Some(ask) = current
            .actions
            .iter_mut()
            .find(|action| action.id == "ask-ai")
        {
            ask.id = "quote".to_owned();
            ask.name = "引用".to_owned();
            ask.icon = "quote".to_owned();
            ask.kind = ActionKind::Ask; // will be rewritten via JSON kind
            ask.enabled = false;
            ask.prompt = None;
            ask.provider_id = None;
            ask.model_id = None;
        }
        let mut value = serde_json::to_value(&current).unwrap();
        if let Some(actions) = value
            .get_mut("actions")
            .and_then(serde_json::Value::as_array_mut)
        {
            for action in actions {
                let Some(object) = action.as_object_mut() else {
                    continue;
                };
                if object.get("id").and_then(serde_json::Value::as_str) == Some("quote") {
                    object.insert(
                        "kind".to_owned(),
                        serde_json::Value::String("quote".to_owned()),
                    );
                    object.remove("prompt");
                    object.remove("providerId");
                    object.remove("modelId");
                    object.remove("thinkingMode");
                }
            }
        }
        if let Some(object) = value.as_object_mut() {
            object.insert("version".to_owned(), serde_json::Value::Number(10.into()));
        }
        fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        let repository =
            SettingsRepository::with_secret_store(&path, Arc::new(MemorySecrets::default()))
                .unwrap();
        let migrated = repository.get_settings();
        assert_eq!(migrated.version, SETTINGS_VERSION);
        let ask = migrated
            .actions
            .iter()
            .find(|action| action.id == "ask-ai")
            .expect("quote becomes ask-ai");
        assert_eq!(ask.kind, ActionKind::Ask);
        assert_eq!(ask.name, "问AI");
        assert_eq!(ask.icon, "message-circle-question");
        // Preserves the previous quote enabled flag (fixture uses false).
        assert!(!ask.enabled);
        assert_eq!(ask.prompt.as_deref(), Some(DEFAULT_ASK_PROMPT));
        assert!(migrated.actions.iter().all(|action| action.id != "quote"));
        assert_eq!(
            migrated
                .actions
                .iter()
                .filter(|action| action.kind == ActionKind::Ask)
                .count(),
            1
        );
    }

    #[test]
    fn upgrades_only_the_untouched_builtin_v5_translation_prompt() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut current = AppSettings::default();
        current.version = 5;
        current
            .actions
            .iter_mut()
            .find(|action| action.id == "translate")
            .unwrap()
            .prompt = Some(LEGACY_V5_TRANSLATE_PROMPT.to_owned());
        let mut custom_translation = current
            .actions
            .iter()
            .find(|action| action.id == "translate")
            .unwrap()
            .clone();
        custom_translation.id = "translate-second".to_owned();
        custom_translation.name = "第二个翻译".to_owned();
        custom_translation.enabled = false;
        custom_translation.order = current.actions.len() as u32;
        custom_translation.prompt = Some(LEGACY_V5_TRANSLATE_PROMPT.to_owned());
        current.actions.push(custom_translation);
        fs::write(&path, serde_json::to_vec_pretty(&current).unwrap()).unwrap();

        let repository =
            SettingsRepository::with_secret_store(&path, Arc::new(MemorySecrets::default()))
                .unwrap();
        let migrated = repository.get_settings();
        assert_eq!(
            migrated
                .actions
                .iter()
                .find(|action| action.id == "translate")
                .and_then(|action| action.prompt.as_deref()),
            Some(DEFAULT_TRANSLATE_PROMPT)
        );
        assert_eq!(
            migrated
                .actions
                .iter()
                .find(|action| action.id == "translate-second")
                .and_then(|action| action.prompt.as_deref()),
            Some(LEGACY_V5_TRANSLATE_PROMPT)
        );
        assert!(fs::read_to_string(path)
            .unwrap()
            .contains("You are a professional multilingual translator."));
    }

    #[test]
    fn upgrades_only_canonical_untouched_v11_default_prompts() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut current = AppSettings::default();
        current.version = 11;
        current
            .actions
            .iter_mut()
            .find(|action| action.id == "translate")
            .unwrap()
            .prompt = Some(LEGACY_V11_TRANSLATE_PROMPT.to_owned());
        current
            .actions
            .iter_mut()
            .find(|action| action.id == "summary")
            .unwrap()
            .prompt = Some(LEGACY_V11_SUMMARY_PROMPT.to_owned());
        current
            .actions
            .iter_mut()
            .find(|action| action.id == "explain")
            .unwrap()
            .prompt = Some(LEGACY_V11_EXPLAIN_PROMPT.to_owned());
        let mut custom_explain = current
            .actions
            .iter()
            .find(|action| action.id == "explain")
            .unwrap()
            .clone();
        custom_explain.id = "explain-custom".to_owned();
        custom_explain.name = "自定义解释".to_owned();
        custom_explain.enabled = false;
        custom_explain.order = current.actions.len() as u32;
        custom_explain.prompt = Some(LEGACY_V11_EXPLAIN_PROMPT.to_owned());
        current.actions.push(custom_explain);
        fs::write(&path, serde_json::to_vec_pretty(&current).unwrap()).unwrap();

        let repository =
            SettingsRepository::with_secret_store(&path, Arc::new(MemorySecrets::default()))
                .unwrap();
        let migrated = repository.get_settings();
        assert_eq!(
            migrated
                .actions
                .iter()
                .find(|action| action.id == "translate")
                .and_then(|action| action.prompt.as_deref()),
            Some(DEFAULT_TRANSLATE_PROMPT)
        );
        assert_eq!(
            migrated
                .actions
                .iter()
                .find(|action| action.id == "summary")
                .and_then(|action| action.prompt.as_deref()),
            Some(DEFAULT_SUMMARY_PROMPT)
        );
        assert_eq!(
            migrated
                .actions
                .iter()
                .find(|action| action.id == "explain")
                .and_then(|action| action.prompt.as_deref()),
            Some(DEFAULT_EXPLAIN_PROMPT)
        );
        assert_eq!(
            migrated
                .actions
                .iter()
                .find(|action| action.id == "explain-custom")
                .and_then(|action| action.prompt.as_deref()),
            Some(LEGACY_V11_EXPLAIN_PROMPT)
        );
        assert!(DEFAULT_EXPLAIN_PROMPT.contains("整体解释"));
        assert!(DEFAULT_EXPLAIN_PROMPT.contains("核心意思"));
        assert!(!DEFAULT_EXPLAIN_PROMPT.contains("必要上下文"));
    }

    #[test]
    fn migrates_v12_provider_and_model_thinking_defaults() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut value = serde_json::to_value(AppSettings::default()).unwrap();
        value["version"] = serde_json::json!(12);
        value["providers"][0]
            .as_object_mut()
            .unwrap()
            .remove("protocol");
        value["providers"][0]["models"] = serde_json::json!([{
            "id": "gpt-5",
            "name": "GPT-5",
            "thinkingLevels": ["low", "medium", "high"]
        }]);
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

        let settings = SettingsRepository::new(&path).unwrap().get_settings();
        assert_eq!(settings.version, SETTINGS_VERSION);
        assert_eq!(
            settings.providers[0].protocol,
            crate::models::ProviderProtocol::OpenAiChat
        );
        assert!(settings.providers[0].models[0].thinking_enabled);
        assert_eq!(settings.providers[0].models[0].thinking_capability, None);
        assert_eq!(
            settings.providers[0].models[0].thinking_default,
            Some(ThinkingLevel::Medium)
        );
    }

    #[test]
    fn migrates_tight_explain_default_that_mentioned_necessary_context() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut current = AppSettings::default();
        current
            .actions
            .iter_mut()
            .find(|action| action.id == "explain")
            .unwrap()
            .prompt = Some(LEGACY_V11_TIGHT_EXPLAIN_PROMPT.to_owned());
        fs::write(&path, serde_json::to_vec_pretty(&current).unwrap()).unwrap();

        let repository =
            SettingsRepository::with_secret_store(&path, Arc::new(MemorySecrets::default()))
                .unwrap();
        let migrated = repository.get_settings();
        assert_eq!(
            migrated
                .actions
                .iter()
                .find(|action| action.id == "explain")
                .and_then(|action| action.prompt.as_deref()),
            Some(DEFAULT_EXPLAIN_PROMPT)
        );
    }

    #[test]
    fn migrates_v5_with_default_close_behavior_and_persists_v7() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut value = serde_json::to_value(AppSettings::default()).unwrap();
        value["version"] = serde_json::json!(5);
        value.as_object_mut().unwrap().remove("application");
        fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        let repository =
            SettingsRepository::with_secret_store(&path, Arc::new(MemorySecrets::default()))
                .unwrap();
        let migrated = repository.get_settings();
        assert_eq!(migrated.version, SETTINGS_VERSION);
        assert_eq!(
            migrated.application.close_behavior,
            crate::models::ApplicationCloseBehavior::HideToTray
        );

        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(persisted["version"], SETTINGS_VERSION);
        assert_eq!(persisted["application"]["closeBehavior"], "hide-to-tray");
    }

    #[test]
    fn close_behavior_update_round_trips() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let repository =
            SettingsRepository::with_secret_store(&path, Arc::new(MemorySecrets::default()))
                .unwrap();

        let saved = repository
            .update(SettingsUpdate {
                application: Some(crate::models::ApplicationSettings {
                    close_behavior: crate::models::ApplicationCloseBehavior::Quit,
                }),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            saved.application.close_behavior,
            crate::models::ApplicationCloseBehavior::Quit
        );

        drop(repository);
        let reopened =
            SettingsRepository::with_secret_store(&path, Arc::new(MemorySecrets::default()))
                .unwrap();
        assert_eq!(
            reopened.get_settings().application.close_behavior,
            crate::models::ApplicationCloseBehavior::Quit
        );
    }

    #[test]
    fn search_engine_preference_migrates_from_v9_global_to_action() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        // Seed a v9-style payload with global active engine.
        let _repository =
            SettingsRepository::with_secret_store(&path, Arc::new(MemorySecrets::default()))
                .unwrap();
        let mut payload: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let object = payload.as_object_mut().unwrap();
        object.insert("version".to_owned(), serde_json::Value::Number(9.into()));
        object.insert(
            "activeSearchEngineId".to_owned(),
            serde_json::Value::String("baidu".to_owned()),
        );
        object.insert(
            "searchEngines".to_owned(),
            serde_json::to_value(crate::models::default_search_engines()).unwrap(),
        );
        std::fs::write(&path, serde_json::to_vec_pretty(&payload).unwrap()).unwrap();

        let migrated =
            SettingsRepository::with_secret_store(&path, Arc::new(MemorySecrets::default()))
                .unwrap();
        let settings = migrated.get_settings();
        let search = settings
            .actions
            .iter()
            .find(|action| action.kind == ActionKind::Search)
            .expect("search action");
        assert_eq!(search.search_engine_id.as_deref(), Some("baidu"));
        assert_eq!(settings.version, crate::models::SETTINGS_VERSION);
        let persisted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(persisted.get("activeSearchEngineId").is_none());
        assert!(persisted.get("searchEngines").is_none());
        assert_eq!(persisted["version"], crate::models::SETTINGS_VERSION);
    }

    #[test]
    fn failed_validation_does_not_change_file_or_memory() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let repository =
            SettingsRepository::with_secret_store(&path, Arc::new(MemorySecrets::default()))
                .unwrap();
        let before = fs::read_to_string(&path).unwrap();
        let result = repository.update(SettingsUpdate {
            capture_shortcut: Some("!!!invalid!!!".to_owned()),
            trigger: Some(crate::models::TriggerSettings {
                mode: crate::models::TriggerMode::Shortcut,
            }),
            ..Default::default()
        });
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), before);
    }

    #[test]
    fn ordinary_settings_updates_preserve_runtime_owned_result_size() {
        let directory = tempdir().unwrap();
        let repository = SettingsRepository::with_secret_store(
            directory.path().join("settings.json"),
            Arc::new(MemorySecrets::default()),
        )
        .unwrap();
        let remembered = WindowSize {
            width: 812.0,
            height: 536.0,
        };
        repository.update_result_last_size(remembered).unwrap();

        let mut stale_result = repository.get_settings().result;
        stale_result.opacity = 0.8;
        stale_result.last_size = None;
        let saved = repository
            .update(SettingsUpdate {
                result: Some(stale_result),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(saved.result.opacity, 0.8);
        assert_eq!(saved.result.last_size, Some(remembered));
        let reset = repository.clear_result_last_size().unwrap();
        assert_eq!(reset.result.last_size, None);
    }

    #[test]
    fn settings_round_trip_preserves_optional_thinking_capability() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let repository = SettingsRepository::new(&path).unwrap();
        let mut settings = repository.get_settings();
        settings.providers[0].models = vec![ProviderModel {
            id: "o3-mini".to_owned(),
            name: "o3-mini".to_owned(),
            thinking_levels: vec![ThinkingLevel::Low, ThinkingLevel::Medium],
            thinking_capability: Some(ThinkingCapability {
                source: ThinkingCapabilitySource::Explicit,
                dialect: Some(ThinkingDialect::ReasoningEffort),
                supports_off: false,
            }),
            thinking_enabled: true,
            thinking_default: None,
        }];
        repository
            .update(SettingsUpdate {
                providers: Some(settings.providers),
                ..Default::default()
            })
            .unwrap();
        drop(repository);
        let reopened = SettingsRepository::new(&path).unwrap();
        assert_eq!(reopened.get_settings().version, SETTINGS_VERSION);
        assert_eq!(
            reopened.get_settings().providers[0].models[0].thinking_capability,
            Some(ThinkingCapability {
                source: ThinkingCapabilitySource::Explicit,
                dialect: Some(ThinkingDialect::ReasoningEffort),
                supports_off: false,
            })
        );
    }
}
