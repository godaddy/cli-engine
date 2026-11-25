//! [`PkceAuthProvider`]'s implementation of the engine's [`AuthProvider`] trait:
//! how a credential is resolved for a command, reported by `auth status`, and cleared.
//!
//! A trait impl rather than methods, so it can live apart from the provider's
//! inherent impl, which the lint against multiple inherent impls keeps in one place.

use async_trait::async_trait;

use super::{
    PkceAuthProvider, StepUp, Verification, ensure_granted, enumerate, granted_scopes,
    plan_step_up, union_scopes,
};
use crate::{Credential, Result, auth::AuthProvider, auth::CredentialRequest, error::CliCoreError};

#[async_trait]
impl AuthProvider for PkceAuthProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn get_credential(&self, env: &str, _command: &str, _tier: &str) -> Result<Credential> {
        let token = self.resolve_token(env).await?;
        self.bearer_credential(env, &token)
    }

    async fn get_credential_for(&self, req: &CredentialRequest<'_>) -> Result<Credential> {
        let env = req.env;
        let required = &req.meta.scopes;

        // Look for a usable token WITHOUT launching a flow, so we can pick the
        // scope set for a single login rather than authenticating twice (e.g.
        // `auth login --scope X` logs out first; resolving defaults and then
        // stepping up would open the browser twice).
        if let Some(token) = self.existing_token(env).await? {
            // Decide based on what the token grants (JWT claim plus the scopes it
            // was obtained with).
            let granted = granted_scopes(&token);
            match plan_step_up(&granted, required, &self.scope_hierarchy) {
                StepUp::Covered => return self.bearer_credential(env, &token),
                // Step-up is re-consent: the authorization server has no silent
                // scope-expansion grant, so acquire the missing scopes with a
                // fresh login — the same browser flow the no-token path runs
                // below, rather than failing when stdio is not a TTY. Resolve
                // per-env defaults only now (off the cached-token hot path) and
                // request defaults ∪ already-granted ∪ required so step-up never
                // drops previously-acquired scopes.
                StepUp::Reauthenticate => {
                    let union = union_scopes(&self.effective_scopes(env)?, &granted, required);
                    let token = self.reauthenticate(env, &union).await?;
                    ensure_granted(env, &token, required, &self.scope_hierarchy)?;
                    return self.bearer_credential(env, &token);
                }
            }
        }

        // No usable token: authenticate once, requesting defaults ∪ required.
        let union = union_scopes(&self.effective_scopes(env)?, &[], required);
        let token = self.reauthenticate(env, &union).await?;
        ensure_granted(env, &token, required, &self.scope_hierarchy)?;
        self.bearer_credential(env, &token)
    }

    async fn status(&self, env: &str) -> Result<Credential> {
        let Some(mut token) = self.load_stored(env).await else {
            return Err(CliCoreError::message(format!(
                "not logged in for environment {env:?}"
            )));
        };
        // Reported as an error, not shown with a blank identity: the stored copy
        // is writable by the user's other processes, so nothing in it is trusted
        // until the verifier has accepted it.
        if let Verification::Rejected(reason) = self.verify_token(env, &mut token).await? {
            return Err(CliCoreError::message(format!(
                "the stored id_token for environment {env:?} failed verification ({reason}); log in again"
            )));
        }
        self.bearer_credential(env, &token)
    }

    async fn logout(&self, env: &str) -> Result<()> {
        self.delete_stored(env).await;
        let mut cache = self.cache.write().await;
        cache.remove(env);
        Ok(())
    }

    async fn list_environments(&self) -> Result<Vec<String>> {
        // Storage backends cannot enumerate, so this reads the provider's own
        // index (plus a probe of declared environments for credentials saved before
        // there was one) and keeps only names the backend really holds.
        Ok(enumerate::environments_with_credentials(self).await)
    }
}
