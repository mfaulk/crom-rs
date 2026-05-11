//! In-memory secret backend used by tests and `examples/auth-demo`.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::error::AuthError;
use crate::types::{AccountId, ProviderId, SecretString, StoredCredentialSecret};

use super::SecretBackend;

#[derive(Default)]
pub struct FakeKeychainBackend {
    inner: Mutex<BTreeMap<String, SerializableSecret>>,
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct SerializableSecret {
    access_token: Option<String>,
    refresh_token: Option<String>,
    api_key: Option<String>,
    id_token: Option<String>,
}

impl From<&StoredCredentialSecret> for SerializableSecret {
    fn from(value: &StoredCredentialSecret) -> Self {
        Self {
            access_token: value.access_token.as_ref().map(|s| s.expose().to_string()),
            refresh_token: value.refresh_token.as_ref().map(|s| s.expose().to_string()),
            api_key: value.api_key.as_ref().map(|s| s.expose().to_string()),
            id_token: value.id_token.as_ref().map(|s| s.expose().to_string()),
        }
    }
}

impl From<SerializableSecret> for StoredCredentialSecret {
    fn from(value: SerializableSecret) -> Self {
        Self {
            access_token: value.access_token.map(SecretString::new),
            refresh_token: value.refresh_token.map(SecretString::new),
            api_key: value.api_key.map(SecretString::new),
            id_token: value.id_token.map(SecretString::new),
        }
    }
}

fn key(provider: &ProviderId, account_id: &AccountId) -> String {
    format!("{}::{}", provider.as_str(), account_id.as_str())
}

impl SecretBackend for FakeKeychainBackend {
    fn name(&self) -> &'static str {
        "fake_keychain"
    }

    fn put(
        &self,
        provider: &ProviderId,
        account_id: &AccountId,
        secret: &StoredCredentialSecret,
    ) -> Result<(), AuthError> {
        self.inner
            .lock()
            .unwrap()
            .insert(key(provider, account_id), secret.into());
        Ok(())
    }

    fn get(
        &self,
        provider: &ProviderId,
        account_id: &AccountId,
    ) -> Result<Option<StoredCredentialSecret>, AuthError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .get(&key(provider, account_id))
            .cloned()
            .map(Into::into))
    }

    fn delete(&self, provider: &ProviderId, account_id: &AccountId) -> Result<(), AuthError> {
        self.inner
            .lock()
            .unwrap()
            .remove(&key(provider, account_id));
        Ok(())
    }
}
